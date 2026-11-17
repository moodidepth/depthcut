//! Voice/dialogue auto-ducking: an RMS envelope follower on the dialogue
//! bus drives smoothed gain reduction on music/effects clips.

use nle_core::clip::{db_to_gain, gain_to_db};
use nle_core::DuckingSettings;

fn coef(ms: f32, sample_rate: f32) -> f32 {
    if ms <= 0.0 {
        0.0
    } else {
        (-1.0 / (ms * 0.001 * sample_rate)).exp()
    }
}

/// Mean-square envelope follower with separate attack/release.
#[derive(Clone, Debug)]
pub struct EnvelopeFollower {
    attack: f32,
    release: f32,
    ms: f32,
}

impl EnvelopeFollower {
    pub fn new(sample_rate: f32, attack_ms: f32, release_ms: f32) -> Self {
        EnvelopeFollower { attack: coef(attack_ms, sample_rate), release: coef(release_ms, sample_rate), ms: 0.0 }
    }
    /// Feed one (mono) sample, return the RMS level.
    #[inline]
    pub fn process(&mut self, x: f32) -> f32 {
        let p = x * x;
        let c = if p > self.ms { self.attack } else { self.release };
        self.ms = c * self.ms + (1.0 - c) * p;
        self.ms.sqrt()
    }
    pub fn level(&self) -> f32 {
        self.ms.sqrt()
    }
    pub fn reset(&mut self) {
        self.ms = 0.0;
    }
}

/// Sidechain ducker for one music/effects clip.
#[derive(Clone, Debug)]
pub struct Ducker {
    settings: DuckingSettings,
    sample_rate: f32,
    detector: EnvelopeFollower,
    gain: f32,
    attack: f32,
    release: f32,
}

impl Ducker {
    pub fn new(sample_rate: f32, settings: DuckingSettings) -> Self {
        Ducker {
            settings,
            sample_rate,
            detector: EnvelopeFollower::new(sample_rate, 5.0, 120.0),
            gain: 1.0,
            attack: coef(settings.attack_ms, sample_rate),
            release: coef(settings.release_ms, sample_rate),
        }
    }

    pub fn update(&mut self, s: &DuckingSettings) {
        if *s != self.settings {
            self.settings = *s;
            self.attack = coef(s.attack_ms, self.sample_rate);
            self.release = coef(s.release_ms, self.sample_rate);
        }
    }

    /// Feed one sidechain (dialogue) sample; returns the gain to apply.
    #[inline]
    pub fn process(&mut self, sidechain: f32) -> f32 {
        if !self.settings.enabled {
            return 1.0;
        }
        let level_db = gain_to_db(self.detector.process(sidechain));
        let target = if level_db > self.settings.threshold_db { db_to_gain(-self.settings.amount_db.abs()) } else { 1.0 };
        let c = if target < self.gain { self.attack } else { self.release };
        self.gain = c * self.gain + (1.0 - c) * target;
        self.gain
    }

    pub fn gain(&self) -> f32 {
        self.gain
    }

    pub fn reset(&mut self) {
        self.detector.reset();
        self.gain = 1.0;
    }
}

/// Offline analysis: per-sample duck gain for a whole mono sidechain.
pub fn duck_curve(sidechain: &[f32], sample_rate: f32, settings: DuckingSettings) -> Vec<f32> {
    let mut d = Ducker::new(sample_rate, settings);
    sidechain.iter().map(|s| d.process(*s)).collect()
}

// ---------------------------------------------------------------------------
// Cached look-ahead ducking (preview == export)
// ---------------------------------------------------------------------------

/// Envelope resolution: one RMS value per 10 ms.
pub const ENV_HOP_SECS: f64 = 0.010;

/// RMS level of the timeline's unmuted dialogue, one value per 10 ms from
/// timeline zero. Computed offline (so it can look ahead), cached, and shared
/// by the preview and export mixers so both duck identically.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct DuckEnvelope {
    pub rms: Vec<f32>,
}

impl DuckEnvelope {
    /// Build from an interleaved stereo mix starting at timeline zero.
    pub fn from_stereo(stereo: &[f32], sample_rate: u32) -> Self {
        let mut acc = RmsAccumulator::new(sample_rate);
        acc.push_stereo(stereo);
        acc.finish()
    }

    /// Per-frame duck gain (linear) for `settings`.
    ///
    /// A frame is "voice" when its level exceeds the threshold. The duck
    /// target covers every frame within `lookahead` before and `hold` after
    /// voice; the gain then moves toward the target in the dB domain at a
    /// constant rate, reaching full depth `attack` ms after the target drops
    /// and full recovery `release` ms after it rises (no overshoot, no clicks).
    pub fn gain_curve(&self, settings: &DuckingSettings) -> DuckCurve {
        let s = settings.sanitized();
        let n = self.rms.len();
        if !s.enabled {
            return DuckCurve { gains: vec![1.0; n] };
        }
        let frames = |ms: f32| ((ms as f64 / 1000.0) / ENV_HOP_SECS).round() as usize;
        let (look, hold) = (frames(s.lookahead_ms), frames(s.hold_ms));
        // prefix sums of "voice" frames → O(n) window queries
        let mut pre = Vec::with_capacity(n + 1);
        pre.push(0u32);
        for r in &self.rms {
            let voice = gain_to_db(*r) > s.threshold_db;
            pre.push(pre.last().copied().unwrap_or(0) + voice as u32);
        }
        let depth = s.amount_db;
        let hop_ms = (ENV_HOP_SECS * 1000.0) as f32;
        let down = depth * hop_ms / s.attack_ms.max(hop_ms);
        let up = depth * hop_ms / s.release_ms.max(hop_ms);
        let mut db = 0.0f32;
        let mut gains = Vec::with_capacity(n);
        for k in 0..n {
            let lo = k.saturating_sub(hold);
            let hi = (k + look + 1).min(n);
            let want = if pre[hi] > pre[lo] { -depth } else { 0.0 };
            db = if want < db { (db - down).max(want) } else { (db + up).min(want) };
            gains.push(db_to_gain(db));
        }
        DuckCurve { gains }
    }
}

/// Streaming 10 ms RMS of a stereo signal (mono-summed).
pub struct RmsAccumulator {
    hop: usize,
    sum: f64,
    count: usize,
    rms: Vec<f32>,
}

impl RmsAccumulator {
    pub fn new(sample_rate: u32) -> Self {
        RmsAccumulator { hop: ((sample_rate as f64 * ENV_HOP_SECS).round() as usize).max(1), sum: 0.0, count: 0, rms: Vec::new() }
    }
    pub fn push_stereo(&mut self, stereo: &[f32]) {
        for f in stereo.chunks_exact(2) {
            let m = 0.5 * (f[0] + f[1]);
            let m = if m.is_finite() { m as f64 } else { 0.0 };
            self.sum += m * m;
            self.count += 1;
            if self.count == self.hop {
                self.rms.push((self.sum / self.hop as f64).sqrt() as f32);
                self.sum = 0.0;
                self.count = 0;
            }
        }
    }
    pub fn finish(mut self) -> DuckEnvelope {
        if self.count > 0 {
            self.rms.push((self.sum / self.count as f64).sqrt() as f32);
        }
        DuckEnvelope { rms: self.rms }
    }
}

/// Duck gain per 10 ms frame for one clip's settings.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct DuckCurve {
    pub gains: Vec<f32>,
}

impl DuckCurve {
    /// Gain at timeline time `secs` (linear between frames; unity outside).
    #[inline]
    pub fn gain_at(&self, secs: f64) -> f32 {
        if !(secs >= 0.0) || self.gains.is_empty() {
            return 1.0;
        }
        let x = secs / ENV_HOP_SECS;
        let i = x.floor() as usize;
        if i + 1 >= self.gains.len() {
            return if i < self.gains.len() { self.gains[i] } else { 1.0 };
        }
        let f = (x - i as f64) as f32;
        self.gains[i] + (self.gains[i + 1] - self.gains[i]) * f
    }

    /// Lowest gain in `[a, b)` seconds — for drawing the duck overlay.
    pub fn min_in(&self, a: f64, b: f64) -> f32 {
        let i0 = (a.max(0.0) / ENV_HOP_SECS) as usize;
        let i1 = ((b.max(0.0) / ENV_HOP_SECS).ceil() as usize).max(i0 + 1).min(self.gains.len());
        self.gains.get(i0..i1).and_then(|g| g.iter().copied().reduce(f32::min)).unwrap_or(1.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ducks_under_dialogue_and_recovers() {
        let sr = 48_000.0;
        let s = DuckingSettings { enabled: true, amount_db: 12.0, threshold_db: -30.0, attack_ms: 50.0, release_ms: 300.0, ..Default::default() };
        let mut side = vec![0.0f32; 48_000];
        // one second of "speech" in the middle of three seconds of silence
        side.extend((0..48_000).map(|i| (i as f32 * 0.05).sin() * 0.5));
        side.extend(vec![0.0f32; 96_000]);
        let curve = duck_curve(&side, sr, s);
        assert!((curve[40_000] - 1.0).abs() < 1e-4, "no ducking before speech");
        let ducked = db_to_gain(-12.0);
        assert!((curve[95_000] - ducked).abs() < 0.02, "ducked during speech: {}", curve[95_000]);
        assert!(curve[190_000] > 0.95, "recovered after release: {}", curve[190_000]);
        // smooth: no sample-to-sample jump larger than 1%
        assert!(curve.windows(2).all(|w| (w[1] - w[0]).abs() < 0.01));
    }

    #[test]
    fn disabled_is_unity() {
        let curve = duck_curve(&[1.0; 1000], 48_000.0, DuckingSettings::default());
        assert!(curve.iter().all(|g| *g == 1.0));
    }

    /// 10 ms frames: silence, voice (−20 dBFS) for frames [100, 200), silence.
    fn voice_env() -> DuckEnvelope {
        let mut rms = vec![0.0f32; 400];
        rms[100..200].fill(0.1);
        DuckEnvelope { rms }
    }

    fn db_at(c: &DuckCurve, secs: f64) -> f32 {
        gain_to_db(c.gain_at(secs))
    }

    #[test]
    fn lookahead_attack_hold_release_follow_the_spec() {
        let s = DuckingSettings { enabled: true, ..Default::default() }; // −14 dB, 100/120/220/380 ms
        let c = voice_env().gain_curve(&s);
        // voice at 1.000 s: untouched until the 100 ms lookahead…
        assert_eq!(c.gain_at(0.5), 1.0);
        assert_eq!(c.gain_at(0.88), 1.0);
        // …then ducks *before* the first word, linear in dB over the 120 ms attack
        assert!(db_at(&c, 0.90) < -1.0, "ducking starts at t−100 ms");
        assert!((db_at(&c, 0.96) + 8.17).abs() < 0.3, "half-way down the attack: {}", db_at(&c, 0.96));
        assert!((db_at(&c, 1.01) + 14.0).abs() < 1e-3, "full depth 120 ms after onset");
        assert!((db_at(&c, 1.5) + 14.0).abs() < 1e-3);
        // voice ends at 2.000 s: held for 220 ms
        assert!((db_at(&c, 2.21) + 14.0).abs() < 1e-3, "hold keeps the duck: {}", db_at(&c, 2.21));
        // then 380 ms release back to unity, monotonically
        let mid = db_at(&c, 2.41);
        assert!(mid > -9.0 && mid < -4.0, "mid-release: {mid}");
        assert!((c.gain_at(2.60) - 1.0).abs() < 1e-6, "recovered 380 ms after hold");
        assert!(c.gains.windows(2).skip(222).take(40).all(|w| w[1] >= w[0]));
        // never deeper than the amount, never above unity
        assert!(c.gains.iter().all(|g| *g <= 1.0 && *g >= db_to_gain(-14.0) - 1e-6));
    }

    #[test]
    fn gaps_shorter_than_hold_plus_lookahead_stay_ducked() {
        let mut env = voice_env();
        env.rms[140..160].fill(0.0); // 200 ms breath between words
        let c = env.gain_curve(&DuckingSettings { enabled: true, ..Default::default() });
        assert!((db_at(&c, 1.50) + 14.0).abs() < 1e-3, "no pumping between words");
    }

    #[test]
    fn amount_threshold_and_disabled() {
        let env = voice_env();
        let deep = env.gain_curve(&DuckingSettings { enabled: true, amount_db: 30.0, ..Default::default() });
        assert!((db_at(&deep, 1.5) + 30.0).abs() < 1e-3);
        // amounts outside −3..−30 dB are clamped
        let wild = env.gain_curve(&DuckingSettings { enabled: true, amount_db: 90.0, ..Default::default() });
        assert!((db_at(&wild, 1.5) + 30.0).abs() < 1e-3);
        // voice below the threshold never ducks
        let deaf = env.gain_curve(&DuckingSettings { enabled: true, threshold_db: -10.0, ..Default::default() });
        assert!(deaf.gains.iter().all(|g| *g == 1.0));
        let off = env.gain_curve(&DuckingSettings::default());
        assert!(off.gains.iter().all(|g| *g == 1.0), "disabled is unity");
        // outside the envelope: unity
        assert_eq!(deep.gain_at(-1.0), 1.0);
        assert_eq!(deep.gain_at(100.0), 1.0);
        assert!(deep.min_in(1.0, 1.2) < 0.05 && deep.min_in(0.0, 0.5) == 1.0);
    }

    #[test]
    fn rms_envelope_is_10ms_frames() {
        let sr = 48_000;
        // 0.25 s silence + 0.25 s of a full-scale 1 kHz sine
        let mut st = vec![0.0f32; 12_000 * 2];
        st.extend((0..12_000).flat_map(|i| {
            let v = (std::f32::consts::TAU * 1_000.0 * i as f32 / sr as f32).sin();
            [v, v]
        }));
        let env = DuckEnvelope::from_stereo(&st, sr);
        assert_eq!(env.rms.len(), 50);
        assert!(env.rms[..25].iter().all(|r| *r == 0.0));
        assert!(env.rms[25..].iter().all(|r| (r - std::f32::consts::FRAC_1_SQRT_2).abs() < 0.01));
        // streaming in odd chunk sizes gives the same envelope
        let mut acc = RmsAccumulator::new(sr);
        for ch in st.chunks(2 * 777) {
            acc.push_stereo(ch);
        }
        assert_eq!(acc.finish(), env);
    }
}
