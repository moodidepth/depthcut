//! Broadcast loudness metering (ITU-R BS.1770-4 / EBU R128): momentary
//! (400 ms), short-term (3 s) and gated integrated loudness, plus 4×
//! oversampled true peak.

use crate::analysis::loudness::{k_weighting, Biquad64};

/// Histogram resolution of the integrated gate (0.1 LU from −70 to +10).
const HIST_MIN: f64 = -70.0;
const HIST_STEP: f64 = 0.1;
const HIST_BINS: usize = 800;
/// 4× true-peak interpolator: 48-tap windowed-sinc, 12 taps per phase.
pub(crate) const TP_PHASES: usize = 4;
pub(crate) const TP_TAPS: usize = 12;

/// Polyphase coefficients of the BS.1770-4 4× true-peak interpolator. Phase
/// `p` evaluated over the last [`TP_TAPS`] input samples (oldest first, taps
/// reversed) yields one of the four upsampled points.
pub(crate) fn true_peak_coefs() -> [[f32; TP_TAPS]; TP_PHASES] {
    let mut tp_coefs = [[0.0f32; TP_TAPS]; TP_PHASES];
    let len = TP_PHASES * TP_TAPS;
    for (p, phase) in tp_coefs.iter_mut().enumerate() {
        for (t, c) in phase.iter_mut().enumerate() {
            // tap index in the 4× upsampled filter
            let i = t * TP_PHASES + p;
            let x = (i as f64 - (len as f64 - 1.0) / 2.0) / TP_PHASES as f64;
            let sinc = if x.abs() < 1e-9 { 1.0 } else { (std::f64::consts::PI * x).sin() / (std::f64::consts::PI * x) };
            let w = 0.42 - 0.5 * (std::f64::consts::TAU * i as f64 / (len - 1) as f64).cos() + 0.08 * (2.0 * std::f64::consts::TAU * i as f64 / (len - 1) as f64).cos();
            *c = (sinc * w) as f32;
        }
    }
    // each phase sums to unity gain at DC
    for phase in &mut tp_coefs {
        let s: f32 = phase.iter().sum();
        phase.iter_mut().for_each(|c| *c /= s);
    }
    tp_coefs
}

fn lufs(z: f64) -> f64 {
    -0.691 + 10.0 * z.max(1e-20).log10()
}

/// One meter reading. Loudness values are `None` until enough audio passed
/// (or below the −70 LUFS absolute gate).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct MeterReading {
    pub momentary: Option<f32>,
    pub short_term: Option<f32>,
    pub integrated: Option<f32>,
    /// Highest true peak since the last loudness reset (dBTP).
    pub true_peak_db: Option<f32>,
    /// Per-channel sample peak since the previous reading (linear).
    pub peak: [f32; 2],
    /// Gain reduction of the bus dynamics since the previous reading (dB).
    pub gr: super::fx::GainReduction,
}

pub struct LoudnessMeter {
    filters: [[Biquad64; 2]; 2],
    step_len: usize,
    step_pos: usize,
    step_acc: f64,
    /// Energies of the last 30 completed 100 ms steps (newest last).
    steps: std::collections::VecDeque<f64>,
    hist_count: Vec<u32>,
    hist_energy: Vec<f64>,
    tp_hist: [[f32; TP_TAPS]; 2],
    tp_coefs: [[f32; TP_TAPS]; TP_PHASES],
    true_peak: f32,
    peak: [f32; 2],
}

impl LoudnessMeter {
    pub fn new(sample_rate: u32) -> Self {
        let rate = sample_rate as f64;
        let tp_coefs = true_peak_coefs();
        LoudnessMeter {
            filters: [k_weighting(rate), k_weighting(rate)],
            step_len: (0.1 * rate).round() as usize,
            step_pos: 0,
            step_acc: 0.0,
            steps: std::collections::VecDeque::with_capacity(31),
            hist_count: vec![0; HIST_BINS],
            hist_energy: vec![0.0; HIST_BINS],
            tp_hist: [[0.0; TP_TAPS]; 2],
            tp_coefs,
            true_peak: 0.0,
            peak: [0.0; 2],
        }
    }

    /// Feed interleaved stereo.
    pub fn push(&mut self, buf: &[f32]) {
        for f in buf.as_chunks::<2>().0 {
            let mut e = 0.0;
            for (c, x) in f.iter().enumerate() {
                let [s1, s2] = &mut self.filters[c];
                let y = s2.process(s1.process(*x as f64));
                e += y * y;
                self.peak[c] = self.peak[c].max(x.abs());
                // true peak: shift in, evaluate the 4 interpolated phases
                let h = &mut self.tp_hist[c];
                h.copy_within(1.., 0);
                h[TP_TAPS - 1] = *x;
                for phase in &self.tp_coefs {
                    let v: f32 = phase.iter().rev().zip(h.iter()).map(|(a, b)| a * b).sum();
                    self.true_peak = self.true_peak.max(v.abs());
                }
            }
            self.step_acc += e;
            self.step_pos += 1;
            if self.step_pos == self.step_len {
                self.finish_step();
            }
        }
    }

    fn finish_step(&mut self) {
        let e = self.step_acc / self.step_len as f64;
        self.step_acc = 0.0;
        self.step_pos = 0;
        if self.steps.len() == 30 {
            self.steps.pop_front();
        }
        self.steps.push_back(e);
        // every 100 ms a new 400 ms gating block (75 % overlap)
        if self.steps.len() >= 4 {
            let z = self.steps.iter().rev().take(4).sum::<f64>() / 4.0;
            let l = lufs(z);
            if l > HIST_MIN {
                let bin = (((l - HIST_MIN) / HIST_STEP) as usize).min(HIST_BINS - 1);
                self.hist_count[bin] += 1;
                self.hist_energy[bin] += z;
            }
        }
    }

    fn window(&self, n: usize) -> Option<f32> {
        if self.steps.len() < n {
            return None;
        }
        let z = self.steps.iter().rev().take(n).sum::<f64>() / n as f64;
        let l = lufs(z);
        (l > HIST_MIN).then_some(l as f32)
    }

    pub fn momentary(&self) -> Option<f32> {
        self.window(4)
    }

    pub fn short_term(&self) -> Option<f32> {
        self.window(30)
    }

    /// Gated integrated loudness (absolute −70 LUFS, relative −10 LU).
    pub fn integrated(&self) -> Option<f32> {
        let sum = |from: usize| {
            let (mut n, mut e) = (0u64, 0.0f64);
            for b in from..HIST_BINS {
                n += self.hist_count[b] as u64;
                e += self.hist_energy[b];
            }
            (n, e)
        };
        let (n, e) = sum(0);
        if n == 0 {
            return None;
        }
        let rel = lufs(e / n as f64) - 10.0;
        let from = (((rel - HIST_MIN) / HIST_STEP).ceil().max(0.0) as usize).min(HIST_BINS);
        let (n, e) = sum(from);
        (n > 0).then(|| lufs(e / n as f64) as f32)
    }

    pub fn true_peak_db(&self) -> Option<f32> {
        (self.true_peak > 1e-6).then(|| 20.0 * self.true_peak.log10())
    }

    /// Reading for the UI; resets the per-reading sample peak.
    pub fn read(&mut self) -> MeterReading {
        MeterReading {
            momentary: self.momentary(),
            short_term: self.short_term(),
            integrated: self.integrated(),
            true_peak_db: self.true_peak_db(),
            peak: std::mem::take(&mut self.peak),
            gr: Default::default(),
        }
    }

    /// Restart integrated loudness and the true-peak hold.
    pub fn reset_loudness(&mut self) {
        self.hist_count.fill(0);
        self.hist_energy.fill(0.0);
        self.true_peak = 0.0;
    }

    /// Clear the short windows (after a seek), keeping integrated.
    pub fn reset_windows(&mut self) {
        self.steps.clear();
        self.step_acc = 0.0;
        self.step_pos = 0;
        self.tp_hist = [[0.0; TP_TAPS]; 2];
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analysis::loudness::integrated_lufs;

    fn sine(hz: f32, amp: f32, secs: f32) -> Vec<f32> {
        (0..(48_000.0 * secs) as usize)
            .flat_map(|i| {
                let v = (std::f32::consts::TAU * hz * i as f32 / 48_000.0).sin() * amp;
                [v, v]
            })
            .collect()
    }

    #[test]
    fn ebu_reference_tone_reads_minus_23() {
        // EBU Tech 3341 case 1: 1 kHz stereo sine at −23 dBFS → −23 LUFS (M, S, I)
        let x = sine(1_000.0, 10f32.powf(-23.0 / 20.0), 20.0);
        let mut m = LoudnessMeter::new(48_000);
        for chunk in x.chunks(512) {
            m.push(chunk);
        }
        let r = m.read();
        for (name, v) in [("M", r.momentary), ("S", r.short_term), ("I", r.integrated)] {
            assert!((v.unwrap() + 23.0).abs() < 0.1, "{name} = {v:?}");
        }
        assert!((r.integrated.unwrap() - integrated_lufs(&x, 2, 48_000).unwrap()).abs() < 0.05, "agrees with the offline meter");
        assert!(r.peak[0] > 0.07 && m.read().peak == [0.0; 2], "peak is per reading");
    }

    #[test]
    fn integrated_gate_ignores_silence_and_tracks_level_changes() {
        // 10 s at −20 LUFS, 10 s of silence, 10 s at −40: silence is gated,
        // −40 is under the relative gate (mean −23 − 10 LU) → excluded.
        let mut m = LoudnessMeter::new(48_000);
        m.push(&sine(1_000.0, 10f32.powf(-20.0 / 20.0), 10.0));
        m.push(&vec![0.0; 48_000 * 2 * 10]);
        let after_silence = m.integrated().unwrap();
        assert!((after_silence + 20.0).abs() < 0.1, "{after_silence}");
        assert!(m.momentary().is_none(), "silence reads −∞");
        m.push(&sine(1_000.0, 10f32.powf(-40.0 / 20.0), 10.0));
        let i = m.integrated().unwrap();
        assert!((i + 20.0).abs() < 0.2, "quiet passage gated out: {i}");
        // −31 is above that gate and does count: mean energy of the two
        m.reset_loudness();
        m.push(&sine(1_000.0, 10f32.powf(-20.0 / 20.0), 10.0));
        m.push(&sine(1_000.0, 10f32.powf(-31.0 / 20.0), 10.0));
        let both = m.integrated().unwrap();
        let expect = 10.0 * (0.5 * (10f32.powf(-2.0) + 10f32.powf(-3.1))).log10();
        assert!((both - expect).abs() < 0.2, "{both} vs {expect}");
        m.reset_loudness();
        assert!(m.integrated().is_none());
    }

    #[test]
    fn true_peak_catches_intersample_overs() {
        // fs/4 sine with 45° phase: every sample is at 0.707·A, the true peak is A
        let x: Vec<f32> = (0..48_000)
            .flat_map(|i| {
                let v = (std::f32::consts::FRAC_PI_2 * (i % 4) as f32 + std::f32::consts::FRAC_PI_4).sin();
                [v, v]
            })
            .collect();
        let mut m = LoudnessMeter::new(48_000);
        m.push(&x);
        let sample_peak = x.iter().fold(0.0f32, |a, b| a.max(b.abs()));
        assert!((sample_peak - std::f32::consts::FRAC_1_SQRT_2).abs() < 1e-3);
        let tp = m.true_peak_db().unwrap();
        assert!(tp > -0.6 && tp < 0.3, "true peak ≈ 0 dBTP, got {tp}");
    }
}
