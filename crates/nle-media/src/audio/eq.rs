//! Real-time parametric EQ (RBJ biquads): HPF / LPF (12–48 dB/oct
//! Butterworth cascades) and six shelf / bell / notch bands.

use nle_core::{EqBand, EqBandType, ParametricEq};

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
    /// Band-reject (gain ignored).
    Notch,
}

impl BandKind {
    fn from_band(t: EqBandType) -> Self {
        match t {
            EqBandType::LowShelf => BandKind::LowShelf,
            EqBandType::Bell => BandKind::Peaking,
            EqBandType::HighShelf => BandKind::HighShelf,
            EqBandType::Notch => BandKind::Notch,
        }
    }
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
            BandKind::Notch => (1.0, -2.0 * cos, 1.0, 1.0 + alpha, -2.0 * cos, 1.0 - alpha),
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
        (20.0 * mag.max(1e-12).log10()) as f32
    }
}

/// Stereo 6-band parametric EQ with cascaded Butterworth HPF / LPF.
#[derive(Clone, Debug)]
pub struct ParametricEqProc {
    pub sample_rate: f32,
    settings: ParametricEq,
    /// Active sections in processing order (HPF, LPF, bands).
    sections: Vec<Biquad>,
    output: f32,
}

impl ParametricEqProc {
    pub fn new(sample_rate: f32, settings: ParametricEq) -> Self {
        let mut eq = ParametricEqProc { sample_rate, settings, sections: Vec::new(), output: 1.0 };
        eq.configure(settings);
        eq
    }

    /// Rebuild the filter sections. Sections that survive keep their state,
    /// so dragging a band while playing does not click.
    pub fn configure(&mut self, s: ParametricEq) {
        self.settings = s;
        let sr = self.sample_rate;
        let mut want: Vec<(BandKind, f32, f32, f32)> = Vec::new();
        if s.enabled {
            if s.hpf.enabled {
                want.extend(s.hpf.slope.section_qs().iter().map(|q| (BandKind::HighPass, s.hpf.freq, 0.0, *q)));
            }
            if s.lpf.enabled {
                want.extend(s.lpf.slope.section_qs().iter().map(|q| (BandKind::LowPass, s.lpf.freq, 0.0, *q)));
            }
            for b in s.bands.iter().filter(|b| b.is_active()) {
                want.push((BandKind::from_band(b.kind), b.freq, b.gain_db, b.q));
            }
        }
        self.sections.resize(want.len(), Biquad::default());
        for (sec, (kind, f, g, q)) in self.sections.iter_mut().zip(want) {
            sec.set(kind, sr, f, g, q);
        }
        self.output = if s.enabled { 10f32.powf(s.output_db / 20.0) } else { 1.0 };
    }

    /// Reconfigure only if the settings changed (cheap per-block call).
    pub fn update(&mut self, s: &ParametricEq) {
        if *s != self.settings {
            self.configure(*s);
        }
    }

    pub fn is_active(&self) -> bool {
        self.settings.is_active()
    }

    /// Process an interleaved stereo buffer in place.
    pub fn process(&mut self, buf: &mut [f32]) {
        if !self.is_active() {
            return;
        }
        let out = self.output;
        for frame in buf.as_chunks_mut::<2>().0 {
            for (ch, s) in frame.iter_mut().enumerate() {
                let mut x = *s;
                for b in &mut self.sections {
                    x = b.process(ch, x);
                }
                *s = x * out;
            }
        }
    }

    pub fn reset(&mut self) {
        for b in &mut self.sections {
            b.reset();
        }
    }

    /// Combined response in dB (for the inspector curve display).
    pub fn response_db(&self, freq: f32) -> f32 {
        if !self.is_active() {
            return 0.0;
        }
        self.sections.iter().map(|b| b.magnitude_db(self.sample_rate, freq)).sum::<f32>() + self.settings.output_db
    }

    /// Response of a single band in dB (for per-band curve shading).
    pub fn band_response_db(sample_rate: f32, band: &EqBand, freq: f32) -> f32 {
        if !band.is_active() {
            return 0.0;
        }
        Biquad::new(BandKind::from_band(band.kind), sample_rate, band.freq, band.gain_db, band.q).magnitude_db(sample_rate, freq)
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
    use nle_core::{EqFilter, FilterSlope};

    fn eq_with(f: impl FnOnce(&mut ParametricEq)) -> ParametricEqProc {
        let mut p = ParametricEq { enabled: true, ..Default::default() };
        f(&mut p);
        ParametricEqProc::new(48_000.0, p)
    }

    #[test]
    fn band_responses() {
        let eq = eq_with(|p| {
            p.bands[0].gain_db = 6.0;
            p.bands[2].gain_db = -9.0;
            p.bands[2].freq = 1_000.0;
            p.bands[5].gain_db = 4.0;
        });
        assert!((eq.response_db(20.0) - 6.0).abs() < 0.4, "low shelf at DC {}", eq.response_db(20.0));
        assert!((eq.response_db(1_000.0) + 9.0).abs() < 0.3, "bell centre {}", eq.response_db(1_000.0));
        assert!((eq.response_db(20_000.0) - 4.0).abs() < 0.6, "high shelf {}", eq.response_db(20_000.0));
        // untouched bands add no sections
        assert_eq!(eq.sections.len(), 3);
        assert_eq!(eq_with(|_| {}).sections.len(), 0);
        assert!(!eq_with(|_| {}).is_active());
    }

    #[test]
    fn filter_slopes_and_notch() {
        for (slope, want) in [(FilterSlope::Db12, -12.0), (FilterSlope::Db24, -24.0), (FilterSlope::Db48, -48.0)] {
            let eq = eq_with(|p| p.hpf = EqFilter { enabled: true, freq: 400.0, slope });
            assert!((eq.response_db(400.0) + 3.0).abs() < 0.2, "{slope:?}: −3 dB at the corner, got {}", eq.response_db(400.0));
            // two octaves below the corner the slope is close to its asymptote
            let oct = eq.response_db(50.0) - eq.response_db(100.0);
            assert!((oct - want).abs() < 1.0, "{slope:?}: {oct} dB/oct");
            assert!(eq.response_db(4_000.0).abs() < 0.1, "passband flat");
        }
        let eq = eq_with(|p| p.lpf = EqFilter { enabled: true, freq: 2_000.0, slope: FilterSlope::Db24 });
        assert!(eq.response_db(8_000.0) < -40.0);
        let eq = eq_with(|p| p.bands[3] = EqBand { enabled: true, kind: EqBandType::Notch, freq: 60.0, gain_db: 0.0, q: 8.0 });
        assert!(eq.response_db(60.0) < -40.0, "hum notch {}", eq.response_db(60.0));
        assert!(eq.response_db(120.0).abs() < 0.5);
    }

    #[test]
    fn processing_matches_response() {
        let mut eq = eq_with(|p| {
            p.bands[2].gain_db = 12.0;
            p.bands[2].freq = 1_000.0;
        });
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
