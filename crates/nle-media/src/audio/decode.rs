//! Whole-file audio decode into the internal 48 kHz stereo `f32` format.

use super::resample;
use crate::ffmpeg_cli;
use crate::frame::AudioBuffer;
use crate::probe;
use crate::MediaError;
use nle_core::MediaAsset;

/// Decode all audio of `asset` and convert to 48 kHz stereo.
pub fn decode_audio(asset: &MediaAsset) -> Result<AudioBuffer, MediaError> {
    let info = asset.audio.as_ref().ok_or_else(|| MediaError::Unsupported(format!("{} has no audio", asset.name)))?;
    if probe::is_synthetic(asset) {
        return Ok(resample::to_internal(&synthetic_audio(asset, info.sample_rate)));
    }
    if !ffmpeg_cli::available() {
        return Err(MediaError::FfmpegMissing);
    }
    // the selected track of a multi-track recording (game audio without voice chat)
    let mut cmd = ffmpeg_cli::command(&ffmpeg_cli::ffmpeg_path());
    cmd.args(["-hide_banner", "-loglevel", "error", "-nostdin", "-i"]).arg(&asset.path).args(["-map", &asset.audio_map(), "-vn"]);
    let out = ffmpeg_cli::output_confined(cmd.args(["-f", "f32le", "-acodec", "pcm_f32le", "pipe:1"])).map_err(MediaError::Io)?;
    if !out.status.success() && out.stdout.is_empty() {
        return Err(MediaError::Decode(format!("audio decode failed for {}", asset.path.display())));
    }
    let samples: Vec<f32> = out.stdout.as_chunks::<4>().0.iter().map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]])).collect();
    let native = AudioBuffer::new(info.sample_rate, info.channels, samples);
    Ok(resample::to_internal(&native))
}

/// Procedural audio for demo assets: "dialogue" bursts for clips with
/// video, a soft chord pad for music beds.
pub fn synthetic_audio(asset: &MediaAsset, rate: u32) -> AudioBuffer {
    let secs = asset.duration.as_secs_f64();
    let n = (secs * rate as f64) as usize;
    let mut out = Vec::with_capacity(n * 2);
    let music = asset.path.to_string_lossy().contains("music");
    let hue = probe::synthetic_hue(asset) as f64;
    let tau = std::f64::consts::TAU;
    for i in 0..n {
        let t = i as f64 / rate as f64;
        let v = if music {
            let chord = [220.0, 277.18, 329.63, 440.0];
            let step = ((t / 2.0) as usize) % 4;
            let shift = [1.0, 0.89, 0.75, 0.84][step];
            chord.iter().map(|f| (tau * f * shift * t).sin()).sum::<f64>() * 0.06
        } else {
            // syllable-like amplitude modulation, speech on for 2.5 s / off 1.5 s
            let on = (t % 4.0) < 2.5;
            let env = if on { (0.5 + 0.5 * (tau * 4.0 * t).sin()).powi(2) } else { 0.0 };
            let f0 = 140.0 + 40.0 * hue;
            ((tau * f0 * t).sin() + 0.5 * (tau * 2.0 * f0 * t).sin() + 0.25 * (tau * 3.0 * f0 * t).sin()) * env * 0.25
        };
        out.push(v as f32);
        out.push(v as f32);
    }
    AudioBuffer::new(rate, 2, out)
}
