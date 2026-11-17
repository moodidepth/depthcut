//! Customizable keyboard & mouse shortcuts modal, presets (`Final Cut Pro` /
//! `DaVinci Resolve`), conflict detection, click-to-record binding and TOML
//! persistence in `%LOCALAPPDATA%\RustCutPro\config\keybindings.toml`.

use crate::app::App;
use crate::media::data_dir;
use crate::theme;
use eframe::egui;
use nle_core::keymap::{Action, ActionMap, KeyBinding, KeyCode, Modifiers, MouseButton, PROFILES};
use std::collections::HashSet;
use std::path::PathBuf;

#[derive(Default)]
pub struct ShortcutsState {
    pub search: String,
    pub recording: Option<Action>,
}

pub fn keybindings_path() -> PathBuf {
    let repo_cfg = PathBuf::from("config").join("keybindings.toml");
    if repo_cfg.is_file() || std::path::Path::new("config").is_dir() {
        return repo_cfg;
    }
    data_dir().join("config").join("keybindings.toml")
}

pub fn load_keymap() -> ActionMap {
    let path = keybindings_path();
    if path.is_file() {
        match ActionMap::load(&path) {
            Ok(map) => return map,
            Err(e) => log::warn!("Could not load {}: {e}; using Argon Montage defaults", path.display()),
        }
    }
    ActionMap::argon_montage()
}

fn save_keymap(map: &ActionMap) -> Result<(), String> {
    let path = keybindings_path();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("create {}: {e}", dir.display()))?;
    }
    map.save(&path).map_err(|e| e.to_string())
}

pub fn to_egui_mods(m: Modifiers) -> egui::Modifiers {
    egui::Modifiers {
        alt: m.alt,
        ctrl: m.ctrl,
        shift: m.shift,
        mac_cmd: false,
        command: m.ctrl,
    }
}

pub fn from_egui_mods(m: egui::Modifiers) -> Modifiers {
    Modifiers {
        ctrl: m.command || m.ctrl,
        shift: m.shift,
        alt: m.alt,
    }
}

pub fn to_egui_pointer(b: MouseButton) -> egui::PointerButton {
    match b {
        MouseButton::Middle => egui::PointerButton::Middle,
        MouseButton::Back => egui::PointerButton::Extra1,
        MouseButton::Forward => egui::PointerButton::Extra2,
    }
}

pub fn from_egui_pointer(b: egui::PointerButton) -> Option<MouseButton> {
    match b {
        egui::PointerButton::Middle => Some(MouseButton::Middle),
        egui::PointerButton::Extra1 => Some(MouseButton::Back),
        egui::PointerButton::Extra2 => Some(MouseButton::Forward),
        _ => None,
    }
}

macro_rules! map_keys {
    ($( $k:ident ),* $(,)?) => {
        pub fn to_egui_key(k: KeyCode) -> egui::Key {
            match k {
                $( KeyCode::$k => egui::Key::$k, )*
            }
        }
        pub fn from_egui_key(k: egui::Key) -> Option<KeyCode> {
            match k {
                $( egui::Key::$k => Some(KeyCode::$k), )*
                _ => None,
            }
        }
    };
}

map_keys!(
    A, B, C, D, E, F, G, H, I, J, K, L, M, N, O, P, Q, R, S, T, U, V, W, X, Y, Z,
    Num0, Num1, Num2, Num3, Num4, Num5, Num6, Num7, Num8, Num9,
    F1, F2, F3, F4, F5, F6, F7, F8, F9, F10, F11, F12,
    Space, Enter, Escape, Tab, Backspace, Delete, Insert, Home, End, PageUp, PageDown,
    ArrowLeft, ArrowRight, ArrowUp, ArrowDown,
    Minus, Equals, Plus, Comma, Period, Slash, Backslash, OpenBracket, CloseBracket, Semicolon, Quote, Backtick,
);

enum Capture {
    Cancel,
    Binding(KeyBinding),
}

fn capture_next_binding(ctx: &egui::Context) -> Option<Capture> {
    ctx.input(|i| {
        for ev in &i.events {
            match ev {
                egui::Event::Key { key, pressed: true, repeat: false, modifiers, .. } => {
                    let mods = from_egui_mods(*modifiers);
                    if *key == egui::Key::Escape && mods == Modifiers::NONE {
                        return Some(Capture::Cancel);
                    }
                    if let Some(kc) = from_egui_key(*key) {
                        return Some(Capture::Binding(KeyBinding::key(kc).with_mods(mods)));
                    }
                }
                egui::Event::PointerButton { button, pressed: true, modifiers, .. } => {
                    if let Some(mb) = from_egui_pointer(*button) {
                        return Some(Capture::Binding(KeyBinding::mouse(mb).with_mods(from_egui_mods(*modifiers))));
                    }
                }
                _ => {}
            }
        }
        None
    })
}

pub fn show(app: &mut App, ctx: &egui::Context) {
    if !app.show_shortcuts {
        app.shortcuts_ui.recording = None;
        return;
    }

    let mut dirty = false;
    if let Some(rec_act) = app.shortcuts_ui.recording {
        match capture_next_binding(ctx) {
            Some(Capture::Cancel) => app.shortcuts_ui.recording = None,
            Some(Capture::Binding(kb)) => {
                app.keymap.add(rec_act, kb);
                app.shortcuts_ui.recording = None;
                dirty = true;
            }
            None => {}
        }
    }

    let mut open = app.show_shortcuts;
    let mut new_preset: Option<&'static str> = None;
    let mut reset_preset = false;

    egui::Window::new("Keyboard & Mouse Shortcuts")
        .open(&mut open)
        .resizable(true)
        .default_width(680.0)
        .default_height(620.0)
        .show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.label("Preset:");
                egui::ComboBox::from_id_salt("keymap-preset")
                    .selected_text(&app.keymap.profile)
                    .width(170.0)
                    .show_ui(ui, |ui| {
                        for p in PROFILES {
                            if ui.selectable_label(app.keymap.profile == p, p).clicked() && app.keymap.profile != p {
                                new_preset = Some(p);
                            }
                        }
                    });
                if ui.button("Reset Preset").on_hover_text("Restore default bindings for the active preset").clicked() {
                    reset_preset = true;
                }
                ui.add_space(8.0);
                ui.add(
                    egui::TextEdit::singleline(&mut app.shortcuts_ui.search)
                        .hint_text("Filter actions or keys…")
                        .desired_width((ui.available_width() - 28.0).max(120.0)),
                );
                if !app.shortcuts_ui.search.is_empty() && ui.small_button("✕").clicked() {
                    app.shortcuts_ui.search.clear();
                }
            });

            if let Some(rec_act) = app.shortcuts_ui.recording {
                ui.add_space(4.0);
                egui::Frame::new()
                    .fill(theme::VIDEO.gamma_multiply(0.2))
                    .stroke(egui::Stroke::new(1.0_f32, theme::VIDEO))
                    .corner_radius(4.0)
                    .inner_margin(egui::Margin::symmetric(8, 6))
                    .show(ui, |ui| {
                        ui.horizontal(|ui| {
                            ui.label(
                                egui::RichText::new(format!(
                                    "Press any key combo or click Middle / Back / Forward mouse button for \"{}\" (Esc to cancel)…",
                                    rec_act.label()
                                ))
                                .color(theme::TEXT),
                            );
                            if ui.small_button("Cancel").clicked() {
                                app.shortcuts_ui.recording = None;
                            }
                        });
                    });
            }

            let conflicts = app.keymap.conflicts();
            let conflict_bindings: HashSet<KeyBinding> = conflicts.iter().map(|(kb, _)| *kb).collect();
            if !conflicts.is_empty() {
                ui.add_space(4.0);
                let summary: Vec<String> = conflicts
                    .iter()
                    .take(4)
                    .map(|(kb, acts)| {
                        let names: Vec<&str> = acts.iter().map(|a| a.label()).collect();
                        format!("{kb}: {}", names.join(" / "))
                    })
                    .collect();
                let extra = if conflicts.len() > 4 {
                    format!(" (+{} more)", conflicts.len() - 4)
                } else {
                    String::new()
                };
                ui.label(
                    egui::RichText::new(format!("⚠ Binding conflicts: {}{extra}", summary.join(" · ")))
                        .small()
                        .color(theme::SNAP),
                );
            }

            ui.separator();

            let q = app.shortcuts_ui.search.trim().to_ascii_lowercase();
            let mut categories: Vec<&'static str> = Vec::new();
            for a in Action::ALL {
                let c = a.category();
                if !categories.contains(&c) {
                    categories.push(c);
                }
            }

            egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                for cat in categories {
                    let actions: Vec<Action> = Action::ALL
                        .iter()
                        .copied()
                        .filter(|a| {
                            if a.category() != cat {
                                return false;
                            }
                            if q.is_empty() {
                                return true;
                            }
                            a.label().to_ascii_lowercase().contains(&q)
                                || a.category().to_ascii_lowercase().contains(&q)
                                || a.id().to_ascii_lowercase().contains(&q)
                                || app.keymap.get(*a).iter().any(|b| b.to_string().to_ascii_lowercase().contains(&q))
                        })
                        .collect();
                    if actions.is_empty() {
                        continue;
                    }

                    egui::CollapsingHeader::new(cat).default_open(true).show(ui, |ui| {
                        egui::Grid::new(("keymap-cat", cat)).striped(true).num_columns(2).spacing([14.0, 6.0]).show(ui, |ui| {
                            for act in actions {
                                ui.label(act.label());
                                ui.horizontal_wrapped(|ui| {
                                    let existing = app.keymap.get(act).to_vec();
                                    for kb in existing {
                                        let is_conflict = conflict_bindings.contains(&kb);
                                        let text = egui::RichText::new(format!("{kb} ✕")).monospace().color(if is_conflict {
                                            theme::PLAYHEAD
                                        } else {
                                            theme::SNAP
                                        });
                                        if ui.small_button(text).on_hover_text("Click to remove binding").clicked() {
                                            app.keymap.remove(act, kb);
                                            dirty = true;
                                        }
                                    }
                                    let is_rec = app.shortcuts_ui.recording == Some(act);
                                    let bind_btn = ui.small_button(if is_rec { "Listening…" } else { "+ Bind" });
                                    if bind_btn.clicked() {
                                        app.shortcuts_ui.recording = if is_rec { None } else { Some(act) };
                                    }
                                    ui.menu_button("🖱", |ui| {
                                        for mb in MouseButton::ALL {
                                            let kb = KeyBinding::mouse(mb);
                                            if ui.button(kb.to_string()).clicked() {
                                                app.keymap.add(act, kb);
                                                dirty = true;
                                                ui.close_menu();
                                            }
                                        }
                                    });
                                });
                                ui.end_row();
                            }
                        });
                    });
                }
            });
        });

    app.show_shortcuts = open;

    if let Some(p) = new_preset {
        if let Some(map) = ActionMap::preset(p) {
            app.keymap = map;
            app.shortcuts_ui.recording = None;
            dirty = true;
            app.status(format!("Loaded shortcut preset: {p}"));
        }
    } else if reset_preset {
        let name = app.keymap.profile.clone();
        app.keymap = ActionMap::preset(&name).unwrap_or_default();
        app.shortcuts_ui.recording = None;
        dirty = true;
        app.status(format!("Reset preset: {name}"));
    }

    if dirty {
        if let Err(e) = save_keymap(&app.keymap) {
            app.status(format!("Could not save shortcuts: {e}"));
        }
    }
}
