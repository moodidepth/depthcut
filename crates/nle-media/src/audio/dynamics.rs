//! Stereo-linked dynamics: downward expander / gate, feed-forward soft-knee
//! compressor and a split-band de-esser. All are zero-latency (no
//! look-ahead) so preview and export stay sample-aligned with video.

use super::eq::{BandKind, Biquad};
use nle_core::{CompressorSettings, DeEsserSettings, GateMode, GateSettings};

#[inline]
fn coef(ms: f32, sr: f32) -> f32 {
    (-1.0 / (ms.max(0.01) * 0.001 * sr)).exp()
}

#[inline]
fn lin_to_db(x: f32) -> f32 {
    20.0 * x.max(1e-9).log10()
}

#[inline]
fn db_to_lin(db: f32) -> f32 {
    10f32.powf(db / 20.0)
}

/// Soft-knee compressor gain computer: gain change in dB (≤ 0) for an
/// input level `l` (dB).
pub fn compressor_curve(l: f32, threshold: f32, ratio: f32, knee: f32) -> f32 {
    let slope = 1.0 / ratio.max(1.0) - 1.0;
    let over = l - threshold;
    if knee > 0.0 && 2.0 * over.abs() <= knee {
        slope * (over + 0.5 * knee).powi(2) / (2.0 * knee)
    } else if over > 0.0 {
        slope * over
    } else {
        0.0
    }
}

/// Peak detector (instant attack, `release` decay) → static curve → gain
/// smoothing with `attack` (release is already in the detector), so a
/// steady tone gets exactly the static gain change instead of an average
/// over its waveform.
pub struct Compressor {
    sr: f32,
    s: CompressorSettings,
    att: f32,
    rel: f32,
    det: f32,
    /// Smoothed gain change (dB, ≤ 0).
    env_db: f32,
    /// Deepest reduction since the last [`Compressor::take_gr`] (dB, ≥ 0).
    gr_max: f32,
}

impl Compressor {
    pub fn new(sr: f32, s: CompressorSettings) -> Self {
        let mut c = Compressor { sr, s, att: 0.0, rel: 0.0, det: 0.0, env_db: 0.0, gr_max: 0.0 };
        c.configure(s);
        c
    }

    fn configure(&mut self, s: CompressorSettings) {
        self.s = s;
        self.att = coef(s.attack_ms, self.sr);
        self.rel = coef(s.release_ms, self.sr);
    }

    pub fn update(&mut self, s: &CompressorSettings) {
        if *s != self.s {
            self.configure(*s);
        }
    }

    pub fn process(&mut self, buf: &mut [f32]) {
        let s = self.s;
        let makeup = s.makeup_db;
        for f in buf.as_chunks_mut::<2>().0 {
            let x = f[0].abs().max(f[1].abs());
            self.det = if x > self.det { x } else { x + (self.det - x) * self.rel };
            let target = compressor_curve(lin_to_db(self.det), s.threshold_db, s.ratio, s.knee_db);
            self.env_db = if target < self.env_db { target + (self.env_db - target) * self.att } else { target };
            self.gr_max = self.gr_max.max(-self.env_db);
            let g = db_to_lin(self.env_db + makeup);
            f[0] *= g;
            f[1] *= g;
        }
    }

    pub fn take_gr(&mut self) -> f32 {
        std::mem::take(&mut self.gr_max)
    }

    pub fn reset(&mut self) {
        self.det = 0.0;
        self.env_db = 0.0;
    }
}

pub struct Gate {
    sr: f32,
    s: GateSettings,
    att: f32,
    rel: f32,
    /// Fast peak follower on the input (linear).
    det: f32,
    det_rel: f32,
    hold_left: u32,
    hold_len: u32,
    env_db: f32,
    gr_max: f32,
}

impl Gate {
    pub fn new(sr: f32, s: GateSettings) -> Self {
        let mut g = Gate { sr, s, att: 0.0, rel: 0.0, det: 0.0, det_rel: coef(20.0, sr), hold_left: 0, hold_len: 0, env_db: 0.0, gr_max: 0.0 };
        g.configure(s);
        g
    }

    fn configure(&mut self, s: GateSettings) {
        self.s = s;
        self.att = coef(s.attack_ms, self.sr);
        self.rel = coef(s.release_ms, self.sr);
        self.hold_len = (s.hold_ms.max(0.0) * 0.001 * self.sr) as u32;
    }

    pub fn update(&mut self, s: &GateSettings) {
        if *s != self.s {
            self.configure(*s);
        }
    }

    /// Static gain change (dB, ≤ 0) for a detector level `l` (dB).
    pub fn curve(s: &GateSettings, l: f32) -> f32 {
        if l >= s.threshold_db {
            return 0.0;
        }
        let range = -s.range_db.abs();
        match s.mode {
            GateMode::Gate => range,
            GateMode::Expander => ((l - s.threshold_db) * (s.ratio.max(1.0) - 1.0)).max(range),
        }
    }

    pub fn process(&mut self, buf: &mut [f32]) {
        let s = self.s;
        for f in buf.as_chunks_mut::<2>().0 {
            let x = f[0].abs().max(f[1].abs());
            self.det = if x > self.det { x } else { x + (self.det - x) * self.det_rel };
            let l = lin_to_db(self.det);
            let mut target = Gate::curve(&s, l);
            if target >= 0.0 {
                self.hold_left = self.hold_len;
            } else if self.hold_left > 0 {
                self.hold_left -= 1;
                target = 0.0;
            }
            // opening (toward 0 dB) uses attack, closing uses release
            let a = if target > self.env_db { self.att } else { self.rel };
            self.env_db = target + (self.env_db - target) * a;
            self.gr_max = self.gr_max.max(-self.env_db);
            let g = db_to_lin(self.env_db);
            f[0] *= g;
            f[1] *= g;
        }
    }

    pub fn take_gr(&mut self) -> f32 {
        std::mem::take(&mut self.gr_max)
    }

    pub fn reset(&mut self) {
        self.det = 0.0;
        self.hold_left = 0;
        self.env_db = 0.0;
    }
}

/// De-esser as a dynamic high shelf: a high-passed sidechain detects
/// sibilance above `freq`, and while it is over the threshold a high shelf
/// at `freq` dips by up to `reduction_db`. Idle (0 dB shelf) it is exactly
/// transparent; vowels below `freq` are never touched.
pub struct DeEsser {
    sr: f32,
    s: DeEsserSettings,
    side: Biquad,
    shelf: Biquad,
    shelf_db: f32,
    det: f32,
    att: f32,
    rel: f32,
    env_db: f32,
    gr_max: f32,
}

/// Shelf coefficients are refreshed every this many frames.
const DE_ESS_UPDATE: usize = 16;

impl DeEsser {
    pub fn new(sr: f32, s: DeEsserSettings) -> Self {
        let mut d =
            DeEsser { sr, s, side: Biquad::default(), shelf: Biquad::default(), shelf_db: 0.0, det: 0.0, att: coef(0.5, sr), rel: coef(60.0, sr), env_db: 0.0, gr_max: 0.0 };
        d.configure(s);
        d
    }

    fn freq(&self) -> f32 {
        self.s.freq.clamp(2_000.0, 16_000.0)
    }

    fn configure(&mut self, s: DeEsserSettings) {
        self.s = s;
        self.side.set(BandKind::HighPass, self.sr, self.freq(), 0.0, 0.707);
        self.shelf.set(BandKind::HighShelf, self.sr, self.freq(), self.shelf_db, 0.707);
    }

    pub fn update(&mut self, s: &DeEsserSettings) {
        if *s != self.s {
            self.configure(*s);
        }
    }

    pub fn process(&mut self, buf: &mut [f32]) {
        let s = self.s;
        let max_cut = -s.reduction_db.abs();
        let freq = self.freq();
        for (i, f) in buf.as_chunks_mut::<2>().0.iter_mut().enumerate() {
            let hl = self.side.process(0, f[0]);
            let hr = self.side.process(1, f[1]);
            let x = hl.abs().max(hr.abs());
            self.det = if x > self.det { x } else { x + (self.det - x) * self.rel };
            let over = lin_to_db(self.det) - s.threshold_db;
            // 4:1 above the threshold, never more than the set reduction
            let target = if over > 0.0 { (-0.75 * over).max(max_cut) } else { 0.0 };
            self.env_db = if target < self.env_db { target + (self.env_db - target) * self.att } else { target };
            self.gr_max = self.gr_max.max(-self.env_db);
            if i % DE_ESS_UPDATE == 0 && (self.env_db - self.shelf_db).abs() > 0.01 {
                self.shelf_db = if self.env_db > -0.01 { 0.0 } else { self.env_db };
                self.shelf.set(BandKind::HighShelf, self.sr, freq, self.shelf_db, 0.707);
            }
            if self.shelf_db != 0.0 {
                f[0] = self.shelf.process(0, f[0]);
                f[1] = self.shelf.process(1, f[1]);
            } else {
                // keep the shelf state warm so re-engaging does not click
                self.shelf.process(0, f[0]);
                self.shelf.process(1, f[1]);
            }
        }
    }

    pub fn take_gr(&mut self) -> f32 {
        std::mem::take(&mut self.gr_max)
    }

    pub fn reset(&mut self) {
        self.side.reset();
        self.shelf.reset();
        self.det = 0.0;
        self.env_db = 0.0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone(hz: f32, amp: f32, secs: f32) -> Vec<f32> {
        (0..(48_000.0 * secs) as usize)
            .flat_map(|i| {
                let v = (std::f32::consts::TAU * hz * i as f32 / 48_000.0).sin() * amp;
                [v, v]
            })
            .collect()
    }

    fn peak(x: &[f32]) -> f32 {
        x.iter().fold(0.0f32, |m, s| m.max(s.abs()))
    }

    #[test]
    fn compressor_static_curve_and_steady_state() {
        assert_eq!(compressor_curve(-30.0, -20.0, 4.0, 0.0), 0.0);
        assert!((compressor_curve(-8.0, -20.0, 4.0, 0.0) + 9.0).abs() < 1e-4, "12 dB over at 4:1 → −9 dB");
        // the knee is continuous at both edges
        assert!((compressor_curve(-23.0, -20.0, 4.0, 6.0)).abs() < 1e-4);
        assert!((compressor_curve(-17.0, -20.0, 4.0, 6.0) - compressor_curve(-17.0, -20.0, 4.0, 0.0)).abs() < 1e-4);

        let s = CompressorSettings { enabled: true, threshold_db: -20.0, ratio: 4.0, knee_db: 0.0, attack_ms: 5.0, release_ms: 50.0, makeup_db: 0.0 };
        let mut c = Compressor::new(48_000.0, s);
        let mut x = tone(1_000.0, db_to_lin(-8.0), 1.0);
        c.process(&mut x);
        let out_db = lin_to_db(peak(&x[48_000..]));
        assert!((out_db + 17.0).abs() < 0.6, "−8 dBFS in → −17 dBFS out, got {out_db}");
        assert!((c.take_gr() - 9.0).abs() < 0.6);
        // below threshold: untouched
        let mut q = tone(1_000.0, db_to_lin(-30.0), 0.5);
        let before = q.clone();
        c.reset();
        c.process(&mut q);
        assert!(q.iter().zip(&before).all(|(a, b)| (a - b).abs() < 1e-6));
    }

    #[test]
    fn gate_closes_on_room_tone_and_opens_on_speech() {
        let s = GateSettings { enabled: true, mode: GateMode::Gate, threshold_db: -40.0, range_db: 40.0, attack_ms: 1.0, hold_ms: 20.0, release_ms: 30.0, ratio: 2.0 };
        let mut g = Gate::new(48_000.0, s);
        let mut noise = tone(200.0, db_to_lin(-55.0), 0.5);
        g.process(&mut noise);
        assert!(lin_to_db(peak(&noise[24_000..])) < -90.0, "room tone gated: {}", lin_to_db(peak(&noise[24_000..])));
        let mut loud = tone(200.0, db_to_lin(-12.0), 0.2);
        g.process(&mut loud);
        assert!((lin_to_db(peak(&loud[4_800..])) + 12.0).abs() < 0.2, "speech passes");

        // expander 2:1: 10 dB under threshold → 10 dB extra cut
        let e = GateSettings { mode: GateMode::Expander, ratio: 2.0, threshold_db: -40.0, range_db: 30.0, ..s };
        assert!((Gate::curve(&e, -50.0) + 10.0).abs() < 1e-4);
        assert_eq!(Gate::curve(&e, -90.0), -30.0, "limited by range");
    }

    #[test]
    fn de_esser_tames_sibilance_only() {
        let s = DeEsserSettings { enabled: true, freq: 5_000.0, threshold_db: -30.0, reduction_db: 10.0 };
        let mut d = DeEsser::new(48_000.0, s);
        let mut ess = tone(8_000.0, db_to_lin(-10.0), 0.5);
        d.process(&mut ess);
        let cut = lin_to_db(peak(&ess[12_000..])) + 10.0;
        // 8 kHz sits above the 5 kHz shelf midpoint: most of the 10 dB
        assert!(cut < -7.0 && cut > -10.5, "sibilance cut: {cut}");
        assert!((d.take_gr() - 10.0).abs() < 0.1);
        // a loud vowel at 300 Hz is not touched
        d.reset();
        let mut vowel = tone(300.0, db_to_lin(-6.0), 0.5);
        d.process(&mut vowel);
        assert!((lin_to_db(peak(&vowel[12_000..])) + 6.0).abs() < 0.2);
    }
}
