//! Turns one frame of input into the bound editor actions.
//!
//! egui-winit reports Ctrl+(Shift+)C / X / V, and Shift+Delete on Windows, as
//! clipboard events with no key press; those presses are rebuilt here.

use crate::shortcuts_ui;
use eframe::egui;
use nle_core::keymap::{Action, ActionMap, KeyBinding, KeyCode, Modifiers};

/// The key presses behind this frame's clipboard events; `paste` is whether
/// Ctrl+V went down. The key's release, if it is in the same frame, says which
/// modifiers were held.
pub fn clipboard_keys(input: &egui::InputState, paste: bool) -> Vec<KeyBinding> {
    use egui::{Event, Key};
    let release = |after: usize, keys: &[Key]| {
        input.events.iter().skip(after).find_map(|e| match e {
            Event::Key { key, pressed: false, modifiers, .. } if keys.contains(key) => Some((Some(*key), *modifiers)),
            _ => None,
        })
    };
    let binding = |key: KeyCode, mods: egui::Modifiers| {
        let m = shortcuts_ui::from_egui_mods(mods);
        // without Ctrl it was Ctrl/Shift+Insert, a dedicated key or a released modifier
        let mods = if m.ctrl { m } else { Modifiers { ctrl: true, ..Modifiers::NONE } };
        KeyBinding::key(key).with_mods(mods)
    };
    let mut out = Vec::new();
    for (i, event) in input.events.iter().enumerate() {
        match event {
            Event::Copy => {
                let (_, mods) = release(i, &[Key::C]).unwrap_or((None, input.modifiers));
                out.push(binding(KeyCode::C, mods));
            }
            Event::Cut => {
                let (key, mods) = release(i, &[Key::X, Key::Delete]).unwrap_or((None, input.modifiers));
                let delete = match key {
                    Some(key) => key == Key::Delete,
                    None => mods.shift && !mods.ctrl && !mods.command,
                };
                out.push(if delete {
                    KeyBinding::key(KeyCode::Delete).with_mods(Modifiers { shift: true, ..shortcuts_ui::from_egui_mods(mods) })
                } else {
                    binding(KeyCode::X, mods)
                });
            }
            _ => {}
        }
    }
    if paste {
        let (_, mods) = release(0, &[Key::V]).unwrap_or((None, input.modifiers));
        out.push(binding(KeyCode::V, mods));
    }
    out.dedup();
    out
}

/// Bound actions triggered this frame; `paste` is the Ctrl+V edge.
pub fn pressed_actions(ctx: &egui::Context, keymap: &ActionMap, paste: bool) -> Vec<Action> {
    let mut swallowed = ctx.input(|i| clipboard_keys(i, paste));

    let mut entries: Vec<(Action, KeyBinding)> = keymap.bindings.iter().flat_map(|(a, list)| list.iter().copied().map(move |b| (*a, b))).collect();
    entries.sort_by_key(|(a, b)| {
        let specificity = (b.mods.ctrl as u8) + (b.mods.shift as u8) + (b.mods.alt as u8);
        (std::cmp::Reverse(specificity), *a)
    });

    let mut out = Vec::new();
    for (action, kb) in entries {
        let matched = if let Some(kc) = kb.key {
            let ek = shortcuts_ui::to_egui_key(kc);
            let em = shortcuts_ui::to_egui_mods(kb.mods);
            let key_hit = ctx.input_mut(|i| {
                let cur = shortcuts_ui::from_egui_mods(i.modifiers);
                let mods_ok = cur == kb.mods || (kc == KeyCode::Plus && i.modifiers.shift && cur.ctrl == kb.mods.ctrl && cur.alt == kb.mods.alt);
                mods_ok && i.consume_key(em, ek)
            });
            let rebuilt = swallowed.iter().position(|s| *s == kb).map(|n| swallowed.remove(n)).is_some();
            key_hit || rebuilt
        } else if let Some(mb) = kb.mouse {
            let ep = shortcuts_ui::to_egui_pointer(mb);
            ctx.input(|i| i.pointer.button_pressed(ep) && shortcuts_ui::from_egui_mods(i.modifiers) == kb.mods)
        } else {
            false
        };
        if matched {
            out.push(action);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::{pressed_actions, Action, ActionMap};
    use eframe::egui::{self, Event, Key, Modifiers};

    const SHIFT: Modifiers = Modifiers::SHIFT;
    const CTRL: Modifiers = Modifiers { alt: false, ctrl: true, shift: false, mac_cmd: false, command: true };
    const CTRL_SHIFT: Modifiers = Modifiers { alt: false, ctrl: true, shift: true, mac_cmd: false, command: true };

    /// One frame as egui-winit delivers it on Windows.
    fn frame(modifiers: Modifiers, events: Vec<Event>) -> Vec<Action> {
        pasting(modifiers, events, false)
    }

    fn pasting(modifiers: Modifiers, events: Vec<Event>, paste: bool) -> Vec<Action> {
        let ctx = egui::Context::default();
        ctx.begin_pass(egui::RawInput { modifiers, events, ..Default::default() });
        let out = pressed_actions(&ctx, &ActionMap::argon_montage(), paste);
        let _ = ctx.end_pass();
        out
    }

    fn key(key: Key, pressed: bool, modifiers: Modifiers) -> Event {
        Event::Key { key, physical_key: Some(key), pressed, repeat: false, modifiers }
    }

    // egui-winit reports these presses as clipboard events, with no key press.
    #[test]
    fn shift_delete_ripple_deletes() {
        assert_eq!(frame(SHIFT, vec![Event::Cut]), [Action::RippleDelete]);
    }

    #[test]
    fn a_quick_shift_delete_tap_ripple_deletes() {
        let events = vec![Event::Cut, key(Key::Delete, false, SHIFT)];
        assert_eq!(frame(Modifiers::NONE, events), [Action::RippleDelete]);
    }

    #[test]
    fn ctrl_shift_c_opens_auto_captions() {
        assert_eq!(frame(CTRL_SHIFT, vec![Event::Copy]), [Action::AutoCaptions]);
    }

    #[test]
    fn clipboard_shortcuts_reach_their_commands() {
        assert_eq!(frame(CTRL, vec![Event::Copy]), [Action::Copy]);
        assert_eq!(frame(CTRL, vec![Event::Cut]), [Action::Cut]);
        assert_eq!(pasting(CTRL, vec![], true), [Action::Paste]);
        assert_eq!(pasting(SHIFT, vec![], true), [Action::Paste]); // Shift+Insert
        assert!(pasting(CTRL_SHIFT, vec![], true).is_empty());
    }

    #[test]
    fn plus_and_minus_zoom() {
        assert_eq!(frame(Modifiers::NONE, vec![key(Key::Minus, true, Modifiers::NONE)]), [Action::ZoomOut]);
        assert_eq!(frame(Modifiers::NONE, vec![key(Key::Equals, true, Modifiers::NONE)]), [Action::ZoomIn]);
        assert_eq!(frame(SHIFT, vec![key(Key::Plus, true, SHIFT)]), [Action::ZoomIn]);
    }

    #[test]
    fn ordinary_keys_still_match_exactly() {
        assert_eq!(frame(CTRL, vec![key(Key::Z, true, CTRL)]), [Action::Undo]);
        assert_eq!(frame(Modifiers::NONE, vec![key(Key::Delete, true, Modifiers::NONE)]), [Action::DeleteWithGap]);
    }
}
