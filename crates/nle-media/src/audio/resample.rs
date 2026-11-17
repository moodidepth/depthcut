//! Conversion of any PCM to the editor's internal format: 48 kHz stereo
//! `f32`. Channel layouts are folded to stereo (ITU-R BS.775 downmix for
//! 5.1) and sample rates converted with a Kaiser-windowed sinc filter,
//! parallelised over the media worker pool.

use crate::frame::AudioBuffer;
use rayon::prelude::*;

pub const INTERNAL_RATE: u32 = 48_000;
const HALF_TAPS: i64 = 24;
const KAISER_BETA: f64 = 8.6;

/// Fold interleaved audio with `channels` channels down/up to stereo.
pub fn to_stereo(samples: &[f32], channels: u16) -> Vec<f32> {
    let ch = channels.max(1) as usize;
    if ch == 2 {
        return samples.to_vec();
    }
    let frames = samples.len() / ch;
    let mut out = Vec::with_capacity(frames * 2);
    const C: f32 = std::f32::consts::FRAC_1_SQRT_2;
    for f in samples.chunks_exact(ch) {
        let (l, r) = match ch {
            1 => (f[0], f[0]),
            // L R C LFE Ls Rs (FFmpeg 5.1 order)
            6 => (f[0] + C * f[2] + C * f[4], f[1] + C * f[2] + C * f[5]),
            _ => {
                // generic: odd channels left, even channels right
                let (mut l, mut r) = (0.0, 0.0);
                for (i, s) in f.iter().enumerate() {
                    if i % 2 == 0 {
                        l += s;
                    } else {
                        r += s;
                    }
                }
                let n = (ch as f32 / 2.0).max(1.0);
                (l / n, r / n)
            }
        };
        out.push(l);
        out.push(r);
    }
    out
}

fn bessel_i0(x: f64) -> f64 {
    let mut sum = 1.0;
    let mut term = 1.0;
    let q = x * x / 4.0;
    for k in 1..50 {
        term *= q / (k * k) as f64;
        sum += term;
        if term < 1e-12 * sum {
            break;
        }
    }
    sum
}

fn kernel(x: f64, cutoff: f64) -> f64 {
    let half = HALF_TAPS as f64;
    if x.abs() >= half {
        return 0.0;
    }
    let sinc = if x.abs() < 1e-9 {
        1.0
    } else {
        let a = std::f64::consts::PI * x * cutoff;
        a.sin() / a
    };
    let r = x / half;
    let w = bessel_i0(KAISER_BETA * (1.0 - r * r).max(0.0).sqrt()) / bessel_i0(KAISER_BETA);
    cutoff * sinc * w
}

/// Resample interleaved stereo from `from` Hz to `to` Hz.
pub fn resample_stereo(input: &[f32], from: u32, to: u32) -> Vec<f32> {
    if from == to || input.is_empty() {
        return input.to_vec();
    }
    let in_frames = input.len() / 2;
    let ratio = from as f64 / to as f64;
    let out_frames = ((in_frames as f64) / ratio).round() as usize;
    let cutoff = (to as f64 / from as f64).min(1.0) * 0.97;
    let mut out = vec![0.0f32; out_frames * 2];
    out.par_chunks_mut(2 * 1024).enumerate().for_each(|(chunk, dst)| {
        for (k, frame) in dst.chunks_exact_mut(2).enumerate() {
            let i = chunk * 1024 + k;
            let pos = i as f64 * ratio;
            let center = pos.floor() as i64;
            let (mut l, mut r) = (0.0f64, 0.0f64);
            let span = (HALF_TAPS as f64 / cutoff).ceil() as i64;
            for j in (center - span + 1)..=(center + span) {
                let w = kernel((pos - j as f64) * cutoff, 1.0) * cutoff;
                if j < 0 || j as usize >= in_frames {
                    continue;
                }
                let j = j as usize;
                l += input[2 * j] as f64 * w;
                r += input[2 * j + 1] as f64 * w;
            }
            frame[0] = l as f32;
            frame[1] = r as f32;
        }
    });
    out
}

/// Convert any buffer to 48 kHz interleaved stereo `f32`.
pub fn to_internal(buf: &AudioBuffer) -> AudioBuffer {
    let stereo = to_stereo(&buf.samples, buf.channels);
    let samples = resample_stereo(&stereo, buf.sample_rate.max(1), INTERNAL_RATE);
    AudioBuffer::new(INTERNAL_RATE, 2, samples)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sine(rate: u32, freq: f64, secs: f64) -> Vec<f32> {
        let n = (rate as f64 * secs) as usize;
        (0..n)
            .flat_map(|i| {
                let v = (2.0 * std::f64::consts::PI * freq * i as f64 / rate as f64).sin() as f32 * 0.5;
                [v, v]
            })
            .collect()
    }

    #[test]
    fn resample_44k1_to_48k_preserves_length_and_tone() {
        let input = sine(44_100, 1_000.0, 0.5);
        let out = resample_stereo(&input, 44_100, 48_000);
        assert_eq!(out.len() / 2, 24_000);
        // compare against an ideal 48 kHz sine away from the edges
        let ideal = sine(48_000, 1_000.0, 0.5);
        let err = (2_000..22_000).map(|i| (out[2 * i] - ideal[2 * i]).abs()).fold(0.0f32, f32::max);
        assert!(err < 0.01, "max error {err}");
    }

    #[test]
    fn downsample_removes_content_above_nyquist() {
        // 30 kHz tone at 96 kHz must vanish when converting to 48 kHz
        let input = sine(96_000, 30_000.0, 0.2);
        let out = resample_stereo(&input, 96_000, 48_000);
        let peak = out[2_000..out.len() - 2_000].iter().fold(0.0f32, |a, b| a.max(b.abs()));
        assert!(peak < 0.02, "aliasing peak {peak}");
    }

    #[test]
    fn downmix_51_and_mono() {
        assert_eq!(to_stereo(&[0.5, -0.5], 1), vec![0.5, 0.5, -0.5, -0.5]);
        let s = to_stereo(&[1.0, 0.0, 0.0, 0.0, 0.0, 0.0], 6);
        assert_eq!(s, vec![1.0, 0.0]);
    }
}
