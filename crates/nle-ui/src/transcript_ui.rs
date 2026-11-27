//! Transcript panel: text-based editing.
//!
//! On the timeline the panel lists the caption words as running text.
//! Clicking a word seeks to it; selecting words and pressing Delete Words
//! ripple-deletes the speech they cover (one undo step, captions follow).
//! Filler words and long pauses can be selected / shortened in one go.
//!
//! With a browser clip in the source viewer the panel shows that clip's own
//! transcript (Transcribe Clip): a text selection becomes its In / Out
//! marks, ready for Insert / Overwrite.

use crate::app::App;
use crate::captions_ui;
use crate::theme;
use eframe::egui::{self, RichText};
use nle_core::text_edit::{self, Word};
use nle_core::three_point::Marks;
use nle_core::{AssetId, Time};
use nle_media::audio::to_mono_16k;
use nle_media::jobs::Job;
use nle_media::transcribe::Transcript;
use std::collections::{BTreeSet, HashSet};
use std::time::Duration;

/// Pause before a word that starts a new paragraph in the panel.
const PARAGRAPH_GAP_MS: i64 = 1500;

pub struct TranscriptState {
    pub open: bool,
    pub selected: BTreeSet<usize>,
    anchor: Option<usize>,
    pub query: String,
    /// Pauses longer than this are shortened.
    pub pause_ms: u32,
    /// Silence left where a pause is shortened.
    pub keep_ms: u32,
    /// Scroll to the word under the playhead while playing.
    pub follow: bool,
    /// Source clip transcription.
    pub job: Option<Job<(AssetId, Transcript)>>,
    /// What the word list was last frame (a change clears the selection).
    sig: Option<(Option<AssetId>, usize, Time, Time)>,
    last_word: Option<usize>,
}

impl Default for TranscriptState {
    fn default() -> Self {
        TranscriptState {
            open: false,
            selected: BTreeSet::new(),
            anchor: None,
            query: String::new(),
            pause_ms: 800,
            keep_ms: 250,
            follow: true,
            job: None,
            sig: None,
            last_word: None,
        }
    }
}

impl TranscriptState {
    pub fn busy(&self) -> bool {
        self.job.is_some()
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Mode {
    Timeline,
    Source(AssetId),
}

fn mode(app: &App) -> Mode {
    match app.source.asset {
        Some(id) if app.source_active() => Mode::Source(id),
        _ => Mode::Timeline,
    }
}

fn words_for(app: &App, mode: Mode) -> Vec<Word> {
    match mode {
        Mode::Timeline => text_edit::timeline_words(&app.project.timeline.captions),
        Mode::Source(id) => app.project.asset(id).map(|a| text_edit::source_words(&a.transcript)).unwrap_or_default(),
    }
}

pub fn poll(app: &mut App, ctx: &egui::Context) {
    let Some(job) = app.transcript.job.as_ref() else { return };
    let Some(res) = job.take() else {
        ctx.request_repaint_after(Duration::from_millis(100));
        return;
    };
    app.transcript.job = None;
    match res {
        Ok((_, tr)) if tr.words.is_empty() => app.status(format!("No speech found in the clip ({})", tr.backend)),
        Ok((id, tr)) => {
            let n = tr.words.len();
            if let Some(a) = app.project.asset_mut(id) {
                a.transcript = tr.words;
                app.assets_changed = true;
                app.status(format!("Transcribed the clip: {n} words ({}, {})", tr.language, tr.backend));
            }
        }
        Err(e) => app.status(format!("Clip transcription failed: {e}")),
    }
}

/// Transcribe a browser clip's own audio (source time) into its transcript.
pub fn start_clip_transcription(app: &mut App, id: AssetId) -> Result<(), String> {
    let Some(a) = app.project.asset(id) else {
        return Err("the clip is no longer in the project".into());
    };
    if a.audio.is_none() {
        return Err(format!("{} has no audio", a.name));
    }
    let Some(buffer) = app.library.audio.get(&id).cloned() else {
        return Err(format!("{}'s audio is still loading — try again in a moment", a.name));
    };
    let model = captions_ui::ready_model(app)?;
    let opts = captions_ui::whisper_options(app, model);
    app.transcript.job = Some(Job::spawn("clip-transcript", move |jctx| {
        jctx.set_progress(0.05, "Converting to 16 kHz mono…");
        let mono16k = to_mono_16k(&buffer.samples);
        if jctx.cancelled() {
            return Err("Cancelled".into());
        }
        captions_ui::run_whisper(jctx, &mono16k, &opts).map(|t| (id, t))
    }));
    Ok(())
}

pub fn show(app: &mut App, ctx: &egui::Context) {
    if !app.transcript.open {
        return;
    }
    let mut open = true;
    egui::Window::new(theme::alpha("Transcript"))
        .id(egui::Id::new("transcript_window"))
        .open(&mut open)
        .default_width(540.0)
        .default_height(560.0)
        .resizable(true)
        .show(ctx, |ui| body(app, ui));
    app.transcript.open &= open;
}

fn secs(t: Time) -> String {
    format!("{:.1} s", t.as_secs_f64())
}

fn clock(t: Time) -> String {
    let s = t.as_secs_f64().max(0.0) as u64;
    format!("{}:{:02}", s / 60, s % 60)
}

fn body(app: &mut App, ui: &mut egui::Ui) {
    let mode = mode(app);
    let words = words_for(app, mode);
    let sig = (
        match mode {
            Mode::Source(id) => Some(id),
            Mode::Timeline => None,
        },
        words.len(),
        words.first().map_or(Time::ZERO, |w| w.start),
        words.last().map_or(Time::ZERO, |w| w.end),
    );
    if app.transcript.sig != Some(sig) {
        app.transcript.sig = Some(sig);
        app.transcript.selected.clear();
        app.transcript.anchor = None;
    }

    match mode {
        Mode::Timeline => {
            ui.label(RichText::new("Timeline transcript — deleting words ripple-deletes the speech they cover (one undo step).").small().color(theme::TEXT_DIM));
        }
        Mode::Source(id) => {
            let name = app.project.asset(id).map(|a| a.name.clone()).unwrap_or_default();
            ui.horizontal(|ui| {
                ui.label(RichText::new(format!("Source clip: {name}")).strong());
                ui.label(RichText::new("— select text to mark In / Out").small().color(theme::TEXT_DIM));
                if !words.is_empty() && ui.add_enabled(!app.transcript.busy(), egui::Button::new("Re-transcribe").small()).clicked() {
                    if let Err(e) = start_clip_transcription(app, id) {
                        app.status(e);
                    }
                }
            });
        }
    }
    progress_ui(app, ui, mode);

    if words.is_empty() {
        empty_ui(app, ui, mode);
        return;
    }
    if mode == Mode::Timeline && !app.focus.is_empty() {
        ui.label(RichText::new("Step out of the compound clip to edit by text (the transcript belongs to the project timeline).").color(theme::SNAP));
    }

    let hits = text_edit::find_phrase(&words, &app.transcript.query);
    let fillers = text_edit::filler_indices(&words);

    // ----- find / select ----------------------------------------------------
    ui.horizontal(|ui| {
        ui.label("Find:");
        let r = ui.add(egui::TextEdit::singleline(&mut app.transcript.query).desired_width(150.0).hint_text("word or phrase"));
        let enter = r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
        let sel_hits = ui.add_enabled(!hits.is_empty(), egui::Button::new(format!("Select matches ({})", hits.len()))).clicked();
        if (enter || sel_hits) && !hits.is_empty() {
            app.transcript.selected = hits.iter().copied().collect();
            seek_word(app, mode, &words, hits[0]);
        }
        if ui.add_enabled(!fillers.is_empty(), egui::Button::new(format!("Select fillers ({})", fillers.len()))).on_hover_text(text_edit::FILLERS.join(", ")).clicked() {
            app.transcript.selected = fillers.iter().copied().collect();
        }
        if ui.add_enabled(!app.transcript.selected.is_empty(), egui::Button::new("Deselect")).clicked() {
            app.transcript.selected.clear();
        }
        ui.checkbox(&mut app.transcript.follow, "Follow").on_hover_text("Scroll to the spoken word while playing");
    });

    // ----- actions -------------------------------------------------------------
    let sel: Vec<usize> = app.transcript.selected.iter().copied().collect();
    let span = text_edit::selection_span(&words, &sel);
    match mode {
        Mode::Timeline => {
            let rate = app.project.timeline.format.rate;
            let editable = app.focus.is_empty();
            let ranges = text_edit::word_ranges(&words, &sel, rate);
            ui.horizontal(|ui| {
                let label = format!("✂ Delete {} word(s)  −{}", sel.len(), secs(text_edit::total(&ranges)));
                if ui.add_enabled(editable && !ranges.is_empty(), egui::Button::new(label)).on_hover_text("Ripple-delete the selected words from the storyline").clicked() {
                    cut(app, ranges, "Delete Words");
                }
                if ui.add_enabled(span.is_some(), egui::Button::new("Set In / Out")).on_hover_text("Mark the selection as the timeline range").clicked() {
                    if let Some((s, e)) = span {
                        app.set_timeline_marks(Marks::new(Some(s.snap_to_frame(rate)), Some(e.snap_to_frame(rate))));
                        app.status(format!("Timeline In / Out set from the text ({} – {})", clock(s), clock(e)));
                    }
                }
            });
            ui.horizontal(|ui| {
                ui.label("Pauses longer than");
                ui.add(egui::DragValue::new(&mut app.transcript.pause_ms).range(200..=10_000).speed(10.0).suffix(" ms"));
                ui.label("→ leave");
                ui.add(egui::DragValue::new(&mut app.transcript.keep_ms).range(0..=2_000).speed(10.0).suffix(" ms"));
                let pauses = text_edit::pause_ranges(&words, Time::from_millis(app.transcript.pause_ms as i64), Time::from_millis(app.transcript.keep_ms as i64), rate);
                let label = format!("Shorten {} pause(s)  −{}", pauses.len(), secs(text_edit::total(&pauses)));
                if ui
                    .add_enabled(editable && !pauses.is_empty(), egui::Button::new(label))
                    .on_hover_text("Ripple-delete the silence between words, leaving the chosen gap")
                    .clicked()
                {
                    cut(app, pauses, "Shorten Pauses");
                }
            });
        }
        Mode::Source(id) => {
            ui.horizontal(|ui| {
                if ui.add_enabled(span.is_some(), egui::Button::new("Set In / Out from selection")).clicked() {
                    if let (Some((s, e)), Some(rate)) = (span, app.project.asset(id).map(|a| a.frame_rate())) {
                        app.source.marks.insert(id, Marks::new(Some(s.snap_to_frame(rate)), Some(e.snap_to_frame(rate))));
                        app.seek_cursor(s);
                        app.status(format!("Source In / Out set from the text ({} – {}) — Insert / Overwrite / Connect to edit it in", clock(s), clock(e)));
                    }
                }
            });
        }
    }
    ui.separator();

    // ----- the text ----------------------------------------------------------
    let now = match mode {
        Mode::Timeline => app.playhead,
        Mode::Source(_) => app.source.pos,
    };
    let current = text_edit::word_at(&words, now);
    let moved = current != app.transcript.last_word;
    app.transcript.last_word = current;
    let scroll_to_current = moved && app.transcript.follow && app.playback.playing;
    let hit_set: HashSet<usize> = hits.into_iter().collect();
    let filler_set: HashSet<usize> = fillers.into_iter().collect();
    let gap = Time::from_millis(PARAGRAPH_GAP_MS);
    let mut clicked: Option<(usize, egui::Modifiers)> = None;
    egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing = egui::vec2(2.0, 4.0);
            ui.spacing_mut().button_padding = egui::vec2(2.0, 1.0);
            for (i, w) in words.iter().enumerate() {
                if i == 0 || text_edit::starts_paragraph(&words, i, gap) {
                    if i > 0 {
                        ui.end_row();
                    }
                    ui.label(RichText::new(clock(w.start)).small().monospace().color(theme::TEXT_DIM));
                }
                let mut text = RichText::new(&w.text);
                if filler_set.contains(&i) {
                    text = text.italics().color(theme::TEXT_DIM);
                }
                if hit_set.contains(&i) {
                    text = text.underline().color(theme::SNAP);
                }
                if Some(i) == current {
                    text = text.strong().color(theme::PLAYHEAD);
                }
                let r = ui.selectable_label(app.transcript.selected.contains(&i), text).on_hover_text(format!("{:.2} – {:.2} s", w.start.as_secs_f64(), w.end.as_secs_f64()));
                if r.clicked() {
                    clicked = Some((i, ui.input(|inp| inp.modifiers)));
                }
                if scroll_to_current && Some(i) == current {
                    r.scroll_to_me(Some(egui::Align::Center));
                }
            }
        });
    });
    if let Some((i, m)) = clicked {
        let st = &mut app.transcript;
        if m.shift {
            let a = st.anchor.unwrap_or(i);
            st.selected = (a.min(i)..=a.max(i)).collect();
        } else if m.command {
            if !st.selected.remove(&i) {
                st.selected.insert(i);
            }
            st.anchor = Some(i);
        } else {
            st.selected = BTreeSet::from([i]);
            st.anchor = Some(i);
            seek_word(app, mode, &words, i);
        }
    }
}

fn seek_word(app: &mut App, mode: Mode, words: &[Word], i: usize) {
    let Some(w) = words.get(i) else { return };
    match mode {
        Mode::Timeline => app.seek(w.start),
        Mode::Source(_) => app.seek_cursor(w.start),
    }
}

fn cut(app: &mut App, ranges: Vec<(Time, Time)>, label: &'static str) {
    let (n, len) = (ranges.len(), text_edit::total(&ranges));
    let Some(cmd) = text_edit::cut_command(&app.project.timeline.captions, &ranges, label) else { return };
    if app.exec(cmd) {
        app.transcript.selected.clear();
        app.transcript.anchor = None;
        app.status(format!("{label}: removed {} in {n} cut(s)", secs(len)));
    }
}

fn progress_ui(app: &App, ui: &mut egui::Ui, mode: Mode) {
    match mode {
        Mode::Timeline => {
            if let Some(j) = &app.captions.gen_job {
                bar(ui, j);
            }
        }
        Mode::Source(_) => {
            if let Some(j) = &app.transcript.job {
                bar(ui, j);
            }
        }
    }
}

fn bar<T: Send + 'static>(ui: &mut egui::Ui, job: &Job<T>) {
    let (frac, msg) = job.progress();
    ui.horizontal(|ui| {
        ui.add(egui::ProgressBar::new(frac).desired_width(ui.available_width() - 80.0).text(format!("{msg} ({:.0}%)", frac * 100.0)));
        if ui.button("Cancel").clicked() {
            job.cancel();
        }
    });
}

fn empty_ui(app: &mut App, ui: &mut egui::Ui, mode: Mode) {
    ui.add_space(8.0);
    match mode {
        Mode::Timeline => {
            ui.label("No transcript yet. Transcribing the timeline gives it captions; their words appear here as editable text.");
            let busy = app.captions.gen_job.is_some() || app.captions.model_job.is_some() || app.transcript.busy();
            if ui.add_enabled(!busy, egui::Button::new("🎙 Transcribe Timeline")).clicked() {
                match captions_ui::start_caption_generation(app) {
                    Ok(()) => app.status("Transcribing the timeline…"),
                    Err(e) => app.status(e),
                }
            }
        }
        Mode::Source(id) => {
            ui.label("This clip has no transcript yet.");
            if ui.add_enabled(!app.transcript.busy() && app.captions.gen_job.is_none(), egui::Button::new("🎙 Transcribe Clip")).clicked() {
                match start_clip_transcription(app, id) {
                    Ok(()) => app.status("Transcribing the clip…"),
                    Err(e) => app.status(e),
                }
            }
        }
    }
    ui.label(RichText::new("Uses the Whisper model and language chosen in the Auto Captions panel.").small().color(theme::TEXT_DIM));
}
