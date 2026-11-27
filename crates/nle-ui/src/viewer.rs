//! Viewer: GPU-rendered frame, fit/zoom, timecode, transport, safe zones,
//! before/after wipe and on-screen transform / crop gizmos.

use crate::app::{drag_key, App};
use crate::theme;
use eframe::egui::{self, Color32, CornerRadius, Pos2, Rect, Sense, Stroke, StrokeKind, Vec2};
use nle_core::clip::{CropAspect, VideoParams};
use nle_core::eval::clip_layer;
use nle_core::eval::{Composition, Layer, RenderNode, ResolvedVideo};
use nle_core::{format_timecode, parse_timecode, ClipId, ClipKind, Command, Time};
use nle_render::manipulate::{self, Guides, Side};
use nle_render::transform::{fit_size, layer_to_output};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Gizmo {
    None,
    Transform,
    Crop,
}

const GIZMO_TAG: u64 = 0x61;
/// How close (points) a drag has to come to a guide to snap.
const SNAP_DISTANCE: f32 = 7.0;
const HANDLE_HIT: f32 = 14.0;

#[derive(Clone, Copy, Debug, PartialEq)]
enum Grab {
    Move,
    Scale(Side),
    Rotate,
    Crop(Side),
}

/// An overlay drag. It works from where it began, so clamping and snapping
/// never drift.
#[derive(Clone, Debug)]
pub struct GizmoDrag {
    id: ClipId,
    grab: Grab,
    /// The clip's time and settings when the drag began.
    local: Time,
    start: VideoParams,
    /// The layer as it was drawn then, and its size.
    drawn: ResolvedVideo,
    layer: [f32; 2],
    /// Where the pointer went down, in output pixels.
    press: [f32; 2],
    /// Rotation so far and the pointer's last direction, in degrees.
    turned: f32,
    direction: f32,
}

pub fn show(app: &mut App, ui: &mut egui::Ui) {
    // ----- top bar
    ui.horizontal(|ui| {
        let source = app.source_active();
        let title = match app.browser.skim.map(|s| s.0).or_else(|| app.source.asset.filter(|_| source)) {
            Some(a) => app.project.asset(a).map(|a| a.name.clone()).unwrap_or_default(),
            None => app.project.timeline.name.clone(),
        };
        if app.source.asset.is_some() {
            let tip = app.keymap.tip("Switch the viewer between the source clip and the timeline", nle_core::Action::ToggleSourceViewer);
            if ui.selectable_label(source, "Source").on_hover_text(&tip).clicked() {
                app.show_source(true);
            }
            if ui.selectable_label(!source, "Timeline").on_hover_text(&tip).clicked() {
                app.show_source(false);
            }
            ui.separator();
        }
        if !source {
            crate::multicam_ui::header(app, ui);
        }
        ui.label(egui::RichText::new(title).strong());
        ui.label(egui::RichText::new(app.project.timeline.format.label()).small().color(theme::TEXT_DIM));
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            let zl = match app.viewer.zoom {
                None => "Fit".to_string(),
                Some(z) => format!("{:.0}%", z * 100.0),
            };
            egui::ComboBox::from_id_salt("viewer-zoom").selected_text(zl).width(64.0).show_ui(ui, |ui| {
                let mut picked = ui.selectable_value(&mut app.viewer.zoom, None, "Fit").clicked();
                for z in [0.25f32, 0.5, 1.0, 2.0] {
                    picked |= ui.selectable_value(&mut app.viewer.zoom, Some(z), format!("{:.0}%", z * 100.0)).clicked();
                }
                if picked {
                    app.viewer.pan = Vec2::ZERO;
                }
            });
            ui.menu_button("View ▾", |ui| {
                ui.checkbox(&mut app.viewer.safe_zones, "Title/Action Safe Zones");
                ui.menu_button("Aspect Guide", |ui| {
                    for a in CropAspect::ALL {
                        let label = if a == CropAspect::Free { "None" } else { a.label() };
                        if ui.radio_value(&mut app.viewer.aspect_guide, a, label).clicked() {
                            ui.close_menu();
                        }
                    }
                });
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
    let source = app.source_active();
    let transport_h = if source { 46.0 + 52.0 } else { 46.0 };
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
        // 100 % = one project pixel per screen pixel, whatever size the preview renders at
        Some(z) => Vec2::new(fw, fh) * (z / ui.ctx().pixels_per_point()),
    };
    if resp.dragged_by(egui::PointerButton::Middle) {
        app.viewer.pan += resp.drag_delta();
    }
    let room = ((size - area.size()) * 0.5).max(Vec2::ZERO);
    app.viewer.pan = app.viewer.pan.clamp(-room, room);
    let img = Rect::from_center_size(area.center() + app.viewer.pan, size);
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

    if let Some(ratio) = app.viewer.aspect_guide.ratio() {
        let size = if fw / fh > ratio { Vec2::new(img.height() * ratio, img.height()) } else { Vec2::new(img.width(), img.width() / ratio) };
        let kept = Rect::from_center_size(img.center(), size);
        let outside = [
            Rect::from_min_max(img.min, Pos2::new(kept.left(), img.bottom())),
            Rect::from_min_max(Pos2::new(kept.right(), img.top()), img.max),
            Rect::from_min_max(Pos2::new(kept.left(), img.top()), Pos2::new(kept.right(), kept.top())),
            Rect::from_min_max(Pos2::new(kept.left(), kept.bottom()), Pos2::new(kept.right(), img.bottom())),
        ];
        for r in outside.into_iter().filter(|r| r.is_positive()) {
            clip_painter.rect_filled(r, CornerRadius::ZERO, Color32::from_black_alpha(120));
        }
        clip_painter.rect_stroke(kept, CornerRadius::ZERO, Stroke::new(1.0_f32, Color32::from_white_alpha(150)), StrokeKind::Inside);
    }
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

    let compare_drag = crate::grade_ui::viewer_compare(app, ui, &clip_painter, img);

    if app.punch.picking {
        pick_punch_anchor(app, ui, &resp, img, (fw, fh));
    } else if crate::multicam_ui::overlay(app, ui, &resp, img) {
        if app.viewer.drag.take().is_some() {
            app.history.seal();
        }
    } else if app.browser.skim.is_none()
        && !source
        && !app.playback.playing
        && !wipe_drag
        && !compare_drag
        && !crate::motion_ui::overlay(app, ui, img)
        && !crate::color_ui::mask_gizmo(app, ui, img, (fw, fh))
    {
        gizmo(app, ui, &resp, img, (fw, fh), &to_screen);
    } else if app.viewer.drag.take().is_some() {
        app.history.seal();
    }
    if resp.double_clicked() {
        app.viewer.zoom = None;
        app.viewer.pan = Vec2::ZERO;
    }

    // ----- video scopes drawer
    if app.viewer.scopes {
        scopes_drawer(app, ui, scopes_h);
    }

    // ----- source clip strip: marks and edit buttons
    if source {
        ui.add_space(4.0);
        crate::source::source_controls(app, ui);
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

/// Native size of `clip`'s layer at `local`.
pub fn clip_layer_size(app: &App, clip: &nle_core::Clip, local: Time) -> [f32; 2] {
    let fmt = app.project.timeline.format;
    let frame = (fmt.width as f32, fmt.height as f32);
    clip_layer(clip, local, clip.lane).map_or([frame.0, frame.1], |layer| layer_size(app, clip, &layer, frame))
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
    let unflipped = ResolvedVideo { flip: [false; 2], ..layer.video.clone() };
    let Some(inv) = layer_to_output(&unflipped, lsize, [frame.0, frame.1]).inverse() else { return };
    let uv = inv.apply(out);
    let (x, y) = (((uv[0] - 0.5) as f64).clamp(-0.5, 0.5), ((uv[1] - 0.5) as f64).clamp(-0.5, 0.5));
    app.punch.anchor = PunchAnchor::Custom { x, y };
    clip.video.anchor_x = nle_core::Param::new(x);
    clip.video.anchor_y = nle_core::Param::new(y);
    if app.exec(Command::UpdateClip { clip: Box::new(clip) }) {
        app.status(format!("Zoom-punch anchor set ({:+.0} %, {:+.0} %)", x * 100.0, y * 100.0));
    }
}

/// `id`'s layer in a composition, as it is drawn (tracks followed).
fn drawn_layer(comp: &Composition, id: ClipId) -> Option<&Layer> {
    comp.nodes.iter().find_map(|node| match node {
        RenderNode::Layer(l) => (l.clip == id).then_some(l),
        RenderNode::Transition { from, to, .. } => [from, to].into_iter().flatten().map(|l| l.as_ref()).find(|l| l.clip == id),
    })
}

/// The resize cursor for a handle lying in direction `d` from the box's middle.
fn resize_cursor(d: Vec2) -> egui::CursorIcon {
    use egui::CursorIcon as C;
    let octant = (d.angle().to_degrees().rem_euclid(180.0) / 45.0).round() as i32 % 4;
    [C::ResizeHorizontal, C::ResizeNwSe, C::ResizeVertical, C::ResizeNeSw][octant as usize]
}

fn gizmo(app: &mut App, ui: &mut egui::Ui, resp: &egui::Response, img: Rect, frame: (f32, f32), to_screen: &dyn Fn([f32; 2]) -> Pos2) {
    let (down, press, pointer, mods) = ui.input(|i| (i.pointer.primary_down(), i.pointer.press_origin(), i.pointer.latest_pos(), i.modifiers));
    let mode = app.viewer.gizmo;
    let selected = app.selection.first().copied().filter(|_| mode != Gizmo::None);
    if app.viewer.drag.as_ref().is_some_and(|d| !down || Some(d.id) != selected) {
        app.viewer.drag = None;
        app.history.seal();
    }
    let Some(id) = selected else { return };
    let Some(clip) = app.project.timeline.clip(id).cloned() else { return };
    let Some(start) = app.project.timeline.clip_start(id) else { return };
    // adjustment layers always cover the frame
    if matches!(clip.kind, ClipKind::Gap | ClipKind::Adjustment) {
        return;
    }
    // the frame the preview shows
    let rate = app.project.timeline.format.rate;
    let t = Time::from_frames(app.view_time().frames_floor(rate), rate);
    if t < start || t >= start + clip.duration {
        return;
    }
    let local = t - start;
    let comp = nle_core::eval::evaluate(&app.project.timeline, t);
    let Some(layer) = drawn_layer(&comp, id) else { return };
    if app.project.timeline.is_locked(layer.lane) {
        return;
    }
    let size = [frame.0, frame.1];
    let lsize = layer_size(app, &clip, layer, frame);
    let v = &layer.video;

    let area = resp.rect;
    let p = ui.painter().with_clip_rect(area);
    let col = if mode == Gizmo::Crop { theme::SNAP } else { Color32::WHITE };
    let frame_corners = manipulate::box_corners(v, lsize, size);
    let corners = frame_corners.map(to_screen);
    p.add(egui::Shape::closed_line(corners.to_vec(), Stroke::new(1.5_f32, col)));
    let middle = corners[0] + (corners[2] - corners[0]) * 0.5;
    let to_frame = |q: Pos2| [(q.x - img.left()) / img.width() * frame.0, (q.y - img.top()) / img.height() * frame.1];

    let mut began = None;
    let sides = [(corners[1] - corners[0]).length(), (corners[3] - corners[0]).length()];
    // on a tiny box the edge handles would cover the corners
    let edges = manipulate::EDGES.into_iter().filter(|s| sides[(s[0] != 0) as usize] >= HANDLE_HIT * 3.0);
    for side in edges.chain(manipulate::CORNERS) {
        let at = to_screen(manipulate::handle_point(v, lsize, size, side));
        if !area.contains(at) {
            continue;
        }
        let hit = Rect::from_center_size(at, Vec2::splat(HANDLE_HIT)).intersect(area);
        let r = ui.interact(hit, ui.id().with(("gz-handle", side)), Sense::drag()).on_hover_cursor(resize_cursor(at - middle));
        let fill = if r.hovered() || r.dragged() { theme::VIDEO } else { col };
        let drawn = if side[0] != 0 && side[1] != 0 { 9.0 } else { 7.0 };
        p.rect_filled(Rect::from_center_size(at, Vec2::splat(drawn)), CornerRadius::same(2), fill);
        if r.drag_started_by(egui::PointerButton::Primary) {
            began = Some(if mode == Gizmo::Crop { Grab::Crop(side) } else { Grab::Scale(side) });
        }
    }

    let pivot = to_screen(manipulate::pivot(v, lsize, size));
    if mode == Gizmo::Transform {
        let top = corners[0] + (corners[1] - corners[0]) * 0.5;
        let (sin, cos) = v.rotation.to_radians().sin_cos();
        let up = Vec2::new(sin, -cos) * 24.0;
        // above the box, or just inside it where that is out of view
        let knob = if area.contains(top + up) { top + up } else { top - up };
        if area.contains(knob) {
            p.line_segment([top, knob], Stroke::new(1.0_f32, col));
            p.circle_filled(knob, 5.0, theme::VIDEO);
            let r =
                ui.interact(Rect::from_center_size(knob, Vec2::splat(HANDLE_HIT)).intersect(area), ui.id().with("gz-rot"), Sense::drag()).on_hover_cursor(egui::CursorIcon::Grab);
            if r.drag_started_by(egui::PointerButton::Primary) {
                began = Some(Grab::Rotate);
            }
        }
        p.circle_stroke(pivot, 6.0, Stroke::new(1.5_f32, col));
        p.line_segment([pivot - Vec2::new(9.0, 0.0), pivot + Vec2::new(9.0, 0.0)], Stroke::new(1.0_f32, col));
        p.line_segment([pivot - Vec2::new(0.0, 9.0), pivot + Vec2::new(0.0, 9.0)], Stroke::new(1.0_f32, col));

        let over = |q: Pos2| manipulate::inside(&frame_corners, to_frame(q));
        if resp.hovered() && pointer.is_some_and(over) {
            ui.ctx().set_cursor_icon(egui::CursorIcon::Move);
        }
        if resp.drag_started_by(egui::PointerButton::Primary) && press.is_some_and(over) {
            began = Some(Grab::Move);
        }
    }

    if let (Some(grab), Some(from)) = (began, press) {
        app.history.seal();
        app.viewer.drag = Some(GizmoDrag {
            id,
            grab,
            local,
            start: clip.video.clone(),
            drawn: v.clone(),
            layer: lsize,
            press: to_frame(from),
            turned: 0.0,
            direction: (from - pivot).angle().to_degrees(),
        });
    }

    let (Some(mut drag), Some(pos)) = (app.viewer.drag.clone(), pointer) else { return };
    let (guides, edited) = drag_edit(&mut drag, &clip.video, to_frame(pos), size, frame.0 / img.width().max(1.0), mods);
    app.viewer.drag = Some(drag);
    let line = Stroke::new(1.0_f32, theme::SNAP);
    if let Some(x) = guides[0] {
        let x = to_screen([x, 0.0]).x;
        p.line_segment([Pos2::new(x, img.top()), Pos2::new(x, img.bottom())], line);
    }
    if let Some(y) = guides[1] {
        let y = to_screen([0.0, y]).y;
        p.line_segment([Pos2::new(img.left(), y), Pos2::new(img.right(), y)], line);
    }
    if edited != clip.video {
        let mut changed = clip;
        changed.video = edited;
        app.exec_key(Command::UpdateClip { clip: Box::new(changed) }, Some(drag_key(GIZMO_TAG, id)));
    }
}

/// The settings with the pointer at `at` (output pixels). An edge handle
/// stretches its own axis; a corner keeps the aspect ratio while the scale is
/// locked. Shift flips that for corners, keeps the aspect on edges and rotates
/// in 15° steps; Ctrl drags without snapping.
fn drag_edit(drag: &mut GizmoDrag, now: &VideoParams, at: [f32; 2], frame: [f32; 2], per_point: f32, mods: egui::Modifiers) -> (Guides, VideoParams) {
    let (t, s, v, layer) = (drag.local, &drag.start, &drag.drawn, drag.layer);
    let tolerance = if mods.command { 0.0 } else { SNAP_DISTANCE * per_point };
    let moved = [at[0] - drag.press[0], at[1] - drag.press[1]];
    let handle = |side: Side| {
        let from = manipulate::handle_point(v, layer, frame, side);
        [from[0] + moved[0], from[1] + moved[1]]
    };
    let mut out = now.clone();
    // setting an animated parameter writes a key, so only set what changes
    let put = |param: &mut nle_core::Param, to: f64| {
        if (param.at(t) - to).abs() > 1e-6 {
            param.set(t, to);
        }
    };
    let mut guides: Guides = [None; 2];
    match drag.grab {
        Grab::Move => {
            let mut by = moved;
            if tolerance > 0.0 {
                let corners = manipulate::box_corners(v, layer, frame).map(|c| [c[0] + by[0], c[1] + by[1]]);
                let (shift, lines) = manipulate::snap_move(&corners, frame, tolerance);
                by = [by[0] + shift[0], by[1] + shift[1]];
                guides = lines;
            }
            put(&mut out.position_x, s.position_x.at(t) + (by[0] / frame[0]) as f64);
            put(&mut out.position_y, s.position_y.at(t) + (by[1] / frame[1]) as f64);
        }
        Grab::Scale(side) => {
            let corner = side[0] != 0 && side[1] != 0;
            let keep = if corner { s.scale_lock != mods.shift } else { mods.shift };
            let (scale, lines) = manipulate::resize(v, layer, frame, side, handle(side), keep, tolerance);
            guides = lines;
            // by ratio, as what is drawn can carry a followed track's scale
            let scaled = |from: &nle_core::Param, i: usize| if v.scale[i].abs() > 1e-6 { from.at(t) * (scale[i] / v.scale[i]) as f64 } else { scale[i] as f64 };
            put(&mut out.scale_x, scaled(&s.scale_x, 0));
            put(&mut out.scale_y, scaled(&s.scale_y, 1));
        }
        Grab::Rotate => {
            let pivot = manipulate::pivot(v, layer, frame);
            let direction = (at[1] - pivot[1]).atan2(at[0] - pivot[0]).to_degrees();
            drag.turned += manipulate::turn(drag.direction, direction);
            drag.direction = direction;
            let mut to = s.rotation.at(t) + drag.turned as f64;
            if mods.shift {
                to = (to / 15.0).round() * 15.0;
            }
            put(&mut out.rotation, to);
        }
        Grab::Crop(side) => {
            let (crop, position, lines) = manipulate::crop_drag(v, layer, frame, side, handle(side), s.crop_lock || mods.shift, tolerance);
            guides = lines;
            let params = [(&mut out.crop_left, &s.crop_left), (&mut out.crop_right, &s.crop_right), (&mut out.crop_top, &s.crop_top), (&mut out.crop_bottom, &s.crop_bottom)];
            for (i, (param, from)) in params.into_iter().enumerate() {
                let to = if (crop[i] - v.crop[i]).abs() > 1e-6 { crop[i] as f64 } else { from.at(t) };
                put(param, to);
            }
            put(&mut out.position_x, s.position_x.at(t) + (position[0] - v.position[0]) as f64);
            put(&mut out.position_y, s.position_y.at(t) + (position[1] - v.position[1]) as f64);
        }
    }
    (guides, out)
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
            app.seek_cursor(Time::ZERO);
        }
        if ui.button(big("⏪")).on_hover_text(app.keymap.tip("Play reverse", nle_core::Action::ShuttleLeft)).clicked() {
            app.shuttle(-1.0);
        }
        let play = if app.playback.playing { "⏸" } else { "▶" };
        if ui.button(big(play)).on_hover_text("Play / Pause (Space)").clicked() {
            app.toggle_play();
        }
        if ui.button(big("⏩")).on_hover_text(app.keymap.tip("Play forward", nle_core::Action::ShuttleRight)).clicked() {
            app.shuttle(1.0);
        }
        if ui.button(big("⏭")).on_hover_text("Go to end (End)").clicked() {
            let end = app.transport_end();
            app.seek_cursor(end);
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
            let dur = format_timecode(app.transport_end(), rate);
            ui.label(egui::RichText::new(format!("/ {dur}")).monospace().color(theme::TEXT_DIM));

            // big timecode — click to type a new position
            let shown = if app.viewer.editing_tc.is_some() { app.cursor() } else { app.view_time() };
            match &mut app.viewer.editing_tc {
                Some(text) => {
                    let out = egui::TextEdit::singleline(text).font(egui::FontId::monospace(18.0)).desired_width(130.0).show(ui);
                    let r = out.response;
                    if !r.has_focus() {
                        // Just opened: select it all so typed digits replace the old timecode.
                        let mut state = out.state;
                        let all = egui::text::CCursorRange::two(egui::text::CCursor::new(0), egui::text::CCursor::new(text.chars().count()));
                        state.cursor.set_char_range(Some(all));
                        state.store(ui.ctx(), r.id);
                    }
                    r.request_focus();
                    if ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                        if let Some(t) = parse_timecode(text, rate) {
                            app.seek_cursor(t);
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

#[cfg(test)]
mod drag_tests {
    use super::*;
    use nle_core::Param;

    const FRAME: [f32; 2] = [1920.0, 1080.0];

    fn drag(start: &VideoParams, grab: Grab, press: [f32; 2]) -> GizmoDrag {
        let drawn =
            ResolvedVideo { scale: [start.scale_x.at_f32(Time::ZERO), start.scale_y.at_f32(Time::ZERO)], flip: [start.flip_horizontal, start.flip_vertical], ..Default::default() };
        GizmoDrag { id: ClipId(1), grab, local: Time::ZERO, start: start.clone(), drawn, layer: FRAME, press, turned: 0.0, direction: 0.0 }
    }

    fn animated(value: f64) -> Param {
        let mut p = Param::new(value);
        p.add_keyframe(Time::from_secs(5));
        p
    }

    #[test]
    fn a_drag_only_keys_what_it_changes() {
        let start = VideoParams { position_x: animated(0.0), position_y: animated(0.0), rotation: animated(0.0), scale_lock: false, ..Default::default() };
        let mut d = drag(&start, Grab::Move, [500.0, 500.0]);
        let (_, out) = drag_edit(&mut d, &start, [700.0, 500.0], FRAME, 1.0, egui::Modifiers::COMMAND);
        assert!((out.position_x.at(Time::ZERO) - 200.0 / 1920.0).abs() < 1e-5);
        assert_eq!((&out.position_y, &out.rotation, &out.scale_x), (&start.position_y, &start.rotation, &start.scale_x));
    }

    #[test]
    fn shift_keeps_the_shape_of_an_unlocked_resize() {
        let start = VideoParams { scale_lock: false, ..Default::default() };
        let shape = |mods| {
            let mut d = drag(&start, Grab::Scale([1, 1]), [1920.0, 1080.0]);
            let (_, out) = drag_edit(&mut d, &start, [1500.0, 1000.0], FRAME, 1.0, mods);
            out.scale_x.at(Time::ZERO) / out.scale_y.at(Time::ZERO)
        };
        assert!((shape(egui::Modifiers::NONE) - 1.0).abs() > 0.1);
        assert!((shape(egui::Modifiers::SHIFT) - 1.0).abs() < 1e-4);
    }

    #[test]
    fn an_edge_handle_stretches_one_axis_even_when_locked() {
        let start = VideoParams::default();
        assert!(start.scale_lock);
        let mut d = drag(&start, Grab::Scale([1, 0]), [1920.0, 540.0]);
        let (_, out) = drag_edit(&mut d, &start, [1440.0, 540.0], FRAME, 1.0, egui::Modifiers::COMMAND);
        assert!((out.scale_x.at(Time::ZERO) - 0.5).abs() < 1e-4, "{}", out.scale_x.at(Time::ZERO));
        assert_eq!(out.scale_y, start.scale_y);
        // Shift keeps the aspect ratio on an edge
        let mut d = drag(&start, Grab::Scale([1, 0]), [1920.0, 540.0]);
        let (_, out) = drag_edit(&mut d, &start, [1440.0, 540.0], FRAME, 1.0, egui::Modifiers::SHIFT | egui::Modifiers::COMMAND);
        assert!((out.scale_y.at(Time::ZERO) - out.scale_x.at(Time::ZERO)).abs() < 1e-4);
    }

    #[test]
    fn shift_frees_a_locked_corner() {
        let start = VideoParams::default();
        let shape = |mods| {
            let mut d = drag(&start, Grab::Scale([1, 1]), [1920.0, 1080.0]);
            let (_, out) = drag_edit(&mut d, &start, [1500.0, 1000.0], FRAME, 1.0, mods);
            out.scale_x.at(Time::ZERO) / out.scale_y.at(Time::ZERO)
        };
        assert!((shape(egui::Modifiers::NONE) - 1.0).abs() < 1e-4);
        assert!((shape(egui::Modifiers::SHIFT) - 1.0).abs() > 0.1);
    }

    #[test]
    fn rotation_crosses_half_a_turn_without_jumping() {
        let start = VideoParams::default();
        let mut d = drag(&start, Grab::Rotate, [1160.0, 540.0]);
        let mut last = 0.0;
        for deg in (10..=370).step_by(10) {
            let a = (deg as f32).to_radians();
            let (_, out) = drag_edit(&mut d, &start, [960.0 + 200.0 * a.cos(), 540.0 + 200.0 * a.sin()], FRAME, 1.0, egui::Modifiers::NONE);
            last = out.rotation.at(Time::ZERO);
        }
        assert!((last - 370.0).abs() < 0.01, "{last}");
    }

    #[test]
    fn a_flipped_crop_keeps_the_picture_still() {
        let start = VideoParams { flip_horizontal: true, ..Default::default() };
        let mut d = drag(&start, Grab::Crop([-1, 0]), [0.0, 540.0]);
        let (_, out) = drag_edit(&mut d, &start, [480.0, 540.0], FRAME, 1.0, egui::Modifiers::COMMAND);
        let t = Time::ZERO;
        // the left of a mirrored picture is its own right side
        assert!((out.crop_right.at(t) - 0.25).abs() < 1e-4 && out.crop_left.at(t).abs() < 1e-4);
        assert!((out.position_x.at(t) - 0.25).abs() < 1e-4);
    }
}
