//! Media probing (ffprobe JSON) and synthetic demo media.

use crate::ffmpeg_cli;
use crate::MediaError;
use nle_core::{AssetKind, AudioStreamInfo, AudioTrackInfo, FrameRate, MediaAsset, Time, VideoStreamInfo};
use serde_json::Value;
use std::path::Path;

pub const SYNTHETIC_SCHEME: &str = "synthetic://";

const IMAGE_EXTS: [&str; 7] = ["png", "jpg", "jpeg", "tif", "tiff", "bmp", "webp"];

/// Default duration given to still images.
pub const STILL_DURATION: Time = Time::from_secs(5);

pub fn is_synthetic(asset: &MediaAsset) -> bool {
    asset.kind == AssetKind::Synthetic || asset.path.to_string_lossy().starts_with(SYNTHETIC_SCHEME)
}

/// Probe a media file into a [`MediaAsset`].
pub fn probe(path: &Path) -> Result<MediaAsset, MediaError> {
    if !path.exists() {
        return Err(MediaError::Io(std::io::Error::new(std::io::ErrorKind::NotFound, path.display().to_string())));
    }
    if !ffmpeg_cli::available() {
        return Err(MediaError::FfmpegMissing);
    }
    let out = ffmpeg_cli::run_capture(&ffmpeg_cli::ffprobe_path(), &["-v", "error", "-print_format", "json", "-show_format", "-show_streams", &path.to_string_lossy()])
        .ok_or_else(|| MediaError::Decode(format!("ffprobe failed on {}", path.display())))?;
    let mut asset = parse_ffprobe_json(path, &out)?;
    if let Some(v) = &mut asset.video {
        if v.is_vfr || v.frame_rate.fps() >= 59.0 {
            if let Some(pts) = probe_packet_pts(path) {
                if !pts.is_empty() {
                    v.pts_table = pts;
                }
            }
        }
    }
    Ok(asset)
}

/// Build a CFR-locked PTS index table from sampled packet timestamps for VFR/high-FPS clips.
fn probe_packet_pts(path: &Path) -> Option<Vec<Time>> {
    let raw = ffmpeg_cli::run_capture(
        &ffmpeg_cli::ffprobe_path(),
        &["-v", "error", "-select_streams", "v:0", "-show_entries", "packet=pts_time", "-of", "csv=p=0", &path.to_string_lossy()],
    )?;
    let mut pts = Vec::with_capacity(4096);
    for line in raw.lines() {
        if let Ok(sec) = line.trim().parse::<f64>() {
            if sec.is_finite() && sec >= 0.0 {
                pts.push(Time::from_secs_f64(sec));
                if pts.len() >= 180_000 {
                    break;
                }
            }
        }
    }
    pts.sort();
    Some(pts)
}

/// Build a synthetic or fallback PTS-to-CFR index table when packet scan isn't needed.
pub fn build_pts_table(duration: Time, rate: FrameRate) -> Vec<Time> {
    let count = (duration.as_secs_f64() * rate.fps()).ceil().clamp(0.0, 180_000.0) as usize;
    let ft = rate.frame_duration();
    (0..count).map(|i| Time(ft.0.saturating_mul(i as i64))).collect()
}

fn parse_rate(s: &str) -> Option<FrameRate> {
    let (n, d) = s.split_once('/')?;
    let (n, d): (u32, u32) = (n.parse().ok()?, d.parse().ok()?);
    if n == 0 || d == 0 {
        return None;
    }
    let fps = n as f64 / d as f64;
    let preset = FrameRate::from_f64(fps);
    Some(if (preset.fps() - fps).abs() < 0.005 { preset } else { FrameRate::new(n, d) })
}

/// Parse `ffprobe -print_format json -show_format -show_streams` output.
pub fn parse_ffprobe_json(path: &Path, json: &str) -> Result<MediaAsset, MediaError> {
    let v: Value = serde_json::from_str(json).map_err(|e| MediaError::Decode(e.to_string()))?;
    let name = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| "Media".into());
    let ext = path.extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default();
    let fmt_dur = v["format"]["duration"].as_str().and_then(|s| s.parse::<f64>().ok());
    let streams = v["streams"].as_array().cloned().unwrap_or_default();

    let mut asset = MediaAsset::new(name, path, Time::ZERO);
    let mut dur = fmt_dur.unwrap_or(0.0);
    for s in &streams {
        let kind = s["codec_type"].as_str().unwrap_or("");
        let is_cover = s["disposition"]["attached_pic"].as_i64() == Some(1);
        if kind == "video" && asset.video.is_none() && !is_cover {
            let pix_fmt = s["pix_fmt"].as_str().unwrap_or("yuv420p").to_string();
            // WebM VP8/VP9 keep alpha in a side stream the stream tag announces
            let alpha_tag = ["alpha_mode", "ALPHA_MODE"].iter().any(|k| s["tags"][*k].as_str().is_some_and(|m| m.trim() == "1"));
            let has_alpha = nle_core::pix_fmt_has_alpha(&pix_fmt) || alpha_tag;
            let bit_depth = s["bits_per_raw_sample"].as_str().and_then(|b| b.parse::<u8>().ok()).unwrap_or(if pix_fmt.contains("10") || pix_fmt.contains("p010") {
                10
            } else if pix_fmt.contains("12") {
                12
            } else {
                8
            });
            let r_rate = s["r_frame_rate"].as_str().and_then(parse_rate);
            let avg_rate = s["avg_frame_rate"].as_str().and_then(parse_rate);
            let is_vfr = matches!((r_rate, avg_rate), (Some(r), Some(a)) if (r.fps() - a.fps()).abs() > 0.5);
            let rate = avg_rate.or(r_rate).unwrap_or_default();
            let v_dur = s["duration"].as_str().and_then(|d| d.parse::<f64>().ok()).unwrap_or(dur);
            if let Some(d) = s["duration"].as_str().and_then(|d| d.parse::<f64>().ok()) {
                dur = dur.max(d);
            }
            asset.field_order = nle_core::FieldOrder::from_ffprobe(s["field_order"].as_str().unwrap_or(""));
            let pts_table = if is_vfr { build_pts_table(Time::from_secs_f64(v_dur.max(0.0)), rate) } else { Vec::new() };
            asset.video = Some(VideoStreamInfo {
                width: s["width"].as_u64().unwrap_or(1920) as u32,
                height: s["height"].as_u64().unwrap_or(1080) as u32,
                frame_rate: rate,
                codec: s["codec_name"].as_str().unwrap_or("unknown").to_string(),
                pix_fmt,
                bit_depth,
                // untagged stays "unknown" so the YCbCr matrix can be guessed from the size
                color_space: s["color_space"].as_str().unwrap_or("unknown").to_string(),
                color_transfer: s["color_transfer"].as_str().unwrap_or("").to_string(),
                color_primaries: s["color_primaries"].as_str().unwrap_or("").to_string(),
                is_vfr,
                pts_table,
                has_alpha,
            });
        } else if kind == "audio" {
            asset.audio_streams.push(audio_track(asset.audio_streams.len(), s));
        }
    }
    asset.audio = asset.audio_streams.first().map(AudioTrackInfo::info);
    if asset.audio_streams.len() < 2 {
        // one track: nothing to choose, and the project file stays as before
        asset.audio_streams.clear();
    }
    asset.kind = if IMAGE_EXTS.contains(&ext.as_str()) {
        AssetKind::Image
    } else if asset.video.is_some() {
        AssetKind::Video
    } else if asset.audio.is_some() {
        AssetKind::Audio
    } else {
        return Err(MediaError::Unsupported(format!("no audio/video streams in {}", path.display())));
    };
    asset.duration = if asset.kind == AssetKind::Image || dur <= 0.0 { STILL_DURATION } else { Time::from_secs_f64(dur) };
    asset.start_timecode = start_timecode(&v, &streams, asset.video.as_ref().map(|v| v.frame_rate), asset.audio.as_ref().map(|a| a.sample_rate));
    Ok(asset)
}

/// Every audio track of `path` (an empty list for files with fewer than two),
/// for projects saved before track selection existed.
pub fn audio_tracks(path: &Path) -> Option<Vec<AudioTrackInfo>> {
    if !path.exists() || !ffmpeg_cli::available() {
        return None;
    }
    let out = ffmpeg_cli::run_capture(&ffmpeg_cli::ffprobe_path(), &["-v", "error", "-print_format", "json", "-select_streams", "a", "-show_streams", &path.to_string_lossy()])?;
    parse_audio_tracks(&out)
}

/// The audio tracks of ffprobe `-show_streams` JSON; empty unless there are two or more.
pub fn parse_audio_tracks(json: &str) -> Option<Vec<AudioTrackInfo>> {
    let v: Value = serde_json::from_str(json).ok()?;
    let mut tracks: Vec<AudioTrackInfo> = Vec::new();
    for s in v["streams"].as_array()?.iter().filter(|s| s["codec_type"].as_str() == Some("audio")) {
        tracks.push(audio_track(tracks.len(), s));
    }
    if tracks.len() < 2 {
        tracks.clear();
    }
    Some(tracks)
}

fn audio_track(index: usize, s: &Value) -> AudioTrackInfo {
    AudioTrackInfo {
        index,
        name: track_name(s),
        sample_rate: s["sample_rate"].as_str().and_then(|r| r.parse().ok()).unwrap_or(48_000),
        channels: s["channels"].as_u64().unwrap_or(2) as u16,
        codec: s["codec_name"].as_str().unwrap_or("unknown").to_string(),
    }
}

/// Title of an audio track: its `title` / `name` tag, else a non-generic
/// `handler_name` (muxers write "SoundHandler" on every track).
fn track_name(s: &Value) -> String {
    let tag =
        |key: &str| s["tags"].as_object().and_then(|t| t.iter().find(|(k, _)| k.eq_ignore_ascii_case(key))).and_then(|(_, v)| v.as_str()).map(str::trim).filter(|v| !v.is_empty());
    let generic = |h: &&str| {
        let h = h.to_ascii_lowercase();
        h.contains("handler") || h.contains("core media") || h == "sound"
    };
    tag("title").or_else(|| tag("name")).or_else(|| tag("handler_name").filter(|h| !generic(h))).unwrap_or_default().to_string()
}

/// The file's start timecode: a `timecode` tag (container first, then any
/// stream, e.g. a QuickTime `tmcd` track) counted at the video rate, else a
/// Broadcast WAV `time_reference` (samples since midnight).
fn start_timecode(v: &Value, streams: &[Value], rate: Option<FrameRate>, sample_rate: Option<u32>) -> Option<Time> {
    let tag = |obj: &Value, key: &str| obj["tags"].as_object().and_then(|t| t.iter().find(|(k, _)| k.eq_ignore_ascii_case(key))).and_then(|(_, v)| v.as_str()).map(str::to_owned);
    let tc = std::iter::once(&v["format"]).chain(streams.iter()).find_map(|o| tag(o, "timecode"));
    let t = if let Some(tc) = tc {
        let rate = rate.unwrap_or_default();
        if tc.contains(';') {
            nle_core::parse_timecode_df(&tc, rate)
        } else {
            nle_core::parse_timecode(&tc, rate)
        }
    } else {
        let samples = tag(&v["format"], "time_reference")?.trim().parse::<u64>().ok()?;
        Some(Time::from_secs_f64(samples as f64 / sample_rate.unwrap_or(48_000).max(1) as f64))
    };
    t.filter(|t| t.is_positive())
}

/// Built-in demo media (SMPTE-style bars with motion + tone) so the editor
/// is usable without any files or FFmpeg installed.
pub fn synthetic_asset(name: &str, seconds: i64, hue: f32) -> MediaAsset {
    let mut a = MediaAsset::new(name, format!("{SYNTHETIC_SCHEME}{name}?hue={hue}"), Time::from_secs(seconds));
    a.kind = AssetKind::Synthetic;
    a.video = Some(VideoStreamInfo {
        width: 1920,
        height: 1080,
        frame_rate: FrameRate::FPS_30,
        codec: "synthetic".into(),
        pix_fmt: "nv12".into(),
        bit_depth: 8,
        color_space: "bt709".into(),
        color_transfer: "bt709".into(),
        color_primaries: "bt709".into(),
        is_vfr: false,
        pts_table: Vec::new(),
        has_alpha: false,
    });
    a.audio = Some(AudioStreamInfo { sample_rate: 48_000, channels: 2, codec: "pcm_f32le".into() });
    a.tags = vec!["demo".into()];
    a
}

/// Synthetic audio-only asset (music bed).
pub fn synthetic_music(name: &str, seconds: i64) -> MediaAsset {
    let mut a = MediaAsset::new(name, format!("{SYNTHETIC_SCHEME}{name}?music"), Time::from_secs(seconds));
    a.kind = AssetKind::Synthetic;
    a.audio = Some(AudioStreamInfo { sample_rate: 44_100, channels: 2, codec: "pcm_f32le".into() });
    a.tags = vec!["demo".into(), "music".into()];
    a
}

/// Hue parameter encoded in a synthetic asset path.
pub fn synthetic_hue(asset: &MediaAsset) -> f32 {
    asset.path.to_string_lossy().split("hue=").nth(1).and_then(|h| h.parse().ok()).unwrap_or(0.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_start_timecode_tags() {
        let p = Path::new("a.mov");
        let video = |tags: &str| {
            format!(
                r#"{{"format":{{"duration":"10.0"{tags}}},"streams":[{{"codec_type":"video","width":1920,"height":1080,"pix_fmt":"yuv420p","avg_frame_rate":"30000/1001","r_frame_rate":"30000/1001"}},{{"codec_type":"data","codec_tag_string":"tmcd","tags":{{"timecode":"01:00:00;00"}}}}]}}"#
            )
        };
        // drop-frame tag on the tmcd track
        let a = parse_ffprobe_json(p, &video("")).unwrap();
        assert_eq!(a.start_timecode, Some(Time::from_frames(107_892, FrameRate::FPS_29_97)));
        // the container tag wins
        let a = parse_ffprobe_json(p, &video(r#","tags":{"TIMECODE":"00:00:10:00"}"#)).unwrap();
        assert_eq!(a.start_timecode, Some(Time::from_frames(300, FrameRate::FPS_29_97)));
        // BWF time reference: samples since midnight
        let wav =
            r#"{"format":{"duration":"5.0","tags":{"time_reference":"172800000"}},"streams":[{"codec_type":"audio","sample_rate":"48000","channels":2,"codec_name":"pcm_s24le"}]}"#;
        assert_eq!(parse_ffprobe_json(Path::new("a.wav"), wav).unwrap().start_timecode, Some(Time::from_secs(3600)));
        // no tags, or a zero timecode: none
        let plain = r#"{"format":{"duration":"5.0","tags":{"timecode":"00:00:00:00"}},"streams":[{"codec_type":"audio","sample_rate":"48000","channels":2}]}"#;
        assert_eq!(parse_ffprobe_json(Path::new("b.wav"), plain).unwrap().start_timecode, None);
    }

    #[test]
    fn parses_ffprobe_output() {
        let json = r#"{
          "streams": [
            {"codec_type":"video","codec_name":"hevc","width":3840,"height":2160,"pix_fmt":"yuv420p10le",
             "avg_frame_rate":"30000/1001","r_frame_rate":"30000/1001","color_space":"bt2020nc","duration":"12.5"},
            {"codec_type":"audio","codec_name":"aac","sample_rate":"48000","channels":2}
          ],
          "format": {"duration":"12.512"}
        }"#;
        let a = parse_ffprobe_json(Path::new("/x/clip.mov"), json).unwrap();
        let v = a.video.as_ref().unwrap();
        assert_eq!((v.width, v.height, v.bit_depth), (3840, 2160, 10));
        assert_eq!(v.frame_rate, FrameRate::FPS_29_97);
        assert_eq!(v.codec, "hevc");
        assert_eq!(a.audio.as_ref().unwrap().sample_rate, 48_000);
        assert_eq!(a.name, "clip");
        assert!((a.duration.as_secs_f64() - 12.512).abs() < 1e-6);
        // a single audio track is not listed as a choice
        assert!(a.audio_streams.is_empty());
        assert_eq!(a.audio_map(), "0:a:0");
    }

    #[test]
    fn flags_alpha_from_pixel_format_or_webm_alpha_mode() {
        let probe = |name: &str, stream: &str| {
            let json = format!(r#"{{"format":{{"duration":"2.0"}},"streams":[{{"codec_type":"video","width":640,"height":360,"avg_frame_rate":"30/1",{stream}}}]}}"#);
            parse_ffprobe_json(Path::new(name), &json).unwrap().video.unwrap()
        };
        let prores = probe("a.mov", r#""codec_name":"prores","pix_fmt":"yuva444p10le""#);
        assert!(prores.has_alpha && prores.alpha());
        assert_eq!(prores.bit_depth, 10);
        assert!(probe("b.mov", r#""codec_name":"qtrle","pix_fmt":"argb""#).has_alpha);
        assert!(probe("c.png", r#""codec_name":"png","pix_fmt":"rgba""#).has_alpha);
        // libvpx alpha: the stream decodes as yuv420p, the tag says alpha
        let webm = probe("d.webm", r#""codec_name":"vp9","pix_fmt":"yuv420p","tags":{"alpha_mode":"1","ENCODER":"Lavc libvpx-vp9"}"#);
        assert!(webm.has_alpha);
        assert!(!probe("e.webm", r#""codec_name":"vp9","pix_fmt":"yuv420p","tags":{"alpha_mode":"0"}"#).has_alpha);
        assert!(!probe("f.mp4", r#""codec_name":"h264","pix_fmt":"yuv420p""#).has_alpha);
    }

    #[test]
    fn lists_every_audio_track_of_a_multitrack_recording() {
        // Medal / OBS layout: full mix first, then the game, encoder, voice chat, mic
        let tags = |name: &str| format!(r#"{{"language":"und","handler_name":"SoundHandler","name":"{name}"}}"#);
        let audio = |name: &str, ch: u32, def: u8| {
            format!(r#"{{"codec_type":"audio","codec_name":"aac","sample_rate":"48000","channels":{ch},"disposition":{{"default":{def}}},"tags":{}}}"#, tags(name))
        };
        let json = format!(
            r#"{{"format":{{"duration":"30.0"}},"streams":[{{"codec_type":"video","codec_name":"h264","width":1920,"height":1080,"pix_fmt":"yuv420p","avg_frame_rate":"60/1","r_frame_rate":"60/1"}},{},{},{},{},{}]}}"#,
            audio("All Audio", 2, 1),
            audio("VALORANT-Win64-Shipping", 2, 0),
            audio("MedalEncoder", 2, 0),
            audio("Discord", 2, 0),
            audio("Microphone", 1, 0),
        );
        let mut a = parse_ffprobe_json(Path::new("/x/MedalTV.mp4"), &json).unwrap();
        let names: Vec<&str> = a.audio_streams.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, ["All Audio", "VALORANT-Win64-Shipping", "MedalEncoder", "Discord", "Microphone"]);
        assert_eq!(a.audio_streams.iter().map(|t| t.index).collect::<Vec<_>>(), [0, 1, 2, 3, 4]);
        // the first track plays until another is picked
        assert_eq!((a.selected_audio_stream, a.audio_map()), (0, "0:a:0".to_string()));
        assert!(a.select_audio_stream(1));
        assert_eq!(a.audio_map(), "0:a:1");
        assert!(a.select_audio_stream(4));
        assert_eq!(a.audio.as_ref().map(|i| i.channels), Some(1));
        // the stream-only probe used for older projects finds the same tracks
        assert_eq!(parse_audio_tracks(&json).unwrap(), a.audio_streams);
        assert_eq!(parse_audio_tracks(r#"{"streams":[{"codec_type":"audio","channels":2}]}"#), Some(vec![]));
        // a title tag wins, a generic handler name is no name
        let titled = r#"{"codec_type":"audio","channels":2,"tags":{"title":"Game","handler_name":"SoundHandler"}}"#;
        assert_eq!(track_name(&serde_json::from_str(titled).unwrap()), "Game");
        let plain = r#"{"codec_type":"audio","channels":2,"tags":{"handler_name":"SoundHandler"}}"#;
        assert_eq!(track_name(&serde_json::from_str(plain).unwrap()), "");
        let custom = r#"{"codec_type":"audio","channels":2,"tags":{"handler_name":"Boom Mic"}}"#;
        assert_eq!(track_name(&serde_json::from_str(custom).unwrap()), "Boom Mic");
    }
}
