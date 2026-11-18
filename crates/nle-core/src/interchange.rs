//! Track-based view of the magnetic timeline, shared by the EDL and
//! OpenTimelineIO exchange formats.
//!
//! Export flattens the storyline into lanes: lane 0 is the primary
//! storyline, connected clips keep their lane (overlapping clips on one lane
//! spill into extra tracks), and secondary storylines are expanded in place.
//! Import goes the other way: the first video track becomes the primary
//! storyline (gaps filled), later video tracks become connected lanes 1, 2, …,
//! and audio-only clips become connected lanes −1, −2, … unless they line up
//! exactly with a video clip from the same file, in which case that clip
//! keeps its audio.

use crate::asset::{AssetKind, MediaAsset};
use crate::clip::{AvFlags, Clip, ClipKind, Transition, TransitionKind};
use crate::project::Project;
use crate::speed::Retime;
use crate::time::{FrameRate, Time};
use std::collections::BTreeMap;
use std::path::PathBuf;

/// A clip at an absolute timeline position.
#[derive(Clone, Copy, Debug)]
pub struct TrackItem<'a> {
    pub start: Time,
    pub clip: &'a Clip,
}

impl TrackItem<'_> {
    pub fn end(&self) -> Time {
        self.start + self.clip.duration
    }
}

/// Non-overlapping clips of one lane.
#[derive(Clone, Debug)]
pub struct LaneTrack<'a> {
    pub lane: i32,
    pub items: Vec<TrackItem<'a>>,
}

/// Every lane as non-overlapping tracks, lowest lane first (lane 0 is always
/// a single track: the primary storyline). Gaps and disabled clips are left
/// out; secondary storylines are expanded into their clips.
pub fn lanes(project: &Project) -> Vec<LaneTrack<'_>> {
    fn push<'a>(by_lane: &mut BTreeMap<i32, Vec<TrackItem<'a>>>, lane: i32, start: Time, clip: &'a Clip) {
        match &clip.kind {
            ClipKind::Compound(c) if c.storyline => {
                let mut t = start;
                for ch in &c.spine {
                    push(by_lane, lane, t, ch);
                    t += ch.duration;
                }
            }
            ClipKind::Gap => {}
            _ if !clip.enabled => {}
            _ => by_lane.entry(lane).or_default().push(TrackItem { start, clip }),
        }
    }
    let mut by_lane = BTreeMap::new();
    let mut acc = Time::ZERO;
    for c in &project.timeline.spine {
        push(&mut by_lane, 0, acc, c);
        for cc in &c.connected {
            push(&mut by_lane, cc.lane, acc + cc.offset, cc);
        }
        acc += c.duration;
    }
    let mut out = Vec::new();
    for (lane, mut items) in by_lane {
        items.sort_by_key(|i| i.start);
        let mut tracks: Vec<Vec<TrackItem>> = Vec::new();
        for it in items {
            match tracks.iter_mut().find(|t| t.last().is_none_or(|l| l.end() <= it.start)) {
                Some(t) => t.push(it),
                None => tracks.push(vec![it]),
            }
        }
        out.extend(tracks.into_iter().map(|items| LaneTrack { lane, items }));
    }
    out
}

/// Constant playback speed of a clip (source seconds per timeline second).
/// Speed ramps are approximated by their average; the flag says so.
pub fn clip_speed(clip: &Clip) -> (f64, bool) {
    match &clip.retime {
        None => (1.0, false),
        Some(r) if r.is_identity() => (1.0, false),
        Some(r) => {
            let constant = r.velocity_curve.as_ref().is_none_or(|c| c.is_empty()) && r.segments.len() == 1 && r.segments[0].ramp.is_zero();
            if constant {
                (r.segments[0].speed, false)
            } else {
                let (lo, hi) = r.source_span();
                let d = clip.duration.as_secs_f64().max(1e-9);
                ((hi - lo).as_secs_f64() / d, true)
            }
        }
    }
}

/// The dissolve between `items[i]` and `items[i + 1]`, if they touch and
/// both carry media.
pub fn dissolve_after(items: &[TrackItem], i: usize) -> Option<Time> {
    let a = items.get(i)?;
    let b = items.get(i + 1)?;
    let tr = a.clip.transition_out.as_ref()?;
    (a.end() == b.start && tr.duration.is_positive()).then_some(tr.duration)
}

/// One clip read from an exchange file, in cut form: transitions are centred
/// on the cut and run on handles beyond the clips' visible ranges.
#[derive(Clone, Debug)]
pub struct ImportClip {
    pub name: String,
    /// Media file; may be a bare file name when the file didn't record a path.
    pub path: PathBuf,
    pub start: Time,
    pub duration: Time,
    pub source_in: Time,
    /// Source seconds per timeline second (negative = reverse).
    pub speed: f64,
    pub video: bool,
    pub audio: bool,
    pub enabled: bool,
    /// Dissolve ending the previous clip and starting this one, centred on
    /// `start` (or from black when nothing precedes it).
    pub transition_in: Option<Time>,
}

impl ImportClip {
    pub fn end(&self) -> Time {
        self.start + self.duration
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TrackKind {
    Video,
    Audio,
}

#[derive(Clone, Debug)]
pub struct ImportTrack {
    pub kind: TrackKind,
    pub clips: Vec<ImportClip>,
}

/// Build a project from flat tracks (see the module docs for the mapping).
pub fn build_project(name: &str, rate: FrameRate, mut tracks: Vec<ImportTrack>) -> Project {
    let mut project = Project::new(name);
    project.timeline.format.rate = rate;
    for t in &mut tracks {
        t.clips.retain(|c| c.duration.is_positive());
        t.clips.sort_by_key(|c| c.start);
    }
    tracks.retain(|t| !t.clips.is_empty());

    // media: one asset per file (or per name when there's no path)
    let mut assets: Vec<MediaAsset> = Vec::new();
    let mut ids: BTreeMap<String, usize> = BTreeMap::new();
    for t in &tracks {
        for c in &t.clips {
            let key = c.path.to_string_lossy().to_lowercase();
            let (lo, hi) = source_bounds(c);
            let i = *ids.entry(key).or_insert_with(|| {
                let name = c.path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| c.name.clone());
                let mut a = MediaAsset::new(name, c.path.clone(), Time::ZERO);
                a.kind = AssetKind::Audio;
                assets.push(a);
                assets.len() - 1
            });
            let a = &mut assets[i];
            a.duration = a.duration.max(hi).max(lo);
            if c.video {
                a.kind = AssetKind::Video;
            }
        }
    }
    let asset_of = |c: &ImportClip| assets[ids[&c.path.to_string_lossy().to_lowercase()]].id;

    // the primary storyline: first video track, else the first track
    let spine_idx = tracks.iter().position(|t| t.kind == TrackKind::Video).unwrap_or(0);
    // audio-only clips that duplicate a video clip become that clip's audio
    let mut audio_on: Vec<Vec<bool>> = tracks.iter().map(|t| t.clips.iter().map(|c| c.audio).collect()).collect();
    let mut consumed: Vec<Vec<bool>> = tracks.iter().map(|t| vec![false; t.clips.len()]).collect();
    for ti in 0..tracks.len() {
        if tracks[ti].kind != TrackKind::Audio {
            continue;
        }
        for ci in 0..tracks[ti].clips.len() {
            let a = &tracks[ti].clips[ci];
            let same =
                |v: &ImportClip| v.video && v.start == a.start && v.duration == a.duration && v.source_in == a.source_in && v.path == a.path && (v.speed - a.speed).abs() < 1e-6;
            let hit = tracks
                .iter()
                .enumerate()
                .filter(|(_, t)| t.kind == TrackKind::Video)
                .find_map(|(vi, t)| t.clips.iter().enumerate().find(|(vc, v)| same(v) && !audio_on[vi][*vc]).map(|(vc, _)| (vi, vc)));
            if let Some((vi, vc)) = hit {
                audio_on[vi][vc] = true;
                consumed[ti][ci] = true;
            }
        }
    }

    let make = |c: &ImportClip, audio: bool| -> Clip {
        let mut clip = Clip::new(c.name.clone(), ClipKind::Asset { asset: asset_of(c) }, c.duration);
        clip.source_in = c.source_in;
        clip.av = AvFlags { video: c.video, audio };
        clip.enabled = c.enabled;
        if c.speed != 0.0 && (c.speed - 1.0).abs() > 1e-6 {
            let src = Time::from_secs_f64(c.duration.as_secs_f64() * c.speed.abs());
            let mut r = Retime::uniform(src, c.speed);
            r.segments[0].duration = c.duration;
            clip.retime = Some(r);
        }
        clip
    };

    // primary storyline
    if let Some(t) = tracks.get(spine_idx) {
        let mut at = Time::ZERO;
        for (ci, c) in t.clips.iter().enumerate() {
            if c.start < at {
                continue; // overlaps the previous clip; dropped (shouldn't happen in cut form)
            }
            let touching = c.start == at && !project.timeline.spine.is_empty();
            if c.start > at {
                project.timeline.spine.push(Clip::gap(c.start - at));
            }
            let mut clip = make(c, audio_on[spine_idx][ci]);
            if let Some(d) = c.transition_in {
                match project.timeline.spine.last_mut() {
                    Some(prev) if touching && !prev.is_gap() => prev.transition_out = Some(Transition::new(TransitionKind::CrossDissolve, d)),
                    _ => clip.transition_in = Some(Transition::new(TransitionKind::CrossDissolve, d.min(c.duration))),
                }
            }
            project.timeline.spine.push(clip);
            at = c.end();
        }
    }
    // everything else connects to it
    let mut video_lane = 0;
    let mut audio_lane = 0;
    for (ti, t) in tracks.iter().enumerate() {
        // tracks whose clips all merged into video clips take no lane
        if ti == spine_idx || consumed[ti].iter().all(|c| *c) {
            continue;
        }
        let lane = match t.kind {
            TrackKind::Video => {
                video_lane += 1;
                video_lane
            }
            TrackKind::Audio => {
                audio_lane -= 1;
                audio_lane
            }
        };
        for (ci, c) in t.clips.iter().enumerate() {
            if consumed[ti][ci] {
                continue;
            }
            let clip = make(c, audio_on[ti][ci]);
            let _ = project.timeline.connect(clip, c.start, lane);
        }
    }
    project.assets = assets;
    project.reserve_ids();
    project
}

/// Lowest and highest source time a clip touches.
fn source_bounds(c: &ImportClip) -> (Time, Time) {
    let span = Time::from_secs_f64(c.duration.as_secs_f64() * c.speed.abs());
    (c.source_in, c.source_in + span)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ic(path: &str, start: i64, dur: i64, video: bool, audio: bool) -> ImportClip {
        ImportClip {
            name: path.into(),
            path: path.into(),
            start: Time::from_secs(start),
            duration: Time::from_secs(dur),
            source_in: Time::ZERO,
            speed: 1.0,
            video,
            audio,
            enabled: true,
            transition_in: None,
        }
    }

    #[test]
    fn audio_duplicates_merge_and_leftovers_connect() {
        let v = ImportTrack { kind: TrackKind::Video, clips: vec![ic("a.mov", 0, 4, true, false), ic("b.mov", 6, 2, true, false)] };
        let a = ImportTrack { kind: TrackKind::Audio, clips: vec![ic("a.mov", 0, 4, false, true), ic("music.wav", 1, 5, false, true)] };
        let p = build_project("t", FrameRate::FPS_25, vec![v, a]);
        let sp = &p.timeline.spine;
        assert_eq!(sp.len(), 3, "a, gap, b");
        assert!(sp[0].av.video && sp[0].av.audio);
        assert!(sp[1].is_gap());
        assert!(sp[2].av.video && !sp[2].av.audio);
        let placed = p.timeline.placed_clips();
        let music = placed.iter().find(|pc| pc.lane == -1).expect("music connected below");
        assert_eq!(music.start, Time::from_secs(1));
        assert_eq!(p.assets.len(), 3);
        assert_eq!(p.assets.iter().find(|a| a.name == "music.wav").unwrap().kind, AssetKind::Audio);
    }
}
