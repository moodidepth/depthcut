//! Real-time 3-band parametric EQ (RBJ biquads): low shelf, peaking mid,
//! high shelf.

use nle_core::EqSettings;

#[derive(Clone, Copy, Debug, Default)]
pub struct Biquad {
    b0: f32,
    b1: f32,
    b2: f32,
    a1: f32,
    a2: f32,
    // per-channel state (stereo)
    z: [[f32; 2]; 2],
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum BandKind {
    LowShelf,
    Peaking,
    HighShelf,
    /// 2nd-order Butterworth-style high-pass (gain ignored).
    HighPass,
    /// 2nd-order resonant low-pass filter (gain ignored).
    LowPass,
    /// Constant 0 dB peak gain band-pass filter (gain ignored).
    BandPass,
}

impl Biquad {
    pub fn new(kind: BandKind, sample_rate: f32, freq: f32, gain_db: f32, q: f32) -> Self {
        let mut b = Biquad::default();
        b.set(kind, sample_rate, freq, gain_db, q);
        b
    }

    pub fn set(&mut self, kind: BandKind, sample_rate: f32, freq: f32, gain_db: f32, q: f32) {
        let freq = freq.clamp(10.0, sample_rate * 0.49);
        let a = 10f32.powf(gain_db / 40.0);
        let w0 = 2.0 * std::f32::consts::PI * freq / sample_rate;
        let (sin, cos) = w0.sin_cos();
        let q = q.max(0.05);
        let alpha = sin / (2.0 * q);
        let (b0, b1, b2, a0, a1, a2) = match kind {
            BandKind::Peaking => (1.0 + alpha * a, -2.0 * cos, 1.0 - alpha * a, 1.0 + alpha / a, -2.0 * cos, 1.0 - alpha / a),
            BandKind::HighPass => ((1.0 + cos) / 2.0, -(1.0 + cos), (1.0 + cos) / 2.0, 1.0 + alpha, -2.0 * cos, 1.0 - alpha),
            BandKind::LowPass => ((1.0 - cos) / 2.0, 1.0 - cos, (1.0 - cos) / 2.0, 1.0 + alpha, -2.0 * cos, 1.0 - alpha),
            BandKind::BandPass => (alpha, 0.0, -alpha, 1.0 + alpha, -2.0 * cos, 1.0 - alpha),
            BandKind::LowShelf => {
                let s = 2.0 * a.sqrt() * alpha;
                (
                    a * ((a + 1.0) - (a - 1.0) * cos + s),
                    2.0 * a * ((a - 1.0) - (a + 1.0) * cos),
                    a * ((a + 1.0) - (a - 1.0) * cos - s),
                    (a + 1.0) + (a - 1.0) * cos + s,
                    -2.0 * ((a - 1.0) + (a + 1.0) * cos),
                    (a + 1.0) + (a - 1.0) * cos - s,
                )
            }
            BandKind::HighShelf => {
                let s = 2.0 * a.sqrt() * alpha;
                (
                    a * ((a + 1.0) + (a - 1.0) * cos + s),
                    -2.0 * a * ((a - 1.0) + (a + 1.0) * cos),
                    a * ((a + 1.0) + (a - 1.0) * cos - s),
                    (a + 1.0) - (a - 1.0) * cos + s,
                    2.0 * ((a - 1.0) - (a + 1.0) * cos),
                    (a + 1.0) - (a - 1.0) * cos - s,
                )
            }
        };
        self.b0 = b0 / a0;
        self.b1 = b1 / a0;
        self.b2 = b2 / a0;
        self.a1 = a1 / a0;
        self.a2 = a2 / a0;
    }

    /// Transposed direct form II.
    #[inline]
    pub fn process(&mut self, ch: usize, x: f32) -> f32 {
        let z = &mut self.z[ch];
        let y = self.b0 * x + z[0];
        z[0] = self.b1 * x - self.a1 * y + z[1];
        z[1] = self.b2 * x - self.a2 * y;
        y
    }

    pub fn reset(&mut self) {
        self.z = [[0.0; 2]; 2];
    }

    /// Magnitude response in dB at `freq`.
    pub fn magnitude_db(&self, sample_rate: f32, freq: f32) -> f32 {
        let w = 2.0 * std::f64::consts::PI * freq as f64 / sample_rate as f64;
        let (c1, s1) = (w.cos(), -w.sin());
        let (c2, s2) = ((2.0 * w).cos(), -(2.0 * w).sin());
        let num = (self.b0 as f64 + self.b1 as f64 * c1 + self.b2 as f64 * c2, self.b1 as f64 * s1 + self.b2 as f64 * s2);
        let den = (1.0 + self.a1 as f64 * c1 + self.a2 as f64 * c2, self.a1 as f64 * s1 + self.a2 as f64 * s2);
        let mag = ((num.0 * num.0 + num.1 * num.1) / (den.0 * den.0 + den.1 * den.1)).sqrt();
        (20.0 * mag.log10()) as f32
    }
}

/// Stereo 3-band EQ.
#[derive(Clone, Debug)]
pub struct ThreeBandEq {
    pub sample_rate: f32,
    settings: EqSettings,
    bands: [Biquad; 3],
}

impl ThreeBandEq {
    pub fn new(sample_rate: f32, settings: EqSettings) -> Self {
        let mut eq = ThreeBandEq { sample_rate, settings, bands: [Biquad::default(); 3] };
        eq.configure(settings);
        eq
    }

    pub fn configure(&mut self, s: EqSettings) {
        self.settings = s;
        let sr = self.sample_rate;
        self.bands[0].set(BandKind::LowShelf, sr, s.low_freq, s.low_gain_db, 0.707);
        self.bands[1].set(BandKind::Peaking, sr, s.mid_freq, s.mid_gain_db, s.mid_q);
        self.bands[2].set(BandKind::HighShelf, sr, s.high_freq, s.high_gain_db, 0.707);
    }

    /// Reconfigure only if the settings changed (cheap per-block call).
    pub fn update(&mut self, s: &EqSettings) {
        if *s != self.settings {
            self.configure(*s);
        }
    }

    pub fn is_active(&self) -> bool {
        self.settings.enabled && (self.settings.low_gain_db != 0.0 || self.settings.mid_gain_db != 0.0 || self.settings.high_gain_db != 0.0)
    }

    /// Process an interleaved stereo buffer in place.
    pub fn process(&mut self, buf: &mut [f32]) {
        if !self.is_active() {
            return;
        }
        for frame in buf.chunks_exact_mut(2) {
            for (ch, s) in frame.iter_mut().enumerate() {
                let mut x = *s;
                for b in &mut self.bands {
                    x = b.process(ch, x);
                }
                *s = x;
            }
        }
    }

    pub fn reset(&mut self) {
        for b in &mut self.bands {
            b.reset();
        }
    }

    /// Combined response in dB (for the inspector curve display).
    pub fn response_db(&self, freq: f32) -> f32 {
        if !self.settings.enabled {
            return 0.0;
        }
        self.bands.iter().map(|b| b.magnitude_db(self.sample_rate, freq)).sum()
    }
}

/// Soft-knee brickwall limiter keeping master output smoothly bounded inside `[-1.0, 1.0]`.
pub fn soft_limit(sample: f32, ceiling: f32) -> f32 {
    let c = ceiling.clamp(0.5, 0.99);
    let a = sample.abs();
    if a <= c {
        sample
    } else {
        let head = (1.0 - c).max(1e-4);
        sample.signum() * (c + head * ((a - c) / head).tanh()).clamp(0.0, 1.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn band_responses() {
        let s = EqSettings { enabled: true, low_gain_db: 6.0, mid_gain_db: -9.0, high_gain_db: 4.0, ..Default::default() };
        let eq = ThreeBandEq::new(48_000.0, s);
        assert!((eq.bands[0].magnitude_db(48_000.0, 20.0) - 6.0).abs() < 0.3, "low shelf at DC");
        assert!((eq.bands[1].magnitude_db(48_000.0, 1_000.0) + 9.0).abs() < 0.05, "peak centre");
        assert!((eq.bands[2].magnitude_db(48_000.0, 20_000.0) - 4.0).abs() < 0.5, "high shelf");
        assert!(eq.bands[1].magnitude_db(48_000.0, 50.0).abs() < 0.5, "peak far away is flat");
    }

    #[test]
    fn processing_matches_response() {
        let s = EqSettings { enabled: true, mid_gain_db: 12.0, mid_freq: 1_000.0, ..Default::default() };
        let mut eq = ThreeBandEq::new(48_000.0, s);
        let mut buf: Vec<f32> = (0..48_000)
            .flat_map(|i| {
                let v = (2.0 * std::f32::consts::PI * 1_000.0 * i as f32 / 48_000.0).sin() * 0.1;
                [v, v]
            })
            .collect();
        eq.process(&mut buf);
        let peak = buf[40_000..].iter().fold(0.0f32, |a, b| a.max(b.abs()));
        assert!((peak / 0.1 - 10f32.powf(12.0 / 20.0)).abs() < 0.05, "gain {}", peak / 0.1);
    }
}
