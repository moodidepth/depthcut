//! End-to-end export through the GPU renderer and FFmpeg encoders.
//! Runs when both a GPU adapter and `ffmpeg` are available; skipped otherwise.

use nle_core::{Clip, Project, Time, Transition, TransitionKind};
use nle_media::encoder_probe;
use nle_media::{ffmpeg_cli, probe};
use nle_render::export::{run_export, working_presets, AudioExport, RateControl};
use nle_render::{ExportSettings, GpuContext};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;

fn make_clip(dir: &Path, name: &str, pattern: &str) -> PathBuf {
    let out = dir.join(name);
    let status = ffmpeg_cli::command(&ffmpeg_cli::ffmpeg_path())
        .args(["-hide_banner", "-loglevel", "error", "-y"])
        .args(["-f", "lavfi", "-i", &format!("{pattern}=size=640x360:rate=30:duration=2")])
        .args(["-f", "lavfi", "-i", "sine=frequency=330:sample_rate=48000:duration=2"])
        .args(["-ac", "2", "-c:v", "libx264", "-preset", "ultrafast", "-pix_fmt", "yuv420p", "-c:a", "aac", "-shortest"])
        .arg(&out)
        .status()
        .expect("run ffmpeg");
    assert!(status.success());
    out
}

#[test]
fn export_every_working_encoder() {
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
    let dir = std::env::temp_dir().join(format!("rcp-export-test-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    let mut project = Project::new("Export Test");
    project.timeline.format.width = 640;
    project.timeline.format.height = 360;
    let a = project.add_asset(probe::probe(&make_clip(&dir, "a.mp4", "testsrc2")).expect("probe a"));
    let b = project.add_asset(probe::probe(&make_clip(&dir, "b.mp4", "smptebars")).expect("probe b"));
    let (a, b) = (project.asset(a).cloned().unwrap(), project.asset(b).cloned().unwrap());
    let mut c0 = Clip::from_asset_range(&a, Time::ZERO, Time::from_millis(1500));
    c0.transition_out = Some(Transition::new(TransitionKind::CrossDissolve, Time::from_millis(500)));
    project.timeline.append(c0).unwrap();
    project.timeline.append(Clip::from_asset_range(&b, Time::ZERO, Time::from_millis(1500))).unwrap();

    let audio: HashMap<_, _> = project.assets.iter().filter_map(|x| nle_media::audio::decode::decode_audio(x).ok().map(|buf| (x.id, std::sync::Arc::new(buf)))).collect();
    assert_eq!(audio.len(), 2, "audio decoded for both clips");

    // Only encoders that passed the hardware probe: each one must then
    // export for real, so probe and export can never disagree.
    let probe = encoder_probe::probe_blocking();
    let presets = working_presets(probe);
    eprintln!("{}: working presets: {:?}", probe.backend.label(), presets.iter().map(|p| p.label()).collect::<Vec<_>>());
    assert!(!presets.is_empty());
    let mut ok = Vec::new();
    for preset in presets {
        let path = dir.join(format!("out-{}.mp4", preset.encoder()));
        let mut s = ExportSettings::new(&path, preset, &project);
        s.rate = RateControl::Quality(preset.default_quality());
        s.audio = AudioExport::Aac { kbps: 192 };
        let cancel = AtomicBool::new(false);
        let mut frames = 0;
        match run_export(&project, &s, gpu.clone(), &audio, &cancel, |p| frames = p.frame) {
            Ok(()) => {
                let out = probe::probe(&path).expect("probe export");
                let v = out.video.as_ref().expect("exported video");
                assert_eq!((v.width, v.height), (640, 360));
                assert!((out.duration.as_secs_f64() - 3.0).abs() < 0.2, "{}: duration {}", preset.encoder(), out.duration.as_secs_f64());
                assert!(out.audio.is_some(), "{}: audio track", preset.encoder());
                assert_eq!(frames, s.total_frames());
                eprintln!("{}: ok ({} frames, codec {})", preset.encoder(), frames, v.codec);
                ok.push(preset);
            }
            Err(e) => panic!("{} export failed: {e}", preset.encoder()),
        }
    }
    assert!(!ok.is_empty(), "at least one encoder exported successfully");
    let _ = std::fs::remove_dir_all(&dir);
}
