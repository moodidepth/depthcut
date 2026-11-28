//! End-to-end export through the GPU renderer and FFmpeg encoders.
//! Runs when both a GPU adapter and `ffmpeg` are available; skipped otherwise.

use nle_core::{Clip, Project, Time, Transition, TransitionKind};
use nle_media::encoder_probe;
use nle_media::{ffmpeg_cli, probe};
use nle_render::export::{mastering_presets, run_export, working_presets, AudioExport, ExportPreset, RateControl};
use nle_render::{ExportSettings, GpuContext};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;

fn make_clip(dir: &Path, name: &str, pattern: &str) -> PathBuf {
    make_clip_from(dir, name, &format!("{pattern}=size=640x360:rate=30:duration=2"))
}

fn make_clip_from(dir: &Path, name: &str, lavfi: &str) -> PathBuf {
    let out = dir.join(name);
    let status = ffmpeg_cli::command(&ffmpeg_cli::ffmpeg_path())
        .args(["-hide_banner", "-loglevel", "error", "-y"])
        .args(["-f", "lavfi", "-i", lavfi])
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
            Ok(_) => {
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

/// Stream info of the first video stream as `key=value` pairs.
fn stream_info(path: &Path) -> HashMap<String, String> {
    let p = path.to_string_lossy();
    let out = ffmpeg_cli::run_capture(
        &ffmpeg_cli::ffprobe_path(),
        &["-v", "error", "-select_streams", "v:0", "-show_entries", "stream=codec_name,profile,pix_fmt", "-of", "default=nw=1", &p],
    )
    .expect("ffprobe");
    out.lines().filter_map(|l| l.split_once('=')).map(|(k, v)| (k.to_string(), v.trim().to_string())).collect()
}

/// First frame, downscaled to 64x36 RGB24 by FFmpeg.
fn first_frame_rgb(path: &Path) -> Vec<u8> {
    let out = ffmpeg_cli::command(&ffmpeg_cli::ffmpeg_path())
        .args(["-hide_banner", "-loglevel", "error", "-i"])
        .arg(path)
        .args(["-frames:v", "1", "-vf", "scale=64:36:flags=area", "-f", "rawvideo", "-pix_fmt", "rgb24", "-"])
        .output()
        .expect("decode");
    assert!(out.status.success(), "decode {}: {}", path.display(), String::from_utf8_lossy(&out.stderr));
    assert_eq!(out.stdout.len(), 64 * 36 * 3, "{}", path.display());
    out.stdout
}

fn mean_abs_diff(a: &[u8], b: &[u8]) -> f64 {
    a.iter().zip(b).map(|(x, y)| (*x as f64 - *y as f64).abs()).sum::<f64>() / a.len() as f64
}

/// ProRes / DNxHR / image sequences and 10-bit HEVC / AV1: every format must
/// encode, carry the promised pixel format / profile, and decode back to the
/// source colour. The source is a flat saturated colour so subsampling can't
/// hide a wrong matrix, range, swapped chroma or broken packing.
#[test]
fn export_mastering_and_ten_bit() {
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
    let dir = std::env::temp_dir().join(format!("rcp-master-test-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    let src = make_clip_from(&dir, "a.mp4", "color=c=0xc03070:size=640x360:rate=30:duration=2");
    let mut project = Project::new("Master Test");
    project.timeline.format.width = 640;
    project.timeline.format.height = 360;
    let a = project.add_asset(probe::probe(&src).expect("probe a"));
    let a = project.asset(a).cloned().unwrap();
    project.timeline.append(Clip::from_asset_range(&a, Time::ZERO, Time::from_millis(1000))).unwrap();
    let audio: HashMap<_, _> = project.assets.iter().filter_map(|x| nle_media::audio::decode::decode_audio(x).ok().map(|buf| (x.id, std::sync::Arc::new(buf)))).collect();
    let reference = first_frame_rgb(&src);

    let mut jobs: Vec<(ExportPreset, bool)> = mastering_presets(nle_media::hwaccel::probe_caps()).into_iter().map(|p| (p, true)).collect();
    assert!(jobs.len() >= 6, "ProRes + DNxHR at least: {jobs:?}");
    let probe = encoder_probe::probe_blocking();
    jobs.extend(working_presets(probe).into_iter().filter(|p| p.supports_ten_bit()).map(|p| (p, false)));

    let mut failures = Vec::new();
    for (preset, mastering) in jobs {
        let sub = dir.join(format!("{preset:?}"));
        let path = sub.join(format!("out.{}", preset.extension()));
        let mut s = ExportSettings::new(&path, preset, &project);
        if mastering {
            s.audio = AudioExport::Pcm;
            s.alpha = preset.supports_alpha();
        } else {
            s.rate = RateControl::Quality(preset.default_quality());
            s.audio = AudioExport::Aac { kbps: 192 };
            s.ten_bit = true;
        }
        let cancel = AtomicBool::new(false);
        if let Err(e) = run_export(&project, &s, gpu.clone(), &audio, &cancel, |_| {}) {
            failures.push(format!("{preset:?}: export failed: {e}"));
            continue;
        }
        let first = if preset.is_image_sequence() {
            let n = std::fs::read_dir(&sub).unwrap().filter(|e| e.as_ref().unwrap().path().extension().is_some_and(|x| x == preset.extension())).count();
            if n as u64 != s.total_frames() {
                failures.push(format!("{preset:?}: {n} frames, want {}", s.total_frames()));
            }
            assert!(sub.join("out.wav").exists(), "{preset:?}: sidecar wav");
            sub.join(format!("out_000000.{}", preset.extension()))
        } else {
            let out = probe::probe(&path).expect("probe export");
            assert!(out.audio.is_some(), "{preset:?}: audio track");
            path.clone()
        };
        let info = stream_info(&first);
        let pix = info.get("pix_fmt").cloned().unwrap_or_default();
        let profile = info.get("profile").cloned().unwrap_or_default();
        let want_pix: &[&str] = match preset {
            ExportPreset::ProRes422 | ExportPreset::ProRes422Hq | ExportPreset::DnxhrHqx => &["yuv422p10le"],
            ExportPreset::ProRes4444 => &["yuva444p10le", "yuva444p12le"],
            ExportPreset::DnxhrHq => &["yuv422p"],
            ExportPreset::Dnxhr444 => &["yuv444p10le"],
            ExportPreset::PngSequence => &["rgba64be"],
            ExportPreset::TiffSequence => &["rgba64le"],
            ExportPreset::ExrSequence => &["gbrapf32le", "gbrapf16le"],
            _ => &["yuv420p10le", "p010le"],
        };
        if !want_pix.contains(&pix.as_str()) {
            failures.push(format!("{preset:?}: pix_fmt {pix}, want {want_pix:?}"));
        }
        let want_profile = match preset {
            ExportPreset::ProRes422 => Some("Standard"),
            ExportPreset::ProRes422Hq => Some("HQ"),
            ExportPreset::ProRes4444 => Some("4444"),
            ExportPreset::DnxhrHq => Some("DNXHR HQ"),
            ExportPreset::DnxhrHqx => Some("DNXHR HQX"),
            ExportPreset::Dnxhr444 => Some("DNXHR 444"),
            p if p.supports_ten_bit() && p.codec() == nle_render::export::ExportCodec::Hevc => Some("Main 10"),
            _ => None,
        };
        if let Some(w) = want_profile.filter(|w| profile != *w) {
            failures.push(format!("{preset:?}: profile {profile:?}, want {w:?}"));
        }
        // EXR is linear light: FFmpeg shows it as-is, so compare only the gamma formats
        if preset != ExportPreset::ExrSequence {
            let got = first_frame_rgb(&first);
            let d = mean_abs_diff(&got, &reference);
            if d > 2.5 {
                failures.push(format!("{preset:?}: first frame differs from source by {d:.1}/255"));
            }
            eprintln!("{preset:?}: ok (pix_fmt {pix}, profile {profile:?}, diff {d:.2}/255)");
        } else {
            eprintln!("{preset:?}: ok (pix_fmt {pix})");
        }
    }
    assert!(failures.is_empty(), "{failures:#?}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// HDR mastering through real encoders: HEVC (x265 and any 10-bit hardware
/// HEVC) in PQ, ProRes in HLG and DNxHR in PQ. The files must be tagged
/// BT.2020 with the right transfer, and x265 must write the HDR10 mastering
/// display / content light level metadata.
#[test]
fn export_hdr_is_tagged_and_carries_hdr10_metadata() {
    use nle_render::HdrOutput;
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
    let dir = std::env::temp_dir().join(format!("rcp-hdr-test-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let src = make_clip_from(&dir, "a.mp4", "testsrc2=size=640x360:rate=30:duration=2");
    let mut project = Project::new("HDR Test");
    project.timeline.format.width = 640;
    project.timeline.format.height = 360;
    let a = project.add_asset(probe::probe(&src).expect("probe a"));
    let a = project.asset(a).cloned().unwrap();
    project.timeline.append(Clip::from_asset_range(&a, Time::ZERO, Time::from_millis(500))).unwrap();
    let pq = HdrOutput::Pq { peak_nits: 1000.0 };
    let mut jobs = vec![(ExportPreset::HevcSoftware, pq), (ExportPreset::ProRes422Hq, HdrOutput::Hlg), (ExportPreset::DnxhrHqx, pq)];
    let probe = encoder_probe::probe_blocking();
    jobs.extend(
        working_presets(probe)
            .into_iter()
            .filter(|p| p.supports_ten_bit() && p.codec() == nle_render::export::ExportCodec::Hevc && *p != ExportPreset::HevcSoftware)
            .map(|p| (p, pq)),
    );

    let mut failures = Vec::new();
    for (preset, hdr) in jobs {
        let path = dir.join(format!("{preset:?}.{}", preset.extension()));
        let mut s = ExportSettings::new(&path, preset, &project);
        s.audio = AudioExport::None;
        s.ten_bit = true;
        s.hdr = hdr;
        if let Err(e) = run_export(&project, &s, gpu.clone(), &HashMap::new(), &AtomicBool::new(false), |_| {}) {
            failures.push(format!("{preset:?}: export failed: {e}"));
            continue;
        }
        let out = probe::probe(&path).expect("probe export");
        let v = out.video.expect("video stream");
        let trc = if hdr == HdrOutput::Hlg { "arib-std-b67" } else { "smpte2084" };
        if v.color_primaries != "bt2020" || v.color_transfer != trc || v.color_space != "bt2020nc" {
            failures.push(format!("{preset:?}: tagged {} / {} / {}, want bt2020 / {trc} / bt2020nc", v.color_primaries, v.color_transfer, v.color_space));
        }
        if preset == ExportPreset::HevcSoftware {
            let p = path.to_string_lossy();
            let side = ffmpeg_cli::run_capture(
                &ffmpeg_cli::ffprobe_path(),
                &["-v", "error", "-select_streams", "v:0", "-read_intervals", "%+#1", "-show_frames", "-show_entries", "frame=side_data_list", &p],
            )
            .unwrap_or_default();
            if !side.contains("Mastering display metadata") || !side.contains("Content light level metadata") {
                failures.push(format!("{preset:?}: no HDR10 side data: {side}"));
            }
        }
        eprintln!("{preset:?}: {} / {} / {}", v.color_primaries, v.color_transfer, v.color_space);
    }
    assert!(failures.is_empty(), "{failures:#?}");
    let _ = std::fs::remove_dir_all(&dir);
}
