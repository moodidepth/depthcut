//! Auto Captions & Whisper transcription modal + live caption segment & style editor.

use crate::app::App;
use crate::theme;
use crate::widgets::slider_f32;
use eframe::egui::{self, Color32};
use nle_core::captions::{parse_srt, CaptionTrack, WordFit, HEAVY_FONT, HIGHLIGHT_CORAL, HIGHLIGHT_GREEN, HIGHLIGHT_YELLOW, OUTLINE_MAX_PX, OUTLINE_MIN_PX, SYNC_OFFSET_MAX_MS};
use nle_core::{Command, Time};
use nle_media::audio::{mixdown, to_mono_16k};
use nle_media::jobs::Job;
use nle_media::online::models::{best_installed, WhisperModel, BASE_Q5_1, MODELS};
use nle_media::transcribe::{self, TranscribeOptions, Transcript};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

const CAPTIONS_DRAG_KEY: u64 = 0xCC00_0001;

pub struct CaptionsState {
    pub model_id: &'static str,
    pub language: String,
    pub use_gpu: bool,
    pub model_job: Option<Job<(&'static str, PathBuf)>>,
    pub gen_job: Option<Job<Transcript>>,
    pub selected_seg: Option<usize>,
    /// How words follow when a caption block is trimmed on the timeline.
    pub word_fit: WordFit,
    /// Backend of the last finished transcription ("GPU: Vulkan (…)" / "CPU Fallback (…)").
    pub last_backend: Option<String>,
}

impl Default for CaptionsState {
    fn default() -> Self {
        let model_id = best_installed().unwrap_or(BASE_Q5_1).id;
        CaptionsState {
            model_id,
            language: String::new(),
            use_gpu: transcribe::GPU_BUILD,
            model_job: None,
            gen_job: None,
            selected_seg: None,
            word_fit: WordFit::Scale,
            last_backend: None,
        }
    }
}

pub fn poll(app: &mut App, ctx: &egui::Context) {
    if let Some(job) = app.captions.model_job.as_ref() {
        if let Some(res) = job.take() {
            app.captions.model_job = None;
            match res {
                Ok((id, path)) => {
                    app.captions.model_id = id;
                    app.status(format!("Whisper model ready: {}", path.display()));
                }
                Err(e) => app.status(format!("Whisper model download failed: {e}")),
            }
        } else {
            ctx.request_repaint_after(Duration::from_millis(50));
        }
    }

    if let Some(job) = app.captions.gen_job.as_ref() {
        if let Some(res) = job.take() {
            app.captions.gen_job = None;
            match res {
                Ok(tr) if tr.words.is_empty() => {
                    app.captions.last_backend = Some(tr.backend.clone());
                    app.status(format!("No speech found — captions unchanged ({})", tr.backend));
                }
                Ok(tr) => {
                    if let Some(reason) = &tr.fallback_reason {
                        log::warn!("captions ran on {}: {reason}", tr.backend);
                    }
                    app.captions.last_backend = Some(match &tr.fallback_reason {
                        Some(reason) => format!("{} — GPU failed: {reason}", tr.backend),
                        None => tr.backend.clone(),
                    });
                    // CapCut phrasing: 2–4 words, break on >250 ms pauses / punctuation
                    let segments = CaptionTrack::phrases(&tr.words);
                    let count = segments.len();
                    let mut track = app.project.timeline.captions.clone();
                    track.enabled = true;
                    track.segments = segments;
                    // the clips carrying this speech become Dialogue (the ducking sidechain) — same undo step
                    let voiced = nle_core::captions::speech_clips(&app.project.timeline, &track);
                    let mut commands = vec![Command::SetCaptions { captions: Box::new(track) }];
                    for id in &voiced {
                        if let Some(mut c) = app.project.timeline.clip(*id).cloned() {
                            c.audio.role = nle_core::AudioRole::Dialogue;
                            commands.push(Command::UpdateClip { clip: Box::new(c) });
                        }
                    }
                    app.exec(Command::Batch { label: "Auto Captions", commands });
                    app.captions.selected_seg = None;
                    let roles = if voiced.is_empty() { String::new() } else { format!(" · {} clip(s) set to Dialogue", voiced.len()) };
                    app.status(format!("Generated {count} caption segments ({}, {}){roles}", tr.language, tr.backend));
                }
                Err(e) => app.status(format!("Caption generation failed: {e}")),
            }
        } else {
            ctx.request_repaint_after(Duration::from_millis(50));
        }
    }
}

pub fn show(app: &mut App, ctx: &egui::Context) {
    if !app.show_captions {
        return;
    }
    let mut open = app.show_captions;
    egui::Window::new("Auto Captions & Whisper").open(&mut open).default_width(560.0).default_height(640.0).resizable(true).show(ctx, |ui| {
        model_manager_ui(app, ui);
        ui.add_space(6.0);
        generate_ui(app, ui);
        ui.separator();
        track_and_style_ui(app, ui);
    });
    app.show_captions = open;
}

fn model_manager_ui(app: &mut App, ui: &mut egui::Ui) {
    let any_installed = MODELS.iter().any(WhisperModel::is_installed);
    egui::CollapsingHeader::new("Whisper Model Manager").default_open(!any_installed).show(ui, |ui| {
        let busy = app.captions.model_job.is_some() || app.captions.gen_job.is_some();
        for m in MODELS {
            let installed = m.is_installed();
            let selected = app.captions.model_id == m.id;
            ui.horizontal(|ui| {
                if ui.radio(selected, m.label).clicked() {
                    app.captions.model_id = m.id;
                }
                ui.label(egui::RichText::new(m.size_label()).small().color(theme::TEXT_DIM));
                if installed {
                    ui.label(egui::RichText::new("Installed").small().color(theme::AUDIO));
                    if ui.add_enabled(!busy, egui::Button::new("Delete")).clicked() {
                        if let Err(e) = m.delete() {
                            app.status(format!("Could not delete model: {e}"));
                        } else if let Some(other) = best_installed() {
                            app.captions.model_id = other.id;
                        }
                    }
                } else if ui.add_enabled(!busy, egui::Button::new("Download")).clicked() {
                    app.captions.model_id = m.id;
                    app.captions.model_job = Some(Job::spawn("whisper-model", move |jctx| {
                        jctx.set_progress(0.0, format!("Downloading {}…", m.label));
                        let c = jctx.cancel_flag();
                        let p = m.download(|f| jctx.set_fraction(f), c)?;
                        Ok((m.id, p))
                    }));
                }
            });
            ui.label(egui::RichText::new(format!("   {}", m.note)).small().color(theme::TEXT_DIM));
        }

        if let Some(job) = &app.captions.model_job {
            let (frac, msg) = job.progress();
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                ui.add(egui::ProgressBar::new(frac).desired_width(ui.available_width() - 80.0).text(format!("{msg} ({:.0}%)", frac * 100.0)));
                if ui.button("Cancel").clicked() {
                    job.cancel();
                }
            });
        }
    });
}

fn generate_ui(app: &mut App, ui: &mut egui::Ui) {
    let model = WhisperModel::by_id(app.captions.model_id);
    let model_ready = model.is_some_and(|m| m.is_installed());
    let has_tl_audio = !app.project.timeline.spine.is_empty() || app.project.timeline.extent() > Time::ZERO;

    ui.horizontal(|ui| {
        if !transcribe::AVAILABLE {
            ui.label(egui::RichText::new("Built without Whisper support").color(theme::PLAYHEAD));
        } else {
            let mode = if transcribe::GPU_BUILD { "Vulkan GPU (with CPU fallback)" } else { "CPU" };
            ui.label(egui::RichText::new(format!("Backend: {mode}")).small().color(theme::TEXT_DIM));
            if let Some(last) = &app.captions.last_backend {
                let color = if last.starts_with("GPU") { theme::AUDIO } else { theme::TEXT_DIM };
                ui.label(egui::RichText::new(format!("· last run: {last}")).small().color(color));
            }
            ui.add_enabled(transcribe::GPU_BUILD, egui::Checkbox::new(&mut app.captions.use_gpu, "Use GPU"));
        }
    });

    ui.horizontal(|ui| {
        ui.label("Language:");
        ui.add(egui::TextEdit::singleline(&mut app.captions.language).desired_width(110.0).hint_text("Auto (en, es, de…)"));

        let can_gen = transcribe::AVAILABLE && model_ready && has_tl_audio && app.captions.gen_job.is_none() && app.captions.model_job.is_none();
        let btn = ui.add_enabled(can_gen, egui::Button::new("🎙 Generate Captions")).on_disabled_hover_text(if !model_ready {
            "Download a Whisper model first"
        } else if !has_tl_audio {
            "Timeline has no audio clips"
        } else {
            "Whisper job already running"
        });

        if btn.clicked() {
            if let Err(e) = start_caption_generation(app) {
                app.status(e);
            }
        }
    });

    if let Some(job) = &app.captions.gen_job {
        let (frac, msg) = job.progress();
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            ui.add(egui::ProgressBar::new(frac).desired_width(ui.available_width() - 80.0).text(format!("{msg} ({:.0}%)", frac * 100.0)));
            if ui.button("Cancel").clicked() {
                job.cancel();
            }
        });
    }
}

/// Start transcribing the timeline into captions (the Generate button and
/// the Copilot's `generate_captions`). `Err` says why it can't start.
pub fn start_caption_generation(app: &mut App) -> Result<(), String> {
    if !transcribe::AVAILABLE {
        return Err("this build has no Whisper support".into());
    }
    if app.captions.gen_job.is_some() || app.captions.model_job.is_some() {
        return Err("a Whisper job is already running".into());
    }
    let Some(model) = WhisperModel::by_id(app.captions.model_id).filter(|m| m.is_installed()) else {
        return Err("no Whisper model downloaded yet (Captions panel ▸ Models)".into());
    };
    if nle_core::audio_spans(&app.project.timeline).is_empty() {
        return Err("the timeline has no audible clips".into());
    }
    if let Some(e) = model.remove_if_corrupt() {
        // Damaged file: deleted above; fetch a clean copy instead of crashing whisper.cpp.
        let m = model;
        app.captions.model_job = Some(Job::spawn("whisper-model", move |jctx| {
            jctx.set_progress(0.0, format!("Re-downloading {} (previous file was damaged)…", m.label));
            let c = jctx.cancel_flag();
            let p = m.download(|f| jctx.set_fraction(f), c)?;
            Ok((m.id, p))
        }));
        return Err(format!("Whisper model {} was damaged ({e}); re-downloading it — generate again when it's done", m.label));
    }
    {
        let m = model;
        let model_path = m.path();
        let language = (!app.captions.language.trim().is_empty()).then(|| app.captions.language.trim().to_string());
        let use_gpu = app.captions.use_gpu;
        let spans = nle_core::audio_spans(&app.project.timeline);
        let extent = app.project.timeline.extent();
        let sources = app.library.audio.clone();

        app.captions.gen_job = Some(Job::spawn("captions", move |jctx| {
            if spans.is_empty() || extent <= Time::ZERO {
                return Err("Timeline has no audible clips".into());
            }
            if jctx.cancelled() {
                return Err("Cancelled".into());
            }
            jctx.set_progress(0.03, "Mixing timeline audio…");
            let mix = mixdown(spans, &sources, Time::ZERO, extent);
            if jctx.cancelled() {
                return Err("Cancelled".into());
            }
            jctx.set_progress(0.10, "Converting to 16 kHz mono…");
            let mono16k = to_mono_16k(&mix);

            let mut opts = TranscribeOptions::new(model_path);
            opts.language = language;
            opts.use_gpu = use_gpu;

            let cancel = Arc::new(AtomicBool::new(false));
            let finished = Arc::new(AtomicBool::new(false));
            let cancel_flag = jctx.cancel_flag();
            let c_watch = cancel.clone();
            let f_watch = finished.clone();
            let watcher = std::thread::Builder::new().name("rcp-whisper-cancel".into()).spawn(move || {
                while !f_watch.load(Ordering::Relaxed) {
                    if cancel_flag() {
                        c_watch.store(true, Ordering::Relaxed);
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(50));
                }
            });

            jctx.set_progress(0.15, "Transcribing with Whisper…");
            let pfn = jctx.progress_fn();
            let prog: Arc<dyn Fn(f32) + Send + Sync> = Arc::new(move |f| pfn(0.15 + 0.85 * f));
            let msg = jctx.message_fn();
            let status: Arc<dyn Fn(String) + Send + Sync> = Arc::new(msg);
            let res = transcribe::transcribe(&mono16k, &opts, prog, status, cancel);
            finished.store(true, Ordering::Relaxed);
            if let Ok(h) = watcher {
                let _ = h.join();
            }
            res
        }));
    }
    Ok(())
}

fn track_and_style_ui(app: &mut App, ui: &mut egui::Ui) {
    let mut track = app.project.timeline.captions.clone();
    let mut discrete_edit = false;
    let mut continuous_edit = false;

    ui.horizontal(|ui| {
        if ui.checkbox(&mut track.enabled, "Enabled").changed() {
            discrete_edit = true;
        }
        let has_words = track.segments.iter().any(|s| !s.words.is_empty());
        if ui.add_enabled(has_words, egui::Button::new("Re-group")).on_hover_text("Re-split into 2–4 word phrases (breaks on pauses > 250 ms and punctuation)").clicked() {
            let words: Vec<_> = track.segments.iter().flat_map(|s| s.words.iter().cloned()).collect();
            track.segments = CaptionTrack::phrases(&words);
            discrete_edit = true;
            app.captions.selected_seg = None;
            app.status(format!("Re-grouped into {} caption phrases", track.segments.len()));
        }
        if ui.button("Import .srt").clicked() {
            if let Some(path) = rfd::FileDialog::new().add_filter("SubRip Subtitles", &["srt"]).pick_file() {
                match std::fs::read_to_string(&path) {
                    Ok(txt) => match parse_srt(&txt) {
                        Ok(mut segs) => {
                            for s in &mut segs {
                                s.retime_words_evenly();
                            }
                            let count = segs.len();
                            track.segments = segs;
                            track.sort();
                            track.enabled = true;
                            discrete_edit = true;
                            app.captions.selected_seg = None;
                            app.status(format!("Imported {count} captions from {}", path.display()));
                        }
                        Err(e) => app.status(format!("Invalid .srt: {e}")),
                    },
                    Err(e) => app.status(format!("Could not read .srt: {e}")),
                }
            }
        }
        if ui.add_enabled(!track.is_empty(), egui::Button::new("Export .srt")).clicked() {
            if let Some(path) = rfd::FileDialog::new().add_filter("SubRip Subtitles", &["srt"]).set_file_name("captions.srt").save_file() {
                match std::fs::write(&path, track.to_srt()) {
                    Ok(()) => app.status(format!("Exported captions to {}", path.display())),
                    Err(e) => app.status(format!("Could not write .srt: {e}")),
                }
            }
        }
        if ui.add_enabled(!track.is_empty(), egui::Button::new("Clear")).clicked() {
            track.segments.clear();
            discrete_edit = true;
            app.captions.selected_seg = None;
        }
    });

    // global sync: shifts every block and word highlight (preview + export)
    ui.horizontal(|ui| {
        let s = &mut track.style;
        let r = ui
            .add(egui::Slider::new(&mut s.sync_offset_ms, -SYNC_OFFSET_MAX_MS..=SYNC_OFFSET_MAX_MS).suffix(" ms").text("Sync Offset"))
            .on_hover_text("+ shows captions later, − earlier. Use when words light up before / after they are heard.");
        continuous_edit |= r.changed();
        if s.sync_offset_ms != 0 && ui.small_button("Reset").clicked() {
            s.sync_offset_ms = 0;
            discrete_edit = true;
        }
    });

    ui.add_space(4.0);
    egui::CollapsingHeader::new("Caption Style").default_open(false).show(ui, |ui| {
        let s = &mut track.style;
        ui.horizontal(|ui| {
            ui.label("Font:");
            let r = ui.add(egui::TextEdit::singleline(&mut s.font_family).desired_width(120.0).hint_text("Sans"));
            continuous_edit |= r.changed();
            if ui.small_button("Heavy").on_hover_text("Montserrat Black → Poppins ExtraBold → Segoe UI Black").clicked() {
                s.font_family = HEAVY_FONT.into();
                discrete_edit = true;
            }
        });
        continuous_edit |= slider_f32(ui, "Tracking", &mut s.tracking, -4.0..=4.0, " px");
        continuous_edit |= slider_f32(ui, "Font Size", &mut s.font_size, 24.0..=120.0, " px");
        continuous_edit |= slider_f32(ui, "Position Y", &mut s.position_y, 0.08..=0.95, "");
        ui.horizontal(|ui| {
            ui.label("Text:");
            continuous_edit |= ui.color_edit_button_rgba_unmultiplied(&mut s.color).changed();
            discrete_edit |= ui.checkbox(&mut s.uppercase, "UPPERCASE").changed();
        });
        ui.horizontal(|ui| {
            discrete_edit |= ui.checkbox(&mut s.word_highlight, "Active Word Highlight").changed();
            if s.word_highlight {
                continuous_edit |= ui.color_edit_button_rgba_unmultiplied(&mut s.highlight_color).changed();
                if ui.small_button("Yellow").on_hover_text("CapCut Yellow #FFE600").clicked() {
                    s.highlight_color = HIGHLIGHT_YELLOW;
                    discrete_edit = true;
                }
                if ui.small_button("Coral").on_hover_text("Coral #FF5A36").clicked() {
                    s.highlight_color = HIGHLIGHT_CORAL;
                    discrete_edit = true;
                }
                if ui.small_button("Green").clicked() {
                    s.highlight_color = HIGHLIGHT_GREEN;
                    discrete_edit = true;
                }
            }
        });
        if s.word_highlight {
            ui.horizontal(|ui| {
                discrete_edit |= ui
                    .checkbox(&mut s.dim_upcoming, "Dim Upcoming Words")
                    .on_hover_text("Words not spoken yet are muted; the spoken word pops in the highlight colour, then turns white")
                    .changed();
                if s.dim_upcoming {
                    continuous_edit |= ui.color_edit_button_rgba_unmultiplied(&mut s.dim_color).changed();
                }
            });
        }
        ui.horizontal(|ui| {
            discrete_edit |= ui.checkbox(&mut s.outline, "Outline").changed();
            if s.outline {
                continuous_edit |= ui.color_edit_button_rgba_unmultiplied(&mut s.outline_color).changed();
            }
        });
        if s.outline {
            continuous_edit |= slider_f32(ui, "Outline Width", &mut s.outline_width, OUTLINE_MIN_PX..=OUTLINE_MAX_PX, " px");
        }
        ui.horizontal(|ui| {
            discrete_edit |= ui.checkbox(&mut s.pill, "Background Pill").changed();
            if s.pill {
                continuous_edit |= ui.color_edit_button_rgba_unmultiplied(&mut s.pill_color).changed();
                discrete_edit |= ui
                    .add_enabled(s.word_highlight, egui::Checkbox::new(&mut s.word_pill, "Slide with word"))
                    .on_hover_text("The pill backs only the spoken word and glides to the next one")
                    .changed();
            }
        });
    });

    ui.horizontal(|ui| {
        ui.label("Trim words:");
        ui.radio_value(&mut app.captions.word_fit, WordFit::Scale, "Scale Words Proportionally").on_hover_text("Dragging a caption edge on the timeline stretches every word");
        ui.radio_value(&mut app.captions.word_fit, WordFit::Hold, "Hold / Extend First & Last Word")
            .on_hover_text("Inner words keep their timing; only the first / last word grows or shrinks");
    });

    // word pills of the selected block
    let ct = track.caption_time(app.playhead);
    if let Some(seg) = app.captions.selected_seg.and_then(|i| track.segments.get_mut(i)) {
        if !seg.words.is_empty() {
            ui.add_space(2.0);
            ui.label(egui::RichText::new("Words (selected block)").strong());
            ui.horizontal_wrapped(|ui| {
                for j in 0..seg.words.len() {
                    let w = &seg.words[j];
                    let mut start_ms = (w.start - seg.start).as_secs_f64() * 1000.0;
                    let mut dur_ms = (w.end - w.start).as_secs_f64() * 1000.0;
                    let text = w.text.clone();
                    let active = ct >= w.start && ct < w.end;
                    egui::Frame::new()
                        .fill(if active { Color32::from_rgb(0x4A, 0x44, 0x10) } else { Color32::from_rgb(0x28, 0x3A, 0x4E) })
                        .corner_radius(10)
                        .inner_margin(egui::Margin::symmetric(8, 3))
                        .show(ui, |ui| {
                            ui.vertical(|ui| {
                                ui.label(egui::RichText::new(text).strong());
                                let mut changed = false;
                                ui.horizontal(|ui| {
                                    changed |= ui.add(egui::DragValue::new(&mut start_ms).speed(5.0).prefix("Start ").suffix(" ms").max_decimals(0)).changed();
                                });
                                ui.horizontal(|ui| {
                                    changed |= ui.add(egui::DragValue::new(&mut dur_ms).speed(5.0).prefix("Dur ").suffix(" ms").max_decimals(0)).changed();
                                });
                                if changed {
                                    let start = seg.start + Time::from_millis(start_ms.round() as i64);
                                    continuous_edit |= seg.set_word_timing(j, start, Time::from_millis(dur_ms.round() as i64));
                                }
                            });
                        });
                }
            });
        }
    }

    ui.add_space(4.0);
    ui.label(egui::RichText::new(format!("Segments ({})", track.segments.len())).strong());
    let rate = app.project.timeline.format.rate;
    let mut remove_idx: Option<usize> = None;
    let mut seek_to: Option<(usize, Time)> = None;

    egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
        if track.segments.is_empty() {
            ui.label(egui::RichText::new("No caption segments yet. Download a model above and click Generate Captions, or import an .srt file.").color(theme::TEXT_DIM));
            return;
        }
        for (i, seg) in track.segments.iter_mut().enumerate() {
            let active = app.playhead >= seg.start && app.playhead < seg.end;
            let selected = app.captions.selected_seg == Some(i);
            ui.horizontal(|ui| {
                let tc = nle_core::format_timecode(seg.start, rate);
                let label = egui::RichText::new(tc).monospace().size(11.0).color(if active { Color32::from_rgb(0x5E, 0xCE, 0xFF) } else { theme::TEXT_DIM });
                if ui.selectable_label(selected || active, label).clicked() {
                    seek_to = Some((i, seg.start));
                }
                let edit = ui.add(egui::TextEdit::singleline(&mut seg.text).desired_width((ui.available_width() - 34.0).max(120.0)));
                if edit.changed() {
                    seg.retime_words_evenly();
                    continuous_edit = true;
                }
                if ui.small_button("🗑").on_hover_text("Remove segment").clicked() {
                    remove_idx = Some(i);
                }
            });
        }
    });

    if let Some((i, t)) = seek_to {
        app.captions.selected_seg = Some(i);
        app.seek(t);
    }
    if let Some(i) = remove_idx {
        track.segments.remove(i);
        discrete_edit = true;
        if app.captions.selected_seg == Some(i) {
            app.captions.selected_seg = None;
        }
    }

    if track != app.project.timeline.captions {
        if discrete_edit && !continuous_edit {
            app.exec(Command::SetCaptions { captions: Box::new(track) });
        } else {
            app.exec_key(Command::SetCaptions { captions: Box::new(track) }, Some(CAPTIONS_DRAG_KEY));
        }
    }
    if !ui.ctx().input(|i| i.pointer.any_down()) && !ui.ctx().wants_keyboard_input() {
        app.history.seal();
    }
}
