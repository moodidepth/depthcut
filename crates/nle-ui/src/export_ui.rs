//! Share / Export dialog: AV1 / HEVC / H.264 on whichever hardware encoder
//! the startup probe found working (QSV → NVENC → AMF), or software x264. Rendering runs on a worker thread on the shared GPU.

use crate::theme;
use eframe::egui;
use nle_core::{AssetId, Clip, ClipKind, Project};
use nle_media::encoder_probe;
use nle_media::AudioBuffer;
use nle_render::export::{fits_size_budget, run_export, working_presets, AudioExport, ExportProgress, RateControl};
use nle_render::{ExportPreset, ExportSettings, GamingDeliveryPreset, GpuContext};
use parking_lot::Mutex;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Instant;

struct Job {
    progress: Arc<Mutex<Option<ExportProgress>>>,
    result: Arc<Mutex<Option<Result<(), String>>>>,
    cancel: Arc<AtomicBool>,
    started: Instant,
    path: PathBuf,
}

pub struct ExportDialog {
    pub open: bool,
    pub delivery: GamingDeliveryPreset,
    preset: Option<ExportPreset>,
    quality: u8,
    use_bitrate: bool,
    bitrate_kbps: u32,
    audio: usize,
    /// 0 = project / delivery size, otherwise `OUTPUT_SIZES[resolution - 1]`.
    resolution: usize,
    /// 0 = project / delivery rate, 1 = 30 fps, 2 = 60 fps.
    fps: usize,
    tier: QualityTier,
    /// Render the caption track into the picture.
    pub burn_captions: bool,
    /// Also write the captions as a `.srt` next to the movie.
    pub export_srt: bool,
    /// Last successfully exported file (for "Open Output Folder").
    done_path: Option<PathBuf>,
    /// Program loudness: 0 = off, 1 = -14 LUFS (YouTube), 2 = -16 LUFS (Shorts/TikTok).
    loudness: usize,
    path: Option<PathBuf>,
    job: Option<Job>,
    last_result: Option<String>,
}

impl Default for ExportDialog {
    fn default() -> Self {
        ExportDialog {
            open: false,
            delivery: GamingDeliveryPreset::ProjectDefault,
            preset: None,
            quality: 24,
            use_bitrate: false,
            bitrate_kbps: 20_000,
            audio: 0,
            resolution: 0,
            fps: 0,
            tier: QualityTier::Balanced,
            burn_captions: true,
            export_srt: false,
            done_path: None,
            loudness: 0,
            path: None,
            job: None,
            last_result: None,
        }
    }
}

impl ExportDialog {
    pub fn running(&self) -> bool {
        self.job.as_ref().is_some_and(|j| j.result.lock().is_none())
    }

    pub fn show(&mut self, ctx: &egui::Context, project: &Project, gpu: Option<&GpuContext>, audio: &HashMap<AssetId, Arc<AudioBuffer>>) {
        // finished job?
        let finished = self.job.as_ref().and_then(|j| j.result.lock().take().map(|r| (r, j.path.clone(), j.started)));
        if let Some((r, path, started)) = finished {
            self.done_path = r.is_ok().then(|| path.clone());
            self.last_result = Some(match r {
                Ok(()) => {
                    let mut msg = format!("Exported {} in {:.1}s", path.display(), started.elapsed().as_secs_f32());
                    if self.export_srt && !project.timeline.captions.is_empty() {
                        let srt = path.with_extension("srt");
                        match std::fs::write(&srt, project.timeline.captions.to_srt()) {
                            Ok(()) => msg.push_str(&format!(" — captions written to {}", srt.display())),
                            Err(e) => msg.push_str(&format!(" — could not write {}: {e}", srt.display())),
                        }
                    }
                    if let Some(cp) = write_credits_file(project, &path) {
                        msg.push_str(&format!(" — credits written to {}", cp.display()));
                    }
                    msg
                }
                Err(e) => format!("Export failed: {e}"),
            });
            self.job = None;
            self.open = true;
        }
        if !self.open {
            return;
        }
        if self.path.is_none() {
            self.path = Some(default_output_path(&project.name));
        }
        // Cached result of the startup hardware probe (None while it runs).
        let probe = encoder_probe::result();
        let presets = probe.map(working_presets).unwrap_or_default();
        if self.preset.is_none_or_not_in(&presets) {
            self.preset = presets.first().copied();
            if let Some(p) = self.preset {
                self.quality = p.default_quality();
            }
        }
        let mut open = self.open;
        egui::Window::new("Render").open(&mut open).resizable(false).collapsible(false).default_width(440.0).show(ctx, |ui| {
            if let Some(job) = &self.job {
                let p = *job.progress.lock();
                let (frac, text) = match p {
                    Some(p) => (p.frame as f32 / p.total.max(1) as f32, format!("Frame {} / {} · {:.1} fps", p.frame, p.total, p.fps)),
                    None => (0.0, "Starting encoder…".into()),
                };
                ui.label(format!("Rendering to {}", job.path.display()));
                ui.add(egui::ProgressBar::new(frac).show_percentage().animate(true));
                ui.label(egui::RichText::new(text).monospace().color(theme::TEXT_DIM));
                if ui.button("Cancel").clicked() {
                    job.cancel.store(true, Ordering::Relaxed);
                }
                ctx.request_repaint();
                return;
            }
            match probe {
                None => {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label("Detecting hardware encoders…");
                    });
                    ctx.request_repaint_after(std::time::Duration::from_millis(100));
                }
                Some(p) if p.encoders.is_empty() => {
                    ui.colored_label(theme::PLAYHEAD, "No working video encoder was found. Install FFmpeg with libx264 or a GPU encoder to export.");
                }
                Some(p) => {
                    let text = if p.has_hardware() { format!("GPU encoder: {}", p.backend.label()) } else { "No GPU encoder detected — using CPU (libx264)".to_string() };
                    ui.label(egui::RichText::new(text).color(theme::TEXT_DIM));
                }
            }
            let f = project.timeline.format;
            let dur_secs = project.timeline.extent().as_secs_f64().max(0.1);
            let mut target_w = f.width;
            let mut target_h = f.height;
            let mut target_rate = f.rate;
            if self.delivery != GamingDeliveryPreset::ProjectDefault {
                let mut preview_s = ExportSettings::new("preview.mp4", self.preset.unwrap_or(ExportPreset::H264Software), project);
                self.delivery.apply(&mut preview_s, f, &presets);
                target_w = preview_s.width;
                target_h = preview_s.height;
                target_rate = preview_s.frame_rate;
            }
            let auto_size = (target_w, target_h);
            let auto_rate = target_rate;
            match self.fps {
                1 => target_rate = nle_core::FrameRate::FPS_30,
                2 => target_rate = nle_core::FrameRate::FPS_60,
                _ => {}
            }
            egui::Grid::new("export").num_columns(2).spacing([12.0, 6.0]).show(ui, |ui| {
                ui.label("Gaming Delivery");
                egui::ComboBox::from_id_salt("gaming-delivery").selected_text(self.delivery.label()).width(240.0).show_ui(ui, |ui| {
                    for g in GamingDeliveryPreset::ALL {
                        if ui.selectable_label(self.delivery == g, g.label()).clicked() {
                            self.delivery = g;
                            if g != GamingDeliveryPreset::ProjectDefault {
                                let mut s = ExportSettings::new("preview.mp4", self.preset.unwrap_or(ExportPreset::H264Software), project);
                                g.apply(&mut s, f, &presets);
                                self.preset = Some(s.preset);
                                if let RateControl::Bitrate(kbps) = s.rate {
                                    self.use_bitrate = true;
                                    self.bitrate_kbps = kbps;
                                }
                                self.audio = 0;
                                self.resolution = 0;
                                self.fps = 0;
                                self.loudness = loudness_choice(s.loudness_target);
                            }
                        }
                    }
                });
                ui.end_row();
                ui.label("Video codec");
                egui::ComboBox::from_id_salt("preset").selected_text(self.preset.map(|p| p.label()).unwrap_or("none")).width(240.0).show_ui(ui, |ui| {
                    for p in &presets {
                        if ui.selectable_label(self.preset == Some(*p), p.label()).clicked() {
                            self.preset = Some(*p);
                            self.quality = p.default_quality();
                        }
                    }
                });
                ui.end_row();
                ui.label("Encoder");
                ui.label(egui::RichText::new(self.preset.map(|p| p.encoder()).unwrap_or("-")).monospace().color(theme::TEXT_DIM));
                ui.end_row();
                ui.label("Quality");
                ui.horizontal(|ui| {
                    for t in QualityTier::ALL {
                        if ui.selectable_label(self.tier == t, t.label()).on_hover_text(t.hint()).clicked() {
                            self.tier = t;
                            if let Some(p) = self.preset {
                                self.quality = t.quality(p.default_quality());
                            }
                        }
                    }
                });
                ui.end_row();
                ui.label("Rate control");
                ui.horizontal(|ui| {
                    ui.selectable_value(&mut self.use_bitrate, false, "Quality");
                    ui.selectable_value(&mut self.use_bitrate, true, "Bitrate");
                });
                ui.end_row();
                if self.use_bitrate {
                    ui.label("Bitrate");
                    ui.add(egui::Slider::new(&mut self.bitrate_kbps, 250..=150_000).suffix(" kbit/s").logarithmic(true));
                } else {
                    ui.label("Quality (lower = better)");
                    ui.add(egui::Slider::new(&mut self.quality, 1..=51));
                }
                ui.end_row();
                ui.label("Resolution");
                let auto_label = format!("{}×{} (project)", auto_size.0, auto_size.1);
                let current = self.resolution.checked_sub(1).and_then(|i| OUTPUT_SIZES.get(i)).map_or(auto_label.clone(), |r| r.2.to_string());
                egui::ComboBox::from_id_salt("export-res").selected_text(current).width(240.0).show_ui(ui, |ui| {
                    ui.selectable_value(&mut self.resolution, 0, auto_label);
                    for (i, (_, _, label)) in OUTPUT_SIZES.iter().enumerate() {
                        ui.selectable_value(&mut self.resolution, i + 1, *label);
                    }
                });
                ui.end_row();
                ui.label("Frame rate");
                ui.horizontal(|ui| {
                    ui.selectable_value(&mut self.fps, 0, format!("Project ({})", auto_rate.label()));
                    ui.selectable_value(&mut self.fps, 1, "30 fps");
                    ui.selectable_value(&mut self.fps, 2, "60 fps");
                });
                ui.end_row();
                ui.label("Captions");
                let has_captions = !project.timeline.captions.is_empty();
                ui.add_enabled_ui(has_captions, |ui| {
                    ui.horizontal(|ui| {
                        ui.checkbox(&mut self.burn_captions, "Burn In Captions").on_hover_text("Render the caption track into the video");
                        ui.checkbox(&mut self.export_srt, "Export .srt").on_hover_text("Also write a SubRip file next to the video");
                    });
                })
                .response
                .on_disabled_hover_text("The timeline has no captions");
                ui.end_row();
                ui.label("Audio");
                ui.horizontal(|ui| {
                    ui.selectable_value(&mut self.audio, 0, format!("AAC {}k", self.delivery.audio_kbps()));
                    ui.selectable_value(&mut self.audio, 1, "PCM");
                    ui.selectable_value(&mut self.audio, 2, "None");
                });
                ui.end_row();
                ui.label("Loudness").on_hover_text("Normalize the whole mix to the platform's integrated-loudness target (EBU R128), with a -1 dBFS look-ahead peak limiter.");
                ui.add_enabled_ui(self.audio != 2, |ui| {
                    ui.horizontal(|ui| {
                        ui.selectable_value(&mut self.loudness, 0, "Off");
                        ui.selectable_value(&mut self.loudness, 1, "-14 LUFS (YouTube)");
                        ui.selectable_value(&mut self.loudness, 2, "-16 LUFS (Shorts)");
                    });
                });
                ui.end_row();
                ui.label("Duration");
                if self.use_bitrate {
                    let a_kbps = match self.audio {
                        0 => self.delivery.audio_kbps(),
                        1 => 2304,
                        _ => 0,
                    };
                    let est_mb = ((self.bitrate_kbps + a_kbps) as f64 * 1000.0 / 8.0) * dur_secs / 1_000_000.0;
                    ui.label(format!("{} (~{est_mb:.1} MB est.)", nle_core::format_timecode(project.timeline.extent(), target_rate)));
                    if let Some(cap) = self.delivery.target_bytes() {
                        if !fits_size_budget(cap, dur_secs, self.bitrate_kbps, a_kbps) {
                            ui.end_row();
                            ui.label("");
                            ui.colored_label(theme::PLAYHEAD, format!("Too long for {} MB at this bitrate — shorten the timeline or lower the bitrate.", cap / 1_000_000));
                        }
                    }
                } else {
                    ui.label(nle_core::format_timecode(project.timeline.extent(), target_rate));
                }
                ui.end_row();
            });
            ui.separator();
            ui.horizontal(|ui| {
                let label = self.path.as_ref().map(|p| p.display().to_string()).unwrap_or_else(|| "Choose destination…".into());
                ui.label("Output");
                if ui.button("📁 Browse…").clicked() {
                    let ext = if self.audio == 1 { "mov" } else { "mp4" };
                    let mut dlg = rfd::FileDialog::new()
                        .set_file_name(format!("{}.{ext}", sanitize_file_name(&project.name)))
                        .add_filter("MP4 video", &["mp4"])
                        .add_filter("Movie", &["mov", "mkv"]);
                    if let Some(dir) = self.path.as_ref().and_then(|p| p.parent()) {
                        dlg = dlg.set_directory(dir);
                    }
                    if let Some(mut p) = dlg.save_file() {
                        if p.extension().is_none() {
                            p.set_extension(ext);
                        }
                        self.path = Some(p);
                    }
                }
                ui.label(egui::RichText::new(label).monospace().small().color(theme::TEXT_DIM));
            });
            if !nle_media::ffmpeg_cli::available() {
                ui.colored_label(theme::PLAYHEAD, "FFmpeg was not found on PATH (set RCP_FFMPEG). Export needs the ffmpeg binary.");
            }
            if gpu.is_none() {
                ui.colored_label(theme::PLAYHEAD, "No GPU device available for rendering.");
            }
            if let Some(r) = &self.last_result {
                ui.label(egui::RichText::new(r).color(theme::SNAP));
            }
            ui.horizontal(|ui| {
                let ready = self.preset.is_some() && self.path.is_some() && gpu.is_some();
                if ui.add_enabled(ready, egui::Button::new(egui::RichText::new("⤴ Render").strong())).clicked() {
                    if let (Some(preset), Some(path), Some(gpu)) = (self.preset, self.path.clone(), gpu) {
                        self.start(project, preset, path, gpu.clone(), audio.clone());
                    }
                }
                if let Some(done) = &self.done_path {
                    if ui.button("📂 Open Output Folder").clicked() {
                        reveal_in_folder(done);
                    }
                }
            });
        });
        self.open = open || self.running();
    }

    fn start(&mut self, project: &Project, preset: ExportPreset, path: PathBuf, gpu: GpuContext, audio: HashMap<AssetId, Arc<AudioBuffer>>) {
        let mut settings = ExportSettings::new(path.clone(), preset, project);
        let available = encoder_probe::result().map(working_presets).unwrap_or_default();
        if self.delivery != GamingDeliveryPreset::ProjectDefault {
            self.delivery.apply(&mut settings, project.timeline.format, &available);
            settings.preset = preset;
        }
        settings.rate = if self.use_bitrate { RateControl::Bitrate(self.bitrate_kbps) } else { RateControl::Quality(self.quality) };
        settings.audio = match self.audio {
            0 => AudioExport::Aac { kbps: self.delivery.audio_kbps() },
            1 => AudioExport::Pcm,
            _ => AudioExport::None,
        };
        settings.loudness_target = match self.loudness {
            1 => Some(nle_media::analysis::loudness::TARGET_LUFS),
            2 => Some(nle_media::analysis::loudness::TARGET_LUFS_SHORTS),
            _ => None,
        };
        if let Some(&(w, h, _)) = self.resolution.checked_sub(1).and_then(|i| OUTPUT_SIZES.get(i)) {
            settings.width = w;
            settings.height = h;
        }
        match self.fps {
            1 => settings.frame_rate = nle_core::FrameRate::FPS_30,
            2 => settings.frame_rate = nle_core::FrameRate::FPS_60,
            _ => {}
        }
        let mut project = project.clone();
        if !self.burn_captions {
            project.timeline.captions.enabled = false;
        }
        let progress = Arc::new(Mutex::new(None));
        let result = Arc::new(Mutex::new(None));
        let cancel = Arc::new(AtomicBool::new(false));
        let (p2, r2, c2) = (progress.clone(), result.clone(), cancel.clone());
        let spawned = std::thread::Builder::new().name("rcp-export".into()).spawn(move || {
            let r = run_export(&project, &settings, gpu, &audio, &c2, |p| *p2.lock() = Some(p));
            *r2.lock() = Some(r.map_err(|e| e.to_string()));
        });
        if let Err(e) = spawned {
            self.last_result = Some(format!("Export failed: could not start the export thread ({e})"));
            return;
        }
        self.last_result = None;
        self.done_path = None;
        self.job = Some(Job { progress, result, cancel, started: Instant::now(), path });
    }
}

/// Output sizes offered in the Render dialog (width, height, label).
pub const OUTPUT_SIZES: [(u32, u32, &str); 4] = [
    (1080, 1920, "1080×1920 (9:16 Shorts / TikTok)"),
    (1920, 1080, "1920×1080 (1080p)"),
    (2560, 1440, "2560×1440 (1440p)"),
    (3840, 2160, "3840×2160 (4K)"),
];

/// Encoder quality tiers: an offset on the codec's default quality value
/// (lower = better), so each tier means the same thing on every encoder.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QualityTier {
    Fast,
    Balanced,
    High,
}

impl QualityTier {
    pub const ALL: [QualityTier; 3] = [QualityTier::Fast, QualityTier::Balanced, QualityTier::High];
    pub fn label(self) -> &'static str {
        match self {
            QualityTier::Fast => "Fast",
            QualityTier::Balanced => "Balanced",
            QualityTier::High => "High",
        }
    }
    fn hint(self) -> &'static str {
        match self {
            QualityTier::Fast => "Smaller file, quicker upload",
            QualityTier::Balanced => "The encoder's recommended quality",
            QualityTier::High => "Near-lossless master, larger file",
        }
    }
    pub fn quality(self, default: u8) -> u8 {
        let q = match self {
            QualityTier::Fast => default as i32 + 5,
            QualityTier::Balanced => default as i32,
            QualityTier::High => default as i32 - 5,
        };
        q.clamp(1, 51) as u8
    }
}

/// `<Videos or Desktop>/<project>.mp4`, numbered so it never overwrites.
pub fn default_output_path(project_name: &str) -> PathBuf {
    let home = std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME")).map(PathBuf::from);
    let dir = home
        .map(|h| {
            let videos = h.join("Videos");
            if videos.is_dir() {
                videos
            } else {
                h.join("Desktop")
            }
        })
        .filter(|d| d.is_dir())
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
    let stem = sanitize_file_name(project_name);
    let mut p = dir.join(format!("{stem}.mp4"));
    let mut n = 2;
    while p.exists() {
        p = dir.join(format!("{stem} ({n}).mp4"));
        n += 1;
    }
    p
}

fn sanitize_file_name(name: &str) -> String {
    let s: String = name.chars().map(|c| if c.is_control() || "<>:\"/\\|?*".contains(c) { '_' } else { c }).collect();
    let s = s.trim().trim_end_matches('.').trim().to_string();
    if s.is_empty() {
        "Untitled".into()
    } else {
        s
    }
}

/// Show `file` selected in the system file manager.
pub(crate) fn reveal_in_folder(file: &Path) {
    #[cfg(windows)]
    {
        let _ = std::process::Command::new("explorer").arg(format!("/select,{}", file.display())).spawn();
    }
    #[cfg(target_os = "macos")]
    {
        let _ = std::process::Command::new("open").arg("-R").arg(file).spawn();
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        if let Some(dir) = file.parent() {
            let _ = std::process::Command::new("xdg-open").arg(dir).spawn();
        }
    }
}

trait PresetCheck {
    fn is_none_or_not_in(&self, list: &[ExportPreset]) -> bool;
}

impl PresetCheck for Option<ExportPreset> {
    fn is_none_or_not_in(&self, list: &[ExportPreset]) -> bool {
        match self {
            None => true,
            Some(p) => !list.contains(p),
        }
    }
}

fn collect_used_assets(clips: &[Clip], out: &mut HashSet<AssetId>) {
    for c in clips {
        if !c.enabled {
            continue;
        }
        match &c.kind {
            ClipKind::Asset { asset } => {
                out.insert(*asset);
            }
            ClipKind::Compound(cc) => collect_used_assets(&cc.spine, out),
            _ => {}
        }
        collect_used_assets(&c.connected, out);
    }
}

fn write_credits_file(project: &Project, movie_path: &Path) -> Option<PathBuf> {
    let mut used = HashSet::new();
    collect_used_assets(&project.timeline.spine, &mut used);
    let lines: Vec<String> = project
        .assets
        .iter()
        .filter(|a| used.contains(&a.id))
        .filter_map(|a| a.attribution.as_ref().filter(|attr| attr.requires_credit()).map(|attr| attr.credit_line()))
        .collect();
    if lines.is_empty() {
        return None;
    }
    let credits_path = movie_path.parent()?.join("project_credits.txt");
    let body = format!("Music & Sound Credits (generated by Rust Cut Pro)\n\n{}\n", lines.join("\n"));
    match std::fs::write(&credits_path, body) {
        Ok(()) => Some(credits_path),
        Err(e) => {
            log::warn!("Failed to write {}: {e}", credits_path.display());
            None
        }
    }
}

/// Map a preset's LUFS target onto the dialog's loudness choice.
fn loudness_choice(target: Option<f32>) -> usize {
    match target {
        Some(t) if t <= -15.0 => 2,
        Some(_) => 1,
        None => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quality_tiers_are_ordered_and_clamped() {
        assert!(QualityTier::High.quality(24) < QualityTier::Balanced.quality(24));
        assert!(QualityTier::Balanced.quality(24) < QualityTier::Fast.quality(24));
        assert_eq!(QualityTier::High.quality(3), 1);
        assert_eq!(QualityTier::Fast.quality(50), 51);
    }

    #[test]
    fn default_output_is_an_unused_mp4_with_a_safe_name() {
        let p = default_output_path("Ace: clutch/round?");
        assert_eq!(p.extension().unwrap(), "mp4");
        assert!(!p.exists());
        let name = p.file_name().unwrap().to_string_lossy().to_string();
        assert!(!name.contains(['/', ':', '?']), "{name}");
        assert_eq!(sanitize_file_name("  ..."), "Untitled");
    }
}
