//! Fairlight-style audio processing settings: 6-band parametric EQ with
//! HPF/LPF, dynamics (expander/gate, compressor), de-esser, delay, reverb,
//! noise reduction, voice isolation, dialogue leveler, and the role busses
//! of the mixer. These are plain data; the DSP lives in `nle-media::audio`.

use crate::clip::{AudioRole, EqSettings};
use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Parametric EQ
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum EqBandType {
    LowShelf,
    #[default]
    Bell,
    HighShelf,
    Notch,
}

impl EqBandType {
    pub const ALL: [EqBandType; 4] = [EqBandType::LowShelf, EqBandType::Bell, EqBandType::HighShelf, EqBandType::Notch];
    pub fn label(self) -> &'static str {
        match self {
            EqBandType::LowShelf => "Low Shelf",
            EqBandType::Bell => "Bell",
            EqBandType::HighShelf => "High Shelf",
            EqBandType::Notch => "Notch",
        }
    }
    /// Whether the band's gain does anything (a notch is a fixed cut).
    pub fn has_gain(self) -> bool {
        self != EqBandType::Notch
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct EqBand {
    pub enabled: bool,
    pub kind: EqBandType,
    pub freq: f32,
    pub gain_db: f32,
    pub q: f32,
}

impl Default for EqBand {
    fn default() -> Self {
        EqBand { enabled: true, kind: EqBandType::Bell, freq: 1_000.0, gain_db: 0.0, q: 1.0 }
    }
}

impl EqBand {
    /// Does this band change the signal at all?
    pub fn is_active(&self) -> bool {
        self.enabled && (self.kind == EqBandType::Notch || self.gain_db.abs() > 1e-3)
    }
}

/// Steepness of the HPF / LPF.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum FilterSlope {
    Db12,
    #[default]
    Db24,
    Db48,
}

impl FilterSlope {
    pub const ALL: [FilterSlope; 3] = [FilterSlope::Db12, FilterSlope::Db24, FilterSlope::Db48];
    pub fn label(self) -> &'static str {
        match self {
            FilterSlope::Db12 => "12 dB/oct",
            FilterSlope::Db24 => "24 dB/oct",
            FilterSlope::Db48 => "48 dB/oct",
        }
    }
    /// Q of each cascaded 2nd-order Butterworth section.
    pub fn section_qs(self) -> &'static [f32] {
        match self {
            FilterSlope::Db12 => &[std::f32::consts::FRAC_1_SQRT_2],
            FilterSlope::Db24 => &[0.541_196_1, 1.306_563],
            FilterSlope::Db48 => &[0.509_795_6, 0.601_344_9, 0.899_976_2, 2.562_915_4],
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct EqFilter {
    pub enabled: bool,
    pub freq: f32,
    pub slope: FilterSlope,
}

impl Default for EqFilter {
    fn default() -> Self {
        EqFilter { enabled: false, freq: 80.0, slope: FilterSlope::Db24 }
    }
}

/// Number of fully parametric bands (besides the HPF and LPF).
pub const EQ_BANDS: usize = 6;

/// 6-band parametric EQ plus high-pass and low-pass filters (Fairlight
/// layout: band 1 low shelf, bands 2–5 bells, band 6 high shelf by default;
/// every band's type can be changed).
#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
pub struct ParametricEq {
    pub enabled: bool,
    pub hpf: EqFilter,
    pub lpf: EqFilter,
    pub bands: [EqBand; EQ_BANDS],
    /// Output trim after the EQ.
    pub output_db: f32,
}

impl Default for ParametricEq {
    fn default() -> Self {
        let b = |kind, freq, q| EqBand { enabled: true, kind, freq, gain_db: 0.0, q };
        ParametricEq {
            enabled: false,
            hpf: EqFilter { enabled: false, freq: 80.0, slope: FilterSlope::Db24 },
            lpf: EqFilter { enabled: false, freq: 18_000.0, slope: FilterSlope::Db12 },
            bands: [
                b(EqBandType::LowShelf, 100.0, 0.707),
                b(EqBandType::Bell, 250.0, 1.0),
                b(EqBandType::Bell, 800.0, 1.0),
                b(EqBandType::Bell, 2_500.0, 1.0),
                b(EqBandType::Bell, 6_000.0, 1.0),
                b(EqBandType::HighShelf, 10_000.0, 0.707),
            ],
            output_db: 0.0,
        }
    }
}

impl ParametricEq {
    pub const FREQ_RANGE: std::ops::RangeInclusive<f32> = 20.0..=20_000.0;
    pub const GAIN_RANGE: std::ops::RangeInclusive<f32> = -20.0..=20.0;
    pub const Q_RANGE: std::ops::RangeInclusive<f32> = 0.3..=12.0;

    /// Does the EQ change the signal at all?
    pub fn is_active(&self) -> bool {
        self.enabled && (self.hpf.enabled || self.lpf.enabled || self.output_db.abs() > 1e-3 || self.bands.iter().any(|b| b.is_active()))
    }

    /// Clamp hand-edited / deserialized values into sane ranges.
    pub fn sanitized(mut self) -> Self {
        let f = |v: f32, d: f32| if v.is_finite() { v.clamp(*Self::FREQ_RANGE.start(), *Self::FREQ_RANGE.end()) } else { d };
        self.hpf.freq = f(self.hpf.freq, 80.0);
        self.lpf.freq = f(self.lpf.freq, 18_000.0);
        for b in &mut self.bands {
            b.freq = f(b.freq, 1_000.0);
            b.gain_db = if b.gain_db.is_finite() { b.gain_db.clamp(-30.0, 30.0) } else { 0.0 };
            b.q = if b.q.is_finite() { b.q.clamp(*Self::Q_RANGE.start(), *Self::Q_RANGE.end()) } else { 1.0 };
        }
        self.output_db = if self.output_db.is_finite() { self.output_db.clamp(-24.0, 24.0) } else { 0.0 };
        self
    }
}

impl From<EqSettings> for ParametricEq {
    /// Older projects stored a 3-band EQ: low shelf → band 1, mid bell →
    /// band 3, high shelf → band 6.
    fn from(e: EqSettings) -> Self {
        let mut p = ParametricEq { enabled: e.enabled, ..Default::default() };
        p.bands[0] = EqBand { enabled: true, kind: EqBandType::LowShelf, freq: e.low_freq, gain_db: e.low_gain_db, q: 0.707 };
        p.bands[2] = EqBand { enabled: true, kind: EqBandType::Bell, freq: e.mid_freq, gain_db: e.mid_gain_db, q: e.mid_q };
        p.bands[5] = EqBand { enabled: true, kind: EqBandType::HighShelf, freq: e.high_freq, gain_db: e.high_gain_db, q: 0.707 };
        p
    }
}

/// Reads both the current layout and the 3-band [`EqSettings`] of older projects.
impl<'de> Deserialize<'de> for ParametricEq {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(default)]
        struct Raw {
            enabled: bool,
            hpf: EqFilter,
            lpf: EqFilter,
            bands: [EqBand; EQ_BANDS],
            output_db: f32,
        }
        impl Default for Raw {
            fn default() -> Self {
                let p = ParametricEq::default();
                Raw { enabled: p.enabled, hpf: p.hpf, lpf: p.lpf, bands: p.bands, output_db: p.output_db }
            }
        }
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Compat {
            Legacy(EqSettings),
            Current(Raw),
        }
        Ok(match Compat::deserialize(d)? {
            Compat::Legacy(e) => e.into(),
            Compat::Current(r) => ParametricEq { enabled: r.enabled, hpf: r.hpf, lpf: r.lpf, bands: r.bands, output_db: r.output_db },
        })
    }
}

// ---------------------------------------------------------------------------
// Dynamics & creative effects
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum GateMode {
    /// Downward expander: gentle `ratio`:1 below the threshold.
    #[default]
    Expander,
    /// Gate: drops to `range_db` below the threshold.
    Gate,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct GateSettings {
    pub enabled: bool,
    pub mode: GateMode,
    pub threshold_db: f32,
    /// Expansion ratio (expander mode).
    pub ratio: f32,
    /// Maximum attenuation (positive dB).
    pub range_db: f32,
    pub attack_ms: f32,
    pub hold_ms: f32,
    pub release_ms: f32,
}

impl Default for GateSettings {
    fn default() -> Self {
        GateSettings { enabled: false, mode: GateMode::Expander, threshold_db: -45.0, ratio: 2.0, range_db: 30.0, attack_ms: 1.0, hold_ms: 60.0, release_ms: 150.0 }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct CompressorSettings {
    pub enabled: bool,
    pub threshold_db: f32,
    pub ratio: f32,
    pub attack_ms: f32,
    pub release_ms: f32,
    /// Soft-knee width (dB).
    pub knee_db: f32,
    pub makeup_db: f32,
}

impl Default for CompressorSettings {
    fn default() -> Self {
        CompressorSettings { enabled: false, threshold_db: -18.0, ratio: 3.0, attack_ms: 10.0, release_ms: 120.0, knee_db: 6.0, makeup_db: 0.0 }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct DeEsserSettings {
    pub enabled: bool,
    /// Lower edge of the sibilance band.
    pub freq: f32,
    pub threshold_db: f32,
    /// Maximum reduction of the sibilance band (positive dB).
    pub reduction_db: f32,
}

impl Default for DeEsserSettings {
    fn default() -> Self {
        DeEsserSettings { enabled: false, freq: 5_500.0, threshold_db: -32.0, reduction_db: 8.0 }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct DelaySettings {
    pub enabled: bool,
    pub time_ms: f32,
    /// 0..0.95.
    pub feedback: f32,
    /// Wet amount 0..1.
    pub mix: f32,
    /// Low-pass in the feedback path (darker repeats).
    pub damping_hz: f32,
    pub ping_pong: bool,
}

impl Default for DelaySettings {
    fn default() -> Self {
        DelaySettings { enabled: false, time_ms: 250.0, feedback: 0.3, mix: 0.2, damping_hz: 6_000.0, ping_pong: false }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ReverbSettings {
    pub enabled: bool,
    /// 0..1 (decay length).
    pub room_size: f32,
    /// 0..1 (high-frequency absorption).
    pub damping: f32,
    /// 0..1 stereo width of the tail.
    pub width: f32,
    pub pre_delay_ms: f32,
    /// Wet amount 0..1.
    pub mix: f32,
}

impl Default for ReverbSettings {
    fn default() -> Self {
        ReverbSettings { enabled: false, room_size: 0.55, damping: 0.45, width: 1.0, pre_delay_ms: 12.0, mix: 0.18 }
    }
}

/// Insert chain shared by clips and busses, in processing order:
/// expander/gate → compressor → de-esser → delay → reverb.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AudioFx {
    pub gate: GateSettings,
    pub compressor: CompressorSettings,
    pub de_esser: DeEsserSettings,
    pub delay: DelaySettings,
    pub reverb: ReverbSettings,
}

impl AudioFx {
    pub fn is_active(&self) -> bool {
        self.gate.enabled || self.compressor.enabled || self.de_esser.enabled || self.delay.enabled || self.reverb.enabled
    }
}

// ---------------------------------------------------------------------------
// Restoration (rendered ahead of time) & leveling
// ---------------------------------------------------------------------------

/// Spectral noise reduction. With no learned profile the noise is estimated
/// from the quietest stretch of the clip.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct NoiseReductionSettings {
    pub enabled: bool,
    /// Maximum attenuation of noise-only bins (positive dB).
    pub reduction_db: f32,
    /// Over-subtraction factor: higher removes more noise (and more detail).
    pub sensitivity: f32,
    /// Learned noise power spectrum ([`NOISE_PROFILE_BINS`] bins).
    pub profile: Option<Vec<f32>>,
}

/// FFT size of the noise reducer (bins = FFT/2 + 1).
pub const NOISE_FFT: usize = 2048;
pub const NOISE_PROFILE_BINS: usize = NOISE_FFT / 2 + 1;

impl Default for NoiseReductionSettings {
    fn default() -> Self {
        NoiseReductionSettings { enabled: false, reduction_db: 15.0, sensitivity: 1.5, profile: None }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct VoiceIsolationSettings {
    pub enabled: bool,
    /// 0 = untouched, 1 = only the voice remains.
    pub amount: f32,
}

impl Default for VoiceIsolationSettings {
    fn default() -> Self {
        VoiceIsolationSettings { enabled: false, amount: 0.85 }
    }
}

/// Dialogue leveler: slow automatic gain riding toward a target level so
/// quiet and loud passages sit together, without boosting silence.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct LevelerSettings {
    pub enabled: bool,
    /// Target short-term level (dBFS RMS).
    pub target_db: f32,
    /// Most boost / cut the leveler may apply.
    pub max_gain_db: f32,
    /// Smoothing time of the gain (ms).
    pub speed_ms: f32,
    /// Below this level the signal counts as silence and is not boosted.
    pub floor_db: f32,
}

impl Default for LevelerSettings {
    fn default() -> Self {
        LevelerSettings { enabled: false, target_db: -20.0, max_gain_db: 12.0, speed_ms: 500.0, floor_db: -50.0 }
    }
}

// ---------------------------------------------------------------------------
// Mixer busses
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct BusSettings {
    /// Fader (dB).
    pub gain_db: f32,
    /// -1 (left) .. 1 (right) balance.
    pub pan: f32,
    pub mute: bool,
    /// Monitoring solo: honoured in playback, ignored by export.
    pub solo: bool,
    pub eq: ParametricEq,
    pub fx: AudioFx,
}

impl Default for BusSettings {
    fn default() -> Self {
        BusSettings { gain_db: 0.0, pan: 0.0, mute: false, solo: false, eq: ParametricEq::default(), fx: AudioFx::default() }
    }
}

impl BusSettings {
    pub const FADER_RANGE: std::ops::RangeInclusive<f32> = -60.0..=12.0;
    /// Linear fader gain (−60 dB and below is silence).
    pub fn gain(&self) -> f32 {
        if self.gain_db <= *Self::FADER_RANGE.start() {
            0.0
        } else {
            crate::clip::db_to_gain(self.gain_db.min(*Self::FADER_RANGE.end()))
        }
    }
    /// Does the bus process or attenuate at all (cheap pass-through otherwise)?
    pub fn is_identity(&self) -> bool {
        self.gain_db == 0.0 && self.pan == 0.0 && !self.mute && !self.eq.is_active() && !self.fx.is_active()
    }
}

/// Which bus a mixer strip addresses.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum BusId {
    Role(AudioRole),
    Master,
}

impl BusId {
    /// Strip order in the mixer: the four role busses, then master.
    pub const ALL: [BusId; 5] = [BusId::Role(AudioRole::Dialogue), BusId::Role(AudioRole::Music), BusId::Role(AudioRole::Effects), BusId::Role(AudioRole::General), BusId::Master];
    pub fn label(self) -> &'static str {
        match self {
            BusId::Role(AudioRole::Dialogue) => "Dialogue",
            BusId::Role(AudioRole::Music) => "Music",
            BusId::Role(AudioRole::Effects) => "SFX",
            BusId::Role(AudioRole::General) => "General",
            BusId::Master => "Master",
        }
    }
    /// Index into [`BusId::ALL`].
    pub fn index(self) -> usize {
        match self {
            BusId::Role(AudioRole::Dialogue) => 0,
            BusId::Role(AudioRole::Music) => 1,
            BusId::Role(AudioRole::Effects) => 2,
            BusId::Role(AudioRole::General) => 3,
            BusId::Master => 4,
        }
    }
}

/// The timeline's mixer: one submix bus per audio role, summed into master
/// (followed by the always-on −1 dBFS limiter).
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct MixerSettings {
    pub dialogue: BusSettings,
    pub music: BusSettings,
    pub effects: BusSettings,
    pub general: BusSettings,
    pub master: BusSettings,
}

impl MixerSettings {
    pub fn bus(&self, id: BusId) -> &BusSettings {
        match id {
            BusId::Role(AudioRole::Dialogue) => &self.dialogue,
            BusId::Role(AudioRole::Music) => &self.music,
            BusId::Role(AudioRole::Effects) => &self.effects,
            BusId::Role(AudioRole::General) => &self.general,
            BusId::Master => &self.master,
        }
    }
    pub fn bus_mut(&mut self, id: BusId) -> &mut BusSettings {
        match id {
            BusId::Role(AudioRole::Dialogue) => &mut self.dialogue,
            BusId::Role(AudioRole::Music) => &mut self.music,
            BusId::Role(AudioRole::Effects) => &mut self.effects,
            BusId::Role(AudioRole::General) => &mut self.general,
            BusId::Master => &mut self.master,
        }
    }
    /// Any role bus soloed?
    pub fn any_solo(&self) -> bool {
        [&self.dialogue, &self.music, &self.effects, &self.general].iter().any(|b| b.solo)
    }
    /// Whether role bus `role` is heard. `honor_solo` is on for playback
    /// monitoring only; mutes always apply.
    pub fn audible(&self, role: AudioRole, honor_solo: bool) -> bool {
        let b = self.bus(BusId::Role(role));
        !b.mute && (!honor_solo || !self.any_solo() || b.solo)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_three_band_eq_loads_into_six_bands() {
        let old = EqSettings { enabled: true, low_gain_db: 4.0, mid_gain_db: -3.0, mid_freq: 1_500.0, mid_q: 2.0, high_gain_db: 2.0, ..Default::default() };
        let json = serde_json::to_string(&old).unwrap();
        let p: ParametricEq = serde_json::from_str(&json).unwrap();
        assert!(p.enabled);
        assert_eq!(p.bands[0].kind, EqBandType::LowShelf);
        assert_eq!(p.bands[0].gain_db, 4.0);
        assert_eq!((p.bands[2].freq, p.bands[2].gain_db, p.bands[2].q), (1_500.0, -3.0, 2.0));
        assert_eq!(p.bands[5].gain_db, 2.0);
        // and the new layout round-trips
        let mut q = p;
        q.hpf.enabled = true;
        q.bands[3].kind = EqBandType::Notch;
        let back: ParametricEq = serde_json::from_str(&serde_json::to_string(&q).unwrap()).unwrap();
        assert_eq!(back, q);
        // missing fields fall back to defaults
        let partial: ParametricEq = serde_json::from_str(r#"{"enabled":true}"#).unwrap();
        assert_eq!(partial, ParametricEq { enabled: true, ..Default::default() });
    }

    #[test]
    fn solo_is_monitoring_only_and_mute_always_applies() {
        let mut m = MixerSettings::default();
        assert!(AudioRole::ALL.iter().all(|r| m.audible(*r, true)));
        m.music.solo = true;
        assert!(m.audible(AudioRole::Music, true));
        assert!(!m.audible(AudioRole::Dialogue, true));
        assert!(m.audible(AudioRole::Dialogue, false), "export ignores solo");
        m.effects.mute = true;
        assert!(!m.audible(AudioRole::Effects, false));
        assert_eq!(BusId::ALL.iter().map(|b| b.index()).collect::<Vec<_>>(), vec![0, 1, 2, 3, 4]);
    }
}
