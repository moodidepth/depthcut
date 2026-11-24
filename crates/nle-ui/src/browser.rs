//! Media browser: import (button or drag-and-drop), the library sidebar
//! (timelines, bins, keyword and smart collections, ratings), search,
//! thumbnail grid or list view, hover-scrub filmstrips, Original/Proxy toggle
//! and editable metadata.

use crate::app::{App, EditMode};
use crate::theme;
use eframe::egui::{self, Color32, CornerRadius, Sense, Stroke, StrokeKind, Vec2};
use nle_core::library::{keywords, KindFilter, MatchContext};
use nle_core::{format_timecode, AssetId, AssetKind, Collection, MediaAsset, Rating, SmartCollection, SmartRule, Time};
use nle_media::proxy::ProxyStatus;
use std::collections::HashSet;

/// Column the media list is sorted by.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SortKey {
    /// Import order.
    Added,
    Name,
    Duration,
    Resolution,
    FrameRate,
    Codec,
    Rating,
}

pub struct BrowserState {
    pub search: String,
    pub collection: Collection,
    /// Hide rejected media outside the Rejected collection.
    pub hide_rejected: bool,
    pub selected: Option<AssetId>,
    pub thumb_size: f32,
    pub new_tag: String,
    /// Asset + source time being hover-scrubbed (shown in the viewer).
    pub skim: Option<(AssetId, Time)>,
    pub list_view: bool,
    pub sort: SortKey,
    pub sort_desc: bool,
    pub show_sidebar: bool,
    /// Bin being renamed: (id, new name).
    pub bin_rename: Option<(u64, String)>,
    /// Smart collection being edited (a copy until saved), and whether it is
    /// new (Cancel then deletes it).
    pub smart_edit: Option<(SmartCollection, bool)>,
}

impl Default for BrowserState {
    fn default() -> Self {
        BrowserState {
            search: String::new(),
            collection: Collection::All,
            hide_rejected: true,
            selected: None,
            thumb_size: 132.0,
            new_tag: String::new(),
            skim: None,
            list_view: false,
            sort: SortKey::Added,
            sort_desc: false,
            show_sidebar: true,
            bin_rename: None,
            smart_edit: None,
        }
    }
}

/// A bin being dragged onto another bin in the sidebar.
#[derive(Clone, Copy)]
struct BinDrag(u64);

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
    validate_collection(app);
    if app.browser.show_sidebar {
        egui::SidePanel::left("browser-library")
            .resizable(true)
            .default_width(150.0)
            .width_range(110.0..=320.0)
            .frame(egui::Frame::default().inner_margin(egui::Margin { right: 6, ..Default::default() }))
            .show_inside(ui, |ui| sidebar(app, ui));
    }
    egui::CentralPanel::default().frame(egui::Frame::default()).show_inside(ui, |ui| media_area(app, ui));
    smart_editor(app, ui.ctx());
}

/// A deleted bin or smart collection must not stay selected.
fn validate_collection(app: &mut App) {
    let lib = &app.project.library;
    let ok = match &app.browser.collection {
        Collection::Bin(b) => lib.bin(*b).is_some(),
        Collection::Smart(s) => lib.smart(*s).is_some(),
        _ => true,
    };
    if !ok {
        app.browser.collection = Collection::All;
    }
}

fn collection_title(app: &App, c: &Collection) -> String {
    match c {
        Collection::All => "All Media".into(),
        Collection::Bin(b) => app.project.library.bin_path(*b),
        Collection::Unfiled => "Unfiled".into(),
        Collection::Keyword(k) => format!("#{k}"),
        Collection::Smart(s) => app.project.library.smart(*s).map(|s| s.name.clone()).unwrap_or_default(),
        Collection::Favorites => "Favorites".into(),
        Collection::Rejected => "Rejected".into(),
    }
}

fn section(ui: &mut egui::Ui, title: &str) -> bool {
    let mut add = false;
    ui.add_space(6.0);
    ui.horizontal(|ui| {
        ui.label(egui::RichText::new(title).small().strong().color(theme::TEXT_DIM));
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            add = ui.small_button("+").on_hover_text(format!("New {}", title.to_lowercase().trim_end_matches('s'))).clicked();
        });
    });
    add
}

/// Sidebar entry for a collection; returns its response for menus and drops.
fn collection_row(app: &mut App, ui: &mut egui::Ui, c: Collection, label: String, indent: f32) -> egui::Response {
    let on = app.browser.collection == c;
    let resp = ui
        .horizontal(|ui| {
            ui.add_space(indent);
            ui.add(egui::SelectableLabel::new(on, label))
        })
        .inner;
    if resp.clicked() {
        app.browser.collection = c;
    }
    resp
}

fn sidebar(app: &mut App, ui: &mut egui::Ui) {
    egui::ScrollArea::vertical().id_salt("browser-sidebar").auto_shrink([false, false]).show(ui, |ui| {
        timelines_section(app, ui);

        ui.add_space(6.0);
        ui.label(egui::RichText::new("LIBRARY").small().strong().color(theme::TEXT_DIM));
        let n = app.project.assets.len();
        collection_row(app, ui, Collection::All, format!("▦ All Media ({n})"), 0.0);
        let unfiled = collection_row(app, ui, Collection::Unfiled, "▢ Unfiled".into(), 0.0);
        if let Some(id) = unfiled.dnd_release_payload::<AssetId>() {
            set_bin(app, *id, None);
        }
        if let Some(b) = unfiled.dnd_release_payload::<BinDrag>() {
            app.project.library.move_bin(b.0, None);
            app.assets_changed = true;
        }
        let fav = collection_row(app, ui, Collection::Favorites, "★ Favorites".into(), 0.0);
        if let Some(id) = fav.dnd_release_payload::<AssetId>() {
            rate(app, &[*id], Rating::Favorite);
        }
        let rej = collection_row(app, ui, Collection::Rejected, "✕ Rejected".into(), 0.0);
        if let Some(id) = rej.dnd_release_payload::<AssetId>() {
            rate(app, &[*id], Rating::Rejected);
        }

        if section(ui, "BINS") {
            new_bin(app, None);
        }
        bin_tree(app, ui, None, 0);
        if app.project.library.bins.is_empty() {
            ui.label(egui::RichText::new("Drag clips onto a bin to file them").small().color(theme::TEXT_DIM));
        }

        ui.add_space(6.0);
        ui.label(egui::RichText::new("KEYWORDS").small().strong().color(theme::TEXT_DIM));
        let kws = keywords(&app.project.assets);
        if kws.is_empty() {
            ui.label(egui::RichText::new("Add keywords in the info panel").small().color(theme::TEXT_DIM));
        }
        for k in kws {
            let count = app.project.assets.iter().filter(|a| a.tags.contains(&k)).count();
            let r = collection_row(app, ui, Collection::Keyword(k.clone()), format!("# {k} ({count})"), 0.0);
            if let Some(id) = r.dnd_release_payload::<AssetId>() {
                if let Some(a) = app.project.asset_mut(*id) {
                    if !a.tags.contains(&k) {
                        a.tags.push(k.clone());
                        app.assets_changed = true;
                    }
                }
            }
            r.context_menu(|ui| {
                if ui.button(format!("Remove #{k} from All Clips")).clicked() {
                    for a in &mut app.project.assets {
                        a.tags.retain(|t| *t != k);
                    }
                    app.assets_changed = true;
                    if app.browser.collection == Collection::Keyword(k.clone()) {
                        app.browser.collection = Collection::All;
                    }
                    ui.close_menu();
                }
            });
        }

        if section(ui, "SMART COLLECTIONS") {
            let id = app.project.library.add_smart("Smart Collection");
            app.assets_changed = true;
            if let Some(s) = app.project.library.smart(id).cloned() {
                app.browser.smart_edit = Some((s, true));
            }
        }
        let smart: Vec<(u64, String)> = app.project.library.smart.iter().map(|s| (s.id, s.name.clone())).collect();
        for (id, name) in smart {
            let r = collection_row(app, ui, Collection::Smart(id), format!("⚙ {name}"), 0.0);
            if r.double_clicked() {
                edit_smart(app, id);
            }
            r.context_menu(|ui| {
                if ui.button("Edit Smart Collection…").clicked() {
                    edit_smart(app, id);
                    ui.close_menu();
                }
                if ui.button("Delete Smart Collection").clicked() {
                    app.project.library.remove_smart(id);
                    app.assets_changed = true;
                    ui.close_menu();
                }
            });
        }
        if app.project.library.smart.is_empty() && ui.small_button("Add Standard Collections").on_hover_text("All Video, Audio Only, Favorites, Unused").clicked() {
            app.project.library.add_default_smart_collections();
            app.assets_changed = true;
        }
    });
}

fn edit_smart(app: &mut App, id: u64) {
    if let Some(s) = app.project.library.smart(id).cloned() {
        app.browser.smart_edit = Some((s, false));
    }
}

fn timelines_section(app: &mut App, ui: &mut egui::Ui) {
    if section(ui, "TIMELINES") {
        app.new_timeline();
    }
    let count = app.project.timeline_count();
    let active = app.project.active_timeline;
    let names: Vec<String> =
        (0..count).map(|i| if i == active { app.root_timeline_name().to_owned() } else { app.project.timeline_at(i).map(|t| t.name.clone()).unwrap_or_default() }).collect();
    for (i, name) in names.into_iter().enumerate() {
        if let Some((ri, buf)) = &mut app.timeline_rename {
            if *ri == i {
                let r = ui.add(egui::TextEdit::singleline(buf).desired_width(f32::INFINITY));
                r.request_focus();
                if r.lost_focus() {
                    let (cancel, name) = (ui.input(|i| i.key_pressed(egui::Key::Escape)), buf.clone());
                    app.timeline_rename = None;
                    if !cancel {
                        app.rename_timeline(i, &name);
                    }
                }
                continue;
            }
        }
        let label = if i == active { egui::RichText::new(format!("▶ {name}")).strong() } else { egui::RichText::new(format!("   {name}")) };
        let r = ui.add(egui::SelectableLabel::new(i == active, label)).on_hover_text("Click to open, double-click to rename");
        if r.clicked() {
            app.switch_timeline(i);
        }
        if r.double_clicked() {
            app.timeline_rename = Some((i, name.clone()));
        }
        r.context_menu(|ui| {
            if ui.add_enabled(i != active, egui::Button::new("Open")).clicked() {
                app.switch_timeline(i);
                ui.close_menu();
            }
            if ui.button("Rename…").clicked() {
                app.timeline_rename = Some((i, name.clone()));
                ui.close_menu();
            }
            if ui.button("Duplicate").clicked() {
                app.duplicate_timeline(i);
                ui.close_menu();
            }
            if ui.add_enabled(i != active, egui::Button::new("Delete")).on_disabled_hover_text("Open another timeline first").clicked() {
                app.delete_timeline(i);
                ui.close_menu();
            }
        });
    }
}

fn new_bin(app: &mut App, parent: Option<u64>) {
    let id = app.project.library.add_bin("New Bin", parent);
    app.assets_changed = true;
    app.browser.bin_rename = Some((id, "New Bin".into()));
}

fn bin_tree(app: &mut App, ui: &mut egui::Ui, parent: Option<u64>, depth: usize) {
    let kids: Vec<(u64, String)> = app.project.library.children(parent).iter().map(|b| (b.id, b.name.clone())).collect();
    for (id, name) in kids {
        let indent = depth as f32 * 12.0;
        if let Some((rid, buf)) = &mut app.browser.bin_rename {
            if *rid == id {
                let r = ui.horizontal(|ui| {
                    ui.add_space(indent);
                    ui.add(egui::TextEdit::singleline(buf).desired_width(f32::INFINITY))
                });
                r.inner.request_focus();
                if r.inner.lost_focus() {
                    let name = buf.clone();
                    app.browser.bin_rename = None;
                    if !ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                        app.project.library.rename_bin(id, name);
                        app.assets_changed = true;
                    }
                }
                bin_tree(app, ui, Some(id), depth + 1);
                continue;
            }
        }
        let count = app.project.assets.iter().filter(|a| a.bin == Some(id)).count();
        let r = collection_row(app, ui, Collection::Bin(id), format!("📁 {name} ({count})"), indent);
        let r = r.interact(Sense::click_and_drag());
        r.dnd_set_drag_payload(BinDrag(id));
        if let Some(asset) = r.dnd_release_payload::<AssetId>() {
            set_bin(app, *asset, Some(id));
        }
        if let Some(b) = r.dnd_release_payload::<BinDrag>() {
            if b.0 != id && !app.project.library.move_bin(b.0, Some(id)) {
                app.status("A bin can't go inside itself");
            }
            app.assets_changed = true;
        }
        r.context_menu(|ui| {
            if ui.button("New Bin Inside").clicked() {
                new_bin(app, Some(id));
                ui.close_menu();
            }
            if ui.button("Rename…").clicked() {
                app.browser.bin_rename = Some((id, name.clone()));
                ui.close_menu();
            }
            if ui.button("Delete Bin").on_hover_text("Its clips and bins move up a level; no media is removed").clicked() {
                let p = &mut app.project;
                p.library.remove_bin(id, &mut p.assets);
                app.assets_changed = true;
                ui.close_menu();
            }
        });
        bin_tree(app, ui, Some(id), depth + 1);
    }
}

fn set_bin(app: &mut App, id: AssetId, bin: Option<u64>) {
    if let Some(a) = app.project.asset_mut(id) {
        if a.bin != bin {
            a.bin = bin;
            app.assets_changed = true;
        }
    }
}

/// Rate assets (not undoable, like other library changes).
pub fn rate(app: &mut App, ids: &[AssetId], r: Rating) {
    for id in ids {
        if let Some(a) = app.project.asset_mut(*id) {
            if a.rating() != r {
                a.set_rating(r);
                app.assets_changed = true;
            }
        }
    }
}

fn sort_assets(v: &mut [&MediaAsset], key: SortKey, desc: bool) {
    let height = |a: &MediaAsset| a.video.as_ref().map(|v| v.height).unwrap_or(0);
    let codec = |a: &MediaAsset| a.video.as_ref().map(|v| v.codec.clone()).or_else(|| a.audio.as_ref().map(|au| au.codec.clone())).unwrap_or_default();
    let rank = |a: &MediaAsset| match a.rating() {
        Rating::Favorite => 0,
        Rating::None => 1,
        Rating::Rejected => 2,
    };
    match key {
        SortKey::Added => {}
        SortKey::Name => v.sort_by_key(|a| a.name.to_lowercase()),
        SortKey::Duration => v.sort_by_key(|a| a.span()),
        SortKey::Resolution => v.sort_by_key(|a| height(a)),
        SortKey::FrameRate => v.sort_by(|a, b| a.frame_rate().fps().total_cmp(&b.frame_rate().fps())),
        SortKey::Codec => v.sort_by_key(|a| codec(a)),
        SortKey::Rating => v.sort_by_key(|a| rank(a)),
    }
    if desc {
        v.reverse();
    }
}

fn media_area(app: &mut App, ui: &mut egui::Ui) {
    // the media column can be narrow: rows wrap instead of overlapping
    ui.horizontal_wrapped(|ui| {
        let tip = if app.browser.show_sidebar { "Hide the library sidebar" } else { "Show the library sidebar" };
        ui.toggle_value(&mut app.browser.show_sidebar, "◧").on_hover_text(tip);
        ui.label(egui::RichText::new(collection_title(app, &app.browser.collection)).strong().size(14.0));
    });
    ui.horizontal_wrapped(|ui| {
        if ui.button("⬇ Import").on_hover_text("Import media (Ctrl+I) — or drop files anywhere").clicked() {
            app.import_dialog();
        }
        let mut proxy = app.use_proxy;
        ui.selectable_value(&mut proxy, false, "Original");
        ui.selectable_value(&mut proxy, true, "Proxy");
        app.toggle_proxy(proxy);
        ui.separator();
        ui.selectable_value(&mut app.browser.list_view, false, "▦").on_hover_text("Thumbnail view");
        ui.selectable_value(&mut app.browser.list_view, true, "☰").on_hover_text("List view");
        ui.toggle_value(&mut app.browser.hide_rejected, "✕").on_hover_text("Hide rejected clips");
    });
    ui.add(egui::TextEdit::singleline(&mut app.browser.search).hint_text("🔍 Search names, keywords, codecs, notes").desired_width(f32::INFINITY));

    let used = app.project.used_assets();
    let shown: Vec<MediaAsset> = {
        let lib = &app.project.library;
        let cx = MatchContext { used: &used, bins: &lib.bins };
        let in_rejected = app.browser.collection == Collection::Rejected;
        let mut v: Vec<&MediaAsset> = app
            .project
            .assets
            .iter()
            .filter(|a| app.browser.collection.matches(a, lib, &cx))
            .filter(|a| a.matches(&app.browser.search))
            .filter(|a| in_rejected || !app.browser.hide_rejected || a.rating() != Rating::Rejected)
            .collect();
        sort_assets(&mut v, app.browser.sort, app.browser.sort_desc);
        v.into_iter().cloned().collect()
    };

    ui.horizontal_wrapped(|ui| {
        ui.label(egui::RichText::new(format!("{} of {} items", shown.len(), app.project.assets.len())).small().color(theme::TEXT_DIM));
        if !app.browser.list_view {
            ui.add_space(8.0);
            ui.label(egui::RichText::new("Size").small().color(theme::TEXT_DIM));
            ui.spacing_mut().slider_width = 70.0;
            ui.add(egui::Slider::new(&mut app.browser.thumb_size, 90.0..=220.0).show_value(false));
        }
    });
    ui.separator();

    let hovering_files = ui.ctx().input(|i| !i.raw.hovered_files.is_empty());
    let meta_h = if app.browser.selected.is_some() { 250.0 } else { 0.0 };
    let grid_h = (ui.available_height() - meta_h).max(120.0);
    let mut skim = None;
    egui::ScrollArea::both().id_salt("browser-grid").max_height(grid_h).auto_shrink([false, false]).show(ui, |ui| {
        if hovering_files {
            let r = ui.available_rect_before_wrap();
            ui.painter().rect_stroke(r.shrink(4.0), CornerRadius::same(8), Stroke::new(2.0_f32, theme::VIDEO), StrokeKind::Inside);
            ui.label(egui::RichText::new("Drop to import").color(theme::VIDEO));
        }
        if shown.is_empty() {
            ui.add_space(20.0);
            ui.vertical_centered(|ui| {
                if app.project.assets.is_empty() {
                    ui.label(egui::RichText::new("No media").color(theme::TEXT_DIM));
                    ui.label(egui::RichText::new("Drag video, audio or image files here").small().color(theme::TEXT_DIM));
                } else {
                    ui.label(egui::RichText::new("Nothing in this collection matches").color(theme::TEXT_DIM));
                }
            });
        }
        if app.browser.list_view {
            list_view(app, ui, &shown, &used);
        } else {
            ui.horizontal_wrapped(|ui| {
                ui.spacing_mut().item_spacing = Vec2::new(8.0, 8.0);
                for a in &shown {
                    if let Some(s) = card(app, ui, a, used.contains(&a.id)) {
                        skim = Some(s);
                    }
                }
            });
        }
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

const REJECT: Color32 = Color32::from_rgb(0xFF, 0x45, 0x3A);

fn rating_mark(r: Rating) -> Option<(&'static str, Color32)> {
    match r {
        Rating::Favorite => Some(("★", theme::AUDIO)),
        Rating::Rejected => Some(("✕", REJECT)),
        Rating::None => None,
    }
}

/// One thumbnail card. Returns the hover-scrub position.
fn card(app: &mut App, ui: &mut egui::Ui, a: &MediaAsset, used: bool) -> Option<(AssetId, Time)> {
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
    // a subclip skims only its own part of the file
    let (lo, hi) = a.bounds();
    let span = (hi - lo).max(Time(1));
    let t = lo + hover_frac.map(|f| span.mul_f64(f as f64)).unwrap_or(Time::ZERO);
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
    // badges: kind stripe, rating bar along the top (FCP style), proxy
    p.rect_filled(egui::Rect::from_min_size(thumb.left_bottom() - Vec2::new(0.0, 3.0), Vec2::new(w, 3.0)), CornerRadius::ZERO, kind_color(a));
    if let Some((mark, col)) = rating_mark(a.rating()) {
        p.rect_filled(egui::Rect::from_min_size(thumb.left_top(), Vec2::new(w, 3.0)), CornerRadius::ZERO, col);
        p.text(thumb.right_top() + Vec2::new(-6.0, 5.0), egui::Align2::RIGHT_TOP, mark, egui::FontId::proportional(13.0), col);
    }
    if a.rating() == Rating::Rejected {
        p.rect_filled(thumb, CornerRadius::same(6), Color32::from_black_alpha(110));
    }
    if used {
        p.text(thumb.right_bottom() + Vec2::new(-5.0, -6.0), egui::Align2::RIGHT_BOTTOM, "●", egui::FontId::proportional(9.0), theme::SNAP);
    }
    // In / Out marks as a range bar along the bottom
    let marks = app.asset_marks(a.id);
    if !marks.is_empty() {
        let fx = |t: Time| thumb.left() + ((t - lo).as_secs_f64() / span.as_secs_f64()).clamp(0.0, 1.0) as f32 * w;
        let (x0, x1) = (fx(marks.mark_in.unwrap_or(lo)), fx(marks.mark_out.unwrap_or(hi)));
        let bar = egui::Rect::from_min_max(egui::pos2(x0, thumb.bottom() - 6.0), egui::pos2(x1.max(x0 + 2.0), thumb.bottom() - 3.0));
        p.rect_filled(bar, CornerRadius::ZERO, theme::SELECTION);
    }
    if a.subclip.is_some() {
        p.text(thumb.left_bottom() + Vec2::new(5.0, -8.0), egui::Align2::LEFT_BOTTOM, "SUBCLIP", egui::FontId::proportional(9.0), theme::SELECTION);
    }
    match app.library.proxy_status(a.id) {
        Some(ProxyStatus::Running { progress, .. }) => {
            let bar = egui::Rect::from_min_size(thumb.left_top() + Vec2::new(4.0, 6.0), Vec2::new((w - 8.0) * progress, 3.0));
            p.rect_filled(bar, CornerRadius::ZERO, theme::SNAP);
        }
        Some(ProxyStatus::Done(_)) => {
            p.text(thumb.left_top() + Vec2::new(5.0, 5.0), egui::Align2::LEFT_TOP, "PROXY", egui::FontId::proportional(9.0), theme::AUDIO);
        }
        _ => {}
    }
    let name_pos = egui::pos2(rect.left() + 4.0, thumb.bottom() + 3.0);
    p.text(name_pos, egui::Align2::LEFT_TOP, truncate(&a.name, (w / 6.5) as usize), egui::FontId::proportional(12.0), theme::TEXT);
    let dur = format_timecode(a.span(), a.frame_rate());
    p.text(name_pos + Vec2::new(0.0, 15.0), egui::Align2::LEFT_TOP, dur, egui::FontId::monospace(10.0), theme::TEXT_DIM);
    if selected {
        p.rect_stroke(rect, CornerRadius::same(6), Stroke::new(2.0_f32, theme::SELECTION), StrokeKind::Inside);
    }
    asset_interaction(app, &resp, a.id, used);
    resp.on_hover_text(hover_summary(app, a, used));
    skim
}

fn hover_summary(app: &App, a: &MediaAsset, used: bool) -> String {
    let mut s = a.summary();
    if let Some(b) = a.bin {
        s.push_str(&format!("\nBin: {}", app.project.library.bin_path(b)));
    }
    if used {
        s.push_str("\nUsed in a timeline");
    }
    s
}

/// Click to select, drag onto the timeline or a bin, double-click to append,
/// right-click for the menu.
fn asset_interaction(app: &mut App, resp: &egui::Response, id: AssetId, used: bool) {
    resp.dnd_set_drag_payload(id);
    if resp.clicked() || resp.drag_started() {
        app.browser.selected = Some(id);
    }
    if resp.double_clicked() {
        app.open_in_source(id, None);
    }
    resp.context_menu(|ui| {
        app.browser.selected = Some(id);
        asset_menu(app, ui, id, used);
    });
}

fn asset_menu(app: &mut App, ui: &mut egui::Ui, id: AssetId, used: bool) {
    if ui.button(app.menu_label("Open in Source Viewer", nle_core::keymap::Action::ToggleSourceViewer)).clicked() {
        app.open_in_source(id, None);
        ui.close_menu();
    }
    ui.separator();
    for (label, action, mode) in [
        ("Append to Storyline", nle_core::keymap::Action::AppendClip, EditMode::Append),
        ("Insert at Playhead", nle_core::keymap::Action::InsertClip, EditMode::Insert),
        ("Connect at Playhead", nle_core::keymap::Action::ConnectClip, EditMode::Connect),
        ("Overwrite at Playhead", nle_core::keymap::Action::OverwriteClip, EditMode::Overwrite),
    ] {
        if ui.button(app.menu_label(label, action)).clicked() {
            app.add_asset_to_timeline(id, mode);
            ui.close_menu();
        }
    }
    ui.separator();
    let current = app.project.asset(id).map(|a| a.rating()).unwrap_or_default();
    ui.menu_button("Rating", |ui| {
        for (r, action) in [
            (Rating::Favorite, nle_core::keymap::Action::RateFavorite),
            (Rating::None, nle_core::keymap::Action::RateClear),
            (Rating::Rejected, nle_core::keymap::Action::RateReject),
        ] {
            if ui.radio(current == r, app.menu_label(r.label(), action)).clicked() {
                rate(app, &[id], r);
                ui.close_menu();
            }
        }
    });
    let bin = app.project.asset(id).and_then(|a| a.bin);
    ui.menu_button("Move to Bin", |ui| {
        if ui.radio(bin.is_none(), "Unfiled").clicked() {
            set_bin(app, id, None);
            ui.close_menu();
        }
        let mut all: Vec<(u64, String)> = app.project.library.bins.iter().map(|b| (b.id, app.project.library.bin_path(b.id))).collect();
        all.sort_by_key(|b| b.1.to_lowercase());
        for (b, path) in all {
            if ui.radio(bin == Some(b), path).clicked() {
                set_bin(app, id, Some(b));
                ui.close_menu();
            }
        }
        ui.separator();
        if ui.button("New Bin with This Clip").clicked() {
            let b = app.project.library.add_bin("New Bin", None);
            set_bin(app, id, Some(b));
            app.assets_changed = true;
            app.browser.bin_rename = Some((b, "New Bin".into()));
            ui.close_menu();
        }
    });
    ui.separator();
    if ui.button("Create Proxy Media").clicked() {
        app.create_proxies(&[id]);
        ui.close_menu();
    }
    if ui.add_enabled(!used, egui::Button::new("Remove from Project")).on_disabled_hover_text("Used in a timeline of this project").clicked() {
        app.project.assets.retain(|a| a.id != id);
        app.assets_changed = true;
        app.frames.forget(id);
        if app.browser.selected == Some(id) {
            app.browser.selected = None;
        }
        ui.close_menu();
    }
}

fn list_view(app: &mut App, ui: &mut egui::Ui, shown: &[MediaAsset], used: &HashSet<AssetId>) {
    const COLS: [(&str, Option<SortKey>); 11] = [
        ("Name", Some(SortKey::Name)),
        ("Duration", Some(SortKey::Duration)),
        ("Format", Some(SortKey::Resolution)),
        ("FPS", Some(SortKey::FrameRate)),
        ("Codec", Some(SortKey::Codec)),
        ("Audio", None),
        ("Rating", Some(SortKey::Rating)),
        ("Bin", None),
        ("Keywords", None),
        ("Scene / Take", None),
        ("Used", None),
    ];
    egui::Grid::new("browser-list").striped(true).num_columns(COLS.len()).spacing([12.0, 4.0]).show(ui, |ui| {
        for (title, key) in COLS {
            match key {
                Some(k) => {
                    let on = app.browser.sort == k;
                    let arrow = if on && app.browser.sort_desc {
                        " ▼"
                    } else if on {
                        " ▲"
                    } else {
                        ""
                    };
                    if ui.add(egui::Button::new(egui::RichText::new(format!("{title}{arrow}")).strong()).frame(false)).clicked() {
                        if on && app.browser.sort_desc {
                            // third click returns to import order
                            app.browser.sort = SortKey::Added;
                            app.browser.sort_desc = false;
                        } else if on {
                            app.browser.sort_desc = true;
                        } else {
                            app.browser.sort = k;
                            app.browser.sort_desc = false;
                        }
                    }
                }
                None => {
                    ui.label(egui::RichText::new(title).strong());
                }
            }
        }
        ui.end_row();
        for a in shown {
            let is_used = used.contains(&a.id);
            let selected = app.browser.selected == Some(a.id);
            let mut name = egui::RichText::new(&a.name).color(kind_color(a));
            if a.rating() == Rating::Rejected {
                name = name.strikethrough();
            }
            let resp = ui.add(egui::SelectableLabel::new(selected, name)).interact(Sense::click_and_drag());
            asset_interaction(app, &resp, a.id, is_used);
            resp.on_hover_text(hover_summary(app, a, is_used));
            let dim = |s: String| egui::RichText::new(s).color(theme::TEXT_DIM);
            ui.label(egui::RichText::new(format_timecode(a.span(), a.frame_rate())).monospace());
            match &a.video {
                Some(v) => {
                    ui.label(format!("{}×{}", v.width, v.height));
                    ui.label(v.frame_rate.label());
                    ui.label(&v.codec);
                }
                None => {
                    ui.label(dim("—".into()));
                    ui.label(dim("—".into()));
                    ui.label(dim(a.audio.as_ref().map(|au| au.codec.clone()).unwrap_or_default()));
                }
            }
            ui.label(a.audio.as_ref().map(|au| format!("{} kHz · {} ch", au.sample_rate / 1000, au.channels)).unwrap_or_else(|| "—".into()));
            match rating_mark(a.rating()) {
                Some((m, c)) => ui.label(egui::RichText::new(format!("{m} {}", a.rating().label())).color(c)),
                None => ui.label(""),
            };
            ui.label(a.bin.map(|b| app.project.library.bin_path(b)).unwrap_or_default());
            ui.label(a.tags.iter().map(|t| format!("#{t}")).collect::<Vec<_>>().join(" "));
            let st = [a.meta.scene.as_str(), a.meta.take.as_str()].iter().filter(|s| !s.is_empty()).copied().collect::<Vec<_>>().join(" / ");
            ui.label(st);
            ui.label(if is_used { "●" } else { "" });
            ui.end_row();
        }
    });
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
                if a.has_video() && ui.small_button("Create Proxy").clicked() {
                    app.create_proxies(&[a.id]);
                }
                let current = a.rating();
                for r in [Rating::Rejected, Rating::None, Rating::Favorite] {
                    let text = match r {
                        Rating::Favorite => egui::RichText::new("★").color(theme::AUDIO),
                        Rating::Rejected => egui::RichText::new("✕").color(REJECT),
                        Rating::None => egui::RichText::new("○"),
                    };
                    if ui.selectable_label(current == r, text).on_hover_text(r.label()).clicked() {
                        rate(app, &[a.id], r);
                    }
                }
            });
        });
        egui::Grid::new("meta").num_columns(2).spacing([10.0, 3.0]).show(ui, |ui| {
            let mut row = |k: &str, v: String| {
                ui.label(egui::RichText::new(k).color(theme::TEXT_DIM));
                // a long path must not widen the panel over the viewer (hover shows all of it)
                ui.add(egui::Label::new(v).truncate());
                ui.end_row();
            };
            row("Duration", format_timecode(a.span(), a.frame_rate()));
            if let Some((i, o)) = a.subclip {
                row("Subclip of file", format!("{} - {}", format_timecode(i, a.frame_rate()), format_timecode(o, a.frame_rate())));
            }
            if let Some(v) = &a.video {
                row("Video", format!("{}×{} · {} · {}", v.width, v.height, v.frame_rate.label(), v.codec));
                row("Format", format!("{} · {}-bit", v.pix_fmt, v.bit_depth));
                row("Color", format!("{} / {} / {}", v.color_space, v.color_primaries, v.color_transfer));
            }
            if let Some(au) = &a.audio {
                row("Audio", format!("{} Hz · {} ch · {}", au.sample_rate, au.channels, au.codec));
            }
            if let Some(tc) = a.start_timecode {
                row("Start TC", format_timecode(tc, a.frame_rate()));
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
        ui.add_space(4.0);
        // editable logging fields
        let mut meta = a.meta.clone();
        let mut bin = a.bin;
        let mut changed = false;
        let mut deinterlace = a.deinterlace;
        egui::Grid::new("meta-edit").num_columns(2).spacing([10.0, 3.0]).show(ui, |ui| {
            if a.has_video() && a.kind != nle_core::AssetKind::Image {
                ui.label(egui::RichText::new("Deinterlace").color(theme::TEXT_DIM));
                ui.horizontal(|ui| {
                    for d in nle_core::Deinterlace::ALL {
                        ui.selectable_value(&mut deinterlace, d, d.label());
                    }
                    ui.label(egui::RichText::new(a.field_order.label()).small().color(theme::TEXT_DIM));
                })
                .response
                .on_hover_text("Auto deinterlaces files probed as interlaced (bwdif, one frame per frame). Proxies made before a change keep the old setting.");
                ui.end_row();
            }
            ui.label(egui::RichText::new("Bin").color(theme::TEXT_DIM));
            let label = bin.map(|b| app.project.library.bin_path(b)).unwrap_or_else(|| "Unfiled".into());
            egui::ComboBox::from_id_salt("meta-bin").selected_text(label).show_ui(ui, |ui| {
                changed |= ui.selectable_value(&mut bin, None, "Unfiled").changed();
                for b in &app.project.library.bins {
                    changed |= ui.selectable_value(&mut bin, Some(b.id), app.project.library.bin_path(b.id)).changed();
                }
            });
            ui.end_row();
            for (k, v) in meta.fields_mut() {
                ui.label(egui::RichText::new(k).color(theme::TEXT_DIM));
                let multi = k == "Notes" || k == "Description";
                let edit = if multi { egui::TextEdit::multiline(v).desired_rows(2) } else { egui::TextEdit::singleline(v) };
                changed |= ui.add(edit.desired_width(f32::INFINITY)).changed();
                ui.end_row();
            }
        });
        if changed {
            if let Some(m) = app.project.asset_mut(a.id) {
                m.meta = meta;
                m.bin = bin;
                app.assets_changed = true;
            }
        }
        if deinterlace != a.deinterlace {
            if let Some(m) = app.project.asset_mut(a.id) {
                m.deinterlace = deinterlace;
                app.assets_changed = true;
            }
            app.reload_decoders(a.id);
        }
        ui.horizontal_wrapped(|ui| {
            ui.label(egui::RichText::new("Keywords").color(theme::TEXT_DIM));
            let mut remove = None;
            for t in &a.tags {
                if ui.small_button(format!("#{t} ✕")).clicked() {
                    remove = Some(t.clone());
                }
            }
            let r = ui.add(egui::TextEdit::singleline(&mut app.browser.new_tag).hint_text("add keyword").desired_width(90.0));
            let commit = r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
            if let Some(m) = app.project.asset_mut(a.id) {
                if let Some(t) = remove {
                    m.tags.retain(|x| *x != t);
                    app.assets_changed = true;
                }
                let tag = app.browser.new_tag.trim().trim_start_matches('#').to_string();
                if commit && !tag.is_empty() && !m.tags.contains(&tag) {
                    m.tags.push(tag);
                    app.browser.new_tag.clear();
                    app.assets_changed = true;
                }
            }
        });
    });
}

// ----- smart collection editor ----------------------------------------------

#[derive(Clone, Copy, PartialEq)]
enum RuleKind {
    Text,
    Keyword,
    Kind,
    Rating,
    MinDuration,
    MaxDuration,
    MinHeight,
    MinFrameRate,
    Codec,
    Used,
    InBin,
}

impl RuleKind {
    const ALL: [RuleKind; 11] = [
        RuleKind::Text,
        RuleKind::Keyword,
        RuleKind::Kind,
        RuleKind::Rating,
        RuleKind::MinDuration,
        RuleKind::MaxDuration,
        RuleKind::MinHeight,
        RuleKind::MinFrameRate,
        RuleKind::Codec,
        RuleKind::Used,
        RuleKind::InBin,
    ];

    fn of(r: &SmartRule) -> RuleKind {
        match r {
            SmartRule::Text(_) => RuleKind::Text,
            SmartRule::Keyword { .. } => RuleKind::Keyword,
            SmartRule::Kind(_) => RuleKind::Kind,
            SmartRule::Rating(_) => RuleKind::Rating,
            SmartRule::MinDuration(_) => RuleKind::MinDuration,
            SmartRule::MaxDuration(_) => RuleKind::MaxDuration,
            SmartRule::MinHeight(_) => RuleKind::MinHeight,
            SmartRule::MinFrameRate(_) => RuleKind::MinFrameRate,
            SmartRule::Codec(_) => RuleKind::Codec,
            SmartRule::Used(_) => RuleKind::Used,
            SmartRule::InBin(_) => RuleKind::InBin,
        }
    }

    fn label(self) -> &'static str {
        match self {
            RuleKind::Text => "Text contains",
            RuleKind::Keyword => "Keyword",
            RuleKind::Kind => "Media kind",
            RuleKind::Rating => "Rating",
            RuleKind::MinDuration => "Duration at least",
            RuleKind::MaxDuration => "Duration at most",
            RuleKind::MinHeight => "Resolution at least",
            RuleKind::MinFrameRate => "Frame rate at least",
            RuleKind::Codec => "Codec contains",
            RuleKind::Used => "Usage",
            RuleKind::InBin => "In bin",
        }
    }

    fn default_rule(self, first_bin: Option<u64>) -> SmartRule {
        match self {
            RuleKind::Text => SmartRule::Text(String::new()),
            RuleKind::Keyword => SmartRule::Keyword { keyword: String::new(), has: true },
            RuleKind::Kind => SmartRule::Kind(KindFilter::Video),
            RuleKind::Rating => SmartRule::Rating(Rating::Favorite),
            RuleKind::MinDuration => SmartRule::MinDuration(Time::from_secs(5)),
            RuleKind::MaxDuration => SmartRule::MaxDuration(Time::from_secs(30)),
            RuleKind::MinHeight => SmartRule::MinHeight(1080),
            RuleKind::MinFrameRate => SmartRule::MinFrameRate(50.0),
            RuleKind::Codec => SmartRule::Codec(String::new()),
            RuleKind::Used => SmartRule::Used(false),
            RuleKind::InBin => SmartRule::InBin(first_bin.unwrap_or(0)),
        }
    }
}

fn rule_editor(ui: &mut egui::Ui, i: usize, rule: &mut SmartRule, app: &App) {
    let mut kind = RuleKind::of(rule);
    egui::ComboBox::from_id_salt(("smart-kind", i)).selected_text(kind.label()).width(150.0).show_ui(ui, |ui| {
        for k in RuleKind::ALL {
            ui.selectable_value(&mut kind, k, k.label());
        }
    });
    if kind != RuleKind::of(rule) {
        *rule = kind.default_rule(app.project.library.bins.first().map(|b| b.id));
    }
    match rule {
        SmartRule::Text(t) | SmartRule::Codec(t) => {
            ui.add(egui::TextEdit::singleline(t).desired_width(140.0));
        }
        SmartRule::Keyword { keyword, has } => {
            egui::ComboBox::from_id_salt(("smart-has", i)).selected_text(if *has { "is" } else { "is not" }).show_ui(ui, |ui| {
                ui.selectable_value(has, true, "is");
                ui.selectable_value(has, false, "is not");
            });
            let all = keywords(&app.project.assets);
            egui::ComboBox::from_id_salt(("smart-kw", i)).selected_text(if keyword.is_empty() { "choose…".to_string() } else { format!("#{keyword}") }).show_ui(ui, |ui| {
                for k in all {
                    let label = format!("#{k}");
                    ui.selectable_value(keyword, k, label);
                }
            });
        }
        SmartRule::Kind(k) => {
            for f in KindFilter::ALL {
                ui.selectable_value(k, f, f.label());
            }
        }
        SmartRule::Rating(r) => {
            for x in Rating::ALL {
                ui.selectable_value(r, x, x.label());
            }
        }
        SmartRule::MinDuration(t) | SmartRule::MaxDuration(t) => {
            let mut s = t.as_secs_f64();
            if ui.add(egui::DragValue::new(&mut s).range(0.0..=36000.0).speed(0.5).suffix(" s")).changed() {
                *t = Time::from_secs_f64(s);
            }
        }
        SmartRule::MinHeight(h) => {
            egui::ComboBox::from_id_salt(("smart-h", i)).selected_text(format!("{h} lines")).show_ui(ui, |ui| {
                for (v, l) in [(480, "480 (SD)"), (720, "720 (HD)"), (1080, "1080 (Full HD)"), (1440, "1440 (QHD)"), (2160, "2160 (UHD 4K)"), (4320, "4320 (8K)")] {
                    ui.selectable_value(h, v, l);
                }
            });
        }
        SmartRule::MinFrameRate(f) => {
            ui.add(egui::DragValue::new(f).range(1.0..=480.0).speed(1.0).suffix(" fps"));
        }
        SmartRule::Used(u) => {
            ui.selectable_value(u, true, "Used in a timeline");
            ui.selectable_value(u, false, "Unused");
        }
        SmartRule::InBin(b) => {
            let lib = &app.project.library;
            let label = lib.bin(*b).map(|_| lib.bin_path(*b)).unwrap_or_else(|| "choose…".into());
            egui::ComboBox::from_id_salt(("smart-bin", i)).selected_text(label).show_ui(ui, |ui| {
                for x in &lib.bins {
                    ui.selectable_value(b, x.id, lib.bin_path(x.id));
                }
            });
        }
    }
}

fn smart_editor(app: &mut App, ctx: &egui::Context) {
    let Some((mut s, is_new)) = app.browser.smart_edit.take() else { return };
    let mut open = true;
    let mut done: Option<bool> = None; // Some(true) = save, Some(false) = cancel
    egui::Window::new("Smart Collection").collapsible(false).resizable(true).default_width(520.0).open(&mut open).show(ctx, |ui| {
        ui.horizontal(|ui| {
            ui.label("Name");
            ui.add(egui::TextEdit::singleline(&mut s.name).desired_width(240.0));
        });
        ui.horizontal(|ui| {
            ui.label("Match");
            ui.selectable_value(&mut s.match_all, true, "all rules");
            ui.selectable_value(&mut s.match_all, false, "any rule");
        });
        ui.separator();
        let mut remove = None;
        for (i, rule) in s.rules.iter_mut().enumerate() {
            ui.horizontal(|ui| {
                rule_editor(ui, i, rule, app);
                if ui.small_button("✕").on_hover_text("Remove rule").clicked() {
                    remove = Some(i);
                }
            });
        }
        if let Some(i) = remove {
            s.rules.remove(i);
        }
        if s.rules.is_empty() {
            ui.label(egui::RichText::new("No rules: every clip matches").small().color(theme::TEXT_DIM));
        }
        if ui.button("+ Add Rule").clicked() {
            s.rules.push(SmartRule::Text(String::new()));
        }
        ui.separator();
        let used = app.project.used_assets();
        let cx = MatchContext { used: &used, bins: &app.project.library.bins };
        let n = app.project.assets.iter().filter(|a| s.matches(a, &cx)).count();
        ui.label(egui::RichText::new(format!("{n} of {} clips match", app.project.assets.len())).color(theme::TEXT_DIM));
        ui.horizontal(|ui| {
            if ui.button("Save").clicked() {
                done = Some(true);
            }
            if ui.button("Cancel").clicked() {
                done = Some(false);
            }
        });
    });
    if !open {
        done = Some(false);
    }
    match done {
        None => app.browser.smart_edit = Some((s, is_new)),
        Some(true) => {
            if s.name.trim().is_empty() {
                s.name = "Smart Collection".into();
            }
            let id = s.id;
            if let Some(slot) = app.project.library.smart_mut(id) {
                *slot = s;
            }
            app.assets_changed = true;
            app.browser.collection = Collection::Smart(id);
        }
        Some(false) => {
            if is_new {
                app.project.library.remove_smart(s.id);
            }
        }
    }
}
