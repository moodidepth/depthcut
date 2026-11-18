//! Media probing (ffprobe JSON) and synthetic demo media.

use crate::ffmpeg_cli;
use crate::MediaError;
use nle_core::{AssetKind, AudioStreamInfo, FrameRate, MediaAsset, Time, VideoStreamInfo};
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
            });
        } else if kind == "audio" && asset.audio.is_none() {
            asset.audio = Some(AudioStreamInfo {
                sample_rate: s["sample_rate"].as_str().and_then(|r| r.parse().ok()).unwrap_or(48_000),
                channels: s["channels"].as_u64().unwrap_or(2) as u16,
                codec: s["codec_name"].as_str().unwrap_or("unknown").to_string(),
            });
        }
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
    Ok(asset)
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
        let v = a.video.unwrap();
        assert_eq!((v.width, v.height, v.bit_depth), (3840, 2160, 10));
        assert_eq!(v.frame_rate, FrameRate::FPS_29_97);
        assert_eq!(v.codec, "hevc");
        assert_eq!(a.audio.unwrap().sample_rate, 48_000);
        assert_eq!(a.name, "clip");
        assert!((a.duration.as_secs_f64() - 12.512).abs() < 1e-6);
    }
}
