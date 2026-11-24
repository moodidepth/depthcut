//! Lining clips up by their sound or their timecode.
//!
//! Audio sync cross-correlates onset envelopes (rises in log energy), which
//! stay comparable between a camera's scratch mic and a field recorder at a
//! very different level. The correlation runs through an FFT, so hour-long
//! recordings line up in well under a second.

use crate::asset::MediaAsset;
use crate::time::Time;
use rustfft::num_complex::Complex32;
use rustfft::FftPlanner;

/// Where one recording sits against another.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SyncMatch {
    /// The other recording's time `t` is the reference's time `t + offset`
    /// (seconds; positive = the other one started later).
    pub offset: f64,
    /// Correlation peak over the best peak elsewhere: about 1 is a guess,
    /// above [`SyncMatch::RELIABLE`] a clear match.
    pub confidence: f32,
}

impl SyncMatch {
    pub const RELIABLE: f32 = 1.5;

    pub fn reliable(&self) -> bool {
        self.confidence >= Self::RELIABLE
    }
}

/// Largest FFT used; longer envelopes are averaged down to fit.
const MAX_FFT: usize = 1 << 21;

/// Onset strength: the rise in log energy from one bucket to the next,
/// normalised to zero mean and unit variance.
fn onsets(env: &[f32], pool: usize) -> Vec<f32> {
    let pooled: Vec<f32> = env.chunks(pool.max(1)).map(|c| c.iter().sum::<f32>() / c.len() as f32).collect();
    let mut prev = None;
    let mut out: Vec<f32> = pooled
        .iter()
        .map(|&x| {
            let l = (x.max(0.0) + 1e-3).ln();
            let d = prev.map_or(0.0, |p: f32| (l - p).max(0.0));
            prev = Some(l);
            d
        })
        .collect();
    let n = out.len().max(1) as f32;
    let mean = out.iter().sum::<f32>() / n;
    let var = out.iter().map(|v| (v - mean) * (v - mean)).sum::<f32>() / n;
    let sd = var.sqrt().max(1e-9);
    for v in &mut out {
        *v = (*v - mean) / sd;
    }
    out
}

/// Find `other` inside `reference`. Both are loudness envelopes (RMS per
/// bucket) at `rate` buckets per second. `max_offset` limits the search
/// (seconds either way); `None` searches every overlap.
pub fn audio_offset(reference: &[f32], other: &[f32], rate: f64, max_offset: Option<f64>) -> Option<SyncMatch> {
    if reference.len() < 8 || other.len() < 8 || rate <= 0.0 {
        return None;
    }
    let pool = (reference.len() + other.len()).div_ceil(MAX_FFT / 2).max(1);
    let rate = rate / pool as f64;
    let r = onsets(reference, pool);
    let o = onsets(other, pool);
    let (nr, no) = (r.len(), o.len());
    let n = (nr + no).next_power_of_two();
    let mut planner = FftPlanner::<f32>::new();
    let fwd = planner.plan_fft_forward(n);
    let inv = planner.plan_fft_inverse(n);
    let pad = |v: &[f32]| {
        let mut b: Vec<Complex32> = v.iter().map(|&x| Complex32::new(x, 0.0)).collect();
        b.resize(n, Complex32::new(0.0, 0.0));
        b
    };
    let (mut fr, mut fo) = (pad(&r), pad(&o));
    fwd.process(&mut fr);
    fwd.process(&mut fo);
    // corr[k] = Σ o[i]·r[i + k]: other's bucket i lines up with reference bucket i + k
    let mut c: Vec<Complex32> = fr.iter().zip(&fo).map(|(a, b)| a * b.conj()).collect();
    inv.process(&mut c);
    let corr = |k: i64| c[k.rem_euclid(n as i64) as usize].re;

    // lags where the recordings overlap enough to judge
    let min_overlap = ((nr.min(no) as f64 * 0.25).min(5.0 * rate)).max(2.0) as i64;
    let lo = -(no as i64) + min_overlap;
    let hi = nr as i64 - min_overlap;
    let (lo, hi) = match max_offset {
        Some(m) => {
            let m = (m * rate).ceil() as i64;
            (lo.max(-m), hi.min(m))
        }
        None => (lo, hi),
    };
    if lo > hi {
        return None;
    }
    let best = (lo..=hi).max_by(|a, b| corr(*a).total_cmp(&corr(*b)))?;
    let peak = corr(best);
    if peak <= 0.0 {
        return None;
    }
    // runner-up outside half a second around the peak
    let guard = (0.5 * rate).ceil() as i64;
    let second = (lo..=hi).filter(|k| (k - best).abs() > guard).map(corr).fold(f32::MIN, f32::max);
    let confidence = if second > 0.0 { peak / second } else { 10.0 };
    // sub-bucket peak by fitting a parabola through its neighbours
    let (a, b) = (corr(best - 1), corr(best + 1));
    let denom = a - 2.0 * peak + b;
    let frac = if denom.abs() > 1e-9 { (0.5 * (a - b) / denom).clamp(-0.5, 0.5) } else { 0.0 };
    Some(SyncMatch { offset: (best as f64 + frac as f64) / rate, confidence })
}

/// `env` at `from` buckets per second, linearly resampled to `to` (44.1 kHz
/// and 48 kHz files give slightly different bucket rates, which would drift
/// over a long take).
pub fn resample_envelope(env: &[f32], from: f64, to: f64) -> Vec<f32> {
    if env.is_empty() || from <= 0.0 || to <= 0.0 || (from - to).abs() < 1e-9 {
        return env.to_vec();
    }
    let n = ((env.len() as f64) * to / from).round().max(1.0) as usize;
    (0..n)
        .map(|i| {
            let x = i as f64 * from / to;
            let j = (x.floor() as usize).min(env.len() - 1);
            let f = (x - j as f64) as f32;
            let next = env[(j + 1).min(env.len() - 1)];
            env[j] * (1.0 - f) + next * f
        })
        .collect()
}

/// Same convention as [`SyncMatch::offset`], from the files' start
/// timecode: `None` when either has none.
pub fn timecode_offset(reference: &MediaAsset, other: &MediaAsset) -> Option<Time> {
    Some(other.start_timecode? - reference.start_timecode?)
}

/// Where each recording starts on a shared clock, earliest at zero, from
/// offsets against the first one (`None` = unknown, placed at the first's
/// start).
pub fn starts_from_offsets(offsets: &[Option<Time>]) -> Vec<Time> {
    let raw: Vec<Time> = offsets.iter().map(|o| o.unwrap_or(Time::ZERO)).collect();
    let min = raw.iter().copied().min().unwrap_or(Time::ZERO);
    raw.into_iter().map(|t| t - min).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Speech-like envelope: random bursts over a noise floor.
    fn envelope(len: usize, seed: u64) -> Vec<f32> {
        let mut s = seed;
        let mut rnd = || {
            s ^= s << 13;
            s ^= s >> 7;
            s ^= s << 17;
            (s % 10_000) as f32 / 10_000.0
        };
        let mut v = vec![0.0f32; len];
        let mut i = 0;
        while i < len {
            let gap = 10 + (rnd() * 80.0) as usize;
            let burst = 5 + (rnd() * 40.0) as usize;
            let level = 0.1 + rnd() * 0.6;
            for x in v.iter_mut().skip(i + gap).take(burst) {
                *x = level * (0.7 + 0.3 * rnd());
            }
            i += gap + burst;
        }
        for x in &mut v {
            *x += 0.002 * rnd();
        }
        v
    }

    #[test]
    fn finds_a_quiet_recording_inside_a_long_one() {
        let rate = 200.0;
        let reference = envelope(200 * 120, 7);
        // the camera started 37.5 s into the recorder, picked it up at a third of the level
        let start = (37.5 * rate) as usize;
        let mut s = 99u64;
        let other: Vec<f32> = reference[start..start + 200 * 30]
            .iter()
            .map(|x| {
                s = s.wrapping_mul(6364136223846793005).wrapping_add(1);
                x * 0.33 + 0.004 * ((s >> 33) % 100) as f32 / 100.0
            })
            .collect();
        let m = audio_offset(&reference, &other, rate, None).unwrap();
        assert!((m.offset - 37.5).abs() < 0.01, "{m:?}");
        assert!(m.reliable(), "{m:?}");
    }

    #[test]
    fn finds_a_recording_that_started_first() {
        let rate = 200.0;
        let long = envelope(200 * 60, 3);
        // the reference started 12 s after the other one
        let reference = long[200 * 12..].to_vec();
        let m = audio_offset(&reference, &long, rate, None).unwrap();
        assert!((m.offset + 12.0).abs() < 0.01, "{m:?}");
    }

    #[test]
    fn unrelated_audio_is_not_reliable() {
        let a = envelope(200 * 40, 11);
        let b = envelope(200 * 40, 12345);
        if let Some(m) = audio_offset(&a, &b, 200.0, None) {
            assert!(!m.reliable(), "{m:?}");
        }
    }

    #[test]
    fn search_window_limits_the_offset() {
        let rate = 200.0;
        let reference = envelope(200 * 60, 5);
        let other = reference[200 * 20..200 * 40].to_vec();
        let m = audio_offset(&reference, &other, rate, Some(5.0)).unwrap();
        assert!(m.offset.abs() <= 5.0 + 1e-9);
    }

    #[test]
    fn starts_line_up_from_the_earliest() {
        let s = starts_from_offsets(&[Some(Time::ZERO), Some(Time::from_secs(-3)), None, Some(Time::from_secs(4))]);
        assert_eq!(s, vec![Time::from_secs(3), Time::ZERO, Time::from_secs(3), Time::from_secs(7)]);
    }

    #[test]
    fn a_different_bucket_rate_is_resampled() {
        let reference = envelope(200 * 90, 21);
        // the other file's buckets come at 200.45 per second (44.1 kHz / 220)
        let src = reference[200 * 30..200 * 70].to_vec();
        let other_rate = 44_100.0 / 220.0;
        let other: Vec<f32> = (0..((src.len() as f64) * other_rate / 200.0) as usize).map(|i| src[((i as f64 * 200.0 / other_rate) as usize).min(src.len() - 1)]).collect();
        let m = audio_offset(&reference, &resample_envelope(&other, other_rate, 200.0), 200.0, None).unwrap();
        assert!((m.offset - 30.0).abs() < 0.02, "{m:?}");
    }

    #[test]
    fn timecode_offsets() {
        let mut a = MediaAsset::new("a", "a.mov", Time::from_secs(10));
        let mut b = MediaAsset::new("b", "b.wav", Time::from_secs(10));
        assert_eq!(timecode_offset(&a, &b), None);
        a.start_timecode = Some(Time::from_secs(3600));
        b.start_timecode = Some(Time::from_secs(3595));
        assert_eq!(timecode_offset(&a, &b), Some(Time::from_secs(-5)));
    }
}
