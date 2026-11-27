//! Integrated loudness per ITU-R BS.1770-4 / EBU R128 (LUFS).
//!
//! K-weighting (high-shelf + RLB high-pass, coefficients derived for any
//! sample rate as in libebur128), 400 ms blocks with 75 % overlap, absolute
//! gate at -70 LUFS and relative gate 10 LU below the ungated mean.

/// Streaming target used by YouTube / Spotify / TikTok.
pub const TARGET_LUFS: f32 = -14.0;

#[derive(Clone, Copy, Debug)]
pub(crate) struct Biquad64 {
    b: [f64; 3],
    a: [f64; 2],
    z: [f64; 2],
}

impl Biquad64 {
    pub(crate) fn process(&mut self, x: f64) -> f64 {
        let y = self.b[0] * x + self.z[0];
        self.z[0] = self.b[1] * x - self.a[0] * y + self.z[1];
        self.z[1] = self.b[2] * x - self.a[1] * y;
        y
    }
}

pub(crate) fn k_weighting(rate: f64) -> [Biquad64; 2] {
    // stage 1: high shelf (+4 dB above ~1.7 kHz)
    let f0 = 1681.974450955533;
    let g = 3.999843853973347;
    let q = 0.7071752369554196;
    let k = (std::f64::consts::PI * f0 / rate).tan();
    let vh = 10f64.powf(g / 20.0);
    let vb = vh.powf(0.4996667741545416);
    let a0 = 1.0 + k / q + k * k;
    let shelf = Biquad64 {
        b: [(vh + vb * k / q + k * k) / a0, 2.0 * (k * k - vh) / a0, (vh - vb * k / q + k * k) / a0],
        a: [2.0 * (k * k - 1.0) / a0, (1.0 - k / q + k * k) / a0],
        z: [0.0; 2],
    };
    // stage 2: RLB high-pass (~38 Hz)
    let f0 = 38.13547087602444;
    let q = 0.5003270373238773;
    let k = (std::f64::consts::PI * f0 / rate).tan();
    let d = 1.0 + k / q + k * k;
    let hp = Biquad64 { b: [1.0, -2.0, 1.0], a: [2.0 * (k * k - 1.0) / d, (1.0 - k / q + k * k) / d], z: [0.0; 2] };
    [shelf, hp]
}

/// Integrated loudness of interleaved audio in LUFS. Returns `None` for
/// silence (everything below the absolute gate) or less than 400 ms.
pub fn integrated_lufs(samples: &[f32], channels: usize, rate: u32) -> Option<f32> {
    let ch = channels.max(1);
    let frames = samples.len() / ch;
    let rate_f = rate as f64;
    let block = (0.4 * rate_f).round() as usize;
    let step = (0.1 * rate_f).round() as usize;
    if frames < block || step == 0 {
        return None;
    }
    // K-weighted squares, summed per 100 ms step (channel weights 1.0 for L/R/C)
    let mut filters: Vec<[Biquad64; 2]> = (0..ch).map(|_| k_weighting(rate_f)).collect();
    let steps = frames / step;
    let mut step_energy = vec![0.0f64; steps];
    for (i, frame) in samples.chunks_exact(ch).take(steps * step).enumerate() {
        let mut e = 0.0;
        for (c, x) in frame.iter().enumerate() {
            // LFE (4th channel of 5.1) is excluded
            if ch >= 6 && c == 3 {
                continue;
            }
            let [s1, s2] = &mut filters[c];
            let y = s2.process(s1.process(*x as f64));
            // surround channels (5.1 Ls/Rs) weigh +1.5 dB
            let w = if ch >= 6 && c >= 4 { 1.41 } else { 1.0 };
            e += w * y * y;
        }
        step_energy[i / step] += e;
    }
    // 400 ms blocks = 4 consecutive steps
    let per_block = block as f64;
    let blocks: Vec<f64> = step_energy.windows(4).map(|w| w.iter().sum::<f64>() / per_block).collect();
    let lufs = |z: f64| -0.691 + 10.0 * z.max(1e-20).log10();
    let abs_gated: Vec<f64> = blocks.iter().copied().filter(|z| lufs(*z) > -70.0).collect();
    if abs_gated.is_empty() {
        return None;
    }
    let rel = lufs(abs_gated.iter().sum::<f64>() / abs_gated.len() as f64) - 10.0;
    let gated: Vec<f64> = abs_gated.into_iter().filter(|z| lufs(*z) > rel).collect();
    if gated.is_empty() {
        return None;
    }
    Some(lufs(gated.iter().sum::<f64>() / gated.len() as f64) as f32)
}

/// Gain (dB) that brings `measured` LUFS to `target`, limited to a sane range.
pub fn normalize_gain_db(measured: f32, target: f32) -> f32 {
    (target - measured).clamp(-40.0, 30.0)
}

/// Short-form (Shorts / TikTok / Reels) mastering target.
pub const TARGET_LUFS_SHORTS: f32 = -16.0;
/// True-peak ceiling for delivery masters (dBTP).
pub const CEILING_DBFS: f32 = -1.0;
/// Ceiling when the mix is then encoded lossily (AAC): the codec's decoded
/// waveform overshoots the limited PCM (+0.5 dB measured on a 320k gameplay
/// mix), so the extra headroom keeps the delivered file under −1 dBTP.
pub const LOSSY_CEILING_DBTP: f32 = -1.5;

/// Result of [`normalize_program`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ProgramLoudness {
    pub measured_lufs: f32,
    pub gain_db: f32,
    /// Largest gain reduction the limiter applied (dB, ≥ 0).
    pub limited_db: f32,
}

/// Fairlight/Premiere-style "normalize to platform target" for a whole
/// interleaved mix: measure integrated loudness, apply make-up gain, then run
/// a linked look-ahead true-peak limiter so neither samples nor the peaks
/// between them (4× oversampled, BS.1770-4) exceed `ceiling_dbfs` (dBTP).
/// Silence / too-short audio is left untouched (`None`).
pub fn normalize_program(samples: &mut [f32], channels: usize, rate: u32, target_lufs: f32, ceiling_dbfs: f32) -> Option<ProgramLoudness> {
    let measured = integrated_lufs(samples, channels, rate)?;
    let ceiling = 10f32.powf(ceiling_dbfs / 20.0);
    let (mut current, mut gain_db, mut limited_db) = (measured, 0.0f32, 0.0f32);
    // Limiting peak-heavy mixes (gunfire, explosions) pulls loudness back
    // down, so re-measure and add make-up gain — at most three passes.
    for _ in 0..3 {
        let step = normalize_gain_db(current, target_lufs).min(30.0 - gain_db);
        if step.abs() < 0.2 {
            break;
        }
        let g = 10f32.powf(step / 20.0);
        samples.iter_mut().for_each(|s| *s *= g);
        gain_db += step;
        limited_db = limited_db.max(peak_limit(samples, channels, rate, ceiling));
        match integrated_lufs(samples, channels, rate) {
            Some(l) => current = l,
            None => break,
        }
    }
    Some(ProgramLoudness { measured_lufs: measured, gain_db, limited_db })
}

/// Per-frame true peak: the largest |sample| across channels, raised by any
/// 4× interpolated point next to the frame. Interpolation is skipped where no
/// sample in the filter window is loud enough to push a point over `ceiling`,
/// so quiet stretches cost one comparison per frame.
fn frame_true_peaks(samples: &[f32], ch: usize, ceiling: f32) -> Vec<f32> {
    use crate::audio::meter::{true_peak_coefs, TP_TAPS};
    let frames = samples.len() / ch;
    let mut tp: Vec<f32> = samples.chunks_exact(ch).map(|f| f.iter().fold(0.0f32, |m, x| m.max(x.abs()))).collect();
    let coefs = true_peak_coefs();
    // |interpolated point| <= l1 * max|input in window|
    let l1 = coefs.iter().map(|ph| ph.iter().map(|c| c.abs()).sum::<f32>()).fold(0.0, f32::max);
    let mut hot = vec![0u32; frames + 1];
    for j in 0..frames {
        hot[j + 1] = hot[j] + u32::from(tp[j] * l1 > ceiling);
    }
    // signal is zero outside [0, frames): starts and ends ring too
    let at = |f: isize, c: usize| if f < 0 || f >= frames as isize { 0.0 } else { samples[f as usize * ch + c] };
    let window_hot = |first: isize| {
        let (a, b) = (first.clamp(0, frames as isize) as usize, (first + TP_TAPS as isize).clamp(0, frames as isize) as usize);
        hot[b] > hot[a]
    };
    // a window ending at frame i yields points between frames i-6 and i-5
    for i in 5..frames as isize + 6 {
        let first = i + 1 - TP_TAPS as isize;
        if !window_hot(first) {
            continue;
        }
        let mut m = 0.0f32;
        for c in 0..ch {
            for ph in &coefs {
                // newest sample takes tap 0 (same orientation as the meter)
                let v: f32 = ph.iter().rev().enumerate().map(|(k, w)| w * at(first + k as isize, c)).sum();
                m = m.max(v.abs());
            }
        }
        for k in [i - 6, i - 5] {
            if (0..frames as isize).contains(&k) {
                tp[k as usize] = tp[k as usize].max(m);
            }
        }
    }
    tp
}

/// Offline linked look-ahead true-peak limiter (5 ms look-ahead, 120 ms
/// release). Delivery specs cap *true* peak (−1 dBTP): a sample-peak limiter
/// leaves inter-sample overs that decoders and lossy codecs turn into clipping.
///
/// Gain is the sliding-window minimum of the required gain over the next
/// look-ahead span, box-smoothed over the previous span: every frame's gain is
/// then provably ≤ what that frame needs (no overs) while gain changes ramp
/// over 5 ms instead of stepping (no clicks). Returns max reduction in dB.
pub fn peak_limit(samples: &mut [f32], channels: usize, rate: u32, ceiling: f32) -> f32 {
    let ch = channels.max(1);
    let frames = samples.len() / ch;
    if frames == 0 || ceiling <= 0.0 {
        return 0.0;
    }
    let need: Vec<f32> = frame_true_peaks(samples, ch, ceiling).into_iter().map(|peak| if peak > ceiling { ceiling / peak } else { 1.0 }).collect();
    if need.iter().all(|g| *g >= 1.0) {
        return 0.0;
    }
    let look = ((rate as f32 * 0.005) as usize).max(1);
    // forward sliding-window minimum over [i, i + look] (monotonic deque)
    let mut wmin = vec![1.0f32; frames];
    let mut dq: std::collections::VecDeque<usize> = std::collections::VecDeque::new();
    for i in (0..frames).rev() {
        while dq.back().is_some_and(|&j| need[j] >= need[i]) {
            dq.pop_back();
        }
        dq.push_back(i);
        while dq.front().is_some_and(|&j| j > i + look) {
            dq.pop_front();
        }
        wmin[i] = dq.front().map_or(1.0, |&j| need[j]);
    }
    // backward box average over [i - look, i]; every term is ≤ need[i]
    let rel = (-1.0 / (rate as f32 * 0.120)).exp();
    // Window of look+1 values; positions before the start count as unity gain.
    let width = (look + 1) as f64;
    let (mut acc, mut env, mut worst) = (width, 1.0f32, 1.0f32);
    for i in 0..frames {
        acc += wmin[i] as f64 - i.checked_sub(look + 1).map_or(1.0, |k| wmin[k] as f64);
        let smooth = (acc / width) as f32;
        // `.min(need[i])` keeps the no-overs guarantee inside the padded head.
        env = smooth.min(1.0 - (1.0 - env) * rel).min(need[i]);
        worst = worst.min(env);
        for s in &mut samples[i * ch..(i + 1) * ch] {
            *s *= env;
        }
    }
    -20.0 * worst.max(1e-6).log10()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn program_normalization_hits_target_and_respects_ceiling() {
        // quiet dialogue-ish tone with sparse loud transients (gunshots)
        let mut x = sine_stereo(48_000, 997.0, 0.05, 8.0);
        for k in 0..8 {
            let at = (k * 48_000 + 24_000) * 2;
            for j in 0..400 {
                x[at + j] = if j % 2 == 0 { 0.9 } else { -0.9 };
            }
        }
        let r = normalize_program(&mut x, 2, 48_000, TARGET_LUFS, CEILING_DBFS).unwrap();
        assert!(r.gain_db > 5.0, "quiet mix is boosted: {r:?}");
        assert!(r.limited_db > 0.5, "transients were limited: {r:?}");
        let ceil = 10f32.powf(CEILING_DBFS / 20.0);
        let peak = x.iter().fold(0.0f32, |m, s| m.max(s.abs()));
        assert!(peak <= ceil + 1e-5, "peak {peak} over ceiling {ceil}");
        let tp = true_peak_dbtp(&x);
        assert!(tp <= CEILING_DBFS + 0.1, "true peak {tp} dBTP over the ceiling");
        let after = integrated_lufs(&x, 2, 48_000).unwrap();
        assert!((after - TARGET_LUFS).abs() < 1.0, "landed at {after} LUFS");
        // Shorts target is 2 LU quieter
        let mut y = sine_stereo(48_000, 997.0, 0.05, 6.0);
        normalize_program(&mut y, 2, 48_000, TARGET_LUFS_SHORTS, CEILING_DBFS).unwrap();
        assert!((integrated_lufs(&y, 2, 48_000).unwrap() - TARGET_LUFS_SHORTS).abs() < 0.3);
        // silence untouched
        let mut z = vec![0.0f32; 48_000 * 2];
        assert!(normalize_program(&mut z, 2, 48_000, TARGET_LUFS, CEILING_DBFS).is_none());
    }

    #[test]
    fn limiter_is_transparent_below_ceiling_and_click_free() {
        let mut x = sine_stereo(48_000, 440.0, 0.5, 1.0);
        let orig = x.clone();
        assert_eq!(peak_limit(&mut x, 2, 48_000, 0.9), 0.0);
        assert_eq!(x, orig);
        // a single hot sample: gain ramps (no single-sample step > 5 %)
        let mut y = vec![0.3f32; 48_000 * 2];
        y[48_000] = 1.0;
        y[48_001] = 1.0;
        peak_limit(&mut y, 2, 48_000, 0.5);
        assert!(y[48_000] <= 0.5 + 1e-6);
        let gains: Vec<f32> = y.chunks(2).map(|f| f[1] / 0.3).collect();
        let max_step = gains.windows(2).enumerate().filter(|(i, _)| *i != 23_999 && *i != 24_000).map(|(_, w)| (w[1] - w[0]).abs()).fold(0.0, f32::max);
        assert!(max_step < 0.05, "gain step {max_step}");
    }

    fn true_peak_dbtp(x: &[f32]) -> f32 {
        let mut m = crate::audio::meter::LoudnessMeter::new(48_000);
        m.push(x);
        m.true_peak_db().unwrap()
    }

    #[test]
    fn limiter_catches_intersample_overs() {
        // fs/4 sine at 45°: every sample sits at 0.707 (−3 dBFS), the
        // waveform between them reaches 1.0 (0 dBTP). A sample-peak limiter
        // with a −1 dB ceiling would leave it alone.
        let mut x: Vec<f32> = (0..48_000)
            .flat_map(|i| {
                let v = (std::f64::consts::FRAC_PI_2 * i as f64 + std::f64::consts::FRAC_PI_4).sin() as f32;
                [v, v]
            })
            .collect();
        assert!(true_peak_dbtp(&x) > -0.5);
        let reduced = peak_limit(&mut x, 2, 48_000, 10f32.powf(CEILING_DBFS / 20.0));
        assert!(reduced > 0.5, "limited {reduced} dB");
        let tp = true_peak_dbtp(&x);
        assert!(tp <= CEILING_DBFS + 0.1, "true peak {tp} dBTP");
    }

    fn sine_stereo(rate: u32, freq: f64, amp: f32, secs: f64) -> Vec<f32> {
        let n = (rate as f64 * secs) as usize;
        (0..n)
            .flat_map(|i| {
                let v = (std::f64::consts::TAU * freq * i as f64 / rate as f64).sin() as f32 * amp;
                [v, v]
            })
            .collect()
    }

    #[test]
    fn full_scale_1khz_reference() {
        // BS.1770: a 0 dBFS 1 kHz sine in one channel reads -3.01 LKFS,
        // so the same tone in both channels reads ~0.0 LUFS.
        let mono: Vec<f32> = sine_stereo(48_000, 997.0, 1.0, 5.0).chunks(2).map(|c| c[0]).collect();
        let l = integrated_lufs(&mono, 1, 48_000).unwrap();
        assert!((l + 3.01).abs() < 0.1, "mono {l}");
        let s = integrated_lufs(&sine_stereo(48_000, 997.0, 1.0, 5.0), 2, 48_000).unwrap();
        assert!(s.abs() < 0.1, "stereo {s}");
        // and at 44.1 kHz
        let s441 = integrated_lufs(&sine_stereo(44_100, 997.0, 1.0, 5.0), 2, 44_100).unwrap();
        assert!(s441.abs() < 0.1, "44.1k {s441}");
    }

    #[test]
    fn level_changes_track_gain_and_gating_ignores_silence() {
        let quiet = integrated_lufs(&sine_stereo(48_000, 997.0, 0.1, 5.0), 2, 48_000).unwrap();
        assert!((quiet + 20.0).abs() < 0.1, "{quiet}");
        // 5 s tone + 5 s silence: silence is gated out
        let mut x = sine_stereo(48_000, 997.0, 0.1, 5.0);
        x.extend(std::iter::repeat_n(0.0, 48_000 * 2 * 5));
        let gated = integrated_lufs(&x, 2, 48_000).unwrap();
        assert!((gated - quiet).abs() < 0.2, "{gated} vs {quiet}");
        assert!(integrated_lufs(&vec![0.0; 48_000 * 2], 2, 48_000).is_none());
        assert!(integrated_lufs(&[0.5; 100], 2, 48_000).is_none());
        assert!((normalize_gain_db(-20.0, TARGET_LUFS) - 6.0).abs() < 1e-6);
    }

    #[test]
    fn low_frequencies_are_weighted_down() {
        let lo = integrated_lufs(&sine_stereo(48_000, 30.0, 0.5, 4.0), 2, 48_000).unwrap();
        let mid = integrated_lufs(&sine_stereo(48_000, 1000.0, 0.5, 4.0), 2, 48_000).unwrap();
        assert!(lo < mid - 2.0, "30 Hz {lo} vs 1 kHz {mid}");
    }
}
