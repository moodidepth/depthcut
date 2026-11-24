//! Record Voiceover: capture a microphone into the project's `Voiceover`
//! folder while the timeline plays, then place the take at the point where
//! recording started (role Dialogue, on a free lane below the spine).

use crate::theme;
use eframe::egui;
use nle_core::Time;
use nle_media::audio::record::{input_devices, Recorder};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// What the app should do for the dialog this frame.
pub enum VoiceoverAction {
    /// Recording started at this timeline time: play the timeline (if asked).
    Started { at: Time, play: bool },
    /// A take was written: stop playback, import it and place it at `at`.
    Finished { path: PathBuf, at: Time },
}

enum State {
    Idle,
    CountIn { until: Instant, path: PathBuf, at: Time },
    Recording { rec: Recorder, at: Time },
}

pub struct VoiceoverDialog {
    pub open: bool,
    devices: Option<Vec<String>>,
    /// `None` = the system default input.
    device: Option<String>,
    count_in: u64,
    play_timeline: bool,
    state: State,
    level: f32,
    last: Option<String>,
}

impl Default for VoiceoverDialog {
    fn default() -> Self {
        VoiceoverDialog { open: false, devices: None, device: None, count_in: 3, play_timeline: true, state: State::Idle, level: 0.0, last: None }
    }
}

/// `dir/Voiceover N.wav` with the first unused N.
fn next_take(dir: &Path) -> PathBuf {
    (1..).map(|n| dir.join(format!("Voiceover {n}.wav"))).find(|p| !p.exists()).expect("unbounded")
}

impl VoiceoverDialog {
    pub fn busy(&self) -> bool {
        !matches!(self.state, State::Idle)
    }

    /// Stop a running take (e.g. the user pressed Stop in the transport).
    pub fn stop(&mut self) -> Option<VoiceoverAction> {
        match std::mem::replace(&mut self.state, State::Idle) {
            State::Idle => None,
            State::CountIn { .. } => {
                self.last = Some("Count-in cancelled".into());
                None
            }
            State::Recording { rec, at } => match rec.stop() {
                Ok(take) if take.frames == 0 => {
                    let _ = std::fs::remove_file(&take.path);
                    self.last = Some("Nothing was recorded (the input sent no audio)".into());
                    None
                }
                Ok(take) => {
                    self.last = Some(format!("Recorded {:.1}s → {}", take.frames as f64 / take.sample_rate.max(1) as f64, take.path.display()));
                    Some(VoiceoverAction::Finished { path: take.path, at })
                }
                Err(e) => {
                    self.last = Some(format!("Recording failed: {e}"));
                    None
                }
            },
        }
    }

    /// `dir` is where takes are written; `playhead` is where a new take starts.
    pub fn show(&mut self, ctx: &egui::Context, dir: &Path, playhead: Time) -> Option<VoiceoverAction> {
        if !self.open && !self.busy() {
            return None;
        }
        let mut action = None;
        // count-in finished → start the take
        if let State::CountIn { until, .. } = &self.state {
            if Instant::now() >= *until {
                if let State::CountIn { path, at, .. } = std::mem::replace(&mut self.state, State::Idle) {
                    action = self.begin(&path, at);
                }
            }
        }
        if let State::Recording { rec, .. } = &self.state {
            let p = rec.take_peak();
            self.level = if p > self.level { p } else { self.level * 0.9 };
        }
        if self.busy() {
            ctx.request_repaint_after(Duration::from_millis(33));
        }
        let mut open = self.open;
        let mut stop = false;
        egui::Window::new("Record Voiceover").open(&mut open).resizable(false).collapsible(false).default_width(360.0).show(ctx, |ui| {
            let idle = !self.busy();
            ui.add_enabled_ui(idle, |ui| {
                egui::Grid::new("vo-grid").num_columns(2).spacing([10.0, 6.0]).show(ui, |ui| {
                    ui.label("Input");
                    ui.horizontal(|ui| {
                        let devices = self.devices.get_or_insert_with(input_devices);
                        let shown = self.device.clone().unwrap_or_else(|| "System default".into());
                        egui::ComboBox::from_id_salt("vo-device").selected_text(shown).width(220.0).show_ui(ui, |ui| {
                            ui.selectable_value(&mut self.device, None, "System default");
                            for d in devices.iter() {
                                ui.selectable_value(&mut self.device, Some(d.clone()), d);
                            }
                        });
                        if ui.small_button("⟳").on_hover_text("Look for inputs again").clicked() {
                            self.devices = None;
                        }
                    });
                    ui.end_row();
                    ui.label("Count-in");
                    ui.horizontal(|ui| {
                        ui.selectable_value(&mut self.count_in, 0, "None");
                        ui.selectable_value(&mut self.count_in, 3, "3 s");
                        ui.selectable_value(&mut self.count_in, 5, "5 s");
                    });
                    ui.end_row();
                    ui.label("");
                    ui.checkbox(&mut self.play_timeline, "Play the timeline while recording");
                    ui.end_row();
                });
            });
            ui.add_space(4.0);
            ui.horizontal(|ui| match &self.state {
                State::Idle => {
                    if ui.button(egui::RichText::new("⏺ Record").color(theme::PLAYHEAD)).clicked() {
                        let path = next_take(dir);
                        if self.count_in == 0 {
                            action = self.begin(&path, playhead);
                        } else {
                            self.state = State::CountIn { until: Instant::now() + Duration::from_secs(self.count_in), path, at: playhead };
                        }
                    }
                    ui.label(egui::RichText::new(format!("starts at {:.2}s", playhead.as_secs_f64())).small().color(theme::TEXT_DIM));
                }
                State::CountIn { until, .. } => {
                    let left = until.saturating_duration_since(Instant::now()).as_secs_f32().ceil();
                    ui.label(egui::RichText::new(format!("Recording in {left:.0}…")).strong());
                    if ui.button("Cancel").clicked() {
                        stop = true;
                    }
                }
                State::Recording { rec, .. } => {
                    ui.label(egui::RichText::new(format!("⏺ {:.1}s", rec.seconds())).strong().color(theme::PLAYHEAD));
                    if ui.button("⏹ Stop").clicked() {
                        stop = true;
                    }
                }
            });
            // input level
            let db = 20.0 * self.level.max(1e-6).log10();
            let frac = ((db + 60.0) / 60.0).clamp(0.0, 1.0);
            let (rect, _) = ui.allocate_exact_size(egui::vec2(ui.available_width(), 8.0), egui::Sense::hover());
            ui.painter().rect_filled(rect, 2.0, theme::VIEWER_BG);
            let fill = if db > -1.0 {
                theme::PLAYHEAD
            } else if db > -12.0 {
                theme::SNAP
            } else {
                theme::AUDIO
            };
            ui.painter().rect_filled(egui::Rect::from_min_size(rect.min, egui::vec2(rect.width() * frac, rect.height())), 2.0, fill);
            ui.label(egui::RichText::new(format!("Takes are saved to {}", dir.display())).small().color(theme::TEXT_DIM));
            if let Some(m) = &self.last {
                ui.label(egui::RichText::new(m).small());
            }
        });
        if stop || (!open && self.busy()) {
            if let Some(a) = self.stop() {
                action = Some(a);
            }
        }
        self.open = open;
        action
    }

    fn begin(&mut self, path: &Path, at: Time) -> Option<VoiceoverAction> {
        match Recorder::start(self.device.as_deref(), path) {
            Ok(rec) => {
                self.last = None;
                self.level = 0.0;
                self.state = State::Recording { rec, at };
                Some(VoiceoverAction::Started { at, play: self.play_timeline })
            }
            Err(e) => {
                self.last = Some(format!("Could not start recording: {e}"));
                None
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn takes_are_numbered_past_existing_files() {
        let dir = std::env::temp_dir().join(format!("rcp-vo-names-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        assert_eq!(next_take(&dir).file_name().unwrap(), "Voiceover 1.wav");
        std::fs::write(dir.join("Voiceover 1.wav"), b"").unwrap();
        assert_eq!(next_take(&dir).file_name().unwrap(), "Voiceover 2.wav");
        let _ = std::fs::remove_dir_all(dir);
    }
}
