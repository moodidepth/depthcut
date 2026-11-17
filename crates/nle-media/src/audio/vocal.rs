//! "Vocal Clarity" preset: 80 Hz high-pass (rumble, plosives), a gentle
//! presence boost at 3.5 kHz, a little air at 10 kHz and a soft peak limiter
//! with a -1 dBFS ceiling.

use super::eq::{BandKind, Biquad};

pub const CEILING: f32 = 0.891; // -1 dBFS

#[derive(Clone, Debug)]
pub struct VocalClarity {
    hpf: Biquad,
    hpf2: Biquad,
    presence: Biquad,
    air: Biquad,
    /// Current limiter gain.
    gain: f32,
    release: f32,
}

impl VocalClarity {
    pub fn new(sample_rate: f32) -> Self {
        VocalClarity {
            // two cascaded 2nd-order sections ≈ 24 dB/oct
            hpf: Biquad::new(BandKind::HighPass, sample_rate, 80.0, 0.0, 0.541),
            hpf2: Biquad::new(BandKind::HighPass, sample_rate, 80.0, 0.0, 1.307),
            presence: Biquad::new(BandKind::Peaking, sample_rate, 3_500.0, 3.0, 1.0),
            air: Biquad::new(BandKind::HighShelf, sample_rate, 10_000.0, 1.5, 0.7),
            gain: 1.0,
            release: (-1.0 / (0.08 * sample_rate)).exp(),
        }
    }

    pub fn reset(&mut self) {
        for b in [&mut self.hpf, &mut self.hpf2, &mut self.presence, &mut self.air] {
            b.reset();
        }
        self.gain = 1.0;
    }

    /// Process interleaved stereo in place.
    pub fn process(&mut self, buf: &mut [f32]) {
        for frame in buf.chunks_exact_mut(2) {
            let mut peak = 0.0f32;
            for (c, s) in frame.iter_mut().enumerate() {
                let mut x = self.hpf.process(c, *s);
                x = self.hpf2.process(c, x);
                x = self.presence.process(c, x);
                x = self.air.process(c, x);
                *s = x;
                peak = peak.max(x.abs());
            }
            // instant attack, 80 ms release
            let target = if peak > CEILING { CEILING / peak } else { 1.0 };
            self.gain = if target < self.gain { target } else { target + (self.gain - target) * self.release };
            for s in frame.iter_mut() {
                let y = *s * self.gain;
                // soft knee safety above the ceiling
                *s = if y.abs() > CEILING { y.signum() * (CEILING + (1.0 - CEILING) * ((y.abs() - CEILING) / (1.0 - CEILING)).tanh()) } else { y };
            }
        }
    }

    /// Steady-state magnitude response (dB) of the EQ part, for display/tests.
    pub fn response_db(&self, sample_rate: f32, freq: f32) -> f32 {
        [&self.hpf, &self.hpf2, &self.presence, &self.air].iter().map(|b| b.magnitude_db(sample_rate, freq)).sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone(freq: f32, amp: f32, secs: f32) -> Vec<f32> {
        (0..(48_000.0 * secs) as usize)
            .flat_map(|i| {
                let v = (std::f32::consts::TAU * freq * i as f32 / 48_000.0).sin() * amp;
                [v, v]
            })
            .collect()
    }

    fn rms(x: &[f32]) -> f32 {
        (x.iter().map(|s| s * s).sum::<f32>() / x.len() as f32).sqrt()
    }

    #[test]
    fn eq_shape() {
        let v = VocalClarity::new(48_000.0);
        assert!(v.response_db(48_000.0, 30.0) < -12.0, "rumble cut {}", v.response_db(48_000.0, 30.0));
        assert!(v.response_db(48_000.0, 1_000.0).abs() < 1.0, "midrange untouched {}", v.response_db(48_000.0, 1_000.0));
        let p = v.response_db(48_000.0, 3_500.0);
        assert!((2.0..4.5).contains(&p), "presence {p}");
    }

    #[test]
    fn rumble_is_removed_and_peaks_are_limited() {
        let mut v = VocalClarity::new(48_000.0);
        let mut low = tone(40.0, 0.5, 1.0);
        v.process(&mut low);
        assert!(rms(&low[48_000..]) < 0.5 * 0.707 * 0.25, "40 Hz attenuated: {}", rms(&low[48_000..]));

        let mut hot = tone(1_000.0, 1.6, 1.0);
        v.reset();
        v.process(&mut hot);
        let peak = hot.iter().fold(0.0f32, |a, b| a.max(b.abs()));
        assert!(peak <= 1.0, "never clips: {peak}");
        assert!(hot[48_000..].iter().all(|s| s.abs() <= CEILING + 0.02), "held at the ceiling");
        // quiet speech-level signal passes almost unchanged
        let mut quiet = tone(1_000.0, 0.1, 0.5);
        let before = rms(&quiet[12_000..]);
        v.reset();
        v.process(&mut quiet);
        assert!((rms(&quiet[12_000..]) / before - 1.0).abs() < 0.15);
    }
}
