//! Automatic VOD kill-spike & highlight transient detector.
//!
//! Combines short-time broadband impact energy transients (gunshots/explosions)
//! with high-frequency kill-chime / hitmarker bursts (`2.0–8.0 kHz`) to locate
//! kills and cluster multi-kill streaks inside long gameplay recordings.

use crate::audio::eq::{BandKind, Biquad};
use nle_core::Time;

#[derive(Clone, Debug, PartialEq)]
pub struct KillPeak {
    pub time: Time,
    pub score: f32,
    pub is_multikill: bool,
    pub label: String,
}

/// Analyze mono or interleaved PCM audio samples for kill/action peaks.
/// `sensitivity` in `0.2..2.0` (default `1.0`).
pub fn analyze_kills_from_samples(samples: &[f32], channels: usize, rate: u32, sensitivity: f32) -> Vec<KillPeak> {
    let ch = channels.max(1);
    let mono: Vec<f32> = samples.chunks_exact(ch).map(|f| f.iter().sum::<f32>() / ch as f32).collect();
    if mono.len() < rate as usize / 10 || rate < 8_000 {
        return Vec::new();
    }
    let sr = rate as f32;
    let sens = sensitivity.clamp(0.2, 2.0);
    // High-frequency kill-chime / hitmarker band (2–8 kHz centered around ~4 kHz).
    let mut chime_bp = Biquad::new(BandKind::BandPass, sr, 4_200.0, 0.0, 0.9);
    // Low/mid broadband punch above 120 Hz.
    let mut punch_hp = Biquad::new(BandKind::HighPass, sr, 120.0, 0.0, 0.707);

    let win = ((rate as f64) * 0.020).round().max(16.0) as usize; // 20 ms analysis windows
    let blocks = mono.len() / win;
    if blocks < 8 {
        return Vec::new();
    }

    let mut scores = Vec::with_capacity(blocks);
    for b in 0..blocks {
        let slice = &mono[b * win..(b + 1) * win];
        let mut e_total = 0.0f32;
        let mut e_chime = 0.0f32;
        for &x in slice {
            let p = punch_hp.process(0, x);
            let c = chime_bp.process(0, x);
            e_total += p * p;
            e_chime += c * c;
        }
        let rms_total = (e_total / win as f32).sqrt();
        let rms_chime = (e_chime / win as f32).sqrt();
        scores.push(rms_total * 0.45 + rms_chime * 1.15);
    }

    // Adaptive baseline over a ±0.8s sliding median/mean window.
    let mean_score = scores.iter().sum::<f32>() / scores.len() as f32;
    let max_score = scores.iter().copied().fold(0.0f32, f32::max);
    if max_score < 0.015 {
        return Vec::new();
    }

    let mut norm: Vec<f32> = scores.iter().map(|s| s / max_score).collect();
    let baseline = (mean_score / max_score).max(0.05);
    let threshold = (baseline * 2.4 / sens).clamp(0.18, 0.75);
    let refractory = (0.55 / 0.020) as usize; // >= 550 ms between distinct kills
    let mut raw_peaks: Vec<(usize, f32)> = Vec::new();

    for i in 2..norm.len().saturating_sub(2) {
        let v = norm[i];
        if v < threshold {
            continue;
        }
        let prev = norm[i.saturating_sub(4)..i].iter().copied().fold(0.0f32, f32::max);
        let jump = v - prev * 0.4;
        if v >= norm[i - 1] && v >= norm[i + 1] && jump > threshold * 0.55 {
            if let Some(&(last_i, last_v)) = raw_peaks.last() {
                if i - last_i < refractory {
                    if v > last_v {
                        *raw_peaks.last_mut().expect("peak") = (i, v);
                    }
                    continue;
                }
            }
            raw_peaks.push((i, v));
        }
    }
    norm.clear();

    // Group rapid kills (within 3.8s of previous kill) into multi-kill streaks.
    let mut out = Vec::with_capacity(raw_peaks.len());
    let mut streak = 0u32;
    let mut prev_sec = -100.0f64;
    for (idx, score) in raw_peaks {
        let secs = idx as f64 * (win as f64 / rate as f64);
        if secs - prev_sec <= 3.8 {
            streak += 1;
        } else {
            streak = 1;
        }
        prev_sec = secs;
        let label = match streak {
            1 => "Kill".to_string(),
            2 => "Double Kill".to_string(),
            3 => "Triple Kill".to_string(),
            4 => "Quadra Kill".to_string(),
            _ => "ACE!".to_string(),
        };
        out.push(KillPeak { time: Time::from_secs_f64(secs), score, is_multikill: streak >= 2, label });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_synthetic_kill_chimes_and_multikill() {
        let rate = 48_000u32;
        let secs = 6.0f64;
        let mut samples = vec![0.0f32; (rate as f64 * secs) as usize];
        // Quiet ambiance
        for (i, s) in samples.iter_mut().enumerate() {
            *s = 0.01 * (2.0 * std::f32::consts::PI * 180.0 * i as f32 / rate as f32).sin();
        }
        // Two kill chimes (3.8 kHz + 5.1 kHz bursts) at 1.2s and 2.5s
        for &t_hit in &[1.2f64, 2.5f64] {
            let start = (t_hit * rate as f64) as usize;
            for i in 0..(rate as usize / 8) {
                let tt = i as f32 / rate as f32;
                let env = (-tt * 32.0).exp();
                let chime = (std::f32::consts::TAU * 3800.0 * tt).sin() * 0.55 + (std::f32::consts::TAU * 5100.0 * tt).sin() * 0.45;
                samples[start + i] += env * chime;
            }
        }
        let peaks = analyze_kills_from_samples(&samples, 1, rate, 1.0);
        assert_eq!(peaks.len(), 2, "expected 2 kills, got {peaks:?}");
        assert!((peaks[0].time.as_secs_f64() - 1.2).abs() < 0.1);
        assert!((peaks[1].time.as_secs_f64() - 2.5).abs() < 0.1);
        assert!(peaks[1].is_multikill);
        assert_eq!(peaks[1].label, "Double Kill");
    }
}
