//! Source viewer and three-point editing: a browser clip opened in the
//! viewer with its own playhead and In / Out marks, timeline In / Out marks,
//! Insert / Overwrite / Connect / Append / Replace / Fit to Fill from the
//! marks, Match Frame and subclips.

use crate::app::{App, EditMode};
use crate::theme;
use eframe::egui::{self, CornerRadius, Pos2, Rect, Sense, Stroke, Vec2};
use nle_core::three_point::{self, Marks};
use nle_core::{format_timecode, AssetId, Clip, ClipKind, Command, MediaAsset, Time, Timeline};
use std::collections::HashMap;

#[derive(Default)]
pub struct SourceView {
    /// Clip loaded into the source viewer.
    pub asset: Option<AssetId>,
    /// Source time of the shown frame.
    pub pos: Time,
    /// The viewer shows the source clip instead of the timeline.
    pub shown: bool,
    /// In / Out marks per browser clip (kept while the project is open).
    pub marks: HashMap<AssetId, Marks>,
    /// Timeline In or Out on its own (both together live in `tl.range`).
    pub record: Marks,
}

/// One-clip timeline the audio engine plays while the source viewer is
/// shown: its time is the source time.
pub fn source_timeline(a: &MediaAsset, format: nle_core::VideoFormat) -> Timeline {
    let mut tl = Timeline::new("Source", format);
    let _ = tl.append(Clip::from_asset_range(a, Time::ZERO, a.duration));
    tl
}

impl App {
    /// The viewer and transport work on the source clip.
    pub fn source_active(&self) -> bool {
        self.source.shown && self.source.asset.is_some_and(|a| self.project.asset(a).is_some())
    }

    fn source_asset(&self) -> Option<&MediaAsset> {
        self.source.asset.and_then(|a| self.project.asset(a))
    }

    /// Load a browser clip into the source viewer and show it.
    pub fn open_in_source(&mut self, asset: AssetId, at: Option<Time>) {
        let Some(a) = self.project.asset(asset) else { return };
        let lo = a.bounds().0;
        let pos = at.unwrap_or_else(|| self.source.marks.get(&asset).and_then(|m| m.mark_in).unwrap_or(lo)).clamp(Time::ZERO, a.duration);
        self.show_source(true);
        self.source.asset = Some(asset);
        self.source.pos = pos;
        self.browser.selected = Some(asset);
    }

    /// Switch the viewer between the source clip and the timeline.
    pub fn show_source(&mut self, on: bool) {
        if self.source.shown != on {
            self.stop();
            self.source.shown = on;
            if let Some(p) = &mut self.preview {
                p.last = None;
            }
        }
    }

    pub fn toggle_source_viewer(&mut self) {
        if self.source.asset.is_none() {
            match self.browser.selected {
                Some(a) => self.open_in_source(a, None),
                None => self.status("Open a browser clip in the source viewer first (double-click it)"),
            }
        } else {
            self.show_source(!self.source.shown);
        }
    }

    // ----- marks --------------------------------------------------------------

    /// In / Out marks of the timeline (a range selection counts as both).
    pub fn timeline_marks(&self) -> Marks {
        match self.tl.range {
            Some((a, b)) => Marks::new(Some(a), Some(b)),
            None => self.source.record,
        }
    }

    pub fn set_timeline_marks(&mut self, m: Marks) {
        match m.range() {
            Some(r) => {
                self.tl.range = Some(r);
                self.source.record = Marks::NONE;
            }
            None => {
                self.tl.range = None;
                self.source.record = m;
            }
        }
    }

    pub fn clear_timeline_marks(&mut self) {
        self.tl.range = None;
        self.source.record = Marks::NONE;
    }

    pub fn asset_marks(&self, asset: AssetId) -> Marks {
        self.source.marks.get(&asset).copied().unwrap_or_default()
    }

    /// What the marking keys act on: a browser clip being skimmed, the
    /// source clip when the viewer shows it, else the timeline.
    fn mark_target(&self) -> Option<(AssetId, Time)> {
        if let Some((a, t)) = self.browser.skim {
            return Some((a, t));
        }
        if self.source_active() {
            return self.source.asset.map(|a| (a, self.source.pos));
        }
        None
    }

    pub fn mark(&mut self, out: bool) {
        let rate = self.project.timeline.format.rate;
        match self.mark_target() {
            Some((a, t)) => {
                let m = self.source.marks.entry(a).or_default();
                if out {
                    m.set_out(t);
                } else {
                    m.set_in(t);
                }
                if self.browser.skim.is_some() {
                    self.browser.selected = Some(a);
                }
                let label = if out { "Out" } else { "In" };
                self.status(format!("Source {label} {}", format_timecode(t, rate)));
            }
            None => {
                let t = self.playhead.snap_to_frame(rate);
                let mut m = self.timeline_marks();
                if out {
                    m.set_out(t);
                } else {
                    m.set_in(t);
                }
                self.set_timeline_marks(m);
            }
        }
    }

    pub fn mark_clip(&mut self) {
        match self.mark_target() {
            Some((a, _)) => {
                if let Some(b) = self.project.asset(a).map(|x| x.bounds()) {
                    self.source.marks.insert(a, Marks::new(Some(b.0), Some(b.1)));
                }
            }
            None => {
                let Some(id) = self.target_clip() else {
                    self.status("No clip under the playhead to mark");
                    return;
                };
                let tl = &self.project.timeline;
                if let (Some(s), Some(c)) = (tl.clip_start(id), tl.clip(id)) {
                    let d = c.duration;
                    self.set_timeline_marks(Marks::new(Some(s), Some(s + d)));
                }
            }
        }
    }

    pub fn clear_marks(&mut self) {
        match self.mark_target() {
            Some((a, _)) => {
                self.source.marks.remove(&a);
            }
            None => self.clear_timeline_marks(),
        }
    }

    pub fn go_to_mark(&mut self, out: bool) {
        let pick = |m: Marks| if out { m.mark_out } else { m.mark_in };
        if self.source_active() {
            let Some(a) = self.source.asset else { return };
            if let Some(t) = pick(self.asset_marks(a)) {
                self.seek_cursor(t);
            }
        } else if let Some(t) = pick(self.timeline_marks()) {
            self.seek(t);
        }
    }

    // ----- edits --------------------------------------------------------------

    /// The browser clip an edit key uses: the source clip while the viewer
    /// shows it, else the browser selection.
    pub fn edit_asset(&self) -> Option<AssetId> {
        if self.source_active() {
            self.source.asset
        } else {
            self.browser.selected.or(self.source.asset)
        }
    }

    /// A browser clip trimmed to its In / Out marks (drag to timeline).
    pub fn marked_clip(&self, a: &MediaAsset) -> Clip {
        match three_point::plan(self.asset_marks(a.id), a.bounds(), Marks::NONE, Time::ZERO, false) {
            Ok(p) => three_point::plan_clip(a, &p),
            Err(_) => Clip::from_asset(a),
        }
    }

    /// Three-point edit of `asset` using its marks and the timeline's.
    pub fn three_point_edit(&mut self, asset: AssetId, mode: EditMode) {
        let Some(a) = self.project.asset(asset).cloned() else { return };
        let record = if mode == EditMode::Append { Marks::NONE } else { self.timeline_marks() };
        let p = match three_point::plan(self.asset_marks(asset), a.bounds(), record, self.playhead, false) {
            Ok(p) => p,
            Err(e) => {
                self.status(e.message());
                return;
            }
        };
        let clip = three_point::plan_clip(&a, &p);
        let at = p.record_in;
        let lane = if a.has_video() { self.project.timeline.free_lane(1, at, p.duration) } else { self.project.timeline.free_lane(-1, at, p.duration) };
        let cmd = match mode {
            EditMode::Append => Command::Append { clip },
            EditMode::Insert => Command::Insert { at, clip },
            EditMode::Overwrite => Command::Overwrite { at, clip },
            EditMode::Connect => Command::Connect { clip, at, lane },
        };
        if self.exec(cmd) {
            self.playhead = if mode == EditMode::Append { self.project.timeline.duration() } else { at + p.duration };
            if !record.is_empty() {
                self.clear_timeline_marks();
            }
        }
    }

    /// Overwrite the timeline range with the source range, changing speed.
    pub fn fit_to_fill(&mut self) {
        let Some(asset) = self.edit_asset() else {
            self.status("Open a clip in the source viewer first");
            return;
        };
        let Some(a) = self.project.asset(asset).cloned() else { return };
        match three_point::plan(self.asset_marks(asset), a.bounds(), self.timeline_marks(), self.playhead, true) {
            Ok(p) => {
                let clip = three_point::plan_clip(&a, &p);
                if self.exec(Command::Overwrite { at: p.record_in, clip }) {
                    self.status(format!("Fit to Fill at {:.0} % speed", p.speed * 100.0));
                    self.playhead = p.record_in + p.duration;
                    self.clear_timeline_marks();
                }
            }
            Err(e) => self.status(e.message()),
        }
    }

    /// Swap the selected clip (or the one under the playhead) for the source
    /// clip, keeping its length and place.
    pub fn replace_edit(&mut self) {
        let Some(asset) = self.edit_asset() else {
            self.status("Open a clip in the source viewer first");
            return;
        };
        let Some(a) = self.project.asset(asset).cloned() else { return };
        let Some(id) = self.target_clip() else {
            self.status("Select the timeline clip to replace");
            return;
        };
        let tl = &self.project.timeline;
        let (Some(start), Some(old)) = (tl.clip_start(id), tl.clip(id).cloned()) else { return };
        if old.is_gap() {
            self.status("Use Overwrite to fill a gap");
            return;
        }
        let pos = if self.source.asset == Some(asset) { self.source.pos } else { a.bounds().0 };
        let src_in = match three_point::replace_source_in(self.asset_marks(asset), pos, a.bounds(), self.playhead, start, old.duration) {
            Ok(t) => t,
            Err(e) => {
                self.status(e.message());
                return;
            }
        };
        let mut clip = Clip::from_asset_range(&a, src_in, old.duration);
        clip.id = old.id;
        clip.enabled = old.enabled;
        clip.transition_in = old.transition_in.clone();
        clip.transition_out = old.transition_out.clone();
        if self.exec(Command::ReplaceClip { label: "Replace Edit", clip: Box::new(clip) }) {
            self.status(format!("Replaced \u{201C}{}\u{201D} with \u{201C}{}\u{201D}", old.name, a.name));
        }
    }

    /// Open the media of the clip under the playhead in the source viewer
    /// on the same frame, marked to the part the clip uses.
    pub fn match_frame(&mut self) {
        let Some(id) = self.target_clip() else {
            self.status("No clip under the playhead");
            return;
        };
        let tl = &self.project.timeline;
        let (Some(start), Some(c)) = (tl.clip_start(id), tl.clip(id)) else { return };
        let ClipKind::Asset { asset } = c.kind else {
            self.status("Match Frame works on media clips");
            return;
        };
        let local = (self.playhead - start).clamp(Time::ZERO, (c.duration - Time(1)).max(Time::ZERO));
        let src = c.source_time(local);
        let (lo, hi) = c.source_range();
        self.source.marks.insert(asset, Marks::new(Some(lo), Some(hi)));
        self.open_in_source(asset, Some(src));
        let rate = self.project.timeline.format.rate;
        self.status(format!("Match frame: source {}", format_timecode(src, rate)));
    }

    /// New browser clip from the source clip's In / Out marks.
    pub fn make_subclip(&mut self) {
        let Some(asset) = self.edit_asset() else {
            self.status("Select or open a clip and mark In and Out first");
            return;
        };
        let Some((a, b)) = self.asset_marks(asset).range() else {
            self.status("Mark In and Out on the clip to make a subclip");
            return;
        };
        if let Some(id) = self.project.add_subclip(asset, a, b, None) {
            self.assets_changed = true;
            self.browser.selected = Some(id);
            let name = self.project.asset(id).map(|x| x.name.clone()).unwrap_or_default();
            self.status(format!("Made subclip \u{201C}{name}\u{201D}"));
        }
    }

    // ----- transport target --------------------------------------------------

    /// The time the transport drives: source time or the playhead.
    pub fn cursor(&self) -> Time {
        if self.source_active() {
            self.source.pos
        } else {
            self.playhead
        }
    }

    /// End of what the transport plays.
    pub fn transport_end(&self) -> Time {
        match self.source_asset() {
            Some(a) if self.source.shown => a.duration,
            _ => self.project.timeline.extent(),
        }
    }

    /// Seek whatever the viewer shows.
    pub fn seek_cursor(&mut self, t: Time) {
        if self.source_active() {
            let t = t.clamp(Time::ZERO, self.transport_end());
            self.source.pos = t;
            if self.playback.playing {
                self.audio.play(t, self.playback.speed);
            }
        } else {
            self.seek(t);
        }
    }

    /// Frame shown in source mode (whole source frames).
    pub fn source_frame(&self) -> Option<(AssetId, Time)> {
        let a = self.source_asset().filter(|_| self.source.shown)?;
        let rate = a.frame_rate();
        let max = (a.duration - Time(1)).max(Time::ZERO);
        Some((a.id, Time::from_frames(self.source.pos.frames_floor(rate), rate).min(max)))
    }
}

/// Source viewer strip: the whole clip with its marks, a draggable
/// playhead and the edit buttons.
pub fn source_controls(app: &mut App, ui: &mut egui::Ui) {
    let Some(a) = app.source.asset.and_then(|a| app.project.asset(a)).cloned() else { return };
    let marks = app.asset_marks(a.id);
    let (bar, resp) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 18.0), Sense::click_and_drag());
    let len = a.duration.as_secs_f64().max(1e-6);
    let x = |t: Time| bar.left() + (t.as_secs_f64() / len) as f32 * bar.width();
    let p = ui.painter();
    p.rect_filled(bar, CornerRadius::same(3), theme::BG);
    if let Some((lo, hi)) = a.subclip {
        p.rect_filled(Rect::from_min_max(Pos2::new(x(lo), bar.top()), Pos2::new(x(hi), bar.bottom())), CornerRadius::ZERO, theme::BORDER);
    }
    let mi = marks.mark_in.unwrap_or(Time::ZERO);
    let mo = marks.mark_out.unwrap_or(a.duration);
    if !marks.is_empty() {
        let r = Rect::from_min_max(Pos2::new(x(mi), bar.top()), Pos2::new(x(mo), bar.bottom()));
        p.rect_filled(r, CornerRadius::ZERO, theme::SELECTION.gamma_multiply(0.35));
        for (t, set) in [(mi, marks.mark_in.is_some()), (mo, marks.mark_out.is_some())] {
            if set {
                p.line_segment([Pos2::new(x(t), bar.top()), Pos2::new(x(t), bar.bottom())], Stroke::new(2.0_f32, theme::SELECTION));
            }
        }
    }
    let ph = x(app.source.pos);
    p.line_segment([Pos2::new(ph, bar.top() - 2.0), Pos2::new(ph, bar.bottom() + 2.0)], Stroke::new(2.0_f32, theme::PLAYHEAD));
    if resp.dragged() || resp.clicked() {
        if let Some(pt) = resp.interact_pointer_pos() {
            let f = ((pt.x - bar.left()) / bar.width()).clamp(0.0, 1.0) as f64;
            let t = Time::from_secs_f64(f * len).snap_to_frame(a.frame_rate());
            app.stop();
            app.source.pos = t;
            app.audio.scrub(t);
        }
    }
    let rate = app.project.timeline.format.rate;
    let span = marks.range().map(|(i, o)| o - i).unwrap_or_else(|| mo.max(mi) - mi);
    resp.on_hover_text(format!("In {}  Out {}  ({})", format_timecode(mi, rate), format_timecode(mo, rate), crate::widgets::secs_label(span)));

    use nle_core::keymap::Action as A;
    ui.horizontal_wrapped(|ui| {
        let km = &app.keymap;
        let tips = [km.tip("Mark In", A::MarkIn), km.tip("Mark Out", A::MarkOut), km.tip("Clear In and Out", A::ClearInOut)];
        let mut act = None;
        if ui.small_button("⇤ In").on_hover_text(&tips[0]).clicked() {
            act = Some(A::MarkIn);
        }
        if ui.small_button("Out ⇥").on_hover_text(&tips[1]).clicked() {
            act = Some(A::MarkOut);
        }
        if ui.small_button("✕").on_hover_text(&tips[2]).clicked() {
            act = Some(A::ClearInOut);
        }
        ui.separator();
        for (label, action) in [
            ("Insert", A::InsertClip),
            ("Overwrite", A::OverwriteClip),
            ("Connect", A::ConnectClip),
            ("Append", A::AppendClip),
            ("Replace", A::ReplaceEdit),
            ("Fit to Fill", A::FitToFill),
        ] {
            let tip = app.keymap.tip(label, action);
            if ui.small_button(label).on_hover_text(tip).clicked() {
                act = Some(action);
            }
        }
        ui.separator();
        let tip = app.keymap.tip("Make subclip from In / Out", A::MakeSubclip);
        if ui.add_enabled(marks.range().is_some(), egui::Button::new("Subclip").small()).on_hover_text(tip).clicked() {
            act = Some(A::MakeSubclip);
        }
        if let Some(a) = act {
            app.dispatch_action(a);
        }
    });
}
