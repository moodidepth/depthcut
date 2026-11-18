//! Creative inserts (stereo / ping-pong delay, Freeverb-style reverb), the
//! dialogue leveler, and [`FxChain`] — the insert chain shared by clips and
//! busses: expander/gate → compressor → de-esser → delay → reverb.

use super::dynamics::{Compressor, DeEsser, Gate};
use nle_core::{AudioFx, DelaySettings, LevelerSettings, ReverbSettings};

// ---------------------------------------------------------------------------
// Delay
// ---------------------------------------------------------------------------

const MAX_DELAY_SECS: f32 = 2.0;

pub struct Delay {
    sr: f32,
    s: DelaySettings,
    lines: [Vec<f32>; 2],
    w: usize,
    /// Smoothed delay time in samples (time changes glide, never click).
    cur: f32,
    damp: [f32; 2],
    damp_a: f32,
}

impl Delay {
    pub fn new(sr: f32, s: DelaySettings) -> Self {
        let n = (MAX_DELAY_SECS * sr) as usize + 4;
        let mut d = Delay { sr, s, lines: [vec![0.0; n], vec![0.0; n]], w: 0, cur: s.time_ms * 0.001 * sr, damp: [0.0; 2], damp_a: 0.0 };
        d.configure(s);
        d
    }

    fn configure(&mut self, s: DelaySettings) {
        self.s = s;
        self.damp_a = (-std::f32::consts::TAU * s.damping_hz.clamp(500.0, 20_000.0) / self.sr).exp();
    }

    pub fn update(&mut self, s: &DelaySettings) {
        if *s != self.s {
            self.configure(*s);
        }
    }

    fn read(line: &[f32], w: usize, delay: f32) -> f32 {
        let n = line.len();
        let pos = w as f32 + n as f32 - delay;
        let i = pos.floor();
        let f = pos - i;
        let i = i as usize % n;
        line[i] * (1.0 - f) + line[(i + 1) % n] * f
    }

    pub fn process(&mut self, buf: &mut [f32]) {
        let s = self.s;
        let target = (s.time_ms.clamp(1.0, MAX_DELAY_SECS * 1000.0 - 1.0) * 0.001 * self.sr).max(1.0);
        let glide = 1.0 - (-1.0 / (0.05 * self.sr)).exp();
        let fb = s.feedback.clamp(0.0, 0.95);
        let mix = s.mix.clamp(0.0, 1.0);
        let n = self.lines[0].len();
        for f in buf.as_chunks_mut::<2>().0 {
            self.cur += (target - self.cur) * glide;
            let mut wet = [Self::read(&self.lines[0], self.w, self.cur), Self::read(&self.lines[1], self.w, self.cur)];
            for (c, v) in wet.iter_mut().enumerate() {
                self.damp[c] = *v * (1.0 - self.damp_a) + self.damp[c] * self.damp_a;
                *v = self.damp[c];
            }
            if s.ping_pong {
                // mono input into the left line; repeats bounce L → R → L
                self.lines[0][self.w] = 0.5 * (f[0] + f[1]) + wet[1] * fb;
                self.lines[1][self.w] = wet[0] * fb;
            } else {
                self.lines[0][self.w] = f[0] + wet[0] * fb;
                self.lines[1][self.w] = f[1] + wet[1] * fb;
            }
            self.w = (self.w + 1) % n;
            f[0] += wet[0] * mix;
            f[1] += wet[1] * mix;
        }
    }

    pub fn reset(&mut self) {
        for l in &mut self.lines {
            l.fill(0.0);
        }
        self.damp = [0.0; 2];
    }
}

// ---------------------------------------------------------------------------
// Reverb (Schroeder/Moorer "Freeverb" topology)
// ---------------------------------------------------------------------------

const COMBS: [usize; 8] = [1116, 1188, 1277, 1356, 1422, 1491, 1557, 1617];
const ALLPASSES: [usize; 4] = [556, 441, 341, 225];
const SPREAD: usize = 23;

struct Comb {
    buf: Vec<f32>,
    i: usize,
    store: f32,
}

impl Comb {
    #[inline]
    fn process(&mut self, x: f32, feedback: f32, damp: f32) -> f32 {
        let y = self.buf[self.i];
        self.store = y * (1.0 - damp) + self.store * damp;
        self.buf[self.i] = x + self.store * feedback;
        self.i = (self.i + 1) % self.buf.len();
        y
    }
}

struct Allpass {
    buf: Vec<f32>,
    i: usize,
}

impl Allpass {
    #[inline]
    fn process(&mut self, x: f32) -> f32 {
        let b = self.buf[self.i];
        self.buf[self.i] = x + b * 0.5;
        self.i = (self.i + 1) % self.buf.len();
        b - x
    }
}

pub struct Reverb {
    s: ReverbSettings,
    combs: [Vec<Comb>; 2],
    allpasses: [Vec<Allpass>; 2],
    pre: Vec<[f32; 2]>,
    pre_w: usize,
    pre_len: usize,
    sr: f32,
}

impl Reverb {
    pub fn new(sr: f32, s: ReverbSettings) -> Self {
        let scale = sr / 44_100.0;
        let len = |n: usize, ch: usize| (((n + ch * SPREAD) as f32 * scale) as usize).max(1);
        let combs = [0, 1].map(|ch| COMBS.iter().map(|n| Comb { buf: vec![0.0; len(*n, ch)], i: 0, store: 0.0 }).collect());
        let allpasses = [0, 1].map(|ch| ALLPASSES.iter().map(|n| Allpass { buf: vec![0.0; len(*n, ch)], i: 0 }).collect());
        let mut r = Reverb { s, combs, allpasses, pre: vec![[0.0; 2]; (0.2 * sr) as usize + 1], pre_w: 0, pre_len: 0, sr };
        r.configure(s);
        r
    }

    fn configure(&mut self, s: ReverbSettings) {
        self.s = s;
        self.pre_len = ((s.pre_delay_ms.clamp(0.0, 199.0) * 0.001 * self.sr) as usize).min(self.pre.len() - 1);
    }

    pub fn update(&mut self, s: &ReverbSettings) {
        if *s != self.s {
            self.configure(*s);
        }
    }

    pub fn process(&mut self, buf: &mut [f32]) {
        let s = self.s;
        let feedback = s.room_size.clamp(0.0, 1.0) * 0.28 + 0.7;
        let damp = s.damping.clamp(0.0, 1.0) * 0.4;
        let wet = s.mix.clamp(0.0, 1.0) * 3.0;
        let width = s.width.clamp(0.0, 1.0);
        let (wet1, wet2) = (wet * (width * 0.5 + 0.5), wet * ((1.0 - width) * 0.5));
        let n = self.pre.len();
        for f in buf.as_chunks_mut::<2>().0 {
            self.pre[self.pre_w] = [f[0], f[1]];
            let r = (self.pre_w + n - self.pre_len) % n;
            self.pre_w = (self.pre_w + 1) % n;
            let input = (self.pre[r][0] + self.pre[r][1]) * 0.015;
            let mut out = [0.0f32; 2];
            for ch in 0..2 {
                let mut acc = 0.0;
                for c in &mut self.combs[ch] {
                    acc += c.process(input, feedback, damp);
                }
                for a in &mut self.allpasses[ch] {
                    acc = a.process(acc);
                }
                out[ch] = acc;
            }
            f[0] += out[0] * wet1 + out[1] * wet2;
            f[1] += out[1] * wet1 + out[0] * wet2;
        }
    }

    pub fn reset(&mut self) {
        for ch in 0..2 {
            for c in &mut self.combs[ch] {
                c.buf.fill(0.0);
                c.store = 0.0;
            }
            for a in &mut self.allpasses[ch] {
                a.buf.fill(0.0);
            }
        }
        self.pre.fill([0.0; 2]);
    }
}

// ---------------------------------------------------------------------------
// Dialogue leveler
// ---------------------------------------------------------------------------

/// Rides the gain toward `target_db` (RMS over ~300 ms). Silence (below
/// `floor_db`) freezes the gain so pauses are not pumped up.
pub struct Leveler {
    sr: f32,
    s: LevelerSettings,
    /// Slow (300 ms) loudness the gain follows.
    ms: f32,
    ms_a: f32,
    /// Fast (10 ms) level that decides whether anyone is talking, so the
    /// gain freezes as soon as a phrase ends instead of creeping up on the
    /// slow detector's decay tail (which would pump room tone).
    fast: f32,
    fast_a: f32,
    gain_db: f32,
    gain_a: f32,
}

impl Leveler {
    pub fn new(sr: f32, s: LevelerSettings) -> Self {
        let mut l = Leveler { sr, s, ms: 0.0, ms_a: (-1.0 / (0.3 * sr)).exp(), fast: 0.0, fast_a: (-1.0 / (0.01 * sr)).exp(), gain_db: 0.0, gain_a: 0.0 };
        l.configure(s);
        l
    }

    fn configure(&mut self, s: LevelerSettings) {
        self.s = s;
        self.gain_a = (-1.0 / (s.speed_ms.clamp(20.0, 5_000.0) * 0.001 * self.sr)).exp();
    }

    pub fn update(&mut self, s: &LevelerSettings) {
        if *s != self.s {
            self.configure(*s);
        }
    }

    pub fn gain_db(&self) -> f32 {
        self.gain_db
    }

    pub fn process(&mut self, buf: &mut [f32]) {
        let s = self.s;
        let max = s.max_gain_db.abs();
        for f in buf.as_chunks_mut::<2>().0 {
            let m = 0.5 * (f[0] * f[0] + f[1] * f[1]);
            self.ms = m + (self.ms - m) * self.ms_a;
            self.fast = m + (self.fast - m) * self.fast_a;
            let level = 10.0 * self.ms.max(1e-12).log10();
            let present = 10.0 * self.fast.max(1e-12).log10() > s.floor_db;
            if present && level > s.floor_db {
                let want = (s.target_db - level).clamp(-max, max);
                self.gain_db = want + (self.gain_db - want) * self.gain_a;
            }
            let g = 10f32.powf(self.gain_db / 20.0);
            f[0] *= g;
            f[1] *= g;
        }
    }

    pub fn reset(&mut self) {
        self.ms = 0.0;
        self.fast = 0.0;
        self.gain_db = 0.0;
    }
}

// ---------------------------------------------------------------------------
// Insert chain
// ---------------------------------------------------------------------------

/// Expander/gate → compressor → de-esser → delay → reverb. Processors are
/// created on first use, so an idle chain costs nothing.
pub struct FxChain {
    sr: f32,
    gate: Option<Gate>,
    comp: Option<Compressor>,
    de_esser: Option<DeEsser>,
    delay: Option<Delay>,
    reverb: Option<Reverb>,
}

/// Gain reduction (dB, ≥ 0) of the dynamics stages since the last read.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct GainReduction {
    pub gate: f32,
    pub comp: f32,
    pub de_esser: f32,
}

impl FxChain {
    pub fn new(sr: f32) -> Self {
        FxChain { sr, gate: None, comp: None, de_esser: None, delay: None, reverb: None }
    }

    pub fn process(&mut self, fx: &AudioFx, buf: &mut [f32]) {
        let sr = self.sr;
        if fx.gate.enabled {
            let g = self.gate.get_or_insert_with(|| Gate::new(sr, fx.gate));
            g.update(&fx.gate);
            g.process(buf);
        } else {
            self.gate = None;
        }
        if fx.compressor.enabled {
            let c = self.comp.get_or_insert_with(|| Compressor::new(sr, fx.compressor));
            c.update(&fx.compressor);
            c.process(buf);
        } else {
            self.comp = None;
        }
        if fx.de_esser.enabled {
            let d = self.de_esser.get_or_insert_with(|| DeEsser::new(sr, fx.de_esser));
            d.update(&fx.de_esser);
            d.process(buf);
        } else {
            self.de_esser = None;
        }
        if fx.delay.enabled {
            let d = self.delay.get_or_insert_with(|| Delay::new(sr, fx.delay));
            d.update(&fx.delay);
            d.process(buf);
        } else {
            self.delay = None;
        }
        if fx.reverb.enabled {
            let r = self.reverb.get_or_insert_with(|| Reverb::new(sr, fx.reverb));
            r.update(&fx.reverb);
            r.process(buf);
        } else {
            self.reverb = None;
        }
    }

    pub fn take_gr(&mut self) -> GainReduction {
        GainReduction {
            gate: self.gate.as_mut().map_or(0.0, |g| g.take_gr()),
            comp: self.comp.as_mut().map_or(0.0, |c| c.take_gr()),
            de_esser: self.de_esser.as_mut().map_or(0.0, |d| d.take_gr()),
        }
    }

    pub fn reset(&mut self) {
        *self = FxChain::new(self.sr);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn impulse(secs: f32) -> Vec<f32> {
        let mut v = vec![0.0f32; (48_000.0 * secs) as usize * 2];
        v[0] = 1.0;
        v[1] = 1.0;
        v
    }

    #[test]
    fn delay_repeats_at_the_set_time_with_feedback() {
        let s = DelaySettings { enabled: true, time_ms: 100.0, feedback: 0.5, mix: 1.0, damping_hz: 20_000.0, ping_pong: false };
        let mut d = Delay::new(48_000.0, s);
        let mut x = impulse(0.5);
        d.process(&mut x);
        let around = |c: usize| (c - 3..c + 4).map(|i| x[2 * i].abs()).fold(0.0f32, f32::max);
        assert!(around(4_800) > 0.8, "first repeat at 100 ms: {}", around(4_800));
        let second = around(9_600);
        assert!((0.35..0.6).contains(&second), "second repeat ≈ feedback: {second}");
        assert!(x[2 * 2_400].abs() < 1e-6, "silence between repeats");

        // ping-pong: first repeat on the left only, second on the right
        let mut p = Delay::new(48_000.0, DelaySettings { ping_pong: true, ..s });
        let mut y = impulse(0.5);
        p.process(&mut y);
        let ch = |c: usize, side: usize| (c - 3..c + 4).map(|i| y[2 * i + side].abs()).fold(0.0f32, f32::max);
        assert!(ch(4_800, 0) > 0.4 && ch(4_800, 1) < 1e-3);
        assert!(ch(9_600, 1) > 0.2 && ch(9_600, 0) < 1e-3);
    }

    #[test]
    fn reverb_tail_decays_and_room_size_lengthens_it() {
        let tail = |room: f32| {
            let mut r = Reverb::new(48_000.0, ReverbSettings { enabled: true, room_size: room, damping: 0.3, width: 1.0, pre_delay_ms: 0.0, mix: 0.5 });
            let mut x = impulse(3.0);
            r.process(&mut x);
            let e = |a: f32, b: f32| x[(a * 96_000.0) as usize..(b * 96_000.0) as usize].iter().map(|s| s * s).sum::<f32>();
            (e(0.05, 0.3), e(1.0, 1.5), x.iter().all(|s| s.is_finite()))
        };
        let (early, late, finite) = tail(0.5);
        assert!(finite);
        assert!(early > 1e-4, "audible tail {early}");
        assert!(late < early * 0.1, "decays: {late} vs {early}");
        let (_, late_big, _) = tail(0.95);
        assert!(late_big > late * 3.0, "bigger room rings longer: {late_big} vs {late}");
    }

    #[test]
    fn leveler_brings_quiet_and_loud_passages_together_and_ignores_silence() {
        let s = LevelerSettings { enabled: true, target_db: -20.0, max_gain_db: 15.0, speed_ms: 300.0, floor_db: -50.0 };
        let tone = |amp: f32, secs: f32| -> Vec<f32> {
            (0..(48_000.0 * secs) as usize)
                .flat_map(|i| {
                    let v = (std::f32::consts::TAU * 220.0 * i as f32 / 48_000.0).sin() * amp;
                    [v, v]
                })
                .collect()
        };
        let rms_db = |x: &[f32]| 10.0 * (x.iter().map(|s| s * s).sum::<f32>() / x.len() as f32).log10();
        let mut l = Leveler::new(48_000.0, s);
        let mut quiet = tone(0.02, 3.0); // ≈ −37 dBFS RMS
        l.process(&mut quiet);
        let mut loud = tone(0.5, 3.0); // ≈ −9 dBFS RMS
        l.process(&mut loud);
        let (q, ld) = (rms_db(&quiet[2 * 96_000..]), rms_db(&loud[2 * 96_000..]));
        assert!((q - -22.0).abs() < 1.5, "quiet lifted to the 15 dB limit: {q}");
        assert!((ld - -20.0).abs() < 1.0, "loud pulled to target: {ld}");
        // silence after speech: gain is held, not raised
        let held = l.gain_db();
        let mut silence = vec![0.0f32; 96_000];
        l.process(&mut silence);
        // the fast detector needs ~0.1 s to see the silence: a fraction of a dB
        assert!((l.gain_db() - held).abs() < 0.5, "held {held} → {}", l.gain_db());
        let after = l.gain_db();
        l.process(&mut silence);
        assert_eq!(l.gain_db(), after, "frozen during the pause");
    }

    #[test]
    fn idle_chain_is_bit_transparent() {
        let mut c = FxChain::new(48_000.0);
        let mut x: Vec<f32> = (0..2_000).map(|i| (i as f32 * 0.01).sin()).collect();
        let before = x.clone();
        c.process(&AudioFx::default(), &mut x);
        assert_eq!(x, before);
    }
}
