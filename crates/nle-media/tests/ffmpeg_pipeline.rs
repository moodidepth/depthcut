//! Real-media pipeline tests. They run when `ffmpeg`/`ffprobe` are available
//! (PATH or RCP_FFMPEG / RCP_FFPROBE) and are skipped otherwise.

use nle_core::Time;
use nle_media::decode::{self, DecodeOptions};
use nle_media::{ffmpeg_cli, probe, proxy, thumbnails, PixelFormat};
use std::path::{Path, PathBuf};

fn have_ffmpeg() -> bool {
    let ok = ffmpeg_cli::available();
    if !ok {
        eprintln!("skipping: ffmpeg/ffprobe not found");
    }
    ok
}

fn workdir(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("rcp-test-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).expect("temp dir");
    d
}

/// 2 s, 640x360 @ 30 fps H.264 with a 440 Hz stereo tone (software encode).
fn make_clip(dir: &Path) -> PathBuf {
    let out = dir.join("clip.mp4");
    let status = ffmpeg_cli::command(&ffmpeg_cli::ffmpeg_path())
        .args(["-hide_banner", "-loglevel", "error", "-y"])
        .args(["-f", "lavfi", "-i", "testsrc2=size=640x360:rate=30:duration=2"])
        .args(["-f", "lavfi", "-i", "sine=frequency=440:sample_rate=44100:duration=2"])
        .args(["-ac", "2", "-c:v", "libx264", "-preset", "ultrafast", "-pix_fmt", "yuv420p", "-c:a", "aac", "-shortest"])
        .arg(&out)
        .status()
        .expect("run ffmpeg");
    assert!(status.success(), "ffmpeg failed to generate the test clip");
    out
}

#[test]
fn probe_decode_audio_thumbnails_proxy() {
    if !have_ffmpeg() {
        return;
    }
    let dir = workdir("media");
    let path = make_clip(&dir);

    // probe
    let asset = probe::probe(&path).expect("probe");
    let v = asset.video.as_ref().expect("video stream");
    assert_eq!((v.width, v.height), (640, 360));
    assert_eq!(v.frame_rate.fps().round() as i64, 30);
    assert!(asset.audio.is_some(), "audio stream");
    let secs = asset.duration.as_secs_f64();
    assert!((secs - 2.0).abs() < 0.15, "duration {secs}");

    // video decode (QSV → D3D11VA/VA-API → software plan)
    let mut dec = decode::open(&asset, DecodeOptions::default()).expect("open decoder");
    eprintln!("decoder backend: {:?}", dec.backend());
    let f = dec.frame_at(Time::from_secs(1)).expect("decode frame at 1 s");
    assert_eq!((f.width, f.height), (640, 360));
    assert_eq!(f.format, PixelFormat::Nv12);
    assert_eq!(f.data.len(), f.format.frame_size(640, 360));
    let luma_var = {
        let y = &f.data[..640 * 360];
        let mean = y.iter().map(|v| *v as f64).sum::<f64>() / y.len() as f64;
        y.iter().map(|v| (*v as f64 - mean).powi(2)).sum::<f64>() / y.len() as f64
    };
    assert!(luma_var > 100.0, "decoded test pattern has detail (variance {luma_var})");
    // sequential frames advance
    let f2 = dec.frame_at(Time::from_frames(31, v.frame_rate)).expect("next frame");
    assert_ne!(f.data, f2.data, "testsrc2 changes every frame");

    // downscaled preview decode
    let mut small = decode::open(&asset, DecodeOptions { use_proxy: false, max_height: Some(180) }).expect("open scaled");
    let s = small.frame_at(Time::ZERO).expect("scaled frame");
    assert_eq!((s.width, s.height), (320, 180));

    // audio → 48 kHz stereo f32
    let buf = nle_media::audio::decode::decode_audio(&asset).expect("decode audio");
    assert_eq!((buf.sample_rate, buf.channels), (48_000, 2));
    let asecs = buf.frames() as f64 / 48_000.0;
    assert!((asecs - 2.0).abs() < 0.15, "audio length {asecs}");
    let rms = (buf.samples.iter().map(|s| s * s).sum::<f32>() / buf.samples.len() as f32).sqrt();
    assert!(rms > 0.05, "tone survives resampling (rms {rms})");

    // thumbnails
    let strip = thumbnails::generate(&asset, 4, 160, 90).expect("filmstrip");
    assert_eq!(strip.frames.len(), 4);
    assert!(strip.frames.iter().all(|t| t.width == 160 && t.height == 90 && t.rgba.len() == 160 * 90 * 4));

    // proxy transcode (QSV encoder first, software fallback)
    let proxy_dir = dir.join("proxies");
    let mut last = 0.0f32;
    let mut encoder = String::new();
    let out = proxy::transcode(&asset, &proxy_dir, |p, e| {
        last = p;
        encoder = e.to_string();
    })
    .expect("proxy transcode");
    assert!(out.exists(), "proxy file written");
    let pa = probe::probe(&out).expect("probe proxy");
    let pv = pa.video.as_ref().expect("proxy video");
    assert_eq!((pv.width, pv.height), nle_media::proxy::proxy_size(640, 360));
    assert!((last - 1.0).abs() < 1e-6, "progress reaches 100 %");
    eprintln!("proxy: {} via {encoder}", out.display());
    if encoder_works("h264_qsv") {
        assert_eq!(encoder, "h264_qsv", "proxies use Quick Sync when it works");
    }

    let _ = std::fs::remove_dir_all(&dir);
}

/// Can FFmpeg encode with `enc` on this machine?
fn encoder_works(enc: &str) -> bool {
    let out = std::env::temp_dir().join(format!("rcp-enc-{enc}-{}.mp4", std::process::id()));
    let ok = ffmpeg_cli::command(&ffmpeg_cli::ffmpeg_path())
        .args(["-hide_banner", "-loglevel", "error", "-y", "-f", "lavfi", "-i", "testsrc2=size=320x180:rate=30:duration=0.3", "-vf", "format=nv12", "-c:v", enc])
        .arg(&out)
        .status()
        .is_ok_and(|s| s.success());
    let _ = std::fs::remove_file(&out);
    ok
}

/// Hardware decode of the codecs the Arc accelerates: HEVC Main10 (P010) and
/// AV1. The sources are produced with the Arc's own Quick Sync encoders, so
/// this test only runs where those work.
#[test]
fn quick_sync_decodes_hevc_10bit_and_av1() {
    if !have_ffmpeg() {
        return;
    }
    let dir = workdir("hwdec");
    let cases = [
        ("hevc_qsv", "p010le", vec!["-profile:v", "main10"], "hevc10.mp4", 10u8, PixelFormat::P010),
        ("av1_qsv", "nv12", vec![], "av1.mp4", 8u8, PixelFormat::Nv12),
    ];
    let mut ran = 0;
    for (enc, pix, extra, name, depth, fmt) in cases {
        if !encoder_works(enc) {
            eprintln!("skipping {enc}: encoder not usable here");
            continue;
        }
        let path = dir.join(name);
        let status = ffmpeg_cli::command(&ffmpeg_cli::ffmpeg_path())
            .args(["-hide_banner", "-loglevel", "error", "-y", "-f", "lavfi", "-i", "testsrc2=size=1280x720:rate=30:duration=1"])
            .args(["-vf", &format!("format={pix}"), "-c:v", enc])
            .args(&extra)
            .arg(&path)
            .status()
            .expect("run ffmpeg");
        assert!(status.success(), "{enc}: source encode");
        let asset = probe::probe(&path).expect("probe");
        let v = asset.video.as_ref().expect("video");
        assert_eq!(v.bit_depth, depth, "{enc}: probed bit depth");
        let mut dec = decode::open(&asset, DecodeOptions::default()).expect("open decoder");
        let f = dec.frame_at(Time::from_millis(500)).expect("decode");
        eprintln!("{name}: backend {:?}, {:?} {}x{}", dec.backend(), f.format, f.width, f.height);
        // CLI decoder: Quick Sync. In-process libav decoder (feature `ffmpeg`):
        // FFmpeg's generic hwaccel (D3D11VA / VA-API) on the same media engine.
        let expected = if cfg!(feature = "ffmpeg") {
            if cfg!(windows) {
                nle_media::HwBackend::D3d11va
            } else {
                nle_media::HwBackend::Vaapi
            }
        } else {
            nle_media::HwBackend::Qsv
        };
        assert_eq!(dec.backend(), expected, "{name} decodes in hardware");
        assert_eq!(f.format, fmt);
        assert_eq!((f.width, f.height), (1280, 720));
        assert_eq!(f.data.len(), fmt.frame_size(1280, 720));
        if fmt == PixelFormat::P010 {
            // P010 keeps samples in the high 10 bits: low 6 bits are zero
            let y: Vec<u16> = f.data[..1280 * 720 * 2].chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
            assert!(y.iter().all(|v| v & 0x3f == 0), "P010 layout");
            assert!(y.iter().any(|v| *v > 0x8000), "bright pixels present");
        }
        ran += 1;
    }
    eprintln!("hardware decode cases run: {ran}");
    let _ = std::fs::remove_dir_all(&dir);
}
