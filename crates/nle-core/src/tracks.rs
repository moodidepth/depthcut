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
