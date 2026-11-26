//! Clip sync and multicam: Synchronize Clips (audio / timecode), the New
//! Multicam Clip window, the angle grid in the viewer and cut-and-switch.

use crate::app::App;
use crate::theme;
use eframe::egui::{self, Color32, CornerRadius, Pos2, Rect, Stroke, StrokeKind, Vec2};
use nle_core::eval::{self, Composition, Layer, LayerSource, RenderNode};
use nle_core::multicam::{self, Multicam};
use nle_core::sync::{self, SyncMatch};
use nle_core::{AssetId, ClipId, ClipKind, Command, MediaAsset, Time};

/// What a click on an angle (or keys 1–9) switches.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SwitchMode {
    #[default]
    Both,
    Video,
    Audio,
}

impl SwitchMode {
    fn label(self) -> &'static str {
        match self {
            SwitchMode::Both => "Video + Audio",
            SwitchMode::Video => "Video only",
            SwitchMode::Audio => "Audio only",
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SyncMethod {
    #[default]
    Audio,
    Timecode,
    ClipStart,
}

impl SyncMethod {
    fn label(self) -> &'static str {
        match self {
            SyncMethod::Audio => "Audio waveforms",
            SyncMethod::Timecode => "Timecode",
            SyncMethod::ClipStart => "Clip starts",
        }
    }
}

pub struct NewMulticam {
    name: String,
    picked: Vec<AssetId>,
    method: SyncMethod,
}

#[derive(Default)]
pub struct MulticamUi {
    /// The viewer shows every angle of the multicam clip under the playhead.
    pub angles: bool,
    pub mode: SwitchMode,
    pub dialog: Option<NewMulticam>,
}

/// Columns of the angle grid.
fn grid_cols(n: usize) -> usize {
    match n {
        0 | 1 => 1,
        2..=4 => 2,
        5..=9 => 3,
        _ => 4,
    }
}

/// Angle `i`'s cell as fractions of the frame (x, y, w, h).
fn cell(i: usize, n: usize) -> [f32; 4] {
    let cols = grid_cols(n);
    let rows = n.div_ceil(cols).max(1);
    let w = 1.0 / cols as f32;
    let top = 0.5 - rows as f32 * w * 0.5;
    [(i % cols) as f32 * w, top + (i / cols) as f32 * w, w, w]
}

/// A loudness envelope for sync: the asset's RMS waveform at `rate` buckets/s.
fn envelope(app: &App, a: AssetId, rate: Option<f64>) -> Option<(Vec<f32>, f64)> {
    let w = app.library.waveforms.get(&a)?;
    let r = w.buckets_per_second();
    Some(match rate {
        Some(to) => (sync::resample_envelope(&w.rms, r, to), to),
        None => (w.rms.clone(), r),
    })
}

/// `other`'s file time t is `reference`'s file time t + offset (seconds).
enum Offset {
    Found(f64, Option<SyncMatch>),
    Loading,
    Unknown,
}

impl App {
    fn offset_between(&mut self, reference: AssetId, other: AssetId, by_timecode: bool) -> Offset {
        if reference == other {
            return Offset::Found(0.0, None);
        }
        let (Some(ra), Some(oa)) = (self.project.asset(reference).cloned(), self.project.asset(other).cloned()) else { return Offset::Unknown };
        if by_timecode {
            return match sync::timecode_offset(&ra, &oa) {
                Some(d) => Offset::Found(d.as_secs_f64(), None),
                None => Offset::Unknown,
            };
        }
        if !ra.has_audio() || !oa.has_audio() {
            return Offset::Unknown;
        }
        let Some((r, rate)) = envelope(self, reference, None) else {
            self.library.ensure(&ra);
            return Offset::Loading;
        };
        let Some((o, _)) = envelope(self, other, Some(rate)) else {
            self.library.ensure(&oa);
            return Offset::Loading;
        };
        match sync::audio_offset(&r, &o, rate, None) {
            Some(m) => Offset::Found(m.offset, Some(m)),
            None => Offset::Unknown,
        }
    }

    /// Move the selected connected clips so they line up with the
    /// reference clip (a selected storyline clip, else the earliest).
    pub fn sync_selected(&mut self, by_timecode: bool) {
        struct Pick {
            id: ClipId,
            asset: AssetId,
            start: Time,
            source_in: Time,
            lane: i32,
            spine: bool,
            retimed: bool,
        }
        let tl = &self.project.timeline;
        let mut picks: Vec<Pick> = tl
            .placed_clips()
            .into_iter()
            .filter(|p| self.selection.contains(&p.clip.id))
            // linked audio follows its picture
            .filter(|p| !(p.clip.link.is_some() && !p.clip.av.video))
            .filter_map(|p| match p.clip.kind {
                ClipKind::Asset { asset } => {
                    Some(Pick { id: p.clip.id, asset, start: p.start, source_in: p.clip.source_in, lane: p.lane, spine: p.parent.is_none(), retimed: p.clip.retime.is_some() })
                }
                _ => None,
            })
            .collect();
        if picks.len() < 2 {
            self.status("Synchronize Clips: select two or more media clips (one stays put, the others move to it)");
            return;
        }
        picks.sort_by_key(|p| (!p.spine, p.start));
        let r = picks.remove(0);
        let (mut cmds, mut weak, mut loading, mut unknown, mut stuck) = (Vec::new(), 0, 0, 0, 0);
        for c in &picks {
            if c.spine || c.retimed {
                stuck += 1;
                continue;
            }
            let d = match self.offset_between(r.asset, c.asset, by_timecode) {
                Offset::Found(d, m) => {
                    weak += usize::from(m.is_some_and(|m| !m.reliable()));
                    d
                }
                Offset::Loading => {
                    loading += 1;
                    continue;
                }
                Offset::Unknown => {
                    unknown += 1;
                    continue;
                }
            };
            // the same moment at the same timeline time
            let at = r.start + c.source_in - r.source_in + Time::from_secs_f64(d);
            if at < Time::ZERO {
                stuck += 1;
                continue;
            }
            if at != c.start {
                cmds.push(Command::MoveClip { id: c.id, at, lane: c.lane });
            }
        }
        let moved = cmds.len();
        if moved > 0 {
            self.exec(Command::Batch { label: "Synchronize Clips", commands: cmds });
        }
        let mut msg =
            format!("Synchronized {moved} clip{} to {}", if moved == 1 { "" } else { "s" }, self.project.asset(r.asset).map(|a| a.name.as_str()).unwrap_or("the reference"));
        if loading > 0 {
            msg += &format!(" \u{2014} {loading} still reading audio, try again in a moment");
        }
        if unknown > 0 {
            msg += &format!(" \u{2014} {unknown} without {}", if by_timecode { "timecode" } else { "usable audio" });
        }
        if weak > 0 {
            msg += &format!(" \u{2014} {weak} weak match{}, check by eye", if weak == 1 { "" } else { "es" });
        }
        if stuck > 0 {
            msg += &format!(" \u{2014} {stuck} not moved (storyline, retimed or would start before 0)");
        }
        self.status(msg);
    }

    pub fn open_new_multicam(&mut self) {
        let mut picked: Vec<AssetId> = self.browser.selected.into_iter().collect();
        // selected timeline clips' media too
        for id in &self.selection {
            if let Some(a) = self.project.timeline.clip(*id).and_then(|c| c.asset_id()) {
                if !picked.contains(&a) {
                    picked.push(a);
                }
            }
        }
        let n = self.project.timeline.placed_clips().iter().filter(|p| multicam::of(p.clip).is_some()).count();
        self.multicam.dialog = Some(NewMulticam { name: format!("Multicam {}", n + 1), picked, method: SyncMethod::Audio });
    }

    pub fn multicam_window(&mut self, ctx: &egui::Context) {
        let Some(mut d) = self.multicam.dialog.take() else { return };
        let mut open = true;
        let mut create = false;
        let assets: Vec<(AssetId, String, bool, bool, bool)> =
            self.project.assets.iter().map(|a| (a.id, a.name.clone(), a.has_video(), a.has_audio(), a.start_timecode.is_some())).collect();
        egui::Window::new("New Multicam Clip").open(&mut open).collapsible(false).resizable(true).default_width(380.0).show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.label("Name");
                ui.text_edit_singleline(&mut d.name);
            });
            ui.horizontal(|ui| {
                ui.label("Line up by");
                egui::ComboBox::from_id_salt("mc-sync").selected_text(d.method.label()).show_ui(ui, |ui| {
                    for m in [SyncMethod::Audio, SyncMethod::Timecode, SyncMethod::ClipStart] {
                        ui.selectable_value(&mut d.method, m, m.label());
                    }
                });
            });
            ui.label(egui::RichText::new("Angles, in order (the first is the sync reference):").small().color(theme::TEXT_DIM));
            egui::ScrollArea::vertical().max_height(260.0).show(ui, |ui| {
                for (id, name, v, a, tc) in &assets {
                    let pos = d.picked.iter().position(|p| p == id);
                    let mut on = pos.is_some();
                    let kind = match (v, a) {
                        (true, true) => "video + audio",
                        (true, false) => "video",
                        (false, true) => "audio",
                        _ => "",
                    };
                    let label = match pos {
                        Some(i) => format!("{}. {name}", i + 1),
                        None => name.clone(),
                    };
                    ui.horizontal(|ui| {
                        if ui.checkbox(&mut on, label).changed() {
                            if on {
                                d.picked.push(*id);
                            } else {
                                d.picked.retain(|p| p != id);
                            }
                        }
                        ui.label(egui::RichText::new(kind).small().color(theme::TEXT_DIM));
                        if *tc {
                            ui.label(egui::RichText::new("TC").small().color(theme::TEXT_DIM));
                        }
                    });
                }
            });
            ui.separator();
            let ok = d.picked.len() >= 2 && !d.name.trim().is_empty();
            ui.horizontal(|ui| {
                if ui.add_enabled(ok, egui::Button::new("Create")).on_disabled_hover_text("Pick two or more clips").clicked() {
                    create = true;
                }
                ui.label(egui::RichText::new("Placed at the end of the timeline").small().color(theme::TEXT_DIM));
            });
        });
        if create {
            if self.create_multicam(&d) {
                return;
            }
        } else if !open {
            return;
        }
        self.multicam.dialog = Some(d);
    }

    /// True when the clip was made (the window closes).
    fn create_multicam(&mut self, d: &NewMulticam) -> bool {
        let reference = d.picked[0];
        let mut offsets = Vec::new();
        let (mut loading, mut unknown, mut weak) = (0, 0, 0);
        for &a in &d.picked {
            let Some(asset) = self.project.asset(a) else { continue };
            let lo = asset.bounds().0;
            let off = match d.method {
                SyncMethod::ClipStart => Some(Time::ZERO),
                m => match self.offset_between(reference, a, m == SyncMethod::Timecode) {
                    Offset::Found(s, m) => {
                        weak += usize::from(m.is_some_and(|m| !m.reliable()));
                        // the clip's first frame on the reference's clock
                        Some(Time::from_secs_f64(s) + lo)
                    }
                    Offset::Loading => {
                        loading += 1;
                        None
                    }
                    Offset::Unknown => {
                        unknown += 1;
                        None
                    }
                },
            };
            offsets.push((a, off));
        }
        if loading > 0 {
            self.status(format!("New Multicam Clip: still reading the audio of {loading} clip(s) \u{2014} try again in a moment"));
            return false;
        }
        let ref_lo = self.project.asset(reference).map(|a| a.bounds().0).unwrap_or(Time::ZERO);
        let raw: Vec<Option<Time>> = offsets.iter().map(|(a, o)| if *a == reference { Some(ref_lo) } else { *o }).collect();
        let starts = if d.method == SyncMethod::ClipStart { vec![Time::ZERO; raw.len()] } else { sync::starts_from_offsets(&raw) };
        let assets: Vec<MediaAsset> = offsets.iter().filter_map(|(a, _)| self.project.asset(*a).cloned()).collect();
        let recordings: Vec<(&MediaAsset, Time)> = assets.iter().zip(starts).collect();
        let Some(clip) = multicam::build(d.name.trim(), &recordings) else {
            self.status("New Multicam Clip: nothing to play");
            return false;
        };
        let id = clip.id;
        if !self.exec(Command::Append { clip }) {
            return false;
        }
        self.selection = vec![id];
        if let Some(t) = self.project.timeline.clip_start(id) {
            self.seek(t);
        }
        self.multicam.angles = true;
        let mut msg = format!("Multicam clip \u{201c}{}\u{201d} with {} angles \u{2014} press 1\u{2013}9 or click an angle to cut", d.name.trim(), assets.len());
        if unknown > 0 {
            msg += &format!("; {unknown} could not be lined up and start with the first");
        }
        if weak > 0 {
            msg += &format!("; {weak} weak audio match(es), check by eye");
        }
        self.status(msg);
        true
    }

    /// The multicam clip under the playhead (the selected one first, then
    /// the storyline, then connected lanes) and the time inside it.
    pub fn active_multicam(&self, t: Time) -> Option<(ClipId, Time, &Multicam)> {
        let tl = &self.project.timeline;
        let mut hits: Vec<_> = tl.placed_clips().into_iter().filter(|p| p.start <= t && t < p.end() && multicam::of(p.clip).is_some()).collect();
        hits.sort_by_key(|p| (!self.selection.contains(&p.clip.id), p.lane != 0, -p.lane));
        let p = hits.into_iter().next()?;
        let local = p.clip.source_time(t - p.start);
        Some((p.clip.id, local, multicam::of(p.clip)?))
    }

    /// Every angle side by side, for the viewer.
    pub fn angle_composition(&self, t: Time) -> Option<Composition> {
        if !self.multicam.angles {
            return None;
        }
        let (_, local, mc) = self.active_multicam(t)?;
        let n = mc.angles.len();
        let nodes = mc
            .angles
            .iter()
            .enumerate()
            .map(|(i, a)| {
                let inner = eval::evaluate_spine(&a.spine, local);
                let mut l = Layer::new(ClipId(u64::MAX - 1000 - i as u64), i as i32, LayerSource::Compound(Box::new(inner)));
                let [x, y, w, _] = cell(i, n);
                l.video.scale = [w, w];
                l.video.position = [x + w * 0.5 - 0.5, y + w * 0.5 - 0.5];
                RenderNode::Layer(l)
            })
            .collect();
        Some(Composition::new(t, nodes))
    }

    /// Switch the multicam clip under the playhead to angle `i` (0-based),
    /// cutting it at the playhead first when `cut`.
    pub fn switch_angle(&mut self, i: usize, cut: bool) {
        let t = self.playhead;
        let Some((id, _, mc)) = self.active_multicam(t) else {
            self.status("No multicam clip under the playhead");
            return;
        };
        if i >= mc.angles.len() {
            self.status(format!("This multicam clip has {} angles", mc.angles.len()));
            return;
        }
        let (video, audio) = match self.multicam.mode {
            SwitchMode::Both => (Some(i), Some(i)),
            SwitchMode::Video => (Some(i), None),
            SwitchMode::Audio => (None, Some(i)),
        };
        // an audio-only angle can't be shown
        let video = video.filter(|_| mc.angles[i].has_video() || self.multicam.mode == SwitchMode::Video);
        let audio = audio.filter(|_| mc.angles[i].has_audio() || self.multicam.mode == SwitchMode::Audio);
        if video.is_none() && audio.is_none() {
            return;
        }
        let name = mc.angles[i].name.clone();
        if self.exec(Command::SwitchAngle { id, at: cut.then_some(t), video, audio }) {
            self.status(format!("Angle {}: {name}", i + 1));
        }
    }

    pub fn toggle_angle_viewer(&mut self) {
        self.multicam.angles = !self.multicam.angles;
        if self.multicam.angles && self.active_multicam(self.playhead).is_none() {
            self.status("Angles show when the playhead is over a multicam clip");
        }
    }
}

/// Viewer header controls while a multicam clip is under the playhead.
pub fn header(app: &mut App, ui: &mut egui::Ui) {
    if app.active_multicam(app.playhead).is_none() {
        return;
    }
    let tip = app.keymap.tip("Show every angle of the multicam clip; click one to cut to it (Shift+click: switch without cutting)", nle_core::Action::ToggleAngleViewer);
    if ui.selectable_label(app.multicam.angles, "Angles").on_hover_text(tip).clicked() {
        app.toggle_angle_viewer();
    }
    if app.multicam.angles {
        egui::ComboBox::from_id_salt("mc-mode").selected_text(app.multicam.mode.label()).width(96.0).show_ui(ui, |ui| {
            for m in [SwitchMode::Both, SwitchMode::Video, SwitchMode::Audio] {
                ui.selectable_value(&mut app.multicam.mode, m, m.label());
            }
        });
    }
    ui.separator();
}

/// Angle names, the shown / heard angle and clicks on the angle grid. True
/// while the grid is shown (no transform gizmo then).
pub fn overlay(app: &mut App, ui: &mut egui::Ui, resp: &egui::Response, img: Rect) -> bool {
    if !app.multicam.angles || app.browser.skim.is_some() || app.source_active() {
        return false;
    }
    // the frame on screen (the skimmer may be over another clip)
    let Some((_, _, mc)) = app.active_multicam(app.view_time()) else { return false };
    let n = mc.angles.len();
    let (video, audio) = (mc.video, mc.audio);
    let names: Vec<(String, bool)> = mc.angles.iter().map(|a| (a.name.clone(), a.has_video())).collect();
    let painter = ui.painter().with_clip_rect(img);
    let mut rects = Vec::with_capacity(n);
    for (i, (name, has_video)) in names.iter().enumerate() {
        let [x, y, w, h] = cell(i, n);
        let r = Rect::from_min_size(img.min + Vec2::new(x * img.width(), y * img.height()), Vec2::new(w * img.width(), h * img.height())).shrink(1.0);
        rects.push(r);
        if !has_video {
            painter.text(r.center(), egui::Align2::CENTER_CENTER, "\u{266a} audio only", egui::FontId::proportional(13.0), theme::TEXT_DIM);
        }
        let (stroke, label) = match (i == video, i == audio) {
            (true, true) => (Stroke::new(3.0_f32, theme::VIDEO), "\u{25b6} \u{266a}"),
            (true, false) => (Stroke::new(3.0_f32, theme::VIDEO), "\u{25b6}"),
            (false, true) => (Stroke::new(3.0_f32, theme::AUDIO), "\u{266a}"),
            _ => (Stroke::new(1.0_f32, Color32::from_white_alpha(60)), ""),
        };
        painter.rect_stroke(r, CornerRadius::ZERO, stroke, StrokeKind::Inside);
        let tag = format!("{}  {name}  {label}", i + 1);
        let pos = r.left_bottom() + Vec2::new(6.0, -6.0);
        let galley = painter.layout_no_wrap(tag, egui::FontId::proportional(11.0), Color32::WHITE);
        let bg = Rect::from_min_size(pos - Vec2::new(3.0, galley.size().y + 2.0), galley.size() + Vec2::new(6.0, 4.0));
        painter.rect_filled(bg, CornerRadius::same(3), Color32::from_black_alpha(160));
        painter.galley(Pos2::new(pos.x, pos.y - galley.size().y), galley, Color32::WHITE);
    }
    if resp.hovered() {
        if let Some(p) = ui.ctx().pointer_hover_pos() {
            if let Some(r) = rects.iter().find(|r| r.contains(p)) {
                painter.rect_stroke(*r, CornerRadius::ZERO, Stroke::new(1.5_f32, Color32::WHITE), StrokeKind::Inside);
                ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
            }
        }
    }
    if resp.clicked() {
        if let Some(i) = resp.interact_pointer_pos().and_then(|p| rects.iter().position(|r| r.contains(p))) {
            let cut = !ui.input(|i| i.modifiers.shift);
            app.switch_angle(i, cut);
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grid_cells_fill_the_frame_width_and_stay_inside() {
        for n in 1..=12 {
            for i in 0..n {
                let [x, y, w, h] = cell(i, n);
                assert!(x >= 0.0 && y >= -1e-6 && x + w <= 1.0 + 1e-6 && y + h <= 1.0 + 1e-6, "n={n} i={i}: {:?}", cell(i, n));
            }
        }
        assert_eq!(cell(1, 2), [0.5, 0.25, 0.5, 0.5]);
        assert_eq!(cell(3, 4), [0.5, 0.5, 0.5, 0.5]);
    }
}
