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
        Timeline { name: name.into(), format, spine: Vec::new(), snapping: true, markers: Vec::new(), captions: CaptionTrack::default() }
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
        let dur = self.duration();
        if at >= dur {
            let pad = (at - dur) + clip.duration;
            self.append(Clip::gap(pad))?;
        }
        let (i, start) = self.spine_index_at(at).ok_or(EditError::OutOfRange)?;
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
        let crossing: Vec<ClipId> = self
            .placed_clips()
            .iter()
            .filter(|p| p.lane != 0 && p.start < at && p.end() > at)
            .map(|p| p.clip.id)
            .collect();
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
        if let Some(r) = &mut c.retime {
            if new_d < r.timeline_duration() {
                let (l, _) = r.split_at(new_d);
                *r = l;
            } else if let Some(last) = r.segments.last_mut() {
                last.duration += new_d - (c.duration);
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
        let beats: Vec<Time> = self
            .markers
            .iter()
            .filter(|m| matches!(m.kind, MarkerKind::Beat | MarkerKind::SubBass | MarkerKind::Snare))
            .map(|m| m.time)
            .collect();
        if beats.is_empty() {
            return 0;
        }
        let min_dur = Time::from_frames(4, self.format.rate);
        let mut synced = 0usize;
        let target_ids: Vec<ClipId> = if ids.is_empty() {
            self.spine.iter().filter(|c| !c.is_gap()).map(|c| c.id).collect()
        } else {
            ids.to_vec()
        };
        for id in target_ids {
            let Some(start) = self.clip_start(id) else { continue };
            let Some(cur_dur) = self.clip(id).map(|c| c.duration) else { continue };
            let cur_end = start + cur_dur;
            if let Some(&nearest_beat) = beats
                .iter()
                .filter(|&&b| b >= start + min_dur)
                .min_by_key(|&&b| (b.0 - cur_end.0).abs())
            {
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
    pub fn trim_start(&mut self, id: ClipId, delta: Time) -> EditResult<Time> {
        let min = self.frame();
        let c = self.top_clip_mut(id)?;
        let unbounded = matches!(c.kind, ClipKind::Gap | ClipKind::Title(_) | ClipKind::Adjustment);
        let mut d = delta.min(c.duration - min);
        if c.retime.is_none() && !unbounded {
            // cannot reveal media before the start of the source
            d = d.max(-c.source_in);
        }
        if d.is_zero() {
            return Ok(Time::ZERO);
        }
        match &mut c.retime {
            Some(r) if d.is_positive() => {
                let (_, right) = r.split_at(d);
                *r = right;
            }
            Some(r) => {
                let extra = -d;
                let s0 = r.segments[0].speed;
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
        c.for_each_param_mut(|p| p.shift(-d));
        for cc in &mut c.connected {
            cc.offset -= d;
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
        let doomed: Vec<ClipId> = self
            .spine
            .iter()
            .zip(starts)
            .filter(|(c, s)| *s >= start && *s + c.duration <= end)
            .map(|(c, _)| c.id)
            .collect();
        for id in &doomed {
            self.remove_ripple(*id)?;
        }
        Ok(doomed.len())
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
        let inner: Vec<Clip> = self.spine.drain(first..=idx[idx.len() - 1]).collect();
        let comp = CompoundClip { spine: inner };
        let dur = comp.duration();
        let mut clip = Clip::new(name, ClipKind::Compound(Box::new(comp)), dur);
        clip.media_duration = Some(dur);
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
        let mut inner = Timeline { name: String::new(), format: self.format, spine: comp.spine.clone(), snapping: false, markers: Vec::new(), captions: CaptionTrack::default() };
        let (lo, hi) = (clip.source_in, clip.source_in + clip.duration);
        if lo > Time::ZERO {
            let _ = inner.blade(lo);
        }
        if hi < inner.duration() {
            let _ = inner.blade(hi);
        }
        let starts = inner.spine_starts();
        let pieces: Vec<(Time, Clip)> = inner
            .spine
            .into_iter()
            .zip(starts)
            .filter(|(c, s)| *s >= lo && *s + c.duration <= hi)
            .map(|(c, s)| (s - lo, c))
            .collect();
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
        let mut out: Vec<Marker> =
            self.markers.iter().filter(|m| !add.iter().any(|a| a.kind == m.kind && (a.time - m.time).abs() <= tolerance)).cloned().collect();
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
        let best = self
            .snap_points(exclude)
            .into_iter()
            .chain(extra.iter().copied())
            .map(|p| (p, (p - t).abs()))
            .filter(|(_, d)| *d <= threshold)
            .min_by_key(|(_, d)| *d);
        match best {
            Some((p, _)) => (p, true),
            None => (t, false),
        }
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
