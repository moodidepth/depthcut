use nle_core::{Clip, FrameRate, Project};
use nle_media::{ffmpeg_cli, probe};
use nle_render::export::{run_export, AudioExport, ExportPreset, ExportSettings};
use nle_render::GpuContext;
use std::collections::HashMap;
use std::sync::atomic::AtomicBool;

#[test]
fn headless_software_export_smoke_test() {
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

    let dir = std::env::temp_dir().join(format!("rcp-headless-test-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    let clip_path = dir.join("input.mp4");
    let status = ffmpeg_cli::command(&ffmpeg_cli::ffmpeg_path())
        .args(["-hide_banner", "-loglevel", "error", "-y"])
        .args(["-f", "lavfi", "-i", "testsrc2=size=640x360:rate=30:duration=2"])
        .args(["-f", "lavfi", "-i", "sine=frequency=440:sample_rate=48000:duration=2"])
        .args(["-c:v", "libx264", "-c:a", "aac", "-shortest"])
        .arg(&clip_path)
        .status()
        .expect("run ffmpeg");
    assert!(status.success());

    let mut project = Project::new("Headless Test");
    project.timeline.format.width = 640;
    project.timeline.format.height = 360;
    project.timeline.format.rate = FrameRate::FPS_30;

    let asset = probe::probe(&clip_path).expect("probe input");
    let asset_id = project.add_asset(asset);
    let asset = project.asset(asset_id).cloned().expect("asset registered");
    project.timeline.append(Clip::from_asset(&asset)).unwrap();
    assert_eq!(settings_frames(&project), 60, "2 s at 30 fps");

    let output_path = dir.join("output.mp4");
    let mut settings = ExportSettings::new(&output_path, ExportPreset::H264Software, &project);
    settings.width = 640;
    settings.height = 360;
    settings.frame_rate = FrameRate::FPS_30;
    settings.audio = AudioExport::Aac { kbps: 128 };

    let audio: HashMap<_, _> = project.assets.iter().filter_map(|a| nle_media::audio::decode::decode_audio(a).ok().map(|b| (a.id, std::sync::Arc::new(b)))).collect();

    let mut progress_frames = 0;
    run_export(&project, &settings, gpu, &audio, &AtomicBool::new(false), |p| {
        progress_frames = p.frame;
    })
    .expect("export failed");

    assert_eq!(progress_frames, 60);

    let out_probe = probe::probe(&output_path).expect("probe output");
    let v = out_probe.video.as_ref().expect("video track");
    assert_eq!(v.width, 640);
    assert_eq!(v.height, 360);
    assert!((out_probe.duration.as_secs_f64() - 2.0).abs() < 0.1);
    assert!(out_probe.audio.is_some());

    let _ = std::fs::remove_dir_all(&dir);
}

fn settings_frames(project: &Project) -> u64 {
    ExportSettings::new("x.mp4", ExportPreset::H264Software, project).total_frames()
}
