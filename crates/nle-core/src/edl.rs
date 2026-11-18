//! CMX3600 EDL import / export.
//!
//! An EDL holds a single track, so export writes one EDL per flattened track
//! (`V1` = primary storyline, then connected lanes). Events use the modern
//! file-based conventions Resolve / Premiere / Avid read and write: reel
//! `AX`, `* FROM CLIP NAME:` / `* SOURCE FILE:` comments, `M2` speed lines,
//! cut-centred dissolves as `D` events, record times from 01:00:00:00.
//!
//! Limitations (also listed in the UI tooltip):
//! - non-drop-frame timecode only (drop-frame `;` timecode is read as NDF);
//! - source timecode is assumed to start at 00:00:00:00 (source TC isn't probed);
//! - titles, generators, adjustment layers and compound clips are skipped
//!   with a comment; effects, colour, keyframes and fades to black are lost;
//! - speed ramps are written as their average constant speed.

use crate::clip::ClipKind;
use crate::interchange::{self, ImportClip, ImportTrack, LaneTrack, TrackKind};
use crate::project::Project;
use crate::time::{format_timecode, parse_timecode, FrameRate, Time};
use std::fmt::Write as _;
use std::path::PathBuf;

/// Record timecode of the first frame of the timeline.
const RECORD_START_HOURS: i64 = 1;

#[derive(Debug, thiserror::Error, PartialEq)]
pub enum EdlError {
    #[error("no events found")]
    Empty,
    #[error("line {0}: {1}")]
    Syntax(usize, String),
}

/// One EDL per track: `(track name, text)`. `V1` is the primary storyline
/// (its audio rides along on the same events); other lanes follow.
pub fn export_tracks(project: &Project) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let (mut v, mut a) = (1, 0);
    let mut lanes = interchange::lanes(project);
    // V1 (the storyline) first, then lanes above it, then lanes below
    lanes.sort_by_key(|t| (t.lane != 0, t.lane < 0, t.lane.abs()));
    for t in lanes {
        let has_video = t.items.iter().any(|i| i.clip.av.video);
        let name = if t.lane == 0 {
            "V1".to_string()
        } else if t.lane > 0 || has_video {
            v += 1;
            format!("V{v}")
        } else {
            a += 1;
            format!("A{}", a + 1)
        };
        let text = export_track(project, &t, &name);
        out.push((name, text));
    }
    if out.is_empty() {
        out.push(("V1".into(), header(project, "V1")));
    }
    out
}

/// The primary storyline as one EDL.
pub fn export(project: &Project) -> String {
    export_tracks(project).into_iter().next().map(|(_, s)| s).unwrap_or_default()
}

fn header(project: &Project, track: &str) -> String {
    let title = if track == "V1" { project.name.clone() } else { format!("{} {track}", project.name) };
    format!("TITLE: {}\nFCM: NON-DROP FRAME\n\n", title.replace(['\r', '\n'], " "))
}

fn export_track(project: &Project, track: &LaneTrack, track_name: &str) -> String {
    let rate = project.timeline.format.rate;
    let rec0 = Time::from_secs(RECORD_START_HOURS * 3600);
    let tc = |t: Time| format_timecode(Time::from_frames(t.frames_round(rate), rate), rate);
    let mut s = header(project, track_name);
    let mut n = 0;
    let items = &track.items;
    for (i, it) in items.iter().enumerate() {
        let c = it.clip;
        let ClipKind::Asset { asset } = &c.kind else {
            let what = match &c.kind {
                ClipKind::Title(_) => "title",
                ClipKind::Adjustment => "adjustment layer",
                _ => "compound clip",
            };
            let _ = writeln!(s, "* SKIPPED {what} \"{}\" at {} (no EDL equivalent)\n", c.name, tc(rec0 + it.start));
            continue;
        };
        let Some(asset) = project.asset(*asset) else { continue };
        let chan = match (c.av.video, c.has_audio()) {
            (true, true) => "AA/V",
            (true, false) => "V",
            (false, true) => "AA",
            (false, false) => continue,
        };
        let (speed, ramped) = interchange::clip_speed(c);
        // source time at record time `t` (constant-speed approximation)
        let src = |t: Time| c.source_in + Time::from_secs_f64((t - it.start).as_secs_f64() * speed);
        let half = |d: Time| d.div_i64(2);
        let d_in = (i > 0).then(|| interchange::dissolve_after(items, i - 1)).flatten().filter(|_| is_media(project, &items[i - 1]));
        let d_out = interchange::dissolve_after(items, i).filter(|_| is_media(project, &items[i + 1]));
        let rec_in = it.start - d_in.map(half).unwrap_or_default();
        let rec_out = it.end() - d_out.map(half).unwrap_or_default();
        if rec_out <= rec_in {
            continue;
        }
        n += 1;
        let reel = "AX";
        // source out follows the record length; M2 carries the real speed
        let line = |s: &mut String, kind: &str, src_in: Time, r0: Time, r1: Time| {
            let _ = writeln!(s, "{n:03}  {reel:<8} {chan:<5} {kind:<8} {} {} {} {}", tc(src_in), tc(src_in + (r1 - r0)), tc(rec0 + r0), tc(rec0 + r1));
        };
        if let Some(d) = d_in {
            let prev = items[i - 1].clip;
            let (pspeed, _) = interchange::clip_speed(prev);
            let p_src = prev.source_in + Time::from_secs_f64((rec_in - items[i - 1].start).as_secs_f64() * pspeed);
            line(&mut s, "C", p_src, rec_in, rec_in);
            line(&mut s, &format!("D {:03}", d.frames_round(rate)), src(rec_in), rec_in, rec_out);
        } else {
            line(&mut s, "C", src(rec_in), rec_in, rec_out);
        }
        if (speed - 1.0).abs() > 1e-6 {
            let fps = speed * rate.nominal() as f64;
            let _ = writeln!(s, "M2   {reel:<8} {fps:>6.1}    {}", tc(src(rec_in)));
        }
        if let Some(d) = d_in {
            let _ = writeln!(s, "* EFFECT NAME: CROSS DISSOLVE ({} FRAMES)", d.frames_round(rate));
            let _ = writeln!(s, "* FROM CLIP NAME: {}", one_line(&items[i - 1].clip.name));
            let _ = writeln!(s, "* TO CLIP NAME: {}", one_line(&c.name));
        } else {
            let _ = writeln!(s, "* FROM CLIP NAME: {}", one_line(&c.name));
        }
        let _ = writeln!(s, "* SOURCE FILE: {}", asset.path.display());
        if ramped {
            let _ = writeln!(s, "* SPEED RAMP WRITTEN AS ITS AVERAGE SPEED");
        }
        s.push('\n');
    }
    s
}

fn is_media(project: &Project, it: &interchange::TrackItem) -> bool {
    matches!(&it.clip.kind, ClipKind::Asset { asset } if project.asset(*asset).is_some())
}

fn one_line(s: &str) -> String {
    s.replace(['\r', '\n'], " ")
}

#[derive(Debug, Default)]
struct Event {
    num: u32,
    reel: String,
    chan: String,
    /// Dissolve / wipe length in frames (`D 030`).
    trans: Option<i64>,
    src_in: Time,
    rec_in: Time,
    rec_out: Time,
    speed: Option<f64>,
    name: Option<String>,
    file: Option<String>,
}

/// Parse an EDL into a project at `rate` (EDLs don't record their frame rate).
pub fn import(src: &str, rate: FrameRate) -> Result<Project, EdlError> {
    let mut title = String::from("Imported EDL");
    let mut events: Vec<Event> = Vec::new();
    // index of the first line of the current event (dissolves span two lines)
    let mut group_start = 0usize;
    for (ln, raw) in src.lines().enumerate() {
        let line = raw.trim();
        if line.is_empty() {
            continue;
        }
        if let Some(t) = line.strip_prefix("TITLE:") {
            title = t.trim().to_string();
            continue;
        }
        if line.starts_with("FCM:") || line.starts_with("SPLIT:") {
            continue;
        }
        if let Some(c) = line.strip_prefix('*') {
            let c = c.trim();
            let field = |k: &str| c.strip_prefix(k).map(|v| v.trim().to_string());
            if let Some(v) = field("FROM CLIP NAME:") {
                if let Some(e) = events.get_mut(group_start) {
                    e.name = Some(v);
                }
            } else if let Some(v) = field("TO CLIP NAME:") {
                if let Some(e) = events.last_mut() {
                    e.name = Some(v);
                }
            } else if let Some(v) = field("SOURCE FILE:") {
                if let Some(e) = events.last_mut() {
                    e.file = Some(v);
                }
            }
            continue;
        }
        let tok: Vec<&str> = line.split_whitespace().collect();
        if tok.first() == Some(&"M2") {
            // M2 <reel> <fps> <src tc>: speed for the event with that reel
            if let (Some(fps), Some(e)) = (tok.get(tok.len().saturating_sub(2)).and_then(|v| v.parse::<f64>().ok()), events.last_mut()) {
                e.speed = Some(fps / rate.nominal() as f64);
            }
            continue;
        }
        let Some(num) = tok.first().and_then(|t| t.parse::<u32>().ok()) else { continue };
        if tok.len() < 8 {
            return Err(EdlError::Syntax(ln + 1, format!("expected at least 8 fields, found {}", tok.len())));
        }
        let tcs: Vec<Time> = tok[tok.len() - 4..].iter().map(|t| parse_timecode(t, rate)).collect::<Option<_>>().ok_or_else(|| EdlError::Syntax(ln + 1, "bad timecode".into()))?;
        let kind = tok[3].to_ascii_uppercase();
        let trans = match kind.as_str() {
            "C" => None,
            _ => Some(tok.get(4).and_then(|d| d.parse::<i64>().ok()).unwrap_or(0)),
        };
        if events.last().is_none_or(|e| e.num != num) {
            group_start = events.len();
        }
        events.push(Event { num, reel: tok[1].to_string(), chan: tok[2].to_ascii_uppercase(), trans, src_in: tcs[0], rec_in: tcs[2], rec_out: tcs[3], ..Default::default() });
    }
    if events.is_empty() {
        return Err(EdlError::Empty);
    }
    // record times relative to the whole hour the edit starts in
    let first = events.iter().map(|e| e.rec_in).min().unwrap_or_default();
    let origin = Time::from_secs((first.as_secs_f64() / 3600.0).floor() as i64 * 3600).clamp(Time::ZERO, first);

    let mut main: Vec<ImportClip> = Vec::new();
    let mut extra_audio: Vec<ImportClip> = Vec::new();
    let mut extra_video: Vec<ImportClip> = Vec::new();
    for e in events {
        if e.rec_out <= e.rec_in || e.reel.eq_ignore_ascii_case("BL") || e.reel.eq_ignore_ascii_case("BLACK") {
            continue;
        }
        let video = e.chan.contains('V') || e.chan == "B";
        let audio = e.chan.starts_with('A') || e.chan.contains("/A") || e.chan == "B";
        let speed = e.speed.unwrap_or(1.0);
        let duration = e.rec_out - e.rec_in;
        let mut source_in = e.src_in;
        if speed < 0.0 {
            // reverse: the event starts at the high end of its source range
            source_in = (e.src_in - Time::from_secs_f64(duration.as_secs_f64() * -speed)).max(Time::ZERO);
        }
        let path = PathBuf::from(e.file.clone().unwrap_or_else(|| e.name.clone().unwrap_or_else(|| e.reel.clone())));
        let mut c = ImportClip {
            name: e.name.clone().unwrap_or_else(|| path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| e.reel.clone())),
            path,
            start: e.rec_in - origin,
            duration,
            source_in,
            speed,
            video,
            audio,
            enabled: true,
            transition_in: None,
        };
        // a dissolve starts at rec_in: centre it on the cut by moving half of
        // it into the outgoing clip
        if let Some(frames) = e.trans.filter(|f| *f > 0) {
            let d = Time::from_frames(frames, rate).min(c.duration);
            let half = d.div_i64(2);
            if let Some(prev) = main.iter_mut().rev().find(|p| p.end() == c.start) {
                prev.duration += half;
                c.start += half;
                c.source_in += Time::from_secs_f64(half.as_secs_f64() * c.speed.abs());
                c.duration -= half;
            }
            c.transition_in = Some(d);
        }
        let overlaps = |v: &Vec<ImportClip>| v.iter().any(|p| p.start < c.end() && c.start < p.end());
        if !overlaps(&main) {
            main.push(c);
        } else if c.video {
            extra_video.push(c);
        } else {
            extra_audio.push(c);
        }
    }
    let kind = if main.iter().any(|c| c.video) { TrackKind::Video } else { TrackKind::Audio };
    let mut tracks = vec![ImportTrack { kind, clips: main }];
    if !extra_video.is_empty() {
        tracks.push(ImportTrack { kind: TrackKind::Video, clips: extra_video });
    }
    if !extra_audio.is_empty() {
        tracks.push(ImportTrack { kind: TrackKind::Audio, clips: extra_audio });
    }
    Ok(interchange::build_project(&title, rate, tracks))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::asset::{AudioStreamInfo, MediaAsset, VideoStreamInfo};
    use crate::clip::{Clip, Transition, TransitionKind};

    fn asset(name: &str, secs: i64) -> MediaAsset {
        let mut a = MediaAsset::new(name, format!("C:/media/{name}"), Time::from_secs(secs));
        a.video = Some(VideoStreamInfo {
            width: 1920,
            height: 1080,
            frame_rate: FrameRate::FPS_25,
            codec: "h264".into(),
            pix_fmt: "yuv420p".into(),
            bit_depth: 8,
            color_space: "bt709".into(),
            color_transfer: String::new(),
            color_primaries: String::new(),
            is_vfr: false,
            pts_table: Vec::new(),
        });
        a.audio = Some(AudioStreamInfo { sample_rate: 48000, channels: 2, codec: "aac".into() });
        a
    }

    fn sample() -> Project {
        let mut p = Project::new("Cut One");
        p.timeline.format.rate = FrameRate::FPS_25;
        let a = asset("A001.mov", 20);
        let b = asset("B002.mov", 20);
        let mut ca = Clip::from_asset_range(&a, Time::from_secs(2), Time::from_secs(4));
        ca.transition_out = Some(Transition::new(TransitionKind::CrossDissolve, Time::from_frames(10, FrameRate::FPS_25)));
        let cb = Clip::from_asset_range(&b, Time::from_secs(5), Time::from_secs(3));
        p.timeline.spine.push(ca);
        p.timeline.spine.push(cb);
        p.add_asset(a);
        p.add_asset(b);
        p
    }

    #[test]
    fn dissolve_is_written_as_a_d_event() {
        let edl = export(&sample());
        assert!(edl.starts_with("TITLE: Cut One\nFCM: NON-DROP FRAME"), "{edl}");
        // A: record 01:00:00:00 → cut − 5 frames (half the dissolve)
        assert!(edl.contains("001  AX       AA/V  C        00:00:02:00 00:00:05:20 01:00:00:00 01:00:03:20"), "{edl}");
        assert!(edl.contains("002  AX       AA/V  D 010    00:00:04:20 00:00:08:00 01:00:03:20 01:00:07:00"), "{edl}");
        assert!(edl.contains("* SOURCE FILE: C:/media/B002.mov"), "{edl}");
    }

    #[test]
    fn round_trip_keeps_cuts_sources_and_dissolves() {
        let p = sample();
        let q = import(&export(&p), FrameRate::FPS_25).unwrap();
        assert_eq!(q.name, "Cut One");
        let sp = &q.timeline.spine;
        assert_eq!(sp.len(), 2, "{sp:#?}");
        assert_eq!((sp[0].source_in, sp[0].duration), (Time::from_secs(2), Time::from_secs(4)));
        assert_eq!((sp[1].source_in, sp[1].duration), (Time::from_secs(5), Time::from_secs(3)));
        assert_eq!(sp[0].transition_out.as_ref().map(|t| t.duration), Some(Time::from_frames(10, FrameRate::FPS_25)));
        assert!(sp[0].av.video && sp[0].av.audio);
        assert_eq!(q.assets.len(), 2);
        assert_eq!(q.assets[0].path, PathBuf::from("C:/media/A001.mov"));
    }

    #[test]
    fn reads_premiere_style_edl_with_gaps_speed_and_audio() {
        let src = "TITLE: Sequence 01\nFCM: NON-DROP FRAME\n\n\
001  AX       V     C        00:00:10:00 00:00:12:00 01:00:00:00 01:00:02:00\n\
* FROM CLIP NAME: interview.mxf\n\n\
002  AX       V     C        00:00:00:00 00:00:02:00 01:00:03:00 01:00:05:00\n\
M2   AX       050.0          00:00:00:00\n\
* FROM CLIP NAME: broll.mp4\n\n\
003  AX       AA    C        00:00:00:00 00:00:04:00 01:00:01:00 01:00:05:00\n\
* FROM CLIP NAME: music.wav\n";
        let p = import(src, FrameRate::FPS_25).unwrap();
        let sp = &p.timeline.spine;
        assert_eq!(sp.len(), 3, "interview, gap, broll: {sp:#?}");
        assert_eq!(sp[0].source_in, Time::from_secs(10));
        assert!(sp[1].is_gap());
        let speed = sp[2].retime.as_ref().map(|r| r.segments[0].speed);
        assert_eq!(speed, Some(2.0));
        let music = p.timeline.placed_clips().into_iter().find(|c| c.lane < 0).expect("overlapping audio is connected");
        assert_eq!(music.start, Time::from_secs(1));
        assert!(music.clip.av.audio && !music.clip.av.video);
    }

    #[test]
    fn connected_lanes_get_their_own_edl() {
        let mut p = sample();
        let a = p.assets[0].clone();
        let c = Clip::from_asset_range(&a, Time::ZERO, Time::from_secs(1));
        p.timeline.connect(c, Time::from_secs(1), 1).unwrap();
        let t = export_tracks(&p);
        assert_eq!(t.iter().map(|(n, _)| n.as_str()).collect::<Vec<_>>(), ["V1", "V2"]);
        assert!(t[1].1.contains("01:00:01:00 01:00:02:00"), "{}", t[1].1);
    }
}
