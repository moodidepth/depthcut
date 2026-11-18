//! Viewer: GPU-rendered frame, fit/zoom, timecode, transport, safe zones,
//! before/after wipe and on-screen transform / crop gizmos.

use crate::app::{drag_key, App};
use crate::theme;
use eframe::egui::{self, Color32, CornerRadius, Pos2, Rect, Sense, Stroke, StrokeKind, Vec2};
use nle_core::eval::clip_layer;
use nle_core::{format_timecode, parse_timecode, ClipId, ClipKind, Command, Time};
use nle_render::transform::{fit_size, layer_corners, layer_to_output};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Gizmo {
    None,
    Transform,
    Crop,
}

const GIZMO_TAG: u64 = 0x61;

pub fn show(app: &mut App, ui: &mut egui::Ui) {
    // ----- top bar
    ui.horizontal(|ui| {
        let title = match app.browser.skim {
            Some((a, _)) => app.project.asset(a).map(|a| a.name.clone()).unwrap_or_default(),
            None => app.project.timeline.name.clone(),
        };
        ui.label(egui::RichText::new(title).strong());
        ui.label(egui::RichText::new(app.project.timeline.format.label()).small().color(theme::TEXT_DIM));
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            let zl = match app.viewer.zoom {
                None => "Fit".to_string(),
                Some(z) => format!("{:.0}%", z * 100.0),
            };
            egui::ComboBox::from_id_salt("viewer-zoom").selected_text(zl).width(64.0).show_ui(ui, |ui| {
                ui.selectable_value(&mut app.viewer.zoom, None, "Fit");
                for z in [0.25f32, 0.5, 1.0, 2.0] {
                    ui.selectable_value(&mut app.viewer.zoom, Some(z), format!("{:.0}%", z * 100.0));
                }
            });
            ui.menu_button("View ▾", |ui| {
                ui.checkbox(&mut app.viewer.safe_zones, "Title/Action Safe Zones");
                let mut wipe = app.viewer.wipe.is_some();
                if ui.checkbox(&mut wipe, "Before/After Wipe").changed() {
                    app.viewer.wipe = if wipe { Some(0.5) } else { None };
                }
                let mut half = app.viewer.half_res;
                if ui.checkbox(&mut half, "Better Performance (half resolution)").changed() {
                    app.viewer.half_res = half;
                    app.resize_preview();
                }
                ui.separator();
                let mut scopes = app.viewer.scopes;
                if ui.checkbox(&mut scopes, format!("Video Scopes ({})", app.keymap.hint(nle_core::keymap::Action::ToggleScopes))).changed() {
                    app.set_scopes(scopes);
                }
            });
            ui.selectable_value(&mut app.viewer.gizmo, Gizmo::Crop, "⛶ Crop");
            ui.selectable_value(&mut app.viewer.gizmo, Gizmo::Transform, "✥ Transform");
            ui.selectable_value(&mut app.viewer.gizmo, Gizmo::None, "Off");
        });
    });

    // ----- image area
    let transport_h = 46.0;
    let scopes_h = if app.viewer.scopes { (ui.available_height() * 0.38).clamp(150.0, 330.0) } else { 0.0 };
    let area = Rect::from_min_size(ui.cursor().min, Vec2::new(ui.available_width(), (ui.available_height() - transport_h - scopes_h).max(80.0)));
    let resp = ui.allocate_rect(area, Sense::click_and_drag());
    ui.painter().rect_filled(area, CornerRadius::same(4), theme::VIEWER_BG);
    let fmt = app.project.timeline.format;
    let (fw, fh) = (fmt.width as f32, fmt.height as f32);
    let fit = {
        let s = (area.width() / fw).min(area.height() / fh);
        Vec2::new(fw * s, fh * s)
    };
    let size = match app.viewer.zoom {
        None => fit,
        Some(z) => {
            let ppp = ui.ctx().pixels_per_point();
            let (pw, ph) = app.preview.as_ref().map(|p| p.renderer.size()).unwrap_or((fmt.width, fmt.height));
            let _ = ph;
            Vec2::new(fw, fh) * (z * pw as f32 / fw / ppp)
        }
    };
    let img = Rect::from_center_size(area.center(), size);
    let clip_painter = ui.painter().with_clip_rect(area);
    match &app.preview {
        Some(p) => {
            clip_painter.image(p.texture, img, Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)), Color32::WHITE);
        }
        None => {
            clip_painter.text(area.center(), egui::Align2::CENTER_CENTER, "No GPU renderer available", egui::FontId::proportional(16.0), theme::TEXT_DIM);
        }
    }
    let to_screen = |p: [f32; 2]| Pos2::new(img.left() + p[0] / fw * img.width(), img.top() + p[1] / fh * img.height());

    if app.viewer.safe_zones {
        for (frac, label) in [(0.9f32, "Action safe"), (0.8, "Title safe")] {
            let r = Rect::from_center_size(img.center(), img.size() * frac);
            clip_painter.rect_stroke(r, CornerRadius::ZERO, Stroke::new(1.0_f32, Color32::from_white_alpha(110)), StrokeKind::Inside);
            clip_painter.text(r.left_top() + Vec2::new(4.0, 2.0), egui::Align2::LEFT_TOP, label, egui::FontId::proportional(10.0), Color32::from_white_alpha(140));
        }
        let c = img.center();
        clip_painter.line_segment([c - Vec2::new(10.0, 0.0), c + Vec2::new(10.0, 0.0)], Stroke::new(1.0_f32, Color32::from_white_alpha(120)));
        clip_painter.line_segment([c - Vec2::new(0.0, 10.0), c + Vec2::new(0.0, 10.0)], Stroke::new(1.0_f32, Color32::from_white_alpha(120)));
    }

    // before/after wipe handle
    let mut wipe_drag = false;
    if let Some(w) = app.viewer.wipe {
        let x = img.left() + w * img.width();
        let handle = Rect::from_center_size(Pos2::new(x, img.bottom() - 16.0), Vec2::splat(18.0));
        let hr = ui.interact(handle.expand(4.0), ui.id().with("wipe"), Sense::drag());
        clip_painter.circle_filled(handle.center(), 9.0, theme::PLAYHEAD);
        clip_painter.text(handle.center(), egui::Align2::CENTER_CENTER, "⟷", egui::FontId::proportional(11.0), Color32::WHITE);
        clip_painter.text(Pos2::new(x - 8.0, img.top() + 8.0), egui::Align2::RIGHT_TOP, "BEFORE", egui::FontId::proportional(10.0), Color32::WHITE);
        clip_painter.text(Pos2::new(x + 8.0, img.top() + 8.0), egui::Align2::LEFT_TOP, "AFTER", egui::FontId::proportional(10.0), Color32::WHITE);
        if hr.dragged() {
            if let Some(p) = hr.interact_pointer_pos() {
                app.viewer.wipe = Some(((p.x - img.left()) / img.width()).clamp(0.0, 1.0));
            }
            wipe_drag = true;
        }
    }

    if app.punch.picking {
        pick_punch_anchor(app, ui, &resp, img, (fw, fh));
    } else if app.browser.skim.is_none() && !app.playback.playing && !wipe_drag {
        gizmo(app, ui, &resp, img, (fw, fh), &to_screen);
    }
    if resp.double_clicked() {
        app.viewer.zoom = None;
    }

    // ----- video scopes drawer
    if app.viewer.scopes {
        scopes_drawer(app, ui, scopes_h);
    }

    // ----- transport bar
    transport(app, ui);
}

/// The video scopes drawer: toolbar plus the selected scopes side by side,
/// each drawn by the renderer into its own texture.
fn scopes_drawer(app: &mut App, ui: &mut egui::Ui, height: f32) {
    use nle_render::ScopeKind;
    let rect = Rect::from_min_size(ui.cursor().min, Vec2::new(ui.available_width(), height));
    ui.allocate_rect(rect, Sense::hover());
    let mut child = ui.new_child(egui::UiBuilder::new().max_rect(rect.shrink2(Vec2::new(2.0, 4.0))));
    let ui = &mut child;
    let mut changed = false;
    ui.horizontal(|ui| {
        ui.label(egui::RichText::new("Scopes").strong());
        for (i, k) in ScopeKind::ALL.iter().enumerate() {
            ui.toggle_value(&mut app.viewer.scope_kinds[i], k.label());
        }
        ui.separator();
        let Some(p) = app.preview.as_mut() else { return };
        let s = &mut p.renderer.scope_settings;
        if app.viewer.scope_kinds[0] {
            changed |= ui.selectable_value(&mut s.rgb_waveform, false, "Luma").on_hover_text("Waveform shows luma").changed();
            changed |= ui.selectable_value(&mut s.rgb_waveform, true, "RGB").on_hover_text("Waveform overlays red, green and blue").changed();
            ui.separator();
        }
        ui.label("Brightness");
        changed |= ui.add(egui::Slider::new(&mut s.gain, 0.25..=8.0).logarithmic(true).show_value(false)).changed();
        if app.viewer.scope_kinds[2] {
            ui.label("Vector zoom");
            changed |= ui.selectable_value(&mut s.vec_zoom, 1.0, "1×").changed();
            changed |= ui.selectable_value(&mut s.vec_zoom, 2.0, "2×").on_hover_text("Magnify the centre: the rim is 50 % saturation").changed();
        }
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if ui.small_button("✕").on_hover_text("Hide scopes").clicked() {
                app.set_scopes(false);
            }
        });
    });
    if changed {
        app.refresh_preview();
    }
    let Some(ids) = app.preview.as_ref().and_then(|p| p.scope_textures) else {
        ui.label(egui::RichText::new("Scopes need the GPU renderer").color(theme::TEXT_DIM));
        return;
    };
    let shown: Vec<(ScopeKind, egui::TextureId)> = ScopeKind::ALL.iter().zip(ids).zip(app.viewer.scope_kinds).filter(|(_, on)| *on).map(|((k, id), _)| (*k, id)).collect();
    if shown.is_empty() {
        ui.label(egui::RichText::new("Pick a scope above").color(theme::TEXT_DIM));
        return;
    }
    let avail = Rect::from_min_size(ui.cursor().min, ui.available_size());
    let gap = 6.0;
    // widths proportional to each scope's aspect at the drawer height
    let aspect = |k: ScopeKind| {
        let (w, h) = k.size();
        w as f32 / h as f32
    };
    let total: f32 = shown.iter().map(|(k, _)| aspect(*k)).sum();
    let h = avail.height().min((avail.width() - gap * (shown.len() as f32 - 1.0)) / total).max(20.0);
    let mut x = avail.left();
    let painter = ui.painter_at(avail);
    for (k, id) in shown {
        let r = Rect::from_min_size(Pos2::new(x, avail.top()), Vec2::new(h * aspect(k), h));
        painter.rect_filled(r, CornerRadius::same(3), Color32::BLACK);
        painter.image(id, r, Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)), Color32::WHITE);
        painter.text(r.left_top() + Vec2::new(5.0, 3.0), egui::Align2::LEFT_TOP, k.label(), egui::FontId::proportional(10.0), Color32::from_gray(150));
        x = r.right() + gap;
    }
}

/// Native size of `clip`'s layer as the compositor fits it (the 9:16
/// reframe effect outputs a 9:16 layer).
fn layer_size(app: &App, clip: &nle_core::Clip, layer: &nle_core::eval::Layer, frame: (f32, f32)) -> [f32; 2] {
    let lsize = match &clip.kind {
        ClipKind::Asset { asset } => app.project.asset(*asset).and_then(|a| a.video.as_ref()).map(|v| [v.width as f32, v.height as f32]),
        _ => None,
    }
    .unwrap_or([frame.0, frame.1]);
    let (rw, rh) = nle_render::reframe::stack_output_size(&layer.effects, (lsize[0] as u32, lsize[1] as u32), frame.1 as u32);
    [rw as f32, rh as f32]
}

/// Frame size ÷ fitted layer size per axis — converts frame fractions to
/// layer-space anchor units for the zoom punch's "Center" anchor.
pub fn frame_per_layer(app: &App, clip: &nle_core::Clip, local: Time) -> [f64; 2] {
    let fmt = app.project.timeline.format;
    let frame = (fmt.width as f32, fmt.height as f32);
    let Some(layer) = clip_layer(clip, local, clip.lane) else { return [1.0, 1.0] };
    let [lw, lh] = layer_size(app, clip, &layer, frame);
    let [fw, fh] = fit_size(lw, lh, frame.0, frame.1);
    [(frame.0 / fw.max(1.0)) as f64, (frame.1 / fh.max(1.0)) as f64]
}

/// "Custom" punch anchor: the next click on the preview sets the anchor of
/// the target clip to the layer point under the cursor.
fn pick_punch_anchor(app: &mut App, ui: &mut egui::Ui, resp: &egui::Response, img: Rect, frame: (f32, f32)) {
    use nle_core::zoom_punch::PunchAnchor;
    let p = ui.painter().with_clip_rect(img);
    p.rect_stroke(img, CornerRadius::ZERO, Stroke::new(2.0_f32, theme::SNAP), StrokeKind::Inside);
    p.text(img.center_top() + Vec2::new(0.0, 10.0), egui::Align2::CENTER_TOP, "Click the zoom-punch anchor (Esc cancels)", egui::FontId::proportional(13.0), theme::SNAP);
    ui.ctx().set_cursor_icon(egui::CursorIcon::Crosshair);
    if ui.input(|i| i.key_pressed(egui::Key::Escape)) {
        app.punch.picking = false;
        return;
    }
    if !resp.clicked() {
        return;
    }
    let Some(pos) = resp.interact_pointer_pos() else { return };
    app.punch.picking = false;
    let Some(id) = app.target_clip() else { return app.status("Select a clip to anchor the zoom punch on") };
    let Some(mut clip) = app.project.timeline.clip(id).cloned() else { return };
    let local = app.local_time(id);
    let Some(layer) = clip_layer(&clip, local, clip.lane) else { return };
    let lsize = layer_size(app, &clip, &layer, frame);
    let out = [(pos.x - img.left()) / img.width() * frame.0, (pos.y - img.top()) / img.height() * frame.1];
    let Some(inv) = layer_to_output(&layer.video, lsize, [frame.0, frame.1]).inverse() else { return };
    let uv = inv.apply(out);
    let (x, y) = (((uv[0] - 0.5) as f64).clamp(-0.5, 0.5), ((uv[1] - 0.5) as f64).clamp(-0.5, 0.5));
    app.punch.anchor = PunchAnchor::Custom { x, y };
    clip.video.anchor_x = nle_core::Param::new(x);
    clip.video.anchor_y = nle_core::Param::new(y);
    if app.exec(Command::UpdateClip { clip: Box::new(clip) }) {
        app.status(format!("Zoom-punch anchor set ({:+.0} %, {:+.0} %)", x * 100.0, y * 100.0));
    }
}

fn gizmo(app: &mut App, ui: &mut egui::Ui, resp: &egui::Response, img: Rect, frame: (f32, f32), to_screen: &dyn Fn([f32; 2]) -> Pos2) {
    if app.viewer.gizmo == Gizmo::None {
        return;
    }
    let Some(id) = app.selection.first().copied() else { return };
    let Some(clip) = app.project.timeline.clip(id).cloned() else { return };
    let Some(start) = app.project.timeline.clip_start(id) else { return };
    let t = app.view_time();
    if t < start || t >= start + clip.duration || matches!(clip.kind, ClipKind::Gap) {
        return;
    }
    let local = t - start;
    let Some(layer) = clip_layer(&clip, local, clip.lane) else { return };
    let lsize = layer_size(app, &clip, &layer, frame);
    let corners = layer_corners(&layer.video, lsize, [frame.0, frame.1]).map(to_screen);
    let centre = Pos2::new(corners.iter().map(|p| p.x).sum::<f32>() / 4.0, corners.iter().map(|p| p.y).sum::<f32>() / 4.0);
    let p = ui.painter().with_clip_rect(img.expand(40.0));
    let col = if app.viewer.gizmo == Gizmo::Crop { theme::SNAP } else { Color32::WHITE };
    p.add(egui::Shape::closed_line(corners.to_vec(), Stroke::new(1.5_f32, col)));

    let screen_per_frame = img.width() / frame.0;
    let key = Some(drag_key(GIZMO_TAG, id));
    let mut edited = clip.clone();
    let mut changed = false;

    match app.viewer.gizmo {
        Gizmo::Transform => {
            // rotation handle above the top edge
            let top_mid = corners[0] + (corners[1] - corners[0]) * 0.5;
            let up = (top_mid - centre).normalized();
            let rot_handle = top_mid + up * 24.0;
            p.line_segment([top_mid, rot_handle], Stroke::new(1.0_f32, col));
            p.circle_filled(rot_handle, 5.0, theme::VIDEO);
            p.circle_stroke(centre, 6.0, Stroke::new(1.5_f32, col));
            p.line_segment([centre - Vec2::new(9.0, 0.0), centre + Vec2::new(9.0, 0.0)], Stroke::new(1.0_f32, col));
            p.line_segment([centre - Vec2::new(0.0, 9.0), centre + Vec2::new(0.0, 9.0)], Stroke::new(1.0_f32, col));
            for (i, c) in corners.iter().enumerate() {
                let r = ui.interact(Rect::from_center_size(*c, Vec2::splat(14.0)), ui.id().with(("gz-corner", i)), Sense::drag());
                p.rect_filled(Rect::from_center_size(*c, Vec2::splat(9.0)), CornerRadius::same(2), if r.hovered() { theme::VIDEO } else { Color32::WHITE });
                if r.dragged() {
                    if let Some(pos) = r.interact_pointer_pos() {
                        let d0 = (pos - r.drag_delta() - centre).length().max(1.0);
                        let d1 = (pos - centre).length().max(1.0);
                        let k = (d1 / d0) as f64;
                        let sx = edited.video.scale_x.at(local) * k;
                        let sy = edited.video.scale_y.at(local) * k;
                        edited.video.scale_x.set(local, sx.clamp(0.01, 20.0));
                        edited.video.scale_y.set(local, sy.clamp(0.01, 20.0));
                        changed = true;
                    }
                }
                r.on_hover_cursor(egui::CursorIcon::ResizeNwSe);
            }
            let rr = ui.interact(Rect::from_center_size(rot_handle, Vec2::splat(14.0)), ui.id().with("gz-rot"), Sense::drag());
            if rr.dragged() {
                if let Some(pos) = rr.interact_pointer_pos() {
                    let a0 = (pos - rr.drag_delta() - centre).angle();
                    let a1 = (pos - centre).angle();
                    let d = (a1 - a0).to_degrees() as f64;
                    let rot = edited.video.rotation.at(local) + d;
                    edited.video.rotation.set(local, rot);
                    changed = true;
                }
            }
            // drag inside the frame = move
            if !changed && resp.dragged() && resp.drag_delta() != Vec2::ZERO {
                let d = resp.drag_delta() / screen_per_frame;
                let px = edited.video.position_x.at(local) + (d.x / frame.0) as f64;
                let py = edited.video.position_y.at(local) + (d.y / frame.1) as f64;
                edited.video.position_x.set(local, px);
                edited.video.position_y.set(local, py);
                changed = true;
            }
        }
        Gizmo::Crop => {
            // edge handles: left, right, top, bottom
            let mids = [
                (corners[3] + (corners[0] - corners[3]) * 0.5, 0usize),
                (corners[1] + (corners[2] - corners[1]) * 0.5, 1),
                (corners[0] + (corners[1] - corners[0]) * 0.5, 2),
                (corners[2] + (corners[3] - corners[2]) * 0.5, 3),
            ];
            let layer_w = (corners[1] - corners[0]).length().max(1.0) / (1.0 - layer.video.crop[0] - layer.video.crop[1]).max(0.05);
            let layer_h = (corners[3] - corners[0]).length().max(1.0) / (1.0 - layer.video.crop[2] - layer.video.crop[3]).max(0.05);
            for (pt, edge) in mids {
                let r = ui.interact(Rect::from_center_size(pt, Vec2::splat(16.0)), ui.id().with(("gz-crop", edge)), Sense::drag());
                p.rect_filled(Rect::from_center_size(pt, Vec2::new(if edge < 2 { 5.0 } else { 16.0 }, if edge < 2 { 16.0 } else { 5.0 })), CornerRadius::same(2), theme::SNAP);
                if r.dragged() {
                    let d = r.drag_delta();
                    let (param, delta) = match edge {
                        0 => (&mut edited.video.crop_left, d.x / layer_w),
                        1 => (&mut edited.video.crop_right, -d.x / layer_w),
                        2 => (&mut edited.video.crop_top, d.y / layer_h),
                        _ => (&mut edited.video.crop_bottom, -d.y / layer_h),
                    };
                    let v = (param.at(local) + delta as f64).clamp(0.0, 0.95);
                    param.set(local, v);
                    changed = true;
                }
            }
        }
        Gizmo::None => {}
    }

    if changed {
        app.exec_key(Command::UpdateClip { clip: Box::new(edited) }, key);
    }
    if !ui.ctx().input(|i| i.pointer.any_down()) {
        app.history.seal();
    }
    let _: ClipId = id;
}

/// Speaker mute toggle + 0–150 % slider (laid out right-to-left). Drives
/// `preview_gain` on the playback engine; export renders at unity.
fn preview_volume(app: &mut App, ui: &mut egui::Ui) {
    use nle_media::audio::{preview_gain, preview_gain_db, PREVIEW_VOLUME_MAX};
    let v = &mut app.viewer;
    let before = (v.volume_pct, v.muted);
    let db_text = |pct: f32, muted: bool| match preview_gain_db(pct, muted) {
        Some(db) => format!("{pct:.0} % ({db:+.1} dB)"),
        None => format!("{pct:.0} % (muted)"),
    };
    let slider = egui::Slider::new(&mut v.volume_pct, 0.0..=PREVIEW_VOLUME_MAX).show_value(false).step_by(1.0);
    let r = ui.add_enabled(!v.muted, slider);
    if r.double_clicked() {
        v.volume_pct = 100.0;
    }
    r.on_hover_text(format!("Preview volume {} — double-click for 100 %. Does not affect export.", db_text(v.volume_pct, false)));
    let silent = v.muted || v.volume_pct <= 0.0;
    let icon = if silent {
        "🔇"
    } else if v.volume_pct < 50.0 {
        "🔈"
    } else if v.volume_pct <= 100.0 {
        "🔉"
    } else {
        "🔊"
    };
    let text = egui::RichText::new(icon).size(15.0).color(if silent { theme::PLAYHEAD } else { theme::TEXT });
    let btn = egui::Button::new(text).fill(if v.muted { theme::PLAYHEAD.gamma_multiply(0.25) } else { egui::Color32::TRANSPARENT });
    let hover = if v.muted { "Unmute preview".to_string() } else { format!("Mute preview — {}", db_text(v.volume_pct, false)) };
    if ui.add(btn).on_hover_text(hover).clicked() {
        v.muted = !v.muted;
    }
    if v.muted {
        ui.label(egui::RichText::new("MUTED").small().strong().color(theme::PLAYHEAD));
    }
    // the engine starts at unity (= 100 %, unmuted); push only on change
    if (v.volume_pct, v.muted) != before {
        let g = preview_gain(v.volume_pct, v.muted);
        app.audio.set_master_gain(g);
    }
}

fn transport(app: &mut App, ui: &mut egui::Ui) {
    let rate = app.project.timeline.format.rate;
    ui.add_space(4.0);
    ui.horizontal(|ui| {
        ui.spacing_mut().button_padding = Vec2::new(10.0, 4.0);
        let big = |s: &str| egui::RichText::new(s).size(16.0);
        if ui.button(big("⏮")).on_hover_text("Go to start (Home)").clicked() {
            app.seek(Time::ZERO);
        }
        if ui.button(big("⏪")).on_hover_text("Play reverse (J)").clicked() {
            app.shuttle(-1.0);
        }
        let play = if app.playback.playing { "⏸" } else { "▶" };
        if ui.button(big(play)).on_hover_text("Play / Pause (Space)").clicked() {
            app.toggle_play();
        }
        if ui.button(big("⏩")).on_hover_text("Play forward (L)").clicked() {
            app.shuttle(1.0);
        }
        if ui.button(big("⏭")).on_hover_text("Go to end (End)").clicked() {
            let end = app.project.timeline.extent();
            app.seek(end);
        }
        if app.playback.playing && (app.playback.speed - 1.0).abs() > 1e-6 {
            ui.label(egui::RichText::new(format!("{:+.0}×", app.playback.speed)).color(theme::SNAP));
        }

        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            // preview volume: perceptual gain on the playback mixer only
            preview_volume(app, ui);
            ui.add_space(6.0);
            // audio meters
            let peaks = app.audio.peaks();
            let (r, _) = ui.allocate_exact_size(Vec2::new(60.0, 22.0), Sense::hover());
            for (i, pk) in peaks.iter().enumerate() {
                let y = r.top() + 4.0 + i as f32 * 8.0;
                let bar = Rect::from_min_size(Pos2::new(r.left(), y), Vec2::new(r.width(), 5.0));
                ui.painter().rect_filled(bar, CornerRadius::same(1), theme::BG);
                let db = 20.0 * pk.max(1e-5).log10();
                let f = ((db + 60.0) / 60.0).clamp(0.0, 1.0);
                let col = if db > -3.0 {
                    theme::PLAYHEAD
                } else if db > -12.0 {
                    theme::SNAP
                } else {
                    theme::AUDIO
                };
                ui.painter().rect_filled(Rect::from_min_size(bar.min, Vec2::new(bar.width() * f, 5.0)), CornerRadius::same(1), col);
            }
            let dur = format_timecode(app.project.timeline.extent(), rate);
            ui.label(egui::RichText::new(format!("/ {dur}")).monospace().color(theme::TEXT_DIM));

            // big timecode — click to type a new position
            let shown = if app.viewer.editing_tc.is_some() { app.playhead } else { app.view_time() };
            match &mut app.viewer.editing_tc {
                Some(text) => {
                    let r = ui.add(egui::TextEdit::singleline(text).font(egui::FontId::monospace(18.0)).desired_width(130.0));
                    r.request_focus();
                    if ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                        if let Some(t) = parse_timecode(text, rate) {
                            app.seek(t);
                        }
                        app.viewer.editing_tc = None;
                    } else if ui.input(|i| i.key_pressed(egui::Key::Escape)) || r.lost_focus() {
                        app.viewer.editing_tc = None;
                    }
                }
                None => {
                    let tc = format_timecode(shown, rate);
                    let r = ui.add(egui::Label::new(egui::RichText::new(&tc).monospace().size(20.0).color(theme::TEXT)).sense(Sense::click()));
                    if r.on_hover_text("Click to enter a timecode").clicked() {
                        app.viewer.editing_tc = Some(tc);
                    }
                }
            }
        });
    });
}
