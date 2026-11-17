//! Media browser: import (button or drag-and-drop), search, tags, favourites,
//! hover-scrub filmstrips, Original/Proxy toggle and metadata.

use crate::app::{App, EditMode};
use crate::theme;
use eframe::egui::{self, Color32, CornerRadius, Sense, Stroke, StrokeKind, Vec2};
use nle_core::{format_timecode, AssetId, AssetKind, MediaAsset, Time};
use nle_media::proxy::ProxyStatus;
use std::collections::BTreeSet;

pub fn show(app: &mut App, ui: &mut egui::Ui) {
    ui.horizontal_wrapped(|ui| {
        ui.selectable_value(&mut app.online.tab, crate::online_ui::BrowserTab::ProjectMedia, "Project Media");
        ui.selectable_value(&mut app.online.tab, crate::online_ui::BrowserTab::MontagePresets, "⚡ Montage Presets");
        ui.selectable_value(&mut app.online.tab, crate::online_ui::BrowserTab::OnlineAudio, "Online Audio & SFX");
    });
    ui.separator();
    match app.online.tab {
        crate::online_ui::BrowserTab::OnlineAudio => {
            crate::online_ui::show(app, ui);
            return;
        }
        crate::online_ui::BrowserTab::MontagePresets => {
            crate::presets_ui::show(app, ui);
            return;
        }
        crate::online_ui::BrowserTab::ProjectMedia => {}
    }

    ui.horizontal(|ui| {
        ui.label(egui::RichText::new("Browser").strong().size(14.0));
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if ui.button("⬇ Import").on_hover_text("Import media (Ctrl+I) — or drop files anywhere").clicked() {
                app.import_dialog();
            }
            let mut proxy = app.use_proxy;
            ui.selectable_value(&mut proxy, true, "Proxy");
            ui.selectable_value(&mut proxy, false, "Original");
            app.toggle_proxy(proxy);
        });
    });
    ui.horizontal(|ui| {
        ui.add(egui::TextEdit::singleline(&mut app.browser.search).hint_text("🔍 Search names, tags, codecs").desired_width(ui.available_width() - 60.0));
        ui.toggle_value(&mut app.browser.favorites_only, "★").on_hover_text("Favorites only");
    });
    // tag chips
    let tags: BTreeSet<String> = app.project.assets.iter().flat_map(|a| a.tags.iter().cloned()).collect();
    if !tags.is_empty() {
        ui.horizontal_wrapped(|ui| {
            if ui.selectable_label(app.browser.tag.is_none(), "All").clicked() {
                app.browser.tag = None;
            }
            for t in &tags {
                let on = app.browser.tag.as_deref() == Some(t.as_str());
                if ui.selectable_label(on, format!("#{t}")).clicked() {
                    app.browser.tag = if on { None } else { Some(t.clone()) };
                }
            }
        });
    }
    ui.horizontal(|ui| {
        ui.label(egui::RichText::new(format!("{} items", app.project.assets.len())).small().color(theme::TEXT_DIM));
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.add(egui::Slider::new(&mut app.browser.thumb_size, 90.0..=220.0).show_value(false));
            ui.label(egui::RichText::new("Size").small().color(theme::TEXT_DIM));
        });
    });
    ui.separator();

    let hovering_files = ui.ctx().input(|i| !i.raw.hovered_files.is_empty());
    let meta_h = if app.browser.selected.is_some() { 210.0 } else { 0.0 };
    let grid_h = (ui.available_height() - meta_h).max(120.0);
    let mut skim = None;
    egui::ScrollArea::vertical().id_salt("browser-grid").max_height(grid_h).auto_shrink([false, false]).show(ui, |ui| {
        if hovering_files {
            let r = ui.available_rect_before_wrap();
            ui.painter().rect_stroke(r.shrink(4.0), CornerRadius::same(8), Stroke::new(2.0_f32, theme::VIDEO), StrokeKind::Inside);
            ui.label(egui::RichText::new("Drop to import").color(theme::VIDEO));
        }
        let assets: Vec<MediaAsset> = app
            .project
            .assets
            .iter()
            .filter(|a| a.matches(&app.browser.search))
            .filter(|a| !app.browser.favorites_only || a.favorite)
            .filter(|a| app.browser.tag.as_ref().map_or(true, |t| a.tags.contains(t)))
            .cloned()
            .collect();
        if assets.is_empty() {
            ui.add_space(20.0);
            ui.vertical_centered(|ui| {
                ui.label(egui::RichText::new("No media").color(theme::TEXT_DIM));
                ui.label(egui::RichText::new("Drag video, audio or image files here").small().color(theme::TEXT_DIM));
            });
        }
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing = Vec2::new(8.0, 8.0);
            for a in &assets {
                if let Some(s) = card(app, ui, a) {
                    skim = Some(s);
                }
            }
        });
    });
    app.browser.skim = skim;

    if let Some(id) = app.browser.selected {
        if let Some(a) = app.project.asset(id).cloned() {
            ui.separator();
            metadata(app, ui, &a);
        } else {
            app.browser.selected = None;
        }
    }
}

fn kind_color(a: &MediaAsset) -> Color32 {
    if !a.has_video() {
        theme::AUDIO
    } else if a.kind == AssetKind::Image {
        theme::TITLE
    } else {
        theme::VIDEO
    }
}

/// One thumbnail card. Returns the hover-scrub position.
fn card(app: &mut App, ui: &mut egui::Ui, a: &MediaAsset) -> Option<(AssetId, Time)> {
    let w = app.browser.thumb_size;
    let strip = app.library.filmstrips.get(&a.id);
    let aspect = strip.map(|s| s.aspect).unwrap_or(16.0 / 9.0).clamp(0.5, 3.0);
    let th = w / aspect;
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(w, th + 34.0), Sense::click_and_drag());
    let thumb = egui::Rect::from_min_size(rect.min, Vec2::new(w, th));
    let selected = app.browser.selected == Some(a.id);
    let p = ui.painter();
    p.rect_filled(rect, CornerRadius::same(6), if selected { theme::WIDGET_HOVER } else { theme::BG });

    let mut skim = None;
    let hover_frac = resp.hover_pos().filter(|pos| thumb.contains(*pos)).map(|pos| ((pos.x - thumb.left()) / thumb.width()).clamp(0.0, 0.999));
    let t = hover_frac.map(|f| a.duration.mul_f64(f as f64)).unwrap_or(Time::ZERO);
    match strip.and_then(|s| s.at(t)) {
        Some(tex) => {
            p.image(tex.id(), thumb, egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)), Color32::WHITE);
        }
        None if !a.has_video() => {
            p.rect_filled(thumb, CornerRadius::same(6), theme::clip_fill(theme::AUDIO));
            if let Some(wf) = app.library.waveforms.get(&a.id) {
                let n = (w / 2.0) as usize;
                let dur = a.duration.as_secs_f64().max(0.01);
                for i in 0..n {
                    let t0 = dur * i as f64 / n as f64;
                    let t1 = dur * (i + 1) as f64 / n as f64;
                    let v = wf.peak_range(t0, t1).min(1.0) * th * 0.45;
                    let x = thumb.left() + i as f32 * 2.0 + 1.0;
                    p.line_segment([egui::pos2(x, thumb.center().y - v), egui::pos2(x, thumb.center().y + v)], Stroke::new(1.2_f32, theme::AUDIO));
                }
            } else {
                p.text(thumb.center(), egui::Align2::CENTER_CENTER, "♫", egui::FontId::proportional(28.0), theme::AUDIO);
            }
        }
        None => {
            p.rect_filled(thumb, CornerRadius::same(6), theme::WIDGET);
            let msg = if app.library.failed.contains_key(&a.id) { "no preview" } else { "…" };
            p.text(thumb.center(), egui::Align2::CENTER_CENTER, msg, egui::FontId::proportional(12.0), theme::TEXT_DIM);
        }
    }
    if let Some(f) = hover_frac {
        let x = thumb.left() + f * thumb.width();
        p.line_segment([egui::pos2(x, thumb.top()), egui::pos2(x, thumb.bottom())], Stroke::new(1.5_f32, theme::SKIMMER));
        if a.has_video() {
            skim = Some((a.id, t));
        }
    }
    // badges
    p.rect_filled(egui::Rect::from_min_size(thumb.left_bottom() - Vec2::new(0.0, 3.0), Vec2::new(w, 3.0)), CornerRadius::ZERO, kind_color(a));
    if a.favorite {
        p.text(thumb.right_top() + Vec2::new(-6.0, 4.0), egui::Align2::RIGHT_TOP, "★", egui::FontId::proportional(13.0), theme::SNAP);
    }
    match app.library.proxy_status(a.id) {
        Some(ProxyStatus::Running { progress, .. }) => {
            let bar = egui::Rect::from_min_size(thumb.left_top() + Vec2::new(4.0, 4.0), Vec2::new((w - 8.0) * progress, 3.0));
            p.rect_filled(bar, CornerRadius::ZERO, theme::SNAP);
        }
        Some(ProxyStatus::Done(_)) => {
            p.text(thumb.left_top() + Vec2::new(5.0, 3.0), egui::Align2::LEFT_TOP, "PROXY", egui::FontId::proportional(9.0), theme::AUDIO);
        }
        _ => {}
    }
    let name_pos = egui::pos2(rect.left() + 4.0, thumb.bottom() + 3.0);
    p.text(name_pos, egui::Align2::LEFT_TOP, truncate(&a.name, (w / 6.5) as usize), egui::FontId::proportional(12.0), theme::TEXT);
    let dur = format_timecode(a.duration, a.frame_rate());
    p.text(name_pos + Vec2::new(0.0, 15.0), egui::Align2::LEFT_TOP, dur, egui::FontId::monospace(10.0), theme::TEXT_DIM);
    if selected {
        p.rect_stroke(rect, CornerRadius::same(6), Stroke::new(2.0_f32, theme::SELECTION), StrokeKind::Inside);
    }

    // drag onto the timeline
    resp.dnd_set_drag_payload(a.id);
    if resp.clicked() || resp.drag_started() {
        app.browser.selected = Some(a.id);
    }
    if resp.double_clicked() {
        app.add_asset_to_timeline(a.id, EditMode::Append);
    }
    let id = a.id;
    resp.context_menu(|ui| {
        app.browser.selected = Some(id);
        if ui.button("Append to Storyline   E").clicked() {
            app.add_asset_to_timeline(id, EditMode::Append);
            ui.close_menu();
        }
        if ui.button("Insert at Playhead   W").clicked() {
            app.add_asset_to_timeline(id, EditMode::Insert);
            ui.close_menu();
        }
        if ui.button("Connect at Playhead   Q").clicked() {
            app.add_asset_to_timeline(id, EditMode::Connect);
            ui.close_menu();
        }
        if ui.button("Overwrite at Playhead   D").clicked() {
            app.add_asset_to_timeline(id, EditMode::Overwrite);
            ui.close_menu();
        }
        ui.separator();
        if ui.button("Create Proxy Media").clicked() {
            app.create_proxies(&[id]);
            ui.close_menu();
        }
        if ui.button("Toggle Favorite").clicked() {
            if let Some(a) = app.project.asset_mut(id) {
                a.favorite = !a.favorite;
                app.assets_changed = true;
            }
            ui.close_menu();
        }
        let used = app.project.timeline.placed_clips().iter().any(|p| p.clip.asset_id() == Some(id));
        if ui.add_enabled(!used, egui::Button::new("Remove from Project")).on_disabled_hover_text("Used in the timeline").clicked() {
            app.project.assets.retain(|a| a.id != id);
            app.assets_changed = true;
            app.frames.forget(id);
            if app.browser.selected == Some(id) {
                app.browser.selected = None;
            }
            ui.close_menu();
        }
    });
    resp.on_hover_text(a.summary());
    skim
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        let mut t: String = s.chars().take(n.saturating_sub(1)).collect();
        t.push('…');
        t
    }
}

fn metadata(app: &mut App, ui: &mut egui::Ui, a: &MediaAsset) {
    egui::ScrollArea::vertical().id_salt("browser-meta").show(ui, |ui| {
        ui.horizontal(|ui| {
            ui.label(egui::RichText::new(&a.name).strong());
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let mut fav = a.favorite;
                if ui.toggle_value(&mut fav, "★").changed() {
                    if let Some(m) = app.project.asset_mut(a.id) {
                        m.favorite = fav;
                        app.assets_changed = true;
                    }
                }
                if a.has_video() && ui.small_button("Create Proxy").clicked() {
                    app.create_proxies(&[a.id]);
                }
            });
        });
        egui::Grid::new("meta").num_columns(2).spacing([10.0, 3.0]).show(ui, |ui| {
            let mut row = |k: &str, v: String| {
                ui.label(egui::RichText::new(k).color(theme::TEXT_DIM));
                ui.label(v);
                ui.end_row();
            };
            row("Duration", format_timecode(a.duration, a.frame_rate()));
            if let Some(v) = &a.video {
                row("Video", format!("{}×{} · {} · {}", v.width, v.height, v.frame_rate.label(), v.codec));
                row("Format", format!("{} · {}-bit", v.pix_fmt, v.bit_depth));
                row("Color", format!("{} / {} / {}", v.color_space, v.color_primaries, v.color_transfer));
            }
            if let Some(au) = &a.audio {
                row("Audio", format!("{} Hz · {} ch · {}", au.sample_rate, au.channels, au.codec));
            }
            if let Some(lufs) = a.loudness_lufs {
                row("Loudness", format!("{lufs:.1} LUFS"));
            }
            if let Some(attr) = &a.attribution {
                row("License", attr.license.clone());
                row("Credit", attr.credit_line());
            }
            if let Some(b) = app.frames.backend(a.id) {
                row("Decoder", b.label().to_string());
            }
            match app.library.proxy_status(a.id) {
                Some(ProxyStatus::Running { progress, encoder }) => row("Proxy", format!("{:.0}% ({encoder})", progress * 100.0)),
                Some(ProxyStatus::Done(p)) => row("Proxy", p.display().to_string()),
                Some(ProxyStatus::Failed(e)) => row("Proxy", format!("failed: {e}")),
                Some(ProxyStatus::Queued) => row("Proxy", "queued".into()),
                None => {}
            }
            row("Path", a.path.display().to_string());
            if let Some(e) = app.library.failed.get(&a.id) {
                row("Note", e.clone());
            }
        });
        ui.horizontal_wrapped(|ui| {
            ui.label(egui::RichText::new("Tags").color(theme::TEXT_DIM));
            let mut remove = None;
            for t in &a.tags {
                if ui.small_button(format!("#{t} ✕")).clicked() {
                    remove = Some(t.clone());
                }
            }
            let r = ui.add(egui::TextEdit::singleline(&mut app.browser.new_tag).hint_text("add tag").desired_width(80.0));
            let commit = r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
            if let Some(m) = app.project.asset_mut(a.id) {
                if let Some(t) = remove {
                    m.tags.retain(|x| *x != t);
                    app.assets_changed = true;
                }
                let tag = app.browser.new_tag.trim().to_string();
                if commit && !tag.is_empty() && !m.tags.contains(&tag) {
                    m.tags.push(tag);
                    app.browser.new_tag.clear();
                    app.assets_changed = true;
                }
            }
        });
    });
}
