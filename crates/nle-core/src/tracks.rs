//! Track controls on the timeline's lanes: eye, mute, solo and lock.
//!
//! Lane 0 is the primary storyline, positive lanes are above it and negative
//! ones below. Eye, mute and solo are applied in [`crate::eval`], so preview
//! and export agree. A lock is enforced around every [`crate::Command`].

use crate::clip::Clip;
use crate::ids::ClipId;
use crate::time::Time;
use crate::timeline::{EditError, EditResult, Timeline};
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrackState {
    pub lane: i32,
    /// Eye off: no picture from this lane.
    #[serde(default)]
    pub hidden: bool,
    #[serde(default)]
    pub muted: bool,
    /// While any lane is soloed, only soloed lanes are heard.
    #[serde(default)]
    pub solo: bool,
    #[serde(default)]
    pub locked: bool,
    /// Track targeting: source edits land on the targeted lane, Lift /
    /// Extract work on targeted lanes, Up / Down stop at their edit points.
    #[serde(default)]
    pub target: bool,
}

impl TrackState {
    pub fn new(lane: i32) -> Self {
        TrackState { lane, ..Default::default() }
    }

    fn is_default(&self) -> bool {
        *self == TrackState::new(self.lane)
    }
}

pub(crate) fn lane_visible(tracks: &[TrackState], lane: i32) -> bool {
    !tracks.iter().any(|t| t.lane == lane && t.hidden)
}

pub(crate) fn lane_audible(tracks: &[TrackState], lane: i32) -> bool {
    let state = tracks.iter().find(|t| t.lane == lane);
    !state.is_some_and(|t| t.muted) && (!tracks.iter().any(|t| t.solo) || state.is_some_and(|t| t.solo))
}

impl Timeline {
    pub fn track(&self, lane: i32) -> TrackState {
        self.tracks.iter().find(|t| t.lane == lane).copied().unwrap_or(TrackState::new(lane))
    }

    pub fn is_locked(&self, lane: i32) -> bool {
        self.tracks.iter().any(|t| t.lane == lane && t.locked)
    }

    pub(crate) fn set_track(&mut self, state: TrackState) {
        self.tracks.retain(|t| t.lane != state.lane);
        if !state.is_default() {
            self.tracks.push(state);
            self.tracks.sort_by_key(|t| t.lane);
        }
    }

    /// The first lane at or beyond `preferred` (away from the storyline) that
    /// is unlocked and free over `[start, start + duration)`.
    pub fn free_lane(&self, preferred: i32, start: Time, duration: Time) -> i32 {
        let placed = self.placed_clips();
        let step = if preferred < 0 { -1 } else { 1 };
        let mut lane = if preferred == 0 { 1 } else { preferred };
        while self.is_locked(lane) || placed.iter().any(|p| p.lane == lane && p.start < start + duration && p.end() > start) {
            lane += step;
        }
        lane
    }

    /// Lanes switched on as edit targets.
    pub fn targeted_lanes(&self) -> Vec<i32> {
        self.tracks.iter().filter(|t| t.target).map(|t| t.lane).collect()
    }

    /// Where a source edit lands: the targeted lane nearest the storyline
    /// on the video side (lane >= 0) or the audio side (< 0).
    pub fn target_lane(&self, video: bool) -> Option<i32> {
        self.targeted_lanes().into_iter().filter(|&l| (l >= 0) == video).min_by_key(|l| l.abs())
    }

    /// `lanes`, or every lane in use (and the storyline) when empty.
    fn range_lanes(&self, lanes: &[i32]) -> Vec<i32> {
        if !lanes.is_empty() {
            return lanes.to_vec();
        }
        let mut all: Vec<i32> = self.placed_clips().iter().map(|p| p.lane).chain([0]).collect();
        all.sort();
        all.dedup();
        all
    }

    /// Remove `[start, end)` from `lane`, cutting clips that cross the
    /// edges. Nothing moves. On the storyline the range becomes a gap.
    pub fn lane_clear(&mut self, lane: i32, start: Time, end: Time) -> EditResult<usize> {
        if end <= start {
            return Err(EditError::Nothing);
        }
        if lane == 0 {
            return self.lift_storyline(start, end);
        }
        for at in [start, end] {
            let crossing: Vec<ClipId> = self.placed_clips().iter().filter(|p| p.lane == lane && p.start < at && p.end() > at).map(|p| p.clip.id).collect();
            for id in crossing {
                self.blade_clip(id, at)?;
            }
        }
        let inside: Vec<ClipId> = self.placed_clips().iter().filter(|p| p.lane == lane && p.start >= start && p.end() <= end).map(|p| p.clip.id).collect();
        for id in &inside {
            self.remove_ripple(*id)?;
        }
        Ok(inside.len())
    }

    /// Replace `[start, end)` of the storyline with a gap; connected
    /// clips keep their places.
    pub fn lift_storyline(&mut self, start: Time, end: Time) -> EditResult<usize> {
        let end = end.min(self.duration());
        if end <= start {
            return Ok(0);
        }
        let _ = self.blade(start);
        let _ = self.blade(end);
        let starts = self.spine_starts();
        let doomed: Vec<ClipId> = self.spine.iter().zip(starts).filter(|(c, s)| !c.is_gap() && *s >= start && *s + c.duration <= end).map(|(c, _)| c.id).collect();
        for id in &doomed {
            self.remove_with_gap(*id)?;
        }
        self.merge_gaps();
        Ok(doomed.len())
    }

    /// Lift: clear `[start, end)` on `lanes` (every lane when empty);
    /// nothing moves.
    pub fn lift_range(&mut self, start: Time, end: Time, lanes: &[i32]) -> EditResult<usize> {
        let mut n = 0;
        for lane in self.range_lanes(lanes) {
            n += self.lane_clear(lane, start, end)?;
        }
        if n == 0 {
            return Err(EditError::Nothing);
        }
        Ok(n)
    }

    /// Extract: clear `[start, end)` on `lanes` (every lane when empty) and
    /// close the gap. The storyline always ripples; lanes that are not
    /// targeted move with it and keep their clips (sync lock), clips that
    /// started inside the range landing at its start.
    pub fn extract_range(&mut self, start: Time, end: Time, lanes: &[i32]) -> EditResult<usize> {
        if end <= start {
            return Err(EditError::Nothing);
        }
        let mut n = 0;
        for lane in self.range_lanes(lanes).into_iter().filter(|&l| l != 0) {
            n += self.lane_clear(lane, start, end)?;
        }
        let ripple_end = end.min(self.duration());
        let len = if ripple_end > start { ripple_end - start } else { Time::ZERO };
        let all: Vec<ClipId> = self.spine.iter().map(|c| c.id).collect();
        let held = self.detach_connected(&all);
        if len.is_positive() {
            n += self.range_delete(start, ripple_end)?;
        }
        let held = held
            .into_iter()
            .map(|(t, c)| {
                let t = if t >= ripple_end {
                    t - len
                } else if t > start {
                    start
                } else {
                    t
                };
                (t, c)
            })
            .collect();
        self.reattach(held)?;
        self.merge_gaps();
        if n == 0 {
            return Err(EditError::Nothing);
        }
        Ok(n)
    }

    /// Overwrite onto `lane`: the storyline (0) or a connected lane, which
    /// is cleared under the clip first.
    pub fn overwrite_on_lane(&mut self, at: Time, clip: Clip, lane: i32) -> EditResult<ClipId> {
        let id = clip.id;
        if lane == 0 {
            self.overwrite(at, clip)?;
            return Ok(id);
        }
        let end = at + clip.duration;
        match self.lane_clear(lane, at.max(Time::ZERO), end) {
            Ok(_) | Err(EditError::Nothing) => {}
            Err(e) => return Err(e),
        }
        self.connect(clip, at, lane)
    }

    /// Insert onto `lane`: everything from `at` on moves later by the
    /// clip's length (a gap opens in the storyline) and the clip lands on
    /// the lane.
    pub fn insert_on_lane(&mut self, at: Time, clip: Clip, lane: i32) -> EditResult<ClipId> {
        let id = clip.id;
        if lane == 0 {
            self.insert(at, clip)?;
            return Ok(id);
        }
        let at = at.max(Time::ZERO);
        if at < self.duration() {
            // the part of a clip on the lane after `at` moves with the ripple
            let crossing: Vec<ClipId> = self.placed_clips().iter().filter(|p| p.lane == lane && p.start < at && p.end() > at).map(|p| p.clip.id).collect();
            for c in crossing {
                self.blade_clip(c, at)?;
            }
            self.insert(at, Clip::gap(clip.duration))?;
        }
        self.connect(clip, at, lane)
    }
}

fn without_connected(c: &Clip) -> Clip {
    let mut c = c.clone();
    c.connected.clear();
    c
}

/// The storyline's own clips, ignoring the padding gaps at its end.
fn storyline(tl: &Timeline) -> Vec<Clip> {
    let end = tl.spine.iter().rposition(|c| !c.is_gap()).map_or(0, |i| i + 1);
    tl.spine[..end].iter().map(without_connected).collect()
}

type Timing = (ClipId, Time, Time, Time);

/// Timing of every clip linked to a clip on a locked lane.
fn locked_links(tl: &Timeline, links: &[u64]) -> Vec<Timing> {
    if links.is_empty() {
        return Vec::new();
    }
    let mut out: Vec<Timing> =
        tl.placed_clips().iter().filter(|p| p.clip.link.is_some_and(|l| links.contains(&l))).map(|p| (p.clip.id, p.start, p.clip.source_in, p.clip.duration)).collect();
    out.sort_by_key(|x| x.0);
    out
}

/// Run `edit` with locked lanes out of its reach. Their clips keep their
/// timeline positions through any ripple, can't be edited and can't gain
/// clips. On error the caller restores the timeline.
pub(crate) fn with_locks(tl: &mut Timeline, edit: impl FnOnce(&mut Timeline) -> EditResult<()>) -> EditResult<()> {
    let locked: Vec<i32> = tl.tracks.iter().filter(|t| t.locked).map(|t| t.lane).collect();
    if locked.is_empty() {
        return edit(tl);
    }
    let base = locked.contains(&0).then(|| storyline(tl));
    let links: Vec<u64> = tl.placed_clips().iter().filter(|p| locked.contains(&p.lane)).filter_map(|p| p.clip.link).collect();
    let starts = tl.spine_starts();
    let mut held: Vec<(Time, Clip)> = Vec::new();
    for (parent, start) in tl.spine.iter_mut().zip(starts) {
        let (out, stay): (Vec<Clip>, Vec<Clip>) = std::mem::take(&mut parent.connected).into_iter().partition(|c| locked.contains(&c.lane));
        parent.connected = stay;
        held.extend(out.into_iter().map(|c| (start + c.offset, c)));
    }
    let partners = locked_links(tl, &links);

    edit(tl).map_err(|e| match e {
        EditError::NotFound(id) if held.iter().any(|(_, c)| c.id == id) => EditError::Locked,
        e => e,
    })?;

    if tl.spine.iter().flat_map(|p| &p.connected).any(|c| locked.contains(&c.lane)) || locked_links(tl, &links) != partners {
        return Err(EditError::Locked);
    }
    if base.is_some_and(|b| b != storyline(tl)) {
        return Err(EditError::Locked);
    }
    tl.reattach(held)
}
