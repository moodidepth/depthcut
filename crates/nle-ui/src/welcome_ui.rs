//! First-launch welcome: who makes Depthcut, a request for donations, and a
//! note on which features are still alpha. Shown once per install (a marker
//! file in the data folder), reopened from Help → Welcome & Support.

use crate::theme;
use eframe::egui::{self, Color32, RichText};
use std::path::PathBuf;

/// Where the "Support" button goes. Empty hides the button.
/// A release build can override it with `DEPTHCUT_DONATE_URL=... cargo build`.
pub const DONATE_URL: &str = match option_env!("DEPTHCUT_DONATE_URL") {
    Some(url) => url,
    None => "",
};

#[derive(Default)]
pub struct WelcomeState {
    pub open: bool,
    /// Set once the first-launch check ran.
    checked: bool,
}

fn marker_path() -> PathBuf {
    crate::media::data_dir().join("welcome_seen")
}

impl WelcomeState {
    /// Open on the first launch of this install.
    pub fn check_first_launch(&mut self) {
        if self.checked {
            return;
        }
        self.checked = true;
        if !marker_path().exists() {
            self.open = true;
        }
    }

    fn mark_seen() {
        let p = marker_path();
        if let Some(dir) = p.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Err(e) = std::fs::write(&p, env!("CARGO_PKG_VERSION")) {
            log::warn!("could not write {}: {e}", p.display());
        }
    }
}

pub fn show(state: &mut WelcomeState, ctx: &egui::Context) {
    if !state.open {
        return;
    }
    let mut open = true;
    let mut close = false;
    egui::Window::new("Welcome to Depthcut")
        .open(&mut open)
        .collapsible(false)
        .resizable(false)
        .default_width(520.0)
        .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
        .show(ctx, |ui| {
            ui.spacing_mut().item_spacing.y = 8.0;
            ui.label(RichText::new("Hi, I'm Moodi Depth").heading().color(theme::TEXT));
            ui.label(
                "I'm a software developer, and I've been building Depthcut on my own since 2022, with help from AI along the way. \
                 There's no company or team behind it. It's just me.",
            );
            ui.label(
                "My goal is a real, professional video editor that anyone can use fully for free: \
                 no subscriptions, no watermarks, no locked features.",
            );
            ui.label(
                "If Depthcut is useful to you, please consider supporting the project. \
                 Every donation helps me keep working on it and keep it free for everyone.",
            );

            ui.add_space(4.0);
            egui::Frame::new().fill(Color32::from_rgb(0x3A, 0x2A, 0x10)).corner_radius(4.0).inner_margin(egui::Margin::same(8)).show(ui, |ui| {
                ui.horizontal(|ui| {
                    theme::alpha_badge(ui);
                    ui.label(RichText::new("This is an early release").strong());
                });
                ui.label(
                    RichText::new(
                        "Features marked (alpha) are still in development and may not work reliably yet: \
                         the AI Copilot, auto captions and transcript editing, AI mattes, voice isolation, \
                         voiceover recording, AAF export and audio plug-ins.",
                    )
                    .small(),
                );
            });

            ui.add_space(4.0);
            ui.horizontal(|ui| {
                if !DONATE_URL.is_empty() && ui.add(egui::Button::new(RichText::new("♥  Support Depthcut").strong().color(Color32::WHITE)).fill(theme::PLAYHEAD)).clicked() {
                    ctx.open_url(egui::OpenUrl::new_tab(DONATE_URL));
                }
                if ui.button("Get started").clicked() {
                    close = true;
                }
            });
            ui.label(RichText::new("You can open this again from Help → Welcome & Support.").small().color(theme::TEXT_DIM));
        });
    if !open || close {
        state.open = false;
        WelcomeState::mark_seen();
    }
}
