//! Help → About Rust Cut Pro: version, FFmpeg status and the
//! "Licenses & Notices" viewer (our MIT / Apache-2.0 licenses and every
//! third-party notice, embedded at build time).

use crate::theme;
use eframe::egui;
use nle_media::ffmpeg_cli;

const NOTICES: &str = include_str!("../../../THIRD_PARTY_NOTICES.md");
const LICENSE_MIT: &str = include_str!("../../../LICENSE-MIT");
const LICENSE_APACHE: &str = include_str!("../../../LICENSE-APACHE");

#[derive(Clone, Copy, PartialEq, Eq, Default)]
enum Tab {
    #[default]
    About,
    Notices,
    Mit,
    Apache,
}

#[derive(Default)]
pub struct AboutState {
    pub open: bool,
    tab: Tab,
    filter: String,
    /// `ffmpeg -version` first line, probed once per opening.
    ffmpeg_version: Option<Option<String>>,
}

impl AboutState {
    pub fn open_licenses(&mut self) {
        self.open = true;
        self.tab = Tab::Notices;
    }
}

pub fn show(state: &mut AboutState, ctx: &egui::Context) {
    if !state.open {
        state.ffmpeg_version = None;
        return;
    }
    let mut open = state.open;
    egui::Window::new("About Rust Cut Pro")
        .open(&mut open)
        .default_size([720.0, 560.0])
        .resizable(true)
        .collapsible(false)
        .show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.selectable_value(&mut state.tab, Tab::About, "About");
                ui.selectable_value(&mut state.tab, Tab::Notices, "Licenses & Notices");
                ui.selectable_value(&mut state.tab, Tab::Mit, "LICENSE-MIT");
                ui.selectable_value(&mut state.tab, Tab::Apache, "LICENSE-APACHE");
            });
            ui.separator();
            match state.tab {
                Tab::About => about(ui, &mut state.ffmpeg_version),
                Tab::Notices => {
                    ui.horizontal(|ui| {
                        ui.label("Filter");
                        ui.add(egui::TextEdit::singleline(&mut state.filter).hint_text("crate name or text").desired_width(240.0));
                        if !state.filter.is_empty() && ui.small_button("✕").clicked() {
                            state.filter.clear();
                        }
                    });
                    text_view(ui, NOTICES, &state.filter);
                }
                Tab::Mit => text_view(ui, LICENSE_MIT, ""),
                Tab::Apache => text_view(ui, LICENSE_APACHE, ""),
            }
        });
    state.open = open;
}

fn about(ui: &mut egui::Ui, version_cache: &mut Option<Option<String>>) {
    ui.heading(format!("Rust Cut Pro {}", env!("CARGO_PKG_VERSION")));
    ui.label("A fast, GPU-accelerated video editor for gaming montages.");
    ui.add_space(6.0);
    ui.label("Licensed under MIT OR Apache-2.0.");
    ui.label(egui::RichText::new("Built with egui, wgpu, whisper.cpp and 300+ open-source Rust crates — see Licenses & Notices.").color(theme::TEXT_DIM));
    ui.add_space(10.0);
    ui.label(egui::RichText::new("FFmpeg").strong());
    if ffmpeg_cli::available() {
        let ff = ffmpeg_cli::ffmpeg_path();
        ui.label(format!("Using {}", ff.display()));
        let version = version_cache.get_or_insert_with(|| {
            ffmpeg_cli::run_capture(&ff, &["-hide_banner", "-version"]).and_then(|v| v.lines().next().map(str::to_string))
        });
        if let Some(v) = version {
            ui.label(egui::RichText::new(v.as_str()).small().color(theme::TEXT_DIM));
        }
    } else {
        ui.label("Not found — use the banner at the top of the window to install or locate it.");
    }
    ui.label(
        egui::RichText::new(
            "FFmpeg is not part of Rust Cut Pro; it runs as a separate program under its own license (GPL for the builds the editor downloads).",
        )
        .small()
        .color(theme::TEXT_DIM),
    );
    let lic = ffmpeg_cli::managed_bin_dir().join("FFMPEG-LICENSE.txt");
    if lic.is_file() && ui.button("Show downloaded FFmpeg license").clicked() {
        crate::export_ui::reveal_in_folder(&lic);
    }
}

/// Monospace, virtualised view of a long text (the notices are ~11k lines).
fn text_view(ui: &mut egui::Ui, text: &str, filter: &str) {
    let needle = filter.trim().to_lowercase();
    let lines: Vec<&str> = if needle.is_empty() {
        text.lines().collect()
    } else {
        text.lines().filter(|l| l.to_lowercase().contains(&needle)).collect()
    };
    let row_h = ui.text_style_height(&egui::TextStyle::Monospace);
    egui::ScrollArea::both().auto_shrink([false, false]).show_rows(ui, row_h, lines.len(), |ui, range| {
        for line in &lines[range] {
            ui.label(egui::RichText::new(*line).monospace().size(11.5));
        }
    });
}
