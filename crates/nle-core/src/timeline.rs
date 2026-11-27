//! The Magnetic Timeline.
//!
//! The primary storyline (`spine`) is an ordered list of clips with *no
//! stored positions*: each clip starts where the previous one ends, so the
//! storyline can never contain accidental black holes, and every insert or
//! delete ripples automatically. Connected clips hang off a spine clip
//! (`Clip::connected`) at an offset relative to that parent and move with it.

use crate::captions::CaptionTrack;
use crate::clip::{Clip, ClipKind, CompoundClip};
use crate::ids::ClipId;
use crate::time::{FrameRate, Time};
use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum EditError {
    #[error("clip {0} not found")]
    NotFound(ClipId),
    #[error("time is outside the timeline")]
    OutOfRange,
    #[error("cut point is on an existing edit boundary")]
    AtBoundary,
    #[error("clip duration must be positive")]
    ZeroDuration,
    #[error("clips must be contiguous on the primary storyline")]
    NotContiguous,
    #[error("operation requires a primary storyline clip")]
    NotOnSpine,
    #[error("clip is not a compound clip")]
    NotCompound,
    #[error("operation requires a connected clip")]
    NotConnected,
    #[error("clips overlap")]
    Overlap,
    #[error("the track is locked")]
    Locked,
    #[error("needs a media clip with picture and sound, or an unlinked video + audio pair")]
    NotLinkable,
    #[error("nothing to do")]
    Nothing,
}

pub type EditResult<T> = Result<T, EditError>;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct VideoFormat {
    pub width: u32,
    pub height: u32,
    pub rate: FrameRate,
}

impl Default for VideoFormat {
    fn default() -> Self {
        VideoFormat { width: 1920, height: 1080, rate: FrameRate::FPS_30 }
    }
}

impl VideoFormat {
    pub fn uhd(rate: FrameRate) -> Self {
        VideoFormat { width: 3840, height: 2160, rate }
    }
    pub fn label(&self) -> String {
        format!("{}×{} {}", self.width, self.height, self.rate.label())
    }
    /// The timeline format that matches a source video stream: even
    /// dimensions (encoders need them), and the nearest standard rate when
    /// the source is variable-rate or within 2 % of one.
    pub fn from_source(width: u32, height: u32, rate: FrameRate, vfr: bool) -> Option<Self> {
        if width < 2 || height < 2 || rate.num == 0 || rate.den == 0 {
            return None;
        }
        let fps = rate.fps();
        let nearest = FrameRate::PRESETS.into_iter().min_by(|a, b| (a.fps() - fps).abs().total_cmp(&(b.fps() - fps).abs()))?;
        let rate = if vfr || (nearest.fps() - fps).abs() <= fps * 0.02 { nearest } else { rate };
        Some(VideoFormat { width: width & !1, height: height & !1, rate })
    }
}

/// Where a clip lives on the timeline.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClipLoc {
    Spine(usize),
    Connected { parent: usize, index: usize },
}

/// A clip with its resolved absolute timeline position.
#[derive(Clone, Debug)]
pub struct PlacedClip<'a> {
    pub clip: &'a Clip,
    pub start: Time,
    pub lane: i32,
    pub parent: Option<ClipId>,
}

impl PlacedClip<'_> {
    pub fn end(&self) -> Time {
        self.start + self.clip.duration
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Timeline {
    pub name: String,
    pub format: VideoFormat,
    pub spine: Vec<Clip>,
    #[serde(default = "yes")]
    pub snapping: bool,
    /// Timeline markers (beat markers snap cuts to music).
    #[serde(default)]
    pub markers: Vec<Marker>,
    #[serde(default)]
    pub captions: CaptionTrack,
    /// Role busses + master (only the project's root timeline is mixed with them).
    #[serde(default)]
    pub mixer: crate::audio_fx::MixerSettings,
    /// Eye / mute / solo / lock of the lanes that differ from the default.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tracks: Vec<crate::tracks::TrackState>,
    /// Someone set this timeline's format (Project Format edit or a reframe).
    /// Until then the first video clip edited into the empty timeline sets
    /// its size and frame rate, as in Final Cut Pro.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub format_chosen: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum MarkerKind {
    #[default]
    Standard,
    /// Detected musical beat.
    Beat,
    /// Sub-bass / kick (20–120 Hz) beat transient.
    SubBass,
    /// Snare / high-hat (1.5–6 kHz) transient.
    Snare,
    /// Detected kill / action highlight peak.
    Kill,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Marker {
    pub time: Time,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub kind: MarkerKind,
    #[serde(default)]
    pub color: Option<[u8; 3]>,
}

impl Marker {
    pub fn new(time: Time, name: impl Into<String>) -> Self {
        Marker { time, name: name.into(), kind: MarkerKind::Standard, color: None }
    }
    pub fn with_color(time: Time, name: impl Into<String>, rgb: [u8; 3]) -> Self {
        Marker { time, name: name.into(), kind: MarkerKind::Standard, color: Some(rgb) }
    }
    pub fn beat(time: Time) -> Self {
        Marker { time, name: String::new(), kind: MarkerKind::Beat, color: None }
    }
    pub fn sub_bass(time: Time) -> Self {
        Marker { time, name: "Kick".into(), kind: MarkerKind::SubBass, color: Some([255, 110, 48]) }
    }
    pub fn snare(time: Time) -> Self {
        Marker { time, name: "Snare".into(), kind: MarkerKind::Snare, color: Some([64, 210, 255]) }
    }
    pub fn kill(time: Time, label: impl Into<String>) -> Self {
        Marker { time, name: label.into(), kind: MarkerKind::Kill, color: Some([255, 69, 58]) }
    }
}

fn yes() -> bool {
    true
}

impl Default for Timeline {
    fn default() -> Self {
        Timeline::new("Untitled Project", VideoFormat::default())
    }
}

impl Timeline {
    pub fn new(name: impl Into<String>, format: VideoFormat) -> Self {
        Timeline {
            name: name.into(),
            format,
            spine: Vec::new(),
            snapping: true,
            markers: Vec::new(),
            captions: CaptionTrack::default(),
            mixer: Default::default(),
            tracks: Vec::new(),
            format_chosen: false,
        }
    }

    pub fn frame(&self) -> Time {
        self.format.rate.frame_duration()
    }

    /// Length of the primary storyline.
    pub fn duration(&self) -> Time {
        self.spine.iter().map(|c| c.duration).sum()
    }

    /// End of the last thing on the timeline (including connected clips).
    pub fn extent(&self) -> Time {
        self.placed_clips().iter().map(|p| p.end()).max().unwrap_or(Time::ZERO).max(self.duration())
    }

    /// Start time of each spine clip.
    pub fn spine_starts(&self) -> Vec<Time> {
        let mut out = Vec::with_capacity(self.spine.len());
        let mut acc = Time::ZERO;
        for c in &self.spine {
            out.push(acc);
            acc += c.duration;
        }
        out
    }

    pub fn spine_start(&self, index: usize) -> Time {
        self.spine[..index.min(self.spine.len())].iter().map(|c| c.duration).sum()
    }

    /// Spine clip covering `t` (half-open) and its start time.
    pub fn spine_index_at(&self, t: Time) -> Option<(usize, Time)> {
        let mut acc = Time::ZERO;
        for (i, c) in self.spine.iter().enumerate() {
            if t >= acc && t < acc + c.duration {
                return Some((i, acc));
            }
            acc += c.duration;
        }
        None
    }

    // ----- lookup ---------------------------------------------------------

    pub fn locate(&self, id: ClipId) -> Option<ClipLoc> {
        for (i, c) in self.spine.iter().enumerate() {
            if c.id == id {
                return Some(ClipLoc::Spine(i));
            }
            if let Some(j) = c.connected.iter().position(|cc| cc.id == id) {
                return Some(ClipLoc::Connected { parent: i, index: j });
            }
        }
        None
    }

    /// Find a clip anywhere (including inside compound clips).
    pub fn clip(&self, id: ClipId) -> Option<&Clip> {
        find_in(&self.spine, id)
    }

    pub fn clip_mut(&mut self, id: ClipId) -> Option<&mut Clip> {
        find_in_mut(&mut self.spine, id)
    }

    fn top_clip_mut(&mut self, id: ClipId) -> EditResult<&mut Clip> {
        match self.locate(id).ok_or(EditError::NotFound(id))? {
            ClipLoc::Spine(i) => Ok(&mut self.spine[i]),
            ClipLoc::Connected { parent, index } => Ok(&mut self.spine[parent].connected[index]),
        }
    }

    /// Absolute start time of a top-level clip.
    pub fn clip_start(&self, id: ClipId) -> Option<Time> {
        match self.locate(id)? {
            ClipLoc::Spine(i) => Some(self.spine_start(i)),
            ClipLoc::Connected { parent, index } => Some(self.spine_start(parent) + self.spine[parent].connected[index].offset),
        }
    }

    /// Every top-level clip with its absolute position.
    pub fn placed_clips(&self) -> Vec<PlacedClip<'_>> {
        let mut out = Vec::new();
        let mut acc = Time::ZERO;
        for c in &self.spine {
            out.push(PlacedClip { clip: c, start: acc, lane: 0, parent: None });
            for cc in &c.connected {
                out.push(PlacedClip { clip: cc, start: acc + cc.offset, lane: cc.lane, parent: Some(c.id) });
            }
            acc += c.duration;
        }
        out
    }

    pub fn all_ids(&self) -> Vec<ClipId> {
        let mut out = Vec::new();
        for c in &self.spine {
            c.collect_ids(&mut out);
        }
        out
    }

    pub fn max_clip_id(&self) -> u64 {
        self.spine.iter().map(|c| c.max_id()).max().unwrap_or(0)
    }

    // ----- magnetic editing -------------------------------------------------

    /// Append to the end of the primary storyline.
    pub fn append(&mut self, mut clip: Clip) -> EditResult<ClipId> {
        if !clip.duration.is_positive() {
            return Err(EditError::ZeroDuration);
        }
        clip.lane = 0;
        clip.offset = Time::ZERO;
        let id = clip.id;
        self.spine.push(clip);
        Ok(id)
    }

    /// Ripple-insert at `at`. If `at` falls inside a clip, that clip is
    /// bladed and the new clip goes between the halves; everything
    /// downstream moves later by the inserted duration.
    pub fn insert(&mut self, at: Time, mut clip: Clip) -> EditResult<usize> {
        if !clip.duration.is_positive() {
            return Err(EditError::ZeroDuration);
        }
        clip.lane = 0;
        clip.offset = Time::ZERO;
        let at = at.max(Time::ZERO);
        let Some((i, start)) = self.spine_index_at(at) else {
            self.spine.push(clip);
            return Ok(self.spine.len() - 1);
        };
        if at == start {
            self.spine.insert(i, clip);
            return Ok(i);
        }
        let (l, r) = self.spine[i].split(at - start).ok_or(EditError::AtBoundary)?;
        self.spine[i] = l;
        self.spine.insert(i + 1, r);
        self.spine.insert(i + 1, clip);
        Ok(i + 1)
    }

    /// Insert at a spine index.
    pub fn insert_at_index(&mut self, index: usize, mut clip: Clip) -> EditResult<usize> {
        if !clip.duration.is_positive() {
            return Err(EditError::ZeroDuration);
        }
        clip.lane = 0;
        clip.offset = Time::ZERO;
        let index = index.min(self.spine.len());
        self.spine.insert(index, clip);
        Ok(index)
    }

    /// Overwrite the range `[at, at + clip.duration)` of the storyline.
    pub fn overwrite(&mut self, at: Time, clip: Clip) -> EditResult<usize> {
        let end = at + clip.duration;
        if at < self.duration() {
            self.range_delete(at, end.min(self.duration()))?;
        } else if at > self.duration() {
            let pad = at - self.duration();
            self.append(Clip::gap(pad))?;
        }
        self.insert(at, clip)
    }

    /// Attach a connected clip at absolute time `at` on `lane` (≠ 0).
    /// If `at` is beyond the end of the storyline, a gap is appended so the
    /// clip has something to anchor to.
    pub fn connect(&mut self, mut clip: Clip, at: Time, lane: i32) -> EditResult<ClipId> {
        if lane == 0 {
            let id = clip.id;
            self.insert(at, clip)?;
            return Ok(id);
        }
        if !clip.duration.is_positive() {
            return Err(EditError::ZeroDuration);
        }
        let at = at.max(Time::ZERO);
        // the parent is the storyline clip under the connection point
        let point = at + clip.anchor_clamped();
        let dur = self.duration();
        if point >= dur {
            let pad = (at + clip.duration) - dur;
            self.append(Clip::gap(pad))?;
        }
        let (i, start) = self.spine_index_at(point).ok_or(EditError::OutOfRange)?;
        clip.lane = lane;
        clip.offset = at - start;
        let id = clip.id;
        let parent = &mut self.spine[i];
        parent.connected.push(clip);
        parent.connected.sort_by_key(|c| (c.offset, c.lane));
        Ok(id)
    }

    /// Delete a clip and close the hole (ripple). Deleting a spine clip also
    /// deletes the clips connected to it.
    pub fn remove_ripple(&mut self, id: ClipId) -> EditResult<Clip> {
        match self.locate(id).ok_or(EditError::NotFound(id))? {
            ClipLoc::Spine(i) => Ok(self.spine.remove(i)),
            ClipLoc::Connected { parent, index } => Ok(self.spine[parent].connected.remove(index)),
        }
    }

    /// Delete a clip but leave a gap clip of the same length in its place
    /// (Shift+Delete). Connected clips stay attached to the gap.
    pub fn remove_with_gap(&mut self, id: ClipId) -> EditResult<ClipId> {
        match self.locate(id).ok_or(EditError::NotFound(id))? {
            ClipLoc::Spine(i) => {
                let old = std::mem::replace(&mut self.spine[i], Clip::gap(Time(1)));
                let mut gap = Clip::gap(old.duration);
                gap.connected = old.connected;
                let gid = gap.id;
                self.spine[i] = gap;
                Ok(gid)
            }
            ClipLoc::Connected { parent, index } => {
                let removed = self.spine[parent].connected.remove(index);
                Ok(removed.id)
            }
        }
    }

    /// Split the spine clip under `at`. Returns `(left_id, right_id)`.
    pub fn blade(&mut self, at: Time) -> EditResult<(ClipId, ClipId)> {
        let (i, start) = self.spine_index_at(at).ok_or(EditError::OutOfRange)?;
        if at == start {
            return Err(EditError::AtBoundary);
        }
        let (l, r) = self.spine[i].split(at - start).ok_or(EditError::AtBoundary)?;
        let ids = (l.id, r.id);
        self.spine[i] = l;
        self.spine.insert(i + 1, r);
        Ok(ids)
    }

    /// Split a specific (spine or connected) clip at absolute time `at`.
    pub fn blade_clip(&mut self, id: ClipId, at: Time) -> EditResult<(ClipId, ClipId)> {
        match self.locate(id).ok_or(EditError::NotFound(id))? {
            ClipLoc::Spine(i) => {
                let start = self.spine_start(i);
                let (l, r) = self.spine[i].split(at - start).ok_or(EditError::AtBoundary)?;
                let ids = (l.id, r.id);
                self.spine[i] = l;
                self.spine.insert(i + 1, r);
                Ok(ids)
            }
            ClipLoc::Connected { parent, index } => {
                let start = self.spine_start(parent) + self.spine[parent].connected[index].offset;
                let c = &self.spine[parent].connected[index];
                let (l, mut r) = c.split(at - start).ok_or(EditError::AtBoundary)?;
                r.offset = l.offset + l.duration;
                let ids = (l.id, r.id);
                let p = &mut self.spine[parent];
                p.connected[index] = l;
                p.connected.insert(index + 1, r);
                Ok(ids)
            }
        }
    }

    /// Blade the spine and every connected clip crossing `at`.
    pub fn blade_all(&mut self, at: Time) -> usize {
        let mut n = 0;
        let crossing: Vec<ClipId> = self.placed_clips().iter().filter(|p| p.lane != 0 && p.start < at && p.end() > at).map(|p| p.clip.id).collect();
        for id in crossing {
            if self.blade_clip(id, at).is_ok() {
                n += 1;
            }
        }
        if self.blade(at).is_ok() {
            n += 1;
        }
        n
    }

    /// Ripple-trim the end of a clip by `delta` (positive lengthens).
    /// Returns the delta actually applied after clamping to media bounds.
    pub fn trim_end(&mut self, id: ClipId, delta: Time) -> EditResult<Time> {
        let min = self.frame();
        let c = self.top_clip_mut(id)?;
        let mut new_d = (c.duration + delta).max(min);
        let bounded = !matches!(c.kind, ClipKind::Gap | ClipKind::Title(_) | ClipKind::Adjustment);
        let (source_in, media) = (c.source_in, c.media_duration);
        if let Some(r) = &mut c.retime {
            if new_d < r.timeline_duration() {
                let (l, _) = r.split_at(new_d);
                *r = l;
            } else if !r.segments.is_empty() {
                let mut extra = new_d - c.duration;
                if bounded {
                    // cannot play past either end of the media
                    extra = retime_tail_limit(r, source_in, media, extra);
                }
                if let Some(last) = r.segments.last_mut() {
                    last.duration += extra;
                }
            }
            new_d = r.timeline_duration();
        } else if let Some(md) = c.media_duration {
            if !matches!(c.kind, ClipKind::Gap | ClipKind::Title(_) | ClipKind::Adjustment) {
                new_d = new_d.min(md - c.source_in).max(min);
            }
        }
        let applied = new_d - c.duration;
        c.duration = new_d;
        Ok(applied)
    }

    /// Pro montage `Q` hotkey: trim everything before `at` on clip `id` and ripple shut.
    pub fn ripple_trim_start_to(&mut self, id: ClipId, at: Time) -> EditResult<Time> {
        let start = self.clip_start(id).ok_or(EditError::NotFound(id))?;
        let dur = self.clip(id).ok_or(EditError::NotFound(id))?.duration;
        let delta = at - start;
        if delta <= Time::ZERO || delta >= dur {
            return Err(EditError::AtBoundary);
        }
        self.trim_start(id, delta)
    }

    /// Pro montage `W` hotkey: trim everything after `at` on clip `id` and ripple forward.
    pub fn ripple_trim_end_to(&mut self, id: ClipId, at: Time) -> EditResult<Time> {
        let start = self.clip_start(id).ok_or(EditError::NotFound(id))?;
        let dur = self.clip(id).ok_or(EditError::NotFound(id))?.duration;
        let end = start + dur;
        let delta = at - end;
        if at <= start || delta >= Time::ZERO {
            return Err(EditError::AtBoundary);
        }
        self.trim_end(id, delta)
    }

    /// Align clip cuts and velocity minima to timeline beat markers (`Beat`, `SubBass`, `Snare`).
    pub fn sync_clips_to_beats(&mut self, ids: &[ClipId]) -> usize {
        let beats: Vec<Time> = self.markers.iter().filter(|m| matches!(m.kind, MarkerKind::Beat | MarkerKind::SubBass | MarkerKind::Snare)).map(|m| m.time).collect();
        if beats.is_empty() {
            return 0;
        }
        let min_dur = Time::from_frames(4, self.format.rate);
        let mut synced = 0usize;
        let target_ids: Vec<ClipId> = if ids.is_empty() { self.spine.iter().filter(|c| !c.is_gap()).map(|c| c.id).collect() } else { ids.to_vec() };
        for id in target_ids {
            let Some(start) = self.clip_start(id) else { continue };
            let Some(cur_dur) = self.clip(id).map(|c| c.duration) else { continue };
            let cur_end = start + cur_dur;
            if let Some(&nearest_beat) = beats.iter().filter(|&&b| b >= start + min_dur).min_by_key(|&&b| (b.0 - cur_end.0).abs()) {
                let delta = nearest_beat - cur_end;
                if !delta.is_zero() && self.trim_end(id, delta).is_ok() {
                    synced += 1;
                }
            }
        }
        synced
    }

    /// Ripple-trim the start of a clip: positive `delta` removes media from
    /// the head, negative reveals more. Downstream clips ripple.
    ///
    /// On a connected clip (B-roll, titles, music) the end stays put and the
    /// start moves; storyline clips keep their start and ripple everything after.
    pub fn trim_start(&mut self, id: ClipId, delta: Time) -> EditResult<Time> {
        let min = self.frame();
        let connected = matches!(self.locate(id), Some(ClipLoc::Connected { .. }));
        let c = self.top_clip_mut(id)?;
        let unbounded = matches!(c.kind, ClipKind::Gap | ClipKind::Title(_) | ClipKind::Adjustment);
        let mut d = delta.min(c.duration - min);
        if !unbounded && d.is_negative() {
            // cannot reveal media before the start (or, reversed, past the end) of the source
            d = match &c.retime {
                None => d.max(-c.source_in),
                Some(r) if !r.segments.is_empty() => -retime_head_limit(r, c.source_in, c.media_duration, -d),
                Some(_) => d,
            };
        }
        if d.is_zero() {
            return Ok(Time::ZERO);
        }
        match &mut c.retime {
            Some(r) if d.is_positive() => {
                let (_, right) = r.split_at(d);
                *r = right;
            }
            Some(r) if r.segments.is_empty() => {}
            Some(r) => {
                let extra = -d;
                let s0 = retime_head_speed(r);
                if let Some(curve) = r.velocity_curve.as_mut().filter(|cv| !cv.is_empty()) {
                    // keep the existing frames on the same source times
                    curve.shift(extra);
                }
                r.segments[0].duration += extra;
                r.segments[0].ramp = Time::ZERO;
                r.segments[0].ramp_offset = Time::ZERO;
                r.segments[0].ramp_from = None;
                r.start_offset -= extra.mul_f64(s0);
            }
            None => {
                if !unbounded {
                    c.source_in += d;
                }
            }
        }
        c.duration -= d;
        c.shift_keys(-d);
        for cc in &mut c.connected {
            cc.offset -= d;
        }
        if connected {
            c.offset += d;
            // the connection point stays on the same frame of the clip
            c.anchor = (c.anchor - d).max(Time::ZERO);
        }
        Ok(d)
    }

    /// Slip: change which part of the source is shown without changing the
    /// clip's position or length.
    pub fn slip(&mut self, id: ClipId, delta: Time) -> EditResult<Time> {
        let c = self.top_clip_mut(id)?;
        let (lo, hi) = c.source_range();
        let mut d = delta.max(-lo);
        if let Some(md) = c.media_duration {
            d = d.min(md - hi);
        }
        c.source_in += d;
        Ok(d)
    }

    /// Slide a spine clip: moves its edit points left/right by trimming the
    /// neighbours, keeping the overall timeline length unchanged.
    pub fn slide(&mut self, id: ClipId, delta: Time) -> EditResult<Time> {
        let i = match self.locate(id).ok_or(EditError::NotFound(id))? {
            ClipLoc::Spine(i) => i,
            ClipLoc::Connected { .. } => return Err(EditError::NotOnSpine),
        };
        if i == 0 || i + 1 >= self.spine.len() {
            return Err(EditError::Nothing);
        }
        let prev = self.spine[i - 1].id;
        let next = self.spine[i + 1].id;
        let applied = self.trim_end(prev, delta)?;
        let applied2 = self.trim_start(next, applied)?;
        if applied2 != applied {
            // undo the imbalance on the previous clip
            self.trim_end(prev, applied2 - applied)?;
        }
        Ok(applied2)
    }

    /// Roll edit (FCP/Resolve/Premiere): move the edit point between spine
    /// clip `left` and the clip after it by `delta`. The left clip's out and
    /// the right clip's in move together, so nothing downstream shifts and
    /// the timeline length is unchanged. Returns the delta actually applied
    /// after clamping to available media on both sides.
    pub fn roll(&mut self, left: ClipId, delta: Time) -> EditResult<Time> {
        let i = match self.locate(left).ok_or(EditError::NotFound(left))? {
            ClipLoc::Spine(i) => i,
            ClipLoc::Connected { .. } => return Err(EditError::NotOnSpine),
        };
        if i + 1 >= self.spine.len() {
            return Err(EditError::AtBoundary);
        }
        let right = self.spine[i + 1].id;
        let min = self.frame();
        // pre-clamp so neither side collapses below one frame
        let (l, r) = (&self.spine[i], &self.spine[i + 1]);
        let mut d = delta.clamp(min - l.duration, r.duration - min);
        if r.retime.is_none() && !matches!(r.kind, ClipKind::Gap | ClipKind::Title(_) | ClipKind::Adjustment) {
            d = d.max(-r.source_in);
        }
        if l.retime.is_none() && !matches!(l.kind, ClipKind::Gap | ClipKind::Title(_) | ClipKind::Adjustment) {
            if let Some(md) = l.media_duration {
                d = d.min(md - l.source_in - l.duration);
            }
        }
        if d.is_zero() {
            return Err(EditError::AtBoundary);
        }
        let applied = self.trim_end(left, d)?;
        let applied2 = self.trim_start(right, applied)?;
        if applied2 != applied {
            self.trim_end(left, applied2 - applied)?;
        }
        if applied2.is_zero() {
            return Err(EditError::AtBoundary);
        }
        Ok(applied2)
    }

    /// Set a clip's J/L split-edit audio extension (clamped to available media).
    pub fn set_audio_split(&mut self, id: ClipId, lead: Time, tail: Time) -> EditResult<()> {
        let c = self.clip_mut(id).ok_or(EditError::NotFound(id))?;
        if c.retime.is_some() || !matches!(c.kind, ClipKind::Asset { .. }) || !c.av.audio {
            return Err(EditError::Nothing);
        }
        c.audio.audio_lead = lead.max(Time::ZERO);
        c.audio.audio_tail = tail.max(Time::ZERO);
        let (l, t) = c.audio_split();
        c.audio.audio_lead = l;
        c.audio.audio_tail = t;
        Ok(())
    }

    /// Move a spine clip to a new index (magnetic reorder).
    pub fn move_spine(&mut self, id: ClipId, to_index: usize) -> EditResult<usize> {
        let i = match self.locate(id).ok_or(EditError::NotFound(id))? {
            ClipLoc::Spine(i) => i,
            ClipLoc::Connected { .. } => return Err(EditError::NotOnSpine),
        };
        let clip = self.spine.remove(i);
        let to = to_index.min(self.spine.len());
        self.spine.insert(to, clip);
        Ok(to)
    }

    /// Spine index at which a dragged clip dropped at `at` should land.
    pub fn drop_index(&self, at: Time) -> usize {
        let mut acc = Time::ZERO;
        for (i, c) in self.spine.iter().enumerate() {
            if at < acc + c.duration.div_i64(2) {
                return i;
            }
            acc += c.duration;
        }
        self.spine.len()
    }

    /// Move a top-level clip to absolute time `at` on `lane`. Lane 0
    /// inserts into the primary storyline (ripple); other lanes re-anchor.
    pub fn move_clip(&mut self, id: ClipId, at: Time, lane: i32) -> EditResult<ClipId> {
        let loc = self.locate(id).ok_or(EditError::NotFound(id))?;
        if lane == 0 {
            if let ClipLoc::Spine(_) = loc {
                let idx = self.drop_index(at);
                let cur = match loc {
                    ClipLoc::Spine(i) => i,
                    _ => unreachable!(),
                };
                let to = if idx > cur { idx - 1 } else { idx };
                self.move_spine(id, to)?;
                return Ok(id);
            }
            let mut clip = self.remove_ripple(id)?;
            clip.connected.clear();
            self.insert(at, clip)?;
            return Ok(id);
        }
        if let ClipLoc::Spine(_) = loc {
            // lifting a spine clip out leaves a gap so the storyline keeps sync
            self.lift_to_lane(id, lane)?;
        }
        let mut clip = self.remove_ripple(id)?;
        clip.offset = Time::ZERO;
        self.connect(clip, at, lane)
    }

    /// Lift a spine clip out of the storyline into a connected lane, leaving
    /// a gap behind (FCP "Lift from Storyline").
    pub fn lift_to_lane(&mut self, id: ClipId, lane: i32) -> EditResult<ClipId> {
        let i = match self.locate(id).ok_or(EditError::NotFound(id))? {
            ClipLoc::Spine(i) => i,
            ClipLoc::Connected { .. } => return Err(EditError::NotOnSpine),
        };
        let start = self.spine_start(i);
        let mut clip = self.spine[i].clone();
        let connected = std::mem::take(&mut clip.connected);
        let mut gap = Clip::gap(clip.duration);
        gap.connected = connected;
        self.spine[i] = gap;
        clip.transition_in = None;
        clip.transition_out = None;
        self.connect(clip, start, if lane == 0 { 1 } else { lane })
    }

    /// Ripple-delete everything in `[start, end)` on the primary storyline.
    pub fn range_delete(&mut self, start: Time, end: Time) -> EditResult<usize> {
        if end <= start {
            return Err(EditError::Nothing);
        }
        let _ = self.blade(start);
        let _ = self.blade(end);
        let starts = self.spine_starts();
        let doomed: Vec<ClipId> = self.spine.iter().zip(starts).filter(|(c, s)| *s >= start && *s + c.duration <= end).map(|(c, _)| c.id).collect();
        for id in &doomed {
            self.remove_ripple(*id)?;
        }
        Ok(doomed.len())
    }

    /// A bare timeline around a nested spine (compound / storyline editing).
    pub fn nested(format: VideoFormat, spine: Vec<Clip>) -> Timeline {
        Timeline { name: String::new(), format, spine, snapping: false, markers: Vec::new(), captions: CaptionTrack::default(), mixer: Default::default(), tracks: Vec::new(), format_chosen: true }
    }

    /// Group connected clips into a secondary storyline (FCP "Create
    /// Storyline", Ctrl+G). The clips keep their timeline positions; holes
    /// between them become gaps inside the storyline, which from then on
    /// ripples independently of the primary storyline and can hold
    /// transitions. The storyline sits on the lane of its earliest clip.
    pub fn make_storyline(&mut self, ids: &[ClipId]) -> EditResult<ClipId> {
        let mut uniq = ids.to_vec();
        uniq.sort_unstable();
        uniq.dedup();
        let mut placed: Vec<(Time, ClipId)> = Vec::with_capacity(uniq.len());
        for id in uniq {
            match self.locate(id).ok_or(EditError::NotFound(id))? {
                ClipLoc::Connected { .. } => placed.push((self.clip_start(id).unwrap_or_default(), id)),
                ClipLoc::Spine(_) => return Err(EditError::NotConnected),
            }
        }
        placed.sort();
        let Some(&(base, first)) = placed.first() else {
            return Err(EditError::Nothing);
        };
        for w in placed.windows(2) {
            let len = self.clip(w[0].1).map_or(Time::ZERO, |c| c.duration);
            if w[0].0 + len > w[1].0 {
                return Err(EditError::Overlap);
            }
        }
        let lane = self.clip(first).map_or(1, |c| c.lane);
        let mut spine = Vec::with_capacity(placed.len());
        let mut acc = base;
        for (start, id) in placed {
            let mut c = self.remove_ripple(id)?;
            if start > acc {
                spine.push(Clip::gap(start - acc));
            }
            c.lane = 0;
            c.offset = Time::ZERO;
            c.anchor = Time::ZERO;
            acc = start + c.duration;
            spine.push(c);
        }
        let comp = CompoundClip::storyline(spine);
        let dur = comp.duration();
        let mut sl = Clip::new("Storyline", ClipKind::Compound(Box::new(comp)), dur);
        sl.media_duration = Some(dur);
        let id = sl.id;
        self.connect(sl, base, lane)?;
        Ok(id)
    }

    /// Run `f` on the spine inside compound clip / storyline `id` (step-in
    /// editing). The container's length follows its contents: a storyline
    /// always matches them, a compound used in full grows or shrinks with
    /// them, and a trimmed compound keeps its window (clamped to what is
    /// left). An edit that empties a top-level storyline removes it.
    pub fn edit_inside(&mut self, id: ClipId, f: impl FnOnce(&mut Timeline) -> EditResult<()>) -> EditResult<()> {
        let format = self.format;
        let min = self.frame();
        let c = self.clip_mut(id).ok_or(EditError::NotFound(id))?;
        let ClipKind::Compound(comp) = &mut c.kind else {
            return Err(EditError::NotCompound);
        };
        let mut inner = Timeline::nested(format, std::mem::take(&mut comp.spine));
        let result = f(&mut inner);
        comp.spine = inner.spine;
        result?;
        crate::multicam::store_spine(comp);
        let new_len = comp.duration();
        let storyline = comp.storyline;
        let old_len = c.media_duration.unwrap_or(new_len);
        if !new_len.is_positive() {
            return if storyline { self.remove_ripple(id).map(drop) } else { Err(EditError::ZeroDuration) };
        }
        c.media_duration = Some(new_len);
        if storyline {
            c.source_in = Time::ZERO;
            c.duration = new_len;
        } else if c.retime.is_none() {
            if c.source_in.is_zero() && c.duration == old_len {
                c.duration = new_len;
            } else {
                c.source_in = c.source_in.min(new_len - min).max(Time::ZERO);
                c.duration = c.duration.min(new_len - c.source_in).max(min);
            }
        }
        Ok(())
    }

    /// Take the connected clips of the given spine clips off the timeline,
    /// with their absolute starts (see [`Timeline::reattach`]).
    pub fn detach_connected(&mut self, ids: &[ClipId]) -> Vec<(Time, Clip)> {
        let starts = self.spine_starts();
        let mut out = Vec::new();
        for (c, s) in self.spine.iter_mut().zip(starts) {
            if ids.contains(&c.id) {
                out.extend(c.connected.drain(..).map(|cc| (s + cc.offset, cc)));
            }
        }
        out
    }

    /// Re-attach detached connected clips at their absolute starts, to
    /// whichever storyline clip is now under each connection point.
    pub fn reattach(&mut self, held: Vec<(Time, Clip)>) -> EditResult<()> {
        for (at, c) in held {
            let lane = c.lane;
            self.connect(c, at, lane)?;
        }
        Ok(())
    }

    /// Position tool (P): move a clip to `at` without rippling anything.
    /// Onto the primary storyline (lane 0) the clip leaves a gap behind and
    /// overwrites whatever is under its new range, with a gap filling any
    /// space past the end; every other clip keeps its timeline position.
    /// Its own connected clips travel with it. Other lanes move as usual.
    pub fn position_move(&mut self, id: ClipId, at: Time, lane: i32) -> EditResult<ClipId> {
        let loc = self.locate(id).ok_or(EditError::NotFound(id))?;
        if lane != 0 {
            return self.move_clip(id, at, lane);
        }
        let mut clip = match loc {
            ClipLoc::Spine(i) => {
                let gap = Clip::gap(self.spine[i].duration);
                std::mem::replace(&mut self.spine[i], gap)
            }
            ClipLoc::Connected { parent, index } => self.spine[parent].connected.remove(index),
        };
        let own = std::mem::take(&mut clip.connected);
        clip.lane = 0;
        clip.offset = Time::ZERO;
        clip.anchor = Time::ZERO;
        // hold everything else in place while the storyline changes under it
        let all: Vec<ClipId> = self.spine.iter().map(|c| c.id).collect();
        let held = self.detach_connected(&all);
        self.overwrite(at.max(Time::ZERO), clip)?;
        self.reattach(held)?;
        if let Some(ClipLoc::Spine(i)) = self.locate(id) {
            self.spine[i].connected = own;
        }
        self.merge_gaps();
        Ok(id)
    }

    /// Merge neighbouring gaps (keeping clips connected to them in place)
    /// and drop empty gaps at the end of the storyline.
    pub fn merge_gaps(&mut self) {
        let mut out: Vec<Clip> = Vec::with_capacity(self.spine.len());
        for c in std::mem::take(&mut self.spine) {
            match out.last_mut() {
                Some(prev) if prev.is_gap() && c.is_gap() && prev.transition_out.is_none() && c.transition_in.is_none() => {
                    let shift = prev.duration;
                    prev.duration += c.duration;
                    prev.connected.extend(c.connected.into_iter().map(|mut cc| {
                        cc.offset += shift;
                        cc
                    }));
                    prev.connected.sort_by_key(|cc| (cc.offset, cc.lane));
                }
                _ => out.push(c),
            }
        }
        while out.last().is_some_and(|c| c.is_gap() && c.connected.is_empty() && c.transition_in.is_none()) {
            out.pop();
        }
        self.spine = out;
    }

    /// Move a connected clip's connection point to absolute time `at`
    /// (Ctrl+Alt+Click). The clip keeps its position but re-attaches to the
    /// storyline clip under `at`, so later edits move it with that clip.
    pub fn set_connection_point(&mut self, id: ClipId, at: Time) -> EditResult<ClipId> {
        let ClipLoc::Connected { parent, index } = self.locate(id).ok_or(EditError::NotFound(id))? else {
            return Err(EditError::NotConnected);
        };
        let start = self.spine_start(parent) + self.spine[parent].connected[index].offset;
        let mut c = self.spine[parent].connected.remove(index);
        c.anchor = (at - start).clamp(Time::ZERO, (c.duration - Time(1)).max(Time::ZERO));
        let lane = c.lane;
        self.connect(c, start, lane)
    }

    /// Add an alternate take to a clip's audition.
    pub fn add_audition_take(&mut self, id: ClipId, mut take: Clip) -> EditResult<()> {
        if !take.duration.is_positive() {
            return Err(EditError::ZeroDuration);
        }
        let c = self.clip_mut(id).ok_or(EditError::NotFound(id))?;
        take.connected.clear();
        take.audition.clear();
        take.lane = 0;
        take.offset = Time::ZERO;
        take.anchor = Time::ZERO;
        take.transition_in = None;
        take.transition_out = None;
        take.link = None;
        c.audition.push(take);
        Ok(())
    }

    /// Make the take `step` places along the active pick of an audition
    /// (wrapping). Position, lane, connection point, connected clips and
    /// transitions belong to the slot and stay; the active pick always
    /// carries the slot's id. A take of another length ripples the storyline.
    pub fn cycle_audition(&mut self, id: ClipId, step: i32) -> EditResult<()> {
        let c = self.clip_mut(id).ok_or(EditError::NotFound(id))?;
        if c.audition.is_empty() {
            return Err(EditError::Nothing);
        }
        let mut takes = std::mem::take(&mut c.audition);
        let mut current = c.clone();
        current.connected.clear();
        current.transition_in = None;
        current.transition_out = None;
        current.lane = 0;
        current.offset = Time::ZERO;
        current.anchor = Time::ZERO;
        takes.insert(0, current);
        let ids: Vec<ClipId> = takes.iter().map(|t| t.id).collect();
        let n = takes.len() as i32;
        takes.rotate_left(step.rem_euclid(n) as usize);
        for (t, id) in takes.iter_mut().zip(ids) {
            t.id = id;
        }
        let mut pick = takes.remove(0);
        pick.lane = c.lane;
        pick.offset = c.offset;
        pick.anchor = c.anchor;
        pick.enabled = c.enabled;
        pick.link = c.link.take();
        pick.connected = std::mem::take(&mut c.connected);
        pick.transition_in = c.transition_in.take();
        pick.transition_out = c.transition_out.take();
        pick.audition = takes;
        *c = pick;
        Ok(())
    }

    /// Keep the active pick and discard the other takes.
    pub fn finalize_audition(&mut self, id: ClipId) -> EditResult<()> {
        let c = self.clip_mut(id).ok_or(EditError::NotFound(id))?;
        if c.audition.is_empty() {
            return Err(EditError::Nothing);
        }
        c.audition.clear();
        Ok(())
    }

    /// Collapse contiguous spine clips into a nested compound clip (Alt+G).
    pub fn make_compound(&mut self, ids: &[ClipId], name: &str) -> EditResult<ClipId> {
        let mut idx: Vec<usize> = ids
            .iter()
            .map(|id| match self.locate(*id) {
                Some(ClipLoc::Spine(i)) => Ok(i),
                Some(_) => Err(EditError::NotOnSpine),
                None => Err(EditError::NotFound(*id)),
            })
            .collect::<EditResult<_>>()?;
        if idx.is_empty() {
            return Err(EditError::Nothing);
        }
        idx.sort_unstable();
        idx.dedup();
        if idx.windows(2).any(|w| w[1] != w[0] + 1) {
            return Err(EditError::NotContiguous);
        }
        let first = idx[0];
        let mut inner: Vec<Clip> = self.spine.drain(first..=idx[idx.len() - 1]).collect();
        // the transitions at the selection's edges join the compound to its neighbours;
        // left inside they would fade to and from black
        let edge_in = inner.first_mut().and_then(|c| c.transition_in.take());
        let edge_out = inner.last_mut().and_then(|c| c.transition_out.take());
        let comp = CompoundClip::new(inner);
        let dur = comp.duration();
        let mut clip = Clip::new(name, ClipKind::Compound(Box::new(comp)), dur);
        clip.media_duration = Some(dur);
        clip.transition_in = edge_in;
        clip.transition_out = edge_out;
        let id = clip.id;
        self.spine.insert(first, clip);
        Ok(id)
    }

    /// Break a compound clip apart into its constituent clips (Shift+Alt+G).
    pub fn break_apart(&mut self, id: ClipId) -> EditResult<Vec<ClipId>> {
        let loc = self.locate(id).ok_or(EditError::NotFound(id))?;
        let (clip, parent_start) = match loc {
            ClipLoc::Spine(i) => (self.spine[i].clone(), self.spine_start(i)),
            ClipLoc::Connected { parent, index } => (self.spine[parent].connected[index].clone(), self.spine_start(parent)),
        };
        let ClipKind::Compound(comp) = &clip.kind else {
            return Err(EditError::NotCompound);
        };
        // Extract the used range of the inner storyline.
        let mut inner = Timeline::nested(self.format, comp.spine.clone());
        let (lo, hi) = (clip.source_in, clip.source_in + clip.duration);
        if lo > Time::ZERO {
            let _ = inner.blade(lo);
        }
        if hi < inner.duration() {
            let _ = inner.blade(hi);
        }
        let starts = inner.spine_starts();
        let mut pieces: Vec<(Time, Clip)> = inner.spine.into_iter().zip(starts).filter(|(c, s)| *s >= lo && *s + c.duration <= hi).map(|(c, s)| (s - lo, c)).collect();
        // the compound's own edge transitions go back to the clips at its edges
        if let Some(tr) = &clip.transition_in {
            if let Some((_, c)) = pieces.first_mut() {
                c.transition_in = Some(tr.clone());
            }
        }
        if let Some(tr) = &clip.transition_out {
            if let Some((_, c)) = pieces.last_mut() {
                c.transition_out = Some(tr.clone());
            }
        }
        let new_ids: Vec<ClipId> = pieces.iter().map(|(_, c)| c.id).collect();

        match loc {
            ClipLoc::Spine(i) => {
                let outer_connected = clip.connected.clone();
                let compound_start = parent_start;
                self.spine.remove(i);
                for (k, (_, piece)) in pieces.into_iter().enumerate() {
                    self.spine.insert(i + k, piece);
                }
                for cc in outer_connected {
                    let at = compound_start + cc.offset;
                    let lane = cc.lane;
                    self.connect(cc, at, lane)?;
                }
            }
            ClipLoc::Connected { parent, index } => {
                let base = clip.offset;
                let lane = clip.lane;
                self.spine[parent].connected.remove(index);
                let abs_base = parent_start + base;
                for (rel, mut piece) in pieces {
                    let children = std::mem::take(&mut piece.connected);
                    let piece_start = abs_base + rel;
                    self.connect(piece, piece_start, lane)?;
                    for ch in children {
                        let l = lane + if ch.lane == 0 { 1 } else { ch.lane };
                        let at = piece_start + ch.offset;
                        self.connect(ch, at, if l == 0 { lane } else { l })?;
                    }
                }
            }
        }
        Ok(new_ids)
    }

    // ----- snapping ---------------------------------------------------------

    /// All edit points: storyline boundaries plus connected clip edges.
    pub fn snap_points(&self, exclude: Option<ClipId>) -> Vec<Time> {
        let mut pts = vec![Time::ZERO];
        for p in self.placed_clips() {
            if Some(p.clip.id) == exclude {
                continue;
            }
            pts.push(p.start);
            pts.push(p.end());
        }
        pts.extend(self.markers.iter().map(|m| m.time));
        pts.sort_unstable();
        pts.dedup();
        pts
    }

    /// Add markers, replacing existing markers of the same kind within
    /// `tolerance` (re-analysing beats does not duplicate them).
    pub fn with_markers(&self, add: &[Marker], tolerance: Time) -> Vec<Marker> {
        let mut out: Vec<Marker> = self.markers.iter().filter(|m| !add.iter().any(|a| a.kind == m.kind && (a.time - m.time).abs() <= tolerance)).cloned().collect();
        out.extend(add.iter().cloned());
        out.sort_by_key(|m| m.time);
        out
    }

    /// Snap `t` to the nearest edit point (or one of `extra`) within
    /// `threshold`. Returns `t` unchanged when snapping is off.
    pub fn snap(&self, t: Time, threshold: Time, extra: &[Time], exclude: Option<ClipId>) -> (Time, bool) {
        if !self.snapping {
            return (t, false);
        }
        let best = self.snap_points(exclude).into_iter().chain(extra.iter().copied()).map(|p| (p, (p - t).abs())).filter(|(_, d)| *d <= threshold).min_by_key(|(_, d)| *d);
        match best {
            Some((p, _)) => (p, true),
            None => (t, false),
        }
    }
}

fn track_owner_in(clips: &[Clip], track: u64) -> Option<ClipId> {
    for c in clips {
        if c.tracks.iter().any(|t| t.id == track) {
            return Some(c.id);
        }
        if let Some(f) = track_owner_in(&c.connected, track) {
            return Some(f);
        }
        if let ClipKind::Compound(comp) = &c.kind {
            if let Some(f) = track_owner_in(&comp.spine, track) {
                return Some(f);
            }
        }
    }
    None
}

fn relink_follows_in(clips: &mut [Clip], owners: &dyn Fn(u64) -> Option<ClipId>) {
    for c in clips {
        if let Some(f) = c.video.follow.as_mut() {
            if let Some(owner) = owners(f.track) {
                f.clip = owner;
            }
        }
        relink_follows_in(&mut c.connected, owners);
        if let ClipKind::Compound(comp) = &mut c.kind {
            relink_follows_in(&mut comp.spine, owners);
        }
    }
}

impl Timeline {
    /// The clip that owns motion track `track` (track ids are unique).
    pub fn track_owner(&self, track: u64) -> Option<ClipId> {
        track_owner_in(&self.spine, track)
    }

    /// Point every follow at the clip that now owns its track (after an
    /// import or paste re-minted clip ids).
    pub fn relink_follows(&mut self) {
        let snapshot = self.spine.clone();
        relink_follows_in(&mut self.spine, &|t| track_owner_in(&snapshot, t));
    }
}

fn find_in(clips: &[Clip], id: ClipId) -> Option<&Clip> {
    for c in clips {
        if c.id == id {
            return Some(c);
        }
        if let Some(f) = find_in(&c.connected, id) {
            return Some(f);
        }
        if let ClipKind::Compound(comp) = &c.kind {
            if let Some(f) = find_in(&comp.spine, id) {
                return Some(f);
            }
        }
    }
    None
}

fn find_in_mut(clips: &mut [Clip], id: ClipId) -> Option<&mut Clip> {
    for c in clips {
        if c.id == id {
            return Some(c);
        }
        if let Some(f) = find_in_mut(&mut c.connected, id) {
            return Some(f);
        }
        if let ClipKind::Compound(comp) = &mut c.kind {
            if let Some(f) = find_in_mut(&mut comp.spine, id) {
                return Some(f);
            }
        }
    }
    None
}

/// Source speed used when a retimed clip's head is extended (the speed the
/// new head frames play at).
fn retime_head_speed(r: &crate::speed::Retime) -> f64 {
    if r.has_velocity_curve() {
        r.speed_at_secs(0.0)
    } else {
        r.segments.first().map_or(1.0, |s| s.speed)
    }
}

/// Largest head extension (at most `want`) of a retimed clip that keeps its
/// first frame inside the media: not before source 0 and, when the media
/// length is known, not past its end (reverse playback).
fn retime_head_limit(r: &crate::speed::Retime, source_in: Time, media: Option<Time>, want: Time) -> Time {
    let speed = retime_head_speed(r);
    let head = (source_in + r.start_offset).as_secs_f64();
    let room = if speed > 0.0 {
        head / speed
    } else if speed < 0.0 {
        match media {
            Some(md) => (md.as_secs_f64() - head) / -speed,
            None => return want,
        }
    } else {
        return want; // freeze frame: the head shows the same frame
    };
    want.min(Time::from_secs_f64(room.max(0.0)))
}

/// Largest tail extension (at most `want`) of a retimed clip that keeps its
/// last frame inside the media (`0 ..= media length`). Found by bisection on
/// the real mapping, so ramps and velocity curves are handled exactly.
fn retime_tail_limit(r: &crate::speed::Retime, source_in: Time, media: Option<Time>, want: Time) -> Time {
    const EPS: f64 = 1e-6;
    let fits = |extra: Time| {
        let total = (r.timeline_duration() + extra).as_secs_f64();
        let mut probe = r.clone();
        if let Some(last) = probe.segments.last_mut() {
            last.duration += extra;
        }
        let end = source_in.as_secs_f64() + probe.map_secs(total);
        end >= -EPS && media.is_none_or(|md| end <= md.as_secs_f64() + EPS)
    };
    if want <= Time::ZERO || fits(want) {
        return want.max(Time::ZERO);
    }
    if !fits(Time::ZERO) {
        return Time::ZERO;
    }
    let (mut lo, mut hi) = (0_i64, want.0);
    while hi - lo > 1 {
        let mid = lo + (hi - lo) / 2;
        if fits(Time(mid)) {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    Time(lo)
}
