//! First-launch FFmpeg banner: shown while no working `ffmpeg`/`ffprobe` is
//! found. Offers a verified one-click download (gyan.dev essentials build,
//! SHA-256 checked) or picking an existing `ffmpeg.exe`.

use crate::app::App;
use crate::theme;
use eframe::egui::{self, Color32};
use nle_media::ffmpeg_cli;
use nle_media::jobs::Job;
use std::path::PathBuf;
use std::time::Duration;

#[derive(Default)]
pub struct FfmpegSetup {
    job: Option<Job<PathBuf>>,
    error: Option<String>,
    /// Hidden for this session ("Later").
    dismissed: bool,
}

impl FfmpegSetup {
    pub fn busy(&self) -> bool {
        self.job.is_some()
    }
}

/// Top banner; call before the central panel is laid out.
pub fn show(app: &mut App, ctx: &egui::Context) {
    poll(app, ctx);
    let busy = app.ffmpeg_setup.busy();
    if !busy && (app.ffmpeg_setup.dismissed || ffmpeg_cli::available()) {
        return;
    }
    let mut download = false;
    let mut locate = false;
    let mut cancel = false;
    let mut later = false;
    egui::TopBottomPanel::top("ffmpeg_setup").frame(egui::Frame::new().fill(Color32::from_rgb(0x3A, 0x2A, 0x10)).inner_margin(egui::Margin::symmetric(10, 6))).show(ctx, |ui| {
        if let Some(job) = &app.ffmpeg_setup.job {
            let (frac, msg) = job.progress();
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new("⬇ Installing FFmpeg").strong());
                ui.add(egui::ProgressBar::new(frac).desired_width(260.0).text(msg));
                if ui.button("Cancel").clicked() {
                    cancel = true;
                }
            });
            return;
        }
        ui.horizontal_wrapped(|ui| {
            ui.label(egui::RichText::new("⚠ FFmpeg not found").strong().color(Color32::from_rgb(0xFF, 0xC8, 0x57)));
            ui.label(egui::RichText::new("Rust Cut Pro needs ffmpeg.exe and ffprobe.exe to import and export video.").color(theme::TEXT));
            if ui
                .button("Download FFmpeg Automatically")
                .on_hover_text(format!(
                    "Downloads the gyan.dev 'release essentials' build (~115 MB, GPL), checks its SHA-256 and installs it to\n{}",
                    ffmpeg_cli::managed_bin_dir().display()
                ))
                .clicked()
            {
                download = true;
            }
            if ui.button("Locate Existing ffmpeg.exe…").clicked() {
                locate = true;
            }
            if ui.small_button("Later").clicked() {
                later = true;
            }
        });
        if let Some(e) = &app.ffmpeg_setup.error {
            ui.colored_label(Color32::from_rgb(0xFF, 0x8A, 0x80), egui::RichText::new(e).small());
        }
    });

    if cancel {
        if let Some(job) = &app.ffmpeg_setup.job {
            job.cancel();
        }
    }
    if later {
        app.ffmpeg_setup.dismissed = true;
        app.ffmpeg_setup.error = None;
        app.status("FFmpeg is still missing — import and export will not work until it is installed");
    }
    if download {
        start_download(app);
    }
    if locate {
        let picked = rfd::FileDialog::new()
            .set_title("Locate ffmpeg.exe (ffprobe.exe must be in the same folder)")
            .add_filter("ffmpeg", &[if cfg!(windows) { "exe" } else { "*" }])
            .pick_file();
        if let Some(path) = picked {
            match ffmpeg_cli::set_custom_ffmpeg(&path) {
                Ok(ff) => {
                    app.ffmpeg_setup.error = None;
                    app.status(format!("Using FFmpeg at {}", ff.display()));
                }
                Err(e) => app.ffmpeg_setup.error = Some(e),
            }
        }
    }
}

pub fn start_download(app: &mut App) {
    if app.ffmpeg_setup.busy() {
        return;
    }
    app.ffmpeg_setup.error = None;
    app.ffmpeg_setup.job = Some(Job::spawn("ffmpeg-install", |ctx| {
        let cancelled = ctx.cancel_flag();
        nle_media::online::ffmpeg_install::install_ffmpeg(|step| ctx.set_progress(step.fraction(), step.label()), cancelled)
    }));
}

fn poll(app: &mut App, ctx: &egui::Context) {
    let Some(job) = &app.ffmpeg_setup.job else { return };
    match job.take() {
        None => ctx.request_repaint_after(Duration::from_millis(100)),
        Some(res) => {
            app.ffmpeg_setup.job = None;
            ffmpeg_cli::refresh();
            match res {
                Ok(path) => {
                    app.ffmpeg_setup.error = None;
                    app.status(format!("FFmpeg installed ({}). Re-import any clips that failed to load.", path.display()));
                }
                Err(e) if e == "Cancelled" => app.status("FFmpeg download cancelled"),
                Err(e) => {
                    log::error!("FFmpeg install failed: {e}");
                    app.ffmpeg_setup.error = Some(format!("Download failed: {e}"));
                }
            }
        }
    }
}
