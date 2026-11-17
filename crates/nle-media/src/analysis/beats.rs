//! Music beat detection.
//!
//! 1. Onset strength: log-compressed magnitude spectrum (2048-point Hann
//!    FFT, 512-sample hop) → half-wave rectified spectral flux.
//! 2. Onset peaks: local maxima above an adaptive (moving-median + delta)
//!    threshold, at least 100 ms apart.
//! 3. Tempo: autocorrelation of the onset envelope between 60 and 200 BPM,
//!    with a mild preference for 120 BPM.
//! 4. Beat grid: the phase that collects the most onset energy; each grid
//!    point snaps to the strongest onset peak within ±12 % of a period, so
//!    markers land on every beat even where the kick is quiet.

use crate::audio::eq::{BandKind, Biquad};
use rustfft::num_complex::Complex32;
use rustfft::FftPlanner;

pub const FFT_SIZE: usize = 2048;
pub const HOP: usize = 512;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BeatBand {
    SubBassKick,
    SnareTransient,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MultiBandBeat {
    pub secs: f64,
    pub band: BeatBand,
    pub strength: f32,
}

/// Detect Sub-Bass/Kick (`20–120 Hz`) and Snare/Transient (`1.5–6 kHz`) beat
/// markers from audio samples using spectral flux within each target band.
pub fn analyze_multiband(samples: &[f32], channels: usize, rate: u32, sensitivity: f32) -> Vec<MultiBandBeat> {
    let ch = channels.max(1);
    let mono: Vec<f32> = samples.chunks_exact(ch).map(|f| f.iter().sum::<f32>() / ch as f32).collect();
    if mono.len() < FFT_SIZE || rate < 8_000 {
        return Vec::new();
    }
    let sens = sensitivity.clamp(0.1, 2.0);
    let sr = rate as f32;
    let mut sub_lp = Biquad::new(BandKind::LowPass, sr, 120.0, 0.0, 0.707);
    let mut sub_hp = Biquad::new(BandKind::HighPass, sr, 20.0, 0.0, 0.707);
    let mut snare_bp = Biquad::new(BandKind::BandPass, sr, 3_200.0, 0.0, 1.1);

    let sub_sig: Vec<f32> = mono.iter().map(|&x| sub_lp.process(0, sub_hp.process(0, x))).collect();
    let snare_sig: Vec<f32> = mono.iter().map(|&x| snare_bp.process(0, x)).collect();

    let hop_secs = HOP as f64 / rate as f64;
    let frame_time = |f: usize| f as f64 * hop_secs + (FFT_SIZE as f64 * 0.5 - HOP as f64 * 0.5) / rate as f64;

    let mut out = Vec::new();
    for (band, sig, min_thr) in [
        (BeatBand::SubBassKick, &sub_sig, 0.14 / sens),
        (BeatBand::SnareTransient, &snare_sig, 0.16 / sens),
    ] {
        let env = band_flux_envelope(sig);
        let peaks = pick_peaks_with_delta(&env, hop_secs, min_thr);
        for p in peaks {
            out.push(MultiBandBeat {
                secs: frame_time(p).max(0.0),
                band,
                strength: env[p],
            });
        }
    }
    out.sort_by(|a, b| a.secs.total_cmp(&b.secs));
    out
}

fn band_flux_envelope(mono: &[f32]) -> Vec<f32> {
    if mono.len() < FFT_SIZE {
        return Vec::new();
    }
    let mut planner = FftPlanner::<f32>::new();
    let fft = planner.plan_fft_forward(FFT_SIZE);
    let window: Vec<f32> = (0..FFT_SIZE).map(|i| 0.5 - 0.5 * (std::f32::consts::TAU * i as f32 / FFT_SIZE as f32).cos()).collect();
    let frames = (mono.len() - FFT_SIZE) / HOP + 1;
    let bins = FFT_SIZE / 2;
    let mut prev = vec![0.0f32; bins];
    let mut buf = vec![Complex32::new(0.0, 0.0); FFT_SIZE];
    let mut env = Vec::with_capacity(frames);
    for f in 0..frames {
        let s = &mono[f * HOP..f * HOP + FFT_SIZE];
        for (b, (x, w)) in buf.iter_mut().zip(s.iter().zip(&window)) {
            *b = Complex32::new(x * w, 0.0);
        }
        fft.process(&mut buf);
        let mut flux = 0.0;
        for (k, p) in prev.iter_mut().enumerate() {
            let mag = (1.0 + 100.0 * buf[k].norm()).ln();
            let d = mag - *p;
            if d > 0.0 {
                flux += d;
            }
            *p = mag;
        }
        env.push(if f == 0 { 0.0 } else { flux });
    }
    let max = env.iter().copied().fold(0.0f32, f32::max);
    if max > 0.0 {
        for v in &mut env {
            *v /= max;
        }
    }
    env
}

fn pick_peaks_with_delta(env: &[f32], hop_secs: f64, delta: f32) -> Vec<usize> {
    let w = 8usize;
    let min_gap = (0.14 / hop_secs).ceil() as usize;
    let mut peaks: Vec<usize> = Vec::new();
    for i in 1..env.len().saturating_sub(1) {
        let lo = i.saturating_sub(w);
        let hi = (i + w + 1).min(env.len());
        let mut neigh: Vec<f32> = env[lo..hi].to_vec();
        let thr = median(&mut neigh) + delta;
        let is_max = env[lo..hi].iter().all(|v| *v <= env[i]);
        if env[i] > thr && is_max {
            match peaks.last() {
                Some(&p) if i - p < min_gap => {
                    if env[i] > env[p] {
                        *peaks.last_mut().expect("peak") = i;
                    }
                }
                _ => peaks.push(i),
            }
        }
    }
    peaks
}

#[derive(Clone, Debug, PartialEq)]
pub struct BeatAnalysis {
    /// Seconds of each detected onset peak.
    pub onsets: Vec<f64>,
    /// Estimated tempo.
    pub bpm: f64,
    /// Seconds of every beat.
    pub beats: Vec<f64>,
}

/// Onset-strength envelope (one value per hop) of mono audio.
pub fn onset_envelope(mono: &[f32], rate: u32) -> Vec<f32> {
    let _ = rate;
    if mono.len() < FFT_SIZE {
        return Vec::new();
    }
    let mut planner = FftPlanner::<f32>::new();
    let fft = planner.plan_fft_forward(FFT_SIZE);
    let window: Vec<f32> = (0..FFT_SIZE).map(|i| 0.5 - 0.5 * (std::f32::consts::TAU * i as f32 / FFT_SIZE as f32).cos()).collect();
    let frames = (mono.len() - FFT_SIZE) / HOP + 1;
    let bins = FFT_SIZE / 2;
    let mut prev = vec![0.0f32; bins];
    let mut buf = vec![Complex32::new(0.0, 0.0); FFT_SIZE];
    let mut env = Vec::with_capacity(frames);
    for f in 0..frames {
        let s = &mono[f * HOP..f * HOP + FFT_SIZE];
        for (b, (x, w)) in buf.iter_mut().zip(s.iter().zip(&window)) {
            *b = Complex32::new(x * w, 0.0);
        }
        fft.process(&mut buf);
        let mut flux = 0.0;
        for (k, p) in prev.iter_mut().enumerate() {
            let mag = (1.0 + 100.0 * buf[k].norm()).ln();
            let d = mag - *p;
            if d > 0.0 {
                flux += d;
            }
            *p = mag;
        }
        env.push(if f == 0 { 0.0 } else { flux });
    }
    // normalise to unit maximum
    let max = env.iter().copied().fold(0.0f32, f32::max);
    if max > 0.0 {
        for v in &mut env {
            *v /= max;
        }
    }
    env
}

fn median(v: &mut [f32]) -> f32 {
    if v.is_empty() {
        return 0.0;
    }
    let mid = v.len() / 2;
    v.select_nth_unstable_by(mid, |a, b| a.total_cmp(b));
    v[mid]
}

/// Indices of onset peaks in an envelope.
pub fn pick_peaks(env: &[f32], hop_secs: f64) -> Vec<usize> {
    let w = 8usize; // ±85 ms neighbourhood for the threshold
    let min_gap = (0.1 / hop_secs).ceil() as usize;
    let mut peaks: Vec<usize> = Vec::new();
    for i in 1..env.len().saturating_sub(1) {
        let lo = i.saturating_sub(w);
        let hi = (i + w + 1).min(env.len());
        let mut neigh: Vec<f32> = env[lo..hi].to_vec();
        let thr = median(&mut neigh) + 0.08;
        let is_max = env[lo..hi].iter().all(|v| *v <= env[i]);
        if env[i] > thr && is_max {
            match peaks.last() {
                Some(&p) if i - p < min_gap => {
                    if env[i] > env[p] {
                        *peaks.last_mut().expect("peak") = i;
                    }
                }
                _ => peaks.push(i),
            }
        }
    }
    peaks
}

/// Tempo in BPM from the onset envelope (autocorrelation, 60–200 BPM).
pub fn estimate_tempo(env: &[f32], hop_secs: f64) -> Option<f64> {
    let lag_min = (60.0 / 200.0 / hop_secs).floor().max(1.0) as usize;
    let lag_max = (1.0 / hop_secs).ceil() as usize; // 60 BPM
    if env.len() < lag_max * 2 {
        return None;
    }
    let mean = env.iter().sum::<f32>() / env.len() as f32;
    let e: Vec<f32> = env.iter().map(|v| v - mean).collect();
    let mut best = (0usize, f64::MIN);
    for lag in lag_min..=lag_max {
        let r: f64 = e.iter().zip(&e[lag..]).map(|(a, b)| (*a * *b) as f64).sum::<f64>() / (e.len() - lag) as f64;
        // log-Gaussian tempo prior centred at 120 BPM (resolves octave errors)
        let bpm = 60.0 / (lag as f64 * hop_secs);
        let prior = (-0.5 * ((bpm / 120.0).log2() / 1.0).powi(2)).exp();
        let score = r * (0.5 + 0.5 * prior);
        if score > best.1 {
            best = (lag, score);
        }
    }
    if best.1 <= 0.0 {
        return None;
    }
    // parabolic refinement of the lag
    let lag = best.0 as f64;
    Some(60.0 / (lag * hop_secs))
}

/// Full analysis of interleaved audio.
pub fn analyze(samples: &[f32], channels: usize, rate: u32) -> BeatAnalysis {
    let ch = channels.max(1);
    let mono: Vec<f32> = samples.chunks_exact(ch).map(|f| f.iter().sum::<f32>() / ch as f32).collect();
    let hop_secs = HOP as f64 / rate as f64;
    let env = onset_envelope(&mono, rate);
    let peaks = pick_peaks(&env, hop_secs);
    // envelope frame f covers [f*hop, f*hop+N); its onset is near the window centre
    let frame_time = |f: f64| f * hop_secs + (FFT_SIZE as f64 * 0.5 - HOP as f64 * 0.5) / rate as f64;
    let onsets: Vec<f64> = peaks.iter().map(|&p| frame_time(p as f64)).collect();
    let Some(bpm) = estimate_tempo(&env, hop_secs) else {
        return BeatAnalysis { beats: onsets.clone(), onsets, bpm: 0.0 };
    };
    let period = 60.0 / bpm / hop_secs; // in envelope frames
    // phase with the most onset energy on the grid
    let steps = period.ceil() as usize;
    let mut best_phase = (0.0, f32::MIN);
    for ph in 0..steps {
        let mut s = 0.0;
        let mut x = ph as f64;
        while x < env.len() as f64 {
            s += env[x.round() as usize % env.len()];
            x += period;
        }
        if s > best_phase.1 {
            best_phase = (ph as f64, s);
        }
    }
    let tol = period * 0.12;
    let mut beats = Vec::new();
    let mut x = best_phase.0;
    while x < env.len() as f64 {
        // snap to the strongest peak near the grid point
        let snapped = peaks
            .iter()
            .filter(|&&p| (p as f64 - x).abs() <= tol)
            .max_by(|a, b| env[**a].total_cmp(&env[**b]))
            .map(|&p| p as f64)
            .unwrap_or(x);
        beats.push(frame_time(snapped).max(0.0));
        // follow tempo drift from the snapped position
        x = snapped + period;
    }
    BeatAnalysis { onsets, bpm, beats }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Kick-like clicks (decaying 60 Hz + noise burst) at `bpm` starting at `offset`.
    fn click_track(rate: u32, bpm: f64, offset: f64, secs: f64) -> Vec<f32> {
        let n = (rate as f64 * secs) as usize;
        let mut out = vec![0.0f32; n];
        let period = 60.0 / bpm;
        let mut seed = 1u32;
        let mut t = offset;
        while t < secs {
            let start = (t * rate as f64) as usize;
            for i in 0..(rate as usize / 10) {
                if start + i >= n {
                    break;
                }
                let tt = i as f32 / rate as f32;
                seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                let noise = (seed >> 9) as f32 / (1 << 23) as f32 - 0.5;
                out[start + i] += (-tt * 40.0).exp() * ((std::f32::consts::TAU * 60.0 * tt).sin() * 0.8 + noise * 0.6);
            }
            t += period;
        }
        // quiet pad underneath
        for (i, v) in out.iter_mut().enumerate() {
            *v += 0.02 * (std::f32::consts::TAU * 220.0 * i as f32 / rate as f32).sin();
        }
        out
    }

    #[test]
    fn finds_every_beat_of_a_click_track() {
        let rate = 48_000;
        let (bpm, offset, secs) = (120.0, 0.13, 12.0);
        let mono = click_track(rate, bpm, offset, secs);
        let a = analyze(&mono, 1, rate);
        assert!((a.bpm - bpm).abs() < 3.0, "tempo {}", a.bpm);
        let truth: Vec<f64> = (0..).map(|i| offset + i as f64 * 60.0 / bpm).take_while(|t| *t < secs - 0.1).collect();
        // every true beat has a detected beat within 25 ms
        for t in &truth {
            let err = a.beats.iter().map(|b| (b - t).abs()).fold(f64::MAX, f64::min);
            assert!(err < 0.025, "beat at {t:.3} missed by {err:.3}s; beats {:?}", &a.beats[..a.beats.len().min(6)]);
        }
        assert!(a.beats.len() <= truth.len() + 1, "{} beats for {} truth", a.beats.len(), truth.len());
        assert!(a.onsets.len() >= truth.len() - 1);
    }

    #[test]
    fn handles_other_tempi_and_stereo() {
        let rate = 44_100;
        let mono = click_track(rate, 95.0, 0.4, 14.0);
        let stereo: Vec<f32> = mono.iter().flat_map(|v| [*v, *v]).collect();
        let a = analyze(&stereo, 2, rate);
        assert!((a.bpm - 95.0).abs() < 3.0, "tempo {}", a.bpm);
    }

    #[test]
    fn silence_and_short_input_give_nothing() {
        assert!(analyze(&vec![0.0; 48_000 * 3], 1, 48_000).beats.is_empty());
        assert!(analyze(&[0.1; 100], 1, 48_000).beats.is_empty());
    }

    #[test]
    fn multiband_separates_sub_bass_and_snare() {
        let rate = 48_000u32;
        let secs = 4.0f64;
        let mut samples = vec![0.0f32; (rate as f64 * secs) as usize];
        // Sub-bass 65 Hz kick at 1.0s
        let kick_idx = rate as usize;
        for i in 0..(rate as usize / 6) {
            let t = i as f32 / rate as f32;
            samples[kick_idx + i] += (-t * 18.0).exp() * (std::f32::consts::TAU * 65.0 * t).sin() * 0.9;
        }
        // High snare / transient at 2.4s (3.4 kHz + 4.5 kHz)
        let snare_idx = (2.4 * rate as f64) as usize;
        for i in 0..(rate as usize / 10) {
            let t = i as f32 / rate as f32;
            samples[snare_idx + i] += (-t * 35.0).exp()
                * ((std::f32::consts::TAU * 3400.0 * t).sin() * 0.5 + (std::f32::consts::TAU * 4500.0 * t).sin() * 0.5);
        }
        let detected = analyze_multiband(&samples, 1, rate, 1.0);
        assert!(detected.iter().any(|b| b.band == BeatBand::SubBassKick && (b.secs - 1.0).abs() < 0.08));
        assert!(detected.iter().any(|b| b.band == BeatBand::SnareTransient && (b.secs - 2.4).abs() < 0.08));
    }
}
