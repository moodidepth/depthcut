//! Export Audio dialog: the full mix and / or one stem per mixer bus as WAV,
//! FLAC or AAC, rendered on a worker thread (no GPU needed).

use crate::theme;
use eframe::egui;
use nle_core::{AssetId, Project};
use nle_media::audio::StemTap;
use nle_media::AudioBuffer;
use nle_render::audio_export::{all_stems, run_audio_export, stem_label, AudioExportOutput, AudioExportSettings, AudioFormat};
use parking_lot::Mutex;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Instant;

/// Filled by the worker: the files written, or why it failed.
type JobResult = Arc<Mutex<Option<Result<AudioExportOutput, String>>>>;

struct Job {
    progress: Arc<Mutex<f32>>,
    result: JobResult,
    cancel: Arc<AtomicBool>,
    started: Instant,
}

pub struct AudioExportDialog {
    pub open: bool,
    format: usize,
    aac_kbps: u32,
    mix: bool,
    stems: Vec<StemTap>,
    /// 0 = off, 1 = −14 LUFS, 2 = −16, 3 = −23 (EBU R128 broadcast).
    loudness: usize,
    /// Raw / unlimited master (32-bit float WAV only).
    raw_master: bool,
    path: Option<PathBuf>,
    job: Option<Job>,
    last: Option<(String, Option<PathBuf>)>,
    /// What the limiters did to the last export's mix (dB).
    last_limited_db: Option<f32>,
}

impl Default for AudioExportDialog {
    fn default() -> Self {
        AudioExportDialog {
            open: false,
            format: 0,
            aac_kbps: 320,
            mix: true,
            stems: Vec::new(),
            loudness: 0,
            raw_master: false,
            path: None,
            job: None,
            last: None,
            last_limited_db: None,
        }
    }
}

const LOUDNESS: [(&str, Option<f32>); 4] = [("Off", None), ("−14 LUFS (streaming)", Some(-14.0)), ("−16 LUFS (podcast)", Some(-16.0)), ("−23 LUFS (EBU R128)", Some(-23.0))];

impl AudioExportDialog {
    pub fn running(&self) -> bool {
        self.job.as_ref().is_some_and(|j| j.result.lock().is_none())
    }

    /// Settings and the last result for the debug state dump.
    pub fn debug_state(&self) -> serde_json::Value {
        serde_json::json!({
            "running": self.running(),
            "format": AudioFormat::ALL[self.format].label(),
            "raw_master": self.raw_master,
            "unlimited": self.raw_master && AudioFormat::ALL[self.format] == AudioFormat::WavFloat,
            "last": self.last.as_ref().map(|(m, _)| m.clone()),
            "last_limited_db": self.last_limited_db,
        })
    }

    fn settings(&self, project: &Project) -> Option<AudioExportSettings> {
        let mut s = AudioExportSettings::new(self.path.clone()?, project);
        s.format = match AudioFormat::ALL[self.format] {
            AudioFormat::Aac { .. } => AudioFormat::Aac { kbps: self.aac_kbps },
            f => f,
        };
        s.mix = self.mix;
        s.stems = self.stems.clone();
        s.loudness_target = LOUDNESS[self.loudness].1;
        s.raw_master = self.raw_master;
        Some(s)
    }

    pub fn show(&mut self, ctx: &egui::Context, project: &Project, audio: &HashMap<AssetId, Arc<AudioBuffer>>) {
        let finished = self.job.as_ref().and_then(|j| j.result.lock().take().map(|r| (r, j.started)));
        if let Some((r, started)) = finished {
            // the next export gets a fresh name instead of replacing this one
            if r.is_ok() {
                if let Some(p) = self.path.as_ref().filter(|p| p.exists()) {
                    self.path = Some(crate::export_ui::next_free_path(p, &[]));
                }
            }
            self.last_limited_db = r.as_ref().ok().and_then(|o| o.master).map(|m| m.limited_db);
            self.last = Some(match r {
                Ok(out) => {
                    let n = out.files.len();
                    let mut msg = format!("Wrote {n} file{} in {:.1}s", if n == 1 { "" } else { "s" }, started.elapsed().as_secs_f32());
                    if let Some(m) = out.master {
                        msg.push_str(&format!(" — mix: {}", m.summary()));
                        if m.is_hot() {
                            msg.push_str(" — the mix is hot; lower the master");
                        }
                    }
                    (msg, out.files.first().cloned())
                }
                Err(e) if e == nle_render::export::ExportError::Cancelled.to_string() => ("Audio export cancelled".into(), None),
                Err(e) => (format!("Audio export failed: {e}"), None),
            });
            self.job = None;
            self.open = true;
        }
        if !self.open {
            return;
        }
        if self.path.is_none() {
            self.path = Some(crate::export_ui::default_output_path(&project.name).with_extension("wav"));
        }
        // stems of deleted busses drop out
        let available = all_stems(project);
        self.stems.retain(|t| available.contains(t));
        let mut open = self.open;
        egui::Window::new("Export Audio").open(&mut open).resizable(false).collapsible(false).default_width(420.0).show(ctx, |ui| {
            if let Some(job) = &self.job {
                ui.label("Rendering audio…");
                ui.add(egui::ProgressBar::new(*job.progress.lock()).show_percentage().animate(true));
                if ui.button("Cancel").clicked() {
                    job.cancel.store(true, Ordering::Relaxed);
                }
                ctx.request_repaint();
                return;
            }
            egui::Grid::new("audio-export-grid").num_columns(2).spacing([10.0, 6.0]).show(ui, |ui| {
                ui.label("Format");
                ui.horizontal(|ui| {
                    egui::ComboBox::from_id_salt("audio-export-format").selected_text(AudioFormat::ALL[self.format].label()).show_ui(ui, |ui| {
                        for (i, f) in AudioFormat::ALL.iter().enumerate() {
                            ui.selectable_value(&mut self.format, i, f.label());
                        }
                    });
                    if matches!(AudioFormat::ALL[self.format], AudioFormat::Aac { .. }) {
                        ui.add(egui::DragValue::new(&mut self.aac_kbps).range(96..=512).speed(8.0).suffix(" kbps"));
                    }
                });
                ui.end_row();
                ui.label("Full mix");
                ui.horizontal(|ui| {
                    ui.checkbox(&mut self.mix, "");
                    ui.add_enabled_ui(self.mix, |ui| {
                        egui::ComboBox::from_id_salt("audio-export-loudness").selected_text(format!("Loudness: {}", LOUDNESS[self.loudness].0)).show_ui(ui, |ui| {
                            for (i, (name, _)) in LOUDNESS.iter().enumerate() {
                                ui.selectable_value(&mut self.loudness, i, *name);
                            }
                        });
                    });
                });
                ui.end_row();
                ui.label("Master");
                let float = AudioFormat::ALL[self.format] == AudioFormat::WavFloat;
                ui.add_enabled_ui(self.mix && float, |ui| {
                    let r = ui
                        .checkbox(&mut self.raw_master, "Raw / Unlimited Master")
                        .on_hover_text("No master-bus or true-peak limiting on the mix, so overs above 0 dBFS survive for later mastering. Only 32-bit float WAV can carry them.")
                        .on_disabled_hover_text("Only for WAV 32-bit float: other formats clip overs, so the mix holds a true-peak ceiling (AAC −1.5 dBTP, PCM −0.3 dBTP).");
                    crate::widgets::describe(&r, "Raw / Unlimited Master", if self.raw_master && float { "on" } else { "off" });
                });
                ui.end_row();
                ui.label("Stems");
                ui.vertical(|ui| {
                    for tap in &available {
                        let mut on = self.stems.contains(tap);
                        let label = match tap {
                            StemTap::Role(_) => stem_label(project, *tap),
                            StemTap::User(_) => format!("{} (submix)", stem_label(project, *tap)),
                        };
                        if ui.checkbox(&mut on, label).changed() {
                            if on {
                                self.stems.push(*tap);
                            } else {
                                self.stems.retain(|t| t != tap);
                            }
                        }
                    }
                    ui.label(
                        egui::RichText::new("One file per bus, after its fader and before Master. A role bus stem includes the submix busses feeding it.")
                            .small()
                            .color(theme::TEXT_DIM),
                    );
                });
                ui.end_row();
                ui.label("File");
                ui.horizontal(|ui| {
                    let shown = self.path.as_ref().map(|p| p.display().to_string()).unwrap_or_default();
                    ui.add(egui::Label::new(egui::RichText::new(shown).small()).truncate()).on_hover_text("Stems are written next to it as “<name> - <bus>”");
                    if ui.button("Choose…").clicked() {
                        let f = AudioFormat::ALL[self.format];
                        let mut d = rfd::FileDialog::new().add_filter(f.label(), &[f.extension()]);
                        if let Some(p) = &self.path {
                            if let Some(dir) = p.parent() {
                                d = d.set_directory(dir);
                            }
                            d = d.set_file_name(p.with_extension(f.extension()).file_name().unwrap_or_default().to_string_lossy());
                        }
                        if let Some(p) = d.save_file() {
                            self.path = Some(p);
                        }
                    }
                });
                ui.end_row();
            });
            let Some(settings) = self.settings(project) else { return };
            let outputs = settings.outputs(project);
            ui.add_space(4.0);
            let existing = outputs.iter().filter(|(_, p)| p.exists()).count();
            for (_, p) in &outputs {
                ui.label(egui::RichText::new(p.file_name().unwrap_or_default().to_string_lossy()).small().monospace().color(theme::TEXT_DIM));
            }
            if existing > 0 {
                ui.label(egui::RichText::new(format!("{existing} of these already exist and will be replaced")).small().color(theme::SNAP));
            }
            ui.horizontal(|ui| {
                if ui.add_enabled(!outputs.is_empty(), egui::Button::new("Export")).clicked() {
                    self.spawn(project, settings, audio);
                }
                if let Some((msg, file)) = &self.last {
                    ui.label(egui::RichText::new(msg).small());
                    if let Some(f) = file {
                        if ui.small_button("Show in folder").clicked() {
                            crate::export_ui::reveal_in_folder(f);
                        }
                    }
                }
            });
        });
        self.open = open || self.running();
    }

    fn spawn(&mut self, project: &Project, settings: AudioExportSettings, audio: &HashMap<AssetId, Arc<AudioBuffer>>) {
        let progress = Arc::new(Mutex::new(0.0));
        let result = Arc::new(Mutex::new(None));
        let cancel = Arc::new(AtomicBool::new(false));
        let (p2, r2, c2) = (progress.clone(), result.clone(), cancel.clone());
        let project = project.clone();
        let audio = audio.clone();
        let spawned = std::thread::Builder::new().name("rcp-audio-export".into()).spawn(move || {
            let r = run_audio_export(&project, &settings, &audio, &c2, |f| *p2.lock() = f);
            *r2.lock() = Some(r.map_err(|e| e.to_string()));
        });
        match spawned {
            Ok(_) => {
                self.last = None;
                self.job = Some(Job { progress, result, cancel, started: Instant::now() });
            }
            Err(e) => self.last = Some((format!("Audio export failed: could not start its thread ({e})"), None)),
        }
    }
}
