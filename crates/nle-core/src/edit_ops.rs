//! Clipboard, duplicate, detach / link audio and constant-speed edits.

use crate::clip::{AvFlags, Clip, ClipKind};
use crate::ids::ClipId;
use crate::speed::Retime;
use crate::time::Time;
use crate::timeline::{ClipLoc, EditError, EditResult, Timeline};
use std::collections::{HashMap, HashSet};

/// Slowest and fastest constant clip speed.
pub const MIN_SPEED: f64 = 0.1;
pub const MAX_SPEED: f64 = 10.0;

/// A copied clip: `start` is relative to the first clip copied with it.
#[derive(Clone, Debug, PartialEq)]
pub struct CopiedClip {
    pub start: Time,
    pub lane: i32,
    pub clip: Clip,
}

impl Timeline {
    /// Copy top-level clips and their linked partners (gaps are left out).
    /// Clips connected to a copied clip are copied only if they are in `ids`.
    pub fn copy_clips(&self, ids: &[ClipId]) -> Vec<CopiedClip> {
        let placed = self.placed_clips();
        let links: Vec<u64> = placed.iter().filter(|p| ids.contains(&p.clip.id)).filter_map(|p| p.clip.link).collect();
        let mut out: Vec<CopiedClip> = placed
            .iter()
            .filter(|p| !p.clip.is_gap() && (ids.contains(&p.clip.id) || p.clip.link.is_some_and(|l| links.contains(&l))))
            .map(|p| {
                let mut clip = p.clip.clone();
                clip.connected.clear();
                CopiedClip { start: p.start, lane: p.lane, clip }
            })
            .collect();
        out.sort_by_key(|c| (c.start, c.lane.abs(), c.lane));
        let base = out.first().map_or(Time::ZERO, |c| c.start);
        out.iter_mut().for_each(|c| c.start -= base);
        out
    }

    /// Paste at `at`: storyline clips ripple-insert (spacing kept with gaps),
    /// connected clips land on their lane, or the next free one.
    pub fn paste_clips(&mut self, items: &[CopiedClip], at: Time) -> EditResult<Vec<ClipId>> {
        if items.is_empty() {
            return Err(EditError::Nothing);
        }
        let mut at = at.max(Time::ZERO);
        let has_spine = items.iter().any(|i| i.lane == 0);
        if has_spine {
            at = at.min(self.duration()); // the storyline has no holes to paste into
        }
        let mut links: HashMap<u64, u64> = HashMap::new();
        let mut spine: Vec<(Time, Clip)> = Vec::new();
        let mut others: Vec<(Time, i32, Clip)> = Vec::new();
        let mut ids = Vec::new();
        for item in items {
            let mut c = item.clip.clone();
            c.remint_ids();
            if let Some(l) = c.link {
                let paired = items.iter().filter(|i| i.clip.link == Some(l)).count() > 1;
                c.link = paired.then(|| *links.entry(l).or_insert_with(|| ClipId::new().0));
            }
            ids.push(c.id);
            if item.lane == 0 {
                spine.push((item.start, c));
            } else {
                others.push((item.start, item.lane, c));
            }
        }
        spine.sort_by_key(|s| s.0);
        if let Some((_, first)) = spine.first_mut() {
            first.transition_in = None;
        }
        if let Some((_, last)) = spine.last_mut() {
            last.transition_out = None;
        }
        let mut end: Option<Time> = None;
        let mut cursor = at + spine.first().map_or(Time::ZERO, |s| s.0);
        cursor = cursor.min(self.duration());
        for (start, c) in spine {
            if let Some(gap) = end.map(|e| start - e).filter(|g| g.is_positive()) {
                self.insert(cursor, Clip::gap(gap))?;
                cursor += gap;
            }
            end = Some(start + c.duration);
            let len = c.duration;
            self.insert(cursor, c)?;
            cursor += len;
        }
        for (start, lane, c) in others {
            let lane = self.free_lane(lane, at + start, c.duration);
            self.connect(c, at + start, lane)?;
        }
        self.relink_follows();
        Ok(ids)
    }

    /// The copies a paste or duplicate of `count` clips made, given the clips
    /// that were there `before` it. Splitting the clip pasted into and filling
    /// gaps make new clips too. Those are not copies, and as ids only grow and
    /// the copies get theirs first, they always have the higher ids.
    pub fn pasted_ids(&self, before: &HashSet<ClipId>, count: usize) -> Vec<ClipId> {
        let mut made: Vec<ClipId> = self.placed_clips().iter().filter(|p| !p.clip.is_gap() && !before.contains(&p.clip.id)).map(|p| p.clip.id).collect();
        made.sort_unstable();
        made.truncate(count);
        made
    }

    /// Paste a copy straight after the selection.
    pub fn duplicate_clips(&mut self, ids: &[ClipId]) -> EditResult<Vec<ClipId>> {
        let items = self.copy_clips(ids);
        let placed = self.placed_clips();
        let chosen = || placed.iter().filter(|p| ids.contains(&p.clip.id) && !p.clip.is_gap());
        let at = chosen().filter(|p| p.lane == 0).map(|p| p.end()).max().or_else(|| chosen().map(|p| p.end()).max()).ok_or(EditError::Nothing)?;
        self.paste_clips(&items, at)
    }

    /// Move a clip's sound into a linked audio-only clip on a lane below.
    /// J/L split edits become that clip's own extent.
    pub fn detach_audio(&mut self, id: ClipId) -> EditResult<ClipId> {
        let loc = self.locate(id).ok_or(EditError::NotFound(id))?;
        let start = self.clip_start(id).ok_or(EditError::NotFound(id))?;
        let c = match loc {
            ClipLoc::Spine(i) => &mut self.spine[i],
            ClipLoc::Connected { parent, index } => &mut self.spine[parent].connected[index],
        };
        if !c.av.video || !c.av.audio || !matches!(c.kind, ClipKind::Asset { .. }) || c.link.is_some() {
            return Err(EditError::NotLinkable);
        }
        let (lead, tail) = c.audio_split();
        let lead = lead.min(start);
        let mut a = c.clone();
        let link = ClipId::new().0;
        c.av.audio = false;
        c.audio.audio_lead = Time::ZERO;
        c.audio.audio_tail = Time::ZERO;
        c.link = Some(link);

        a.connected.clear();
        a.audition.clear();
        a.remint_ids();
        a.link = Some(link);
        a.name = format!("{} (audio)", a.name);
        a.av = AvFlags { video: false, audio: true };
        a.video = Default::default();
        a.color = Default::default();
        a.effects.clear();
        a.tracks.clear();
        a.stabilization = None;
        a.preset_base = None;
        a.preset_bases.clear();
        a.transition_in = None;
        a.transition_out = None;
        a.audio.audio_lead = Time::ZERO;
        a.audio.audio_tail = Time::ZERO;
        a.source_in -= lead;
        a.duration += lead + tail;
        a.shift_keys(lead);
        a.anchor = lead;
        let lane = self.free_lane(-1, start - lead, a.duration);
        self.connect(a, start - lead, lane)
    }

    /// Break the link of `id`'s pair.
    pub fn unlink(&mut self, id: ClipId) -> EditResult<()> {
        let link = self.clip(id).ok_or(EditError::NotFound(id))?.link.ok_or(EditError::Nothing)?;
        for c in self.spine.iter_mut() {
            if c.link == Some(link) {
                c.link = None;
            }
            c.connected.iter_mut().filter(|cc| cc.link == Some(link)).for_each(|cc| cc.link = None);
        }
        Ok(())
    }

    /// Link a picture clip and an audio-only clip.
    pub fn link_clips(&mut self, a: ClipId, b: ClipId) -> EditResult<()> {
        let mut picture = 0;
        for id in [a, b] {
            self.locate(id).ok_or(EditError::NotFound(id))?;
            let c = self.clip(id).ok_or(EditError::NotFound(id))?;
            if c.link.is_some() || !matches!(c.kind, ClipKind::Asset { .. }) {
                return Err(EditError::NotLinkable);
            }
            picture += usize::from(c.av.video);
        }
        if a == b || picture != 1 {
            return Err(EditError::NotLinkable);
        }
        let link = ClipId::new().0;
        for id in [a, b] {
            self.clip_mut(id).ok_or(EditError::NotFound(id))?.link = Some(link);
        }
        Ok(())
    }

    /// Play the media the clip uses at a constant `speed` (negative = reverse).
    pub fn set_speed(&mut self, id: ClipId, speed: f64) -> EditResult<()> {
        if !speed.is_finite() || speed == 0.0 {
            return Err(EditError::OutOfRange);
        }
        let speed = speed.signum() * speed.abs().clamp(MIN_SPEED, MAX_SPEED);
        let c = self.clip_mut(id).ok_or(EditError::NotFound(id))?;
        if c.is_gap() {
            return Err(EditError::Nothing);
        }
        let (lo, hi) = c.source_range();
        // a frozen clip uses a single instant: play on from there
        let len = if hi > lo { hi - lo } else { c.duration };
        let len = c.media_duration.map_or(len, |md| len.min(md - lo)).max(Time(1));
        c.source_in = lo;
        if (speed - 1.0).abs() < 1e-9 {
            c.retime = None;
            c.duration = len;
        } else {
            let mut r = Retime::uniform(len, speed);
            r.blend = c.frame_quality();
            c.duration = r.timeline_duration();
            c.retime = Some(r);
        }
        Ok(())
    }
}
