//! Audio sync on real encoded files: a "recorder" WAV at 48 kHz and a
//! "camera" AAC at 44.1 kHz that picked up part of the same sound at a third
//! of the level over its own noise (the decoder brings both to one rate),
//! decoded, enveloped and matched the way the app does it. Skips itself
//! without FFmpeg.

use nle_core::sync;
use nle_media::{audio::decode::decode_audio, ffmpeg_cli, probe, Waveform};
use std::path::{Path, PathBuf};

struct Dir(PathBuf);

impl Drop for Dir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn ffmpeg(args: &[&str], out: &Path) {
    let status = ffmpeg_cli::command(&ffmpeg_cli::ffmpeg_path()).args(["-hide_banner", "-loglevel", "error", "-y"]).args(args).arg(out).status().unwrap();
    assert!(status.success(), "could not create {}", out.display());
}

fn envelope(path: &Path) -> (Vec<f32>, f64) {
    let asset = probe::probe(path).unwrap();
    let buf = decode_audio(&asset).unwrap();
    let w = Waveform::compute(&buf, 200);
    let rate = w.buckets_per_second();
    (w.rms, rate)
}

#[test]
fn camera_audio_lines_up_with_the_recorder() {
    if !ffmpeg_cli::available() {
        eprintln!("skipping: ffmpeg not found");
        return;
    }
    let dir = Dir(std::env::temp_dir().join(format!("rcp-sync-real-{}", std::process::id())));
    std::fs::create_dir_all(&dir.0).unwrap();
    let recorder = dir.0.join("recorder.wav");
    let camera = dir.0.join("camera.m4a");
    // irregular bursts of noise: incommensurate sines gate the level
    let src = "aevalsrc='0.5*gt(sin(2.3*t)+sin(3.7*t)+sin(0.93*t)+sin(5.11*t),1.1)*(2*random(0)-1)':s=48000:d=90";
    ffmpeg(&["-f", "lavfi", "-i", src, "-c:a", "pcm_s16le"], &recorder);
    let graph = "[0:a]atrim=17.3:57.3,asetpts=PTS-STARTPTS,volume=0.33[a];anoisesrc=d=40:c=white:r=48000:a=0.01[n];[a][n]amix=inputs=2:normalize=0,aresample=44100[out]";
    ffmpeg(&["-i", recorder.to_str().unwrap(), "-filter_complex", graph, "-map", "[out]", "-c:a", "aac", "-b:a", "128k"], &camera);

    let (r, rate) = envelope(&recorder);
    let (o, o_rate) = envelope(&camera);
    let o = sync::resample_envelope(&o, o_rate, rate);
    let m = sync::audio_offset(&r, &o, rate, None).unwrap();
    eprintln!("offset {:.4} s, confidence {:.2}", m.offset, m.confidence);
    assert!((m.offset - 17.3).abs() < 0.03, "{m:?}");
    assert!(m.reliable(), "{m:?}");
}
