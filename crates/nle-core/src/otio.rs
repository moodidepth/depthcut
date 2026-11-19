//! OpenTimelineIO (`.otio`, JSON) import / export.
//!
//! Export writes a `Timeline.1` whose stack holds video tracks (lowest lane
//! first, so the primary storyline is `V1`) followed by audio tracks (`A1` =
//! the storyline's audio). Clips use `Clip.2` with an `ExternalReference.1`
//! (`file://` URL), constant speed changes are a `LinearTimeWarp.1` effect,
//! and dissolves are `Transition.1` (`SMPTE_Dissolve`) items between clips.
//! Times are frame counts at the timeline rate.
//!
//! Import reads `Clip.1` / `Clip.2`, `Gap.1`, `Transition.1` and
//! `LinearTimeWarp.1`; nested stacks / tracks are skipped as gaps of their
//! length. Titles are written as clips with a `MissingReference.1` (so other
//! tools keep a placeholder of the right length) and skipped on import.
//! Effects, colour, keyframes, markers and fades to / from black are not
//! exchanged.

use crate::clip::ClipKind;
use crate::fcpxml::{path_to_url, url_to_path};
use crate::interchange::{self, ImportClip, ImportTrack, LaneTrack, TrackKind};
use crate::project::Project;
use crate::time::{FrameRate, Time};
use serde_json::{json, Map, Value};
use std::path::PathBuf;

#[derive(Debug, thiserror::Error)]
pub enum OtioError {
    #[error("not valid JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("not an OpenTimelineIO timeline (OTIO_SCHEMA is {0:?})")]
    NotATimeline(String),
}

const ORIGIN: &str = "rustcutpro";

fn rt(t: Time, rate: FrameRate) -> Value {
    json!({ "OTIO_SCHEMA": "RationalTime.1", "rate": rate.fps(), "value": t.frames_round(rate) as f64 })
}

fn range(start: Time, dur: Time, rate: FrameRate) -> Value {
    json!({ "OTIO_SCHEMA": "TimeRange.1", "start_time": rt(start, rate), "duration": rt(dur, rate) })
}

fn gap(dur: Time, rate: FrameRate) -> Value {
    json!({ "OTIO_SCHEMA": "Gap.1", "name": "", "source_range": range(Time::ZERO, dur, rate), "effects": [], "markers": [], "metadata": {}, "enabled": true })
}

fn track_json(project: &Project, t: &LaneTrack, kind: TrackKind, name: &str) -> Value {
    let rate = project.timeline.format.rate;
    let mut children = Vec::new();
    let mut at = Time::ZERO;
    let items: Vec<_> = t.items.iter().filter(|i| if kind == TrackKind::Video { i.clip.av.video } else { i.clip.has_audio() }).copied().collect();
    for (i, it) in items.iter().enumerate() {
        let c = it.clip;
        if it.start > at {
            children.push(gap(it.start - at, rate));
        }
        let reference = match &c.kind {
            ClipKind::Asset { asset } => match project.asset(*asset) {
                Some(a) => json!({
                    "OTIO_SCHEMA": "ExternalReference.1",
                    "name": a.name,
                    "target_url": path_to_url(&a.path),
                    "available_range": range(Time::ZERO, a.duration, rate),
                    "metadata": {},
                }),
                None => json!({ "OTIO_SCHEMA": "MissingReference.1", "name": c.name, "metadata": {} }),
            },
            other => {
                let what = match other {
                    ClipKind::Title(_) => "title",
                    ClipKind::Adjustment => "adjustment",
                    _ => "compound",
                };
                json!({ "OTIO_SCHEMA": "MissingReference.1", "name": c.name, "metadata": { ORIGIN: { "kind": what } } })
            }
        };
        let (speed, ramped) = interchange::clip_speed(c);
        let mut effects = Vec::new();
        if (speed - 1.0).abs() > 1e-6 {
            effects.push(json!({ "OTIO_SCHEMA": "LinearTimeWarp.1", "name": "", "effect_name": "LinearTimeWarp", "time_scalar": speed, "metadata": {} }));
        }
        let mut md = Map::new();
        if ramped {
            md.insert(ORIGIN.into(), json!({ "speed_ramp_averaged": true }));
        }
        children.push(json!({
            "OTIO_SCHEMA": "Clip.2",
            "name": c.name,
            "source_range": range(c.source_in, c.duration, rate),
            "media_references": { "DEFAULT_MEDIA": reference },
            "active_media_reference_key": "DEFAULT_MEDIA",
            "effects": effects,
            "markers": [],
            "metadata": Value::Object(md),
            "enabled": true,
        }));
        at = it.end();
        if let Some(d) = interchange::dissolve_after(&items, i) {
            let half = d.div_i64(2);
            children.push(json!({
                "OTIO_SCHEMA": "Transition.1",
                "name": "Cross Dissolve",
                "transition_type": "SMPTE_Dissolve",
                "in_offset": rt(half, rate),
                "out_offset": rt(d - half, rate),
                "metadata": {},
            }));
        }
    }
    json!({
        "OTIO_SCHEMA": "Track.1",
        "name": name,
        "kind": if kind == TrackKind::Video { "Video" } else { "Audio" },
        "source_range": null,
        "children": children,
        "effects": [],
        "markers": [],
        "metadata": {},
        "enabled": true,
    })
}

/// The whole timeline as an OTIO JSON document.
pub fn export(project: &Project) -> String {
    let rate = project.timeline.format.rate;
    let lanes = interchange::lanes(project);
    let mut tracks = Vec::new();
    let mut n = 0;
    for t in lanes.iter().filter(|t| t.items.iter().any(|i| i.clip.av.video)) {
        n += 1;
        tracks.push(track_json(project, t, TrackKind::Video, &format!("V{n}")));
    }
    let mut audio: Vec<&LaneTrack> = lanes.iter().filter(|t| t.items.iter().any(|i| i.clip.has_audio())).collect();
    // A1 = the storyline's audio, then lanes nearest to it
    audio.sort_by_key(|t| (t.lane != 0, t.lane > 0, t.lane.abs()));
    for (i, t) in audio.iter().enumerate() {
        tracks.push(track_json(project, t, TrackKind::Audio, &format!("A{}", i + 1)));
    }
    let doc = json!({
        "OTIO_SCHEMA": "Timeline.1",
        "name": project.name,
        // the frame labelled 01:00:00:00 (drop-frame at 29.97 / 59.94, as
        // OTIO's `to_timecode` infers), not 3600 s: NTSC timecode runs slow
        "global_start_time": rt(crate::time::parse_timecode_df("01:00:00:00", rate).unwrap_or_default(), rate),
        "metadata": { ORIGIN: { "width": project.timeline.format.width, "height": project.timeline.format.height } },
        "tracks": {
            "OTIO_SCHEMA": "Stack.1",
            "name": "tracks",
            "source_range": null,
            "children": tracks,
            "effects": [],
            "markers": [],
            "metadata": {},
            "enabled": true,
        },
    });
    serde_json::to_string_pretty(&doc).unwrap_or_default()
}

fn schema(v: &Value) -> &str {
    v.get("OTIO_SCHEMA").and_then(Value::as_str).unwrap_or("")
}

/// RationalTime → (time, rate).
fn read_rt(v: Option<&Value>) -> Option<(Time, f64)> {
    let v = v?;
    let rate = v.get("rate")?.as_f64()?;
    let value = v.get("value")?.as_f64()?;
    (rate > 0.0).then(|| (Time::from_secs_f64(value / rate), rate))
}

fn read_range(v: Option<&Value>) -> Option<(Time, Time)> {
    let v = v.filter(|v| !v.is_null())?;
    Some((read_rt(v.get("start_time"))?.0, read_rt(v.get("duration"))?.0))
}

/// Length of an item without its own `source_range` (nested track / stack).
fn item_duration(v: &Value) -> Time {
    if let Some((_, d)) = read_range(v.get("source_range")) {
        return d;
    }
    let children = v.get("children").and_then(Value::as_array).map(Vec::as_slice).unwrap_or_default();
    let durs = children.iter().filter(|c| schema(c) != "Transition.1").map(item_duration);
    if schema(v).starts_with("Stack") {
        durs.max().unwrap_or_default()
    } else {
        durs.sum()
    }
}

fn snap(t: Time, rate: FrameRate) -> Time {
    Time::from_frames(t.frames_round(rate), rate)
}

/// Parse an OTIO JSON document.
pub fn import(src: &str) -> Result<Project, OtioError> {
    let doc: Value = serde_json::from_str(src)?;
    if !schema(&doc).starts_with("Timeline.") {
        return Err(OtioError::NotATimeline(schema(&doc).to_string()));
    }
    let name = doc.get("name").and_then(Value::as_str).filter(|s| !s.is_empty()).unwrap_or("Imported OTIO").to_string();
    let stack = doc.get("tracks").cloned().unwrap_or(Value::Null);
    let tracks_json = stack.get("children").and_then(Value::as_array).cloned().unwrap_or_default();

    // frame rate: the first clip's, else the global start time's
    let mut fps = None;
    for t in &tracks_json {
        for c in t.get("children").and_then(Value::as_array).into_iter().flatten() {
            if fps.is_none() && schema(c).starts_with("Clip") {
                fps = c.get("source_range").and_then(|r| read_rt(r.get("duration"))).map(|(_, r)| r);
            }
        }
    }
    let fps = fps.or_else(|| read_rt(doc.get("global_start_time")).map(|(_, r)| r)).unwrap_or(30.0);
    let rate = FrameRate::from_f64(fps);

    let mut tracks = Vec::new();
    for t in &tracks_json {
        if !schema(t).starts_with("Track") {
            continue;
        }
        let kind = if t.get("kind").and_then(Value::as_str) == Some("Audio") { TrackKind::Audio } else { TrackKind::Video };
        let track_enabled = t.get("enabled").and_then(Value::as_bool).unwrap_or(true);
        let mut clips: Vec<ImportClip> = Vec::new();
        let mut at = Time::ZERO;
        let mut pending: Option<Time> = None;
        for item in t.get("children").and_then(Value::as_array).into_iter().flatten() {
            let s = schema(item);
            if s.starts_with("Transition") {
                let a = read_rt(item.get("in_offset")).map(|x| x.0).unwrap_or_default();
                let b = read_rt(item.get("out_offset")).map(|x| x.0).unwrap_or_default();
                pending = Some(snap(a + b, rate));
                continue;
            }
            let dur = item_duration(item);
            if s.starts_with("Clip") {
                let (src_in, d) = read_range(item.get("source_range")).unwrap_or((Time::ZERO, dur));
                let refs = item.get("media_references").and_then(|m| {
                    let key = item.get("active_media_reference_key").and_then(Value::as_str).unwrap_or("DEFAULT_MEDIA");
                    m.get(key)
                });
                let reference = refs.or_else(|| item.get("media_reference")).cloned().unwrap_or(Value::Null);
                let clip_name = item.get("name").and_then(Value::as_str).unwrap_or("").to_string();
                let ours = reference.get("metadata").and_then(|m| m.get(ORIGIN)).is_some();
                let path = match schema(&reference) {
                    s if s.starts_with("ExternalReference") => reference.get("target_url").and_then(Value::as_str).map(url_to_path),
                    // offline media: keep the name so Relink can find it
                    s if s.starts_with("MissingReference") && !ours => {
                        let n = reference.get("name").and_then(Value::as_str).filter(|n| !n.is_empty()).unwrap_or(&clip_name);
                        (!n.is_empty()).then(|| PathBuf::from(n))
                    }
                    _ => None,
                };
                let speed = item
                    .get("effects")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .find(|e| schema(e).starts_with("LinearTimeWarp"))
                    .and_then(|e| e.get("time_scalar").and_then(Value::as_f64))
                    .filter(|s| *s != 0.0)
                    .unwrap_or(1.0);
                if let Some(path) = path {
                    clips.push(ImportClip {
                        name: if clip_name.is_empty() { path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default() } else { clip_name },
                        path,
                        start: snap(at, rate),
                        duration: snap(d, rate),
                        source_in: src_in,
                        speed,
                        video: kind == TrackKind::Video,
                        audio: kind == TrackKind::Audio,
                        enabled: track_enabled && item.get("enabled").and_then(Value::as_bool).unwrap_or(true),
                        transition_in: pending.take(),
                    });
                }
                at += d;
            } else {
                // Gap / nested track or stack
                at += dur;
            }
            pending = None;
        }
        tracks.push(ImportTrack { kind, clips });
    }
    let mut p = interchange::build_project(&name, rate, tracks);
    if let Some(md) = doc.get("metadata").and_then(|m| m.get(ORIGIN)) {
        let dim = |k: &str| md.get(k).and_then(Value::as_u64).map(|v| v as u32);
        if let (Some(w), Some(h)) = (dim("width"), dim("height")) {
            p.timeline.format.width = w;
            p.timeline.format.height = h;
        }
    }
    Ok(p)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::asset::{AudioStreamInfo, MediaAsset, VideoStreamInfo};
    use crate::clip::{Clip, TitleParams, Transition, TransitionKind};
    use crate::speed::Retime;

    fn asset(name: &str) -> MediaAsset {
        let mut a = MediaAsset::new(name, format!("D:/Footage/Day 1/{name}"), Time::from_secs(30));
        a.video = Some(VideoStreamInfo {
            width: 3840,
            height: 2160,
            frame_rate: FrameRate::FPS_24,
            codec: "prores".into(),
            pix_fmt: "yuv422p10le".into(),
            bit_depth: 10,
            color_space: "bt709".into(),
            color_transfer: String::new(),
            color_primaries: String::new(),
            is_vfr: false,
            pts_table: Vec::new(),
        });
        a.audio = Some(AudioStreamInfo { sample_rate: 48000, channels: 2, codec: "pcm_s24le".into() });
        a
    }

    #[test]
    fn round_trip_keeps_structure() {
        let mut p = Project::new("Doc Cut");
        p.timeline.format = crate::timeline::VideoFormat { width: 3840, height: 2160, rate: FrameRate::FPS_24 };
        let a = asset("A 001.mov");
        let b = asset("B002.mov");
        let mut music = MediaAsset::new("score.wav", "D:/Audio/score.wav", Time::from_secs(60));
        music.kind = crate::asset::AssetKind::Audio;
        music.audio = Some(AudioStreamInfo { sample_rate: 48000, channels: 2, codec: "pcm_s16le".into() });
        let mut ca = Clip::from_asset_range(&a, Time::from_secs(3), Time::from_secs(5));
        ca.transition_out = Some(Transition::new(TransitionKind::CrossDissolve, Time::from_frames(12, FrameRate::FPS_24)));
        let mut cb = Clip::from_asset_range(&b, Time::from_secs(1), Time::from_secs(4));
        let mut r = Retime::uniform(Time::from_secs(8), 2.0);
        r.segments[0].duration = Time::from_secs(4);
        cb.retime = Some(r);
        p.timeline.spine.push(ca);
        p.timeline.spine.push(cb);
        let cm = Clip::from_asset_range(&music, Time::ZERO, Time::from_secs(6));
        p.timeline.connect(cm, Time::from_secs(2), -1).unwrap();
        let title = Clip::title(TitleParams::default(), Time::from_secs(2));
        p.timeline.connect(title, Time::from_secs(1), 1).unwrap();
        p.add_asset(a);
        p.add_asset(b);
        p.add_asset(music);

        let text = export(&p);
        // `scripts/validate_otio.py` loads this with the opentimelineio library
        if let Some(dir) = std::env::var_os("RCP_OTIO_DUMP") {
            std::fs::write(PathBuf::from(dir).join("rcp_export.otio"), &text).unwrap();
        }
        assert!(text.contains("\"OTIO_SCHEMA\": \"Timeline.1\""));
        assert!(text.contains("file:///D:/Footage/Day%201/A%20001.mov"), "{text}");
        assert!(text.contains("LinearTimeWarp.1"));
        assert!(text.contains("SMPTE_Dissolve"));

        let q = import(&text).unwrap();
        assert_eq!(q.name, "Doc Cut");
        assert_eq!((q.timeline.format.width, q.timeline.format.rate), (3840, FrameRate::FPS_24));
        let sp = &q.timeline.spine;
        assert_eq!(sp.len(), 2, "{sp:#?}");
        assert_eq!((sp[0].source_in, sp[0].duration), (Time::from_secs(3), Time::from_secs(5)));
        assert!(sp[0].av.video && sp[0].av.audio, "A1 merged back into V1");
        assert_eq!(sp[0].transition_out.as_ref().map(|t| t.duration), Some(Time::from_frames(12, FrameRate::FPS_24)));
        assert_eq!(sp[1].retime.as_ref().map(|r| r.segments[0].speed), Some(2.0));
        let placed = q.timeline.placed_clips();
        let m = placed.iter().find(|c| c.lane == -1).expect("music on a lane below");
        assert_eq!(m.start, Time::from_secs(2));
        assert!(placed.iter().all(|c| c.lane != 1), "the title isn't media: skipped");
        assert_eq!(q.assets.iter().find(|a| a.name == "A 001.mov").map(|a| a.path.clone()), Some(PathBuf::from("D:/Footage/Day 1/A 001.mov")));
    }

    #[test]
    fn global_start_is_the_01_hour_timecode_label() {
        for (rate, frames) in [(FrameRate::FPS_23_976, 86_400.0), (FrameRate::FPS_29_97, 107_892.0), (FrameRate::FPS_25, 90_000.0)] {
            let mut p = Project::new("x");
            p.timeline.format.rate = rate;
            let doc: Value = serde_json::from_str(&export(&p)).unwrap();
            assert_eq!(doc["global_start_time"]["value"].as_f64(), Some(frames), "{rate:?}");
        }
    }

    /// Written by the opentimelineio library itself (0.18.1, see
    /// `scripts/validate_otio.py`): 29.97, Clip.2 with several media
    /// references, a transition, time warp, gap, nested stack, offline clip.
    #[test]
    fn reads_file_written_by_the_otio_library() {
        let q = import(include_str!("../tests/fixtures/otio_library_written.otio")).unwrap();
        let r = FrameRate::FPS_29_97;
        let f = |n: i64| Time::from_frames(n, r);
        assert_eq!((q.name.as_str(), q.timeline.format.rate), ("Library Cut", r));
        let placed = q.timeline.placed_clips();
        let v: Vec<_> = placed.iter().filter(|c| c.lane == 0 && !c.clip.is_gap()).collect();
        let got: Vec<_> = v.iter().map(|c| (c.clip.name.as_str(), c.start, c.clip.duration, c.clip.source_in)).collect();
        assert_eq!(
            got,
            [
                ("Interview A", f(0), f(150), f(300)),
                ("B-roll", f(150), f(90), Time::ZERO),
                // the nested stack and gap before it keep their length
                ("Missing", f(330), f(45), Time::ZERO),
                ("Interview A", f(375), f(120), f(900)),
            ]
        );
        // dissolve = in + out offsets, on the cut it straddles
        assert_eq!(v[0].clip.transition_out.as_ref().map(|t| t.duration), Some(f(15)));
        assert_eq!(v[1].clip.retime.as_ref().map(|rt| rt.segments[0].speed), Some(0.5));
        // the active ("high") media reference, not the proxy; one asset for both uses
        let interview: Vec<_> = q.assets.iter().filter(|a| a.name.starts_with("Interview A")).collect();
        assert_eq!(interview.len(), 1, "{:#?}", q.assets);
        assert_eq!(interview[0].path, PathBuf::from("C:/Media/Interview A.mov"));
        assert!(!q.assets.iter().any(|a| a.path.to_string_lossy().contains("Proxy")));
        assert_eq!(v[0].clip.asset_id(), v[3].clip.asset_id());
        // A1 music after its gap, on its own lane
        let m = placed.iter().find(|c| c.clip.name == "music.wav").expect("music");
        assert!(m.lane != 0 && m.clip.av.audio);
        assert_eq!((m.start, m.clip.duration), (f(60), f(240)));
    }

    #[test]
    fn reads_resolve_style_clip1_with_missing_media() {
        let src = r#"{"OTIO_SCHEMA":"Timeline.1","name":"Resolve TL","global_start_time":{"OTIO_SCHEMA":"RationalTime.1","rate":25.0,"value":90000.0},
          "tracks":{"OTIO_SCHEMA":"Stack.1","children":[{"OTIO_SCHEMA":"Track.1","kind":"Video","children":[
            {"OTIO_SCHEMA":"Gap.1","source_range":{"OTIO_SCHEMA":"TimeRange.1","start_time":{"OTIO_SCHEMA":"RationalTime.1","rate":25.0,"value":0.0},"duration":{"OTIO_SCHEMA":"RationalTime.1","rate":25.0,"value":25.0}}},
            {"OTIO_SCHEMA":"Clip.1","name":"C0001.MP4","source_range":{"OTIO_SCHEMA":"TimeRange.1","start_time":{"OTIO_SCHEMA":"RationalTime.1","rate":25.0,"value":50.0},"duration":{"OTIO_SCHEMA":"RationalTime.1","rate":25.0,"value":75.0}},
             "media_reference":{"OTIO_SCHEMA":"MissingReference.1","name":"C0001.MP4"}}]}]}}"#;
        let p = import(src).unwrap();
        assert_eq!(p.timeline.format.rate, FrameRate::FPS_25);
        let sp = &p.timeline.spine;
        assert_eq!(sp.len(), 2);
        assert!(sp[0].is_gap());
        assert_eq!((sp[1].source_in, sp[1].duration), (Time::from_secs(2), Time::from_secs(3)));
        assert_eq!(p.assets[0].path, PathBuf::from("C0001.MP4"), "offline media keeps its name for relinking");
    }
}
