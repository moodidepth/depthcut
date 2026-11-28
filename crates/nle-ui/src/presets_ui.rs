//! Montage Presets browser tab:
//! Displays the 16+ authentic CS2 & Valorant built-in presets + user presets
//! from `presets/user/`. Supports category filtering, search, 1-click Apply at
//! playhead, drag-and-drop onto timeline clips / Adjustment Clips, and saving
//! custom user presets.

use crate::app::App;
use crate::theme;
use eframe::egui::{self, Color32, CornerRadius, Sense, Stroke, StrokeKind};
use nle_core::presets::{MontagePreset, PresetCategory};
use std::path::{Path, PathBuf};

pub struct PresetsUiState {
    pub category: Option<PresetCategory>,
    pub search: String,
    pub new_name: String,
    pub new_cat: PresetCategory,
    pub user_presets: Vec<MontagePreset>,
    pub loaded: bool,
}

impl Default for PresetsUiState {
    fn default() -> Self {
        Self { category: None, search: String::new(), new_name: String::new(), new_cat: PresetCategory::ShakeAndImpact, user_presets: Vec::new(), loaded: false }
    }
}

pub fn user_presets_dir() -> PathBuf {
    let repo_dir = Path::new("presets").join("user");
    if Path::new("presets").is_dir() {
        return repo_dir;
    }
    crate::media::data_dir().join("presets").join("user")
}

/// Badge colour of a preset category.
pub fn category_color(cat: PresetCategory) -> Color32 {
    match cat {
        PresetCategory::RsmbMotionBlur => Color32::from_rgb(0x40, 0xD2, 0xFF),
        PresetCategory::VelocityGraph => Color32::from_rgb(0x30, 0xD1, 0x58),
        PresetCategory::ShakeAndImpact => Color32::from_rgb(0xFF, 0x6E, 0x30),
        PresetCategory::ColorAndSignatureFx => Color32::from_rgb(0xBF, 0x5A, 0xF2),
        PresetCategory::Transitions => Color32::from_rgb(0xFF, 0xD6, 0x0A),
    }
}

/// What a click on the applied-preset badges asks for.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ClearPreset {
    Category(PresetCategory),
    All,
}

/// The montage presets stacked on `clip`, one coloured badge per category,
/// oldest first. With `clearable` each badge gets a ✕ and the row a
/// "Clear All Presets" button; returns what was clicked.
pub fn applied_badges(ui: &mut egui::Ui, clip: &nle_core::Clip, clearable: bool) -> Option<ClearPreset> {
    let applied = MontagePreset::applied(clip);
    let mut clicked = None;
    if applied.is_empty() {
        return None;
    }
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing.x = 4.0;
        for p in &applied {
            let col = category_color(p.category);
            egui::Frame::new().stroke(Stroke::new(1.0_f32, col)).corner_radius(CornerRadius::same(4)).inner_margin(egui::Margin::symmetric(4, 1)).show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 2.0;
                    let text = format!("{}: {}", p.category.label(), p.name);
                    let r = ui.label(egui::RichText::new(text).small().color(col));
                    crate::widgets::describe(&r, format!("{} preset", p.category.label()), p.name.clone());
                    if clearable {
                        let x =
                            ui.add(egui::Button::new(egui::RichText::new("✕").small()).frame(false)).on_hover_text(format!("Clear the {} preset ({})", p.category.label(), p.name));
                        let name = format!("Clear {} preset", p.category.label());
                        ui.ctx().accesskit_node_builder(x.id, |n| n.set_label(name));
                        if x.clicked() {
                            clicked = Some(ClearPreset::Category(p.category));
                        }
                    }
                });
            });
        }
        if clearable && ui.small_button("Clear All Presets").on_hover_text("Take every montage preset off this clip (your own edits stay)").clicked() {
            clicked = Some(ClearPreset::All);
        }
    });
    clicked
}

pub fn find_preset_with_user(name: &str, user_presets: &[MontagePreset]) -> Option<MontagePreset> {
    let q = name.trim().to_lowercase();
    user_presets.iter().find(|p| p.name.to_lowercase() == q).cloned().or_else(|| MontagePreset::find(name))
}

pub fn show(app: &mut App, ui: &mut egui::Ui) {
    if !app.presets_ui.loaded {
        app.presets_ui.user_presets = MontagePreset::load_user_presets(&user_presets_dir());
        app.presets_ui.loaded = true;
    }

    ui.horizontal(|ui| {
        ui.label(egui::RichText::new("⚡ CS2 / Valorant Montage Presets").strong().size(14.0));
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if ui.small_button("↻").on_hover_text("Reload user presets from presets/user/").clicked() {
                app.presets_ui.user_presets = MontagePreset::load_user_presets(&user_presets_dir());
            }
        });
    });
    ui.label(egui::RichText::new("Drag a preset onto any timeline clip / Adjustment Clip, or click Apply at playhead.").small().color(theme::TEXT_DIM));

    // what is stacked on the clip Apply would hit
    let mut clear = None;
    if let Some(clip) = app.target_clip().and_then(|id| app.project.timeline.clip(id)).cloned() {
        if !MontagePreset::applied(&clip).is_empty() {
            ui.label(egui::RichText::new(format!("Applied to \u{201c}{}\u{201d} (one per category, they stack):", clip.name)).small().color(theme::TEXT_DIM));
            clear = applied_badges(ui, &clip, true).map(|c| (clip.id, c));
        }
    }
    if let Some((id, what)) = clear {
        app.clear_montage_preset(id, what);
    }

    ui.add(egui::TextEdit::singleline(&mut app.presets_ui.search).hint_text("🔍 Filter presets (e.g. RSMB, Flow Sync, Shake, Halo…)").desired_width(ui.available_width()));

    ui.horizontal_wrapped(|ui| {
        if ui.selectable_label(app.presets_ui.category.is_none(), "All").clicked() {
            app.presets_ui.category = None;
        }
        for cat in PresetCategory::ALL {
            let active = app.presets_ui.category == Some(cat);
            if ui.selectable_label(active, cat.label()).clicked() {
                app.presets_ui.category = if active { None } else { Some(cat) };
            }
        }
    });

    let mut save_clicked = false;
    egui::CollapsingHeader::new("💾 Save Selected Clip as User Preset").default_open(false).show(ui, |ui| {
        let has_sel = app.target_clip().is_some();
        if !has_sel {
            ui.label(egui::RichText::new("Select a timeline clip to capture its FX, Color, and Transitions.").small().color(theme::TEXT_DIM));
        }
        ui.horizontal(|ui| {
            ui.add(egui::TextEdit::singleline(&mut app.presets_ui.new_name).hint_text("My Signature Shake"));
            egui::ComboBox::from_id_salt("new-preset-cat").selected_text(app.presets_ui.new_cat.label()).show_ui(ui, |ui| {
                for cat in PresetCategory::ALL {
                    ui.selectable_value(&mut app.presets_ui.new_cat, cat, cat.label());
                }
            });
        });
        if ui.add_enabled(has_sel && !app.presets_ui.new_name.trim().is_empty(), egui::Button::new("Save to presets/user/")).clicked() {
            save_clicked = true;
        }
    });

    if save_clicked {
        if let Some(cid) = app.target_clip() {
            if let Some(clip) = app.project.timeline.clip(cid).cloned() {
                let p = MontagePreset::from_clip(app.presets_ui.new_name.trim(), app.presets_ui.new_cat, &clip);
                let dir = user_presets_dir();
                match p.save_user_preset(&dir) {
                    Ok(path) => {
                        app.presets_ui.new_name.clear();
                        app.presets_ui.user_presets = MontagePreset::load_user_presets(&dir);
                        app.status(format!("Saved user preset to {}", path.display()));
                    }
                    Err(e) => app.status(format!("Failed to save user preset: {e}")),
                }
            }
        }
    }

    ui.separator();

    let query = app.presets_ui.search.trim().to_lowercase();
    let cat_filter = app.presets_ui.category;
    let mut presets: Vec<MontagePreset> = MontagePreset::built_in();
    presets.extend(app.presets_ui.user_presets.iter().cloned());

    let mut apply_name: Option<String> = None;

    egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
        for preset in presets {
            if let Some(c) = cat_filter {
                if preset.category != c {
                    continue;
                }
            }
            if !query.is_empty()
                && !preset.name.to_lowercase().contains(&query)
                && !preset.description.to_lowercase().contains(&query)
                && !preset.game_tag.to_lowercase().contains(&query)
            {
                continue;
            }

            let badge_col = category_color(preset.category);

            // The card's drag-to-timeline area must be registered *before* its contents: egui gives a
            // drag-only widget on top every click under it, which made the ⚡ Apply button dead. Its rect
            // is last frame's card rect (the card doesn't move between frames unless the list scrolls).
            let drag_id = ui.make_persistent_id(&preset.name);
            let rect_key = drag_id.with("card-rect");
            let prev_rect = ui.ctx().data(|d| d.get_temp::<egui::Rect>(rect_key)).unwrap_or(egui::Rect::NOTHING);
            let drag_resp = ui.interact(prev_rect, drag_id, Sense::drag());
            drag_resp.dnd_set_drag_payload(preset.name.clone());

            let card_resp = egui::Frame::new()
                .fill(Color32::from_rgb(0x19, 0x1A, 0x1F))
                .stroke(Stroke::new(1.0_f32, theme::BORDER))
                .corner_radius(CornerRadius::same(6))
                .inner_margin(egui::Margin::same(8))
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.label(egui::RichText::new(preset.name.as_str()).strong().color(Color32::WHITE));
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            let apply = ui.small_button("⚡ Apply").on_hover_text(format!("Apply \u{201c}{}\u{201d} at the playhead to the selected clip", preset.name));
                            // one name per card for screen readers and the UI driver
                            let name = format!("Apply {}", preset.name);
                            ui.ctx().accesskit_node_builder(apply.id, |n| n.set_label(name));
                            if apply.clicked() {
                                apply_name = Some(preset.name.clone());
                            }
                            if !preset.game_tag.is_empty() {
                                ui.label(egui::RichText::new(&preset.game_tag).small().color(badge_col));
                            }
                        });
                    });
                    ui.label(egui::RichText::new(preset.description.as_str()).small().color(theme::TEXT_DIM));
                    ui.label(egui::RichText::new(preset.category.label()).small().color(badge_col));
                })
                .response;

            if prev_rect != card_resp.rect {
                ui.ctx().data_mut(|d| d.insert_temp(rect_key, card_resp.rect));
                ui.ctx().request_repaint();
            }
            if ui.rect_contains_pointer(card_resp.rect) {
                ui.painter().rect_stroke(card_resp.rect, CornerRadius::same(6), Stroke::new(1.0_f32, badge_col), StrokeKind::Inside);
            }
            ui.add_space(4.0);
        }
    });

    if let Some(name) = apply_name {
        if let Some(id) = app.target_clip() {
            app.apply_montage_preset_by_name(id, &name);
        } else {
            app.status("Select a clip on the timeline first to apply preset");
        }
    }
}
