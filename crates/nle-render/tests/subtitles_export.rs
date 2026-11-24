//! Captions leave an export as a soft subtitle track (MP4 mov_text, MKV
//! SubRip) and as a sidecar file, both timed from the start of the exported
//! range. Runs when a GPU adapter and `ffmpeg` are available; skipped
//! otherwise.

use nle_core::{CaptionSegment, Clip, Project, SubtitleFormat, Time};
use nle_media::{ffmpeg_cli, probe};
use nle_render::export::{run_export, AudioExport, ExportPreset, RateControl};
use nle_render::{ExportSettings, GpuContext};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;

struct Dir(PathBuf);

impl Drop for Dir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// The movie's subtitle streams as (codec, SubRip text).
fn subtitle_track(movie: &Path) -> (String, String) {
    let out = ffmpeg_cli::command(&ffmpeg_cli::ffprobe_path())
        .args(["-v", "error", "-select_streams", "s", "-show_entries", "stream=codec_name", "-of", "csv=p=0"])
        .arg(movie)
        .output()
        .unwrap();
    let codec = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let out = ffmpeg_cli::command(&ffmpeg_cli::ffmpeg_path()).args(["-v", "error", "-i"]).arg(movie).args(["-map", "0:s:0", "-f", "srt", "-"]).output().unwrap();
    (codec, String::from_utf8_lossy(&out.stdout).replace("\r\n", "\n"))
}

#[test]
fn captions_become_a_subtitle_track_and_sidecar() {
    if !ffmpeg_cli::available() {
        eprintln!("skipping: ffmpeg not found");
        return;
    }
    let gpu = match GpuContext::new_headless() {
        Ok(g) => g,
        Err(e) => {
            eprintln!("skipping: {e}");
            return;
        }
    };
    let dir = Dir(std::env::temp_dir().join(format!("rcp-subs-export-{}", std::process::id())));
    std::fs::create_dir_all(&dir.0).unwrap();
    let src = dir.0.join("src.mp4");
    let status = ffmpeg_cli::command(&ffmpeg_cli::ffmpeg_path())
        .args(["-hide_banner", "-loglevel", "error", "-y", "-f", "lavfi", "-i", "testsrc2=size=320x180:rate=30:duration=3"])
        .args(["-c:v", "libx264", "-preset", "ultrafast", "-pix_fmt", "yuv420p"])
        .arg(&src)
        .status()
        .unwrap();
    assert!(status.success());

    let mut project = Project::new("Subs");
    project.timeline.format.width = 320;
    project.timeline.format.height = 180;
    let id = project.add_asset(probe::probe(&src).unwrap());
    let asset = project.asset(id).cloned().unwrap();
    project.timeline.append(Clip::from_asset_range(&asset, Time::ZERO, Time::from_secs_f64(3.0))).unwrap();
    let caps = &mut project.timeline.captions;
    caps.segments.push(CaptionSegment::new(Time::from_millis(200), Time::from_millis(800), "too early"));
    caps.segments.push(CaptionSegment::new(Time::from_millis(1_200), Time::from_millis(1_900), "first line"));
    caps.segments.push(CaptionSegment::new(Time::from_millis(2_000), Time::from_millis(2_900), "second line"));
    caps.enabled = false; // not burned in

    for (name, codec) in [("movie.mp4", "mov_text"), ("movie.mkv", "subrip")] {
        let path = dir.0.join(name);
        let mut s = ExportSettings::new(&path, ExportPreset::H264Software, &project);
        s.rate = RateControl::Quality(30);
        s.audio = AudioExport::None;
        s.start = Time::from_secs_f64(1.0);
        s.end = Time::from_secs_f64(2.5);
        s.embed_subtitles = true;
        s.subtitle_sidecar = Some(SubtitleFormat::WebVtt);
        run_export(&project, &s, gpu.clone(), &HashMap::new(), &AtomicBool::new(false), |_| {}).unwrap();

        let (got, srt) = subtitle_track(&path);
        eprintln!("{name}: {got}\n{srt}");
        assert_eq!(got, codec);
        assert!(srt.contains("00:00:00,200 --> 00:00:00,900\nfirst line"), "{srt}");
        assert!(srt.contains("00:00:01,000 --> 00:00:01,500\nsecond line"), "clipped at the range end: {srt}");
        assert!(!srt.contains("too early"));

        let vtt = std::fs::read_to_string(path.with_extension("vtt")).unwrap();
        assert!(vtt.starts_with("WEBVTT\n\n00:00:00.200 --> 00:00:00.900\nfirst line"), "{vtt}");
    }
    let leftovers: Vec<_> = std::fs::read_dir(std::env::temp_dir())
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_string_lossy().starts_with(&format!("rcp-export-{}-", std::process::id())))
        .collect();
    assert!(leftovers.is_empty(), "temporary subtitle / audio files are removed");
}
