//! Clips and their per-clip effect parameters.

use crate::asset::MediaAsset;
use crate::effects::Effect;
use crate::ids::{AssetId, ClipId};
use crate::keyframe::{Interpolation, Param};
use crate::speed::{FrameBlendMode, Retime};
use crate::text_anim::TextAnimation;
use crate::time::Time;
use serde::{Deserialize, Serialize};

/// Display names of the built-in animatable parameters, in
/// [`Clip::animatable_params`] order.
pub const ANIMATABLE_NAMES: [&str; 20] = [
    "Opacity",
    "Position X",
    "Position Y",
    "Scale X",
    "Scale Y",
    "Rotation",
    "Anchor X",
    "Anchor Y",
    "Crop L",
    "Crop R",
    "Crop T",
    "Crop B",
    "Exposure",
    "Brightness",
    "Contrast",
    "Saturation",
    "Temperature",
    "Tint",
    "Volume",
    "Pan",
];

// ---------------------------------------------------------------------------
// Video (transform / crop / compositing)
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum BlendMode {
    #[default]
    Normal,
    Multiply,
    Screen,
    Overlay,
    Add,
    // New modes are appended: `shader_index` is the discriminant, and the
    // order above is shared with `composite.wgsl`.
    SoftLight,
    HardLight,
    ColorDodge,
    ColorBurn,
    Darken,
    Lighten,
    Difference,
    Exclusion,
    /// Non-separable: the base's hue and saturation with the layer's luminance.
    Luminosity,
}

impl BlendMode {
    pub const ALL: [BlendMode; 14] = [
        BlendMode::Normal,
        BlendMode::Multiply,
        BlendMode::Screen,
        BlendMode::Overlay,
        BlendMode::Add,
        BlendMode::SoftLight,
        BlendMode::HardLight,
        BlendMode::ColorDodge,
        BlendMode::ColorBurn,
        BlendMode::Darken,
        BlendMode::Lighten,
        BlendMode::Difference,
        BlendMode::Exclusion,
        BlendMode::Luminosity,
    ];
    pub fn label(self) -> &'static str {
        match self {
            BlendMode::Normal => "Normal",
            BlendMode::Multiply => "Multiply",
            BlendMode::Screen => "Screen",
            BlendMode::Overlay => "Overlay",
            BlendMode::Add => "Add",
            BlendMode::SoftLight => "Soft Light",
            BlendMode::HardLight => "Hard Light",
            BlendMode::ColorDodge => "Color Dodge",
            BlendMode::ColorBurn => "Color Burn",
            BlendMode::Darken => "Darken",
            BlendMode::Lighten => "Lighten",
            BlendMode::Difference => "Difference",
            BlendMode::Exclusion => "Exclusion",
            BlendMode::Luminosity => "Luminosity",
        }
    }
    /// Parse an FCPXML `adjust-blend` mode ("Soft Light", "softlight", "soft-light").
    pub fn from_name(s: &str) -> Option<BlendMode> {
        let norm = |x: &str| x.chars().filter(|c| c.is_ascii_alphanumeric()).map(|c| c.to_ascii_lowercase()).collect::<String>();
        let want = norm(s);
        BlendMode::ALL.into_iter().find(|b| norm(b.label()) == want)
    }
    pub fn shader_index(self) -> u32 {
        self as u32
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum CropMode {
    #[default]
    Trim,
    /// Animate between `ken_burns_start` and `ken_burns_end` rectangles.
    KenBurns,
}

/// Normalised rectangle `[x, y, w, h]` in source-frame fractions.
pub type NormRect = [f32; 4];

/// Aspect ratio of the cropped picture.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum CropAspect {
    #[default]
    Free,
    Wide16x9,
    Tall9x16,
    Square,
    Portrait4x5,
}

impl CropAspect {
    pub const ALL: [CropAspect; 5] = [CropAspect::Free, CropAspect::Wide16x9, CropAspect::Tall9x16, CropAspect::Square, CropAspect::Portrait4x5];
    pub fn label(self) -> &'static str {
        match self {
            CropAspect::Free => "Free",
            CropAspect::Wide16x9 => "16:9",
            CropAspect::Tall9x16 => "9:16",
            CropAspect::Square => "1:1",
            CropAspect::Portrait4x5 => "4:5",
        }
    }
    /// Width over height; `None` when free.
    pub fn ratio(self) -> Option<f32> {
        match self {
            CropAspect::Free => None,
            CropAspect::Wide16x9 => Some(16.0 / 9.0),
            CropAspect::Tall9x16 => Some(9.0 / 16.0),
            CropAspect::Square => Some(1.0),
            CropAspect::Portrait4x5 => Some(0.8),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct VideoParams {
    /// Offset from frame centre, as a fraction of the frame size.
    pub position_x: Param,
    pub position_y: Param,
    pub scale_x: Param,
    pub scale_y: Param,
    /// Degrees, clockwise.
    pub rotation: Param,
    pub anchor_x: Param,
    pub anchor_y: Param,
    pub opacity: Param,
    pub crop_left: Param,
    pub crop_right: Param,
    pub crop_top: Param,
    pub crop_bottom: Param,
    pub crop_mode: CropMode,
    pub ken_burns_start: NormRect,
    pub ken_burns_end: NormRect,
    pub blend_mode: BlendMode,
    /// Ride along with a track on another clip (titles, callouts, stickers).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub follow: Option<crate::motion::Follow>,
    /// Mirror the picture inside its cropped box.
    #[serde(default, skip_serializing_if = "is_false")]
    pub flip_horizontal: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub flip_vertical: bool,
    /// Editing one scale axis scales the other by the same factor.
    #[serde(default = "default_true", skip_serializing_if = "is_true")]
    pub scale_lock: bool,
    /// The last crop preset chosen.
    #[serde(default, skip_serializing_if = "is_default")]
    pub crop_aspect: CropAspect,
    /// Crop edits keep the cropped picture's aspect ratio.
    #[serde(default, skip_serializing_if = "is_false")]
    pub crop_lock: bool,
}

impl Default for VideoParams {
    fn default() -> Self {
        VideoParams {
            position_x: Param::new(0.0),
            position_y: Param::new(0.0),
            scale_x: Param::new(1.0),
            scale_y: Param::new(1.0),
            rotation: Param::new(0.0),
            anchor_x: Param::new(0.0),
            anchor_y: Param::new(0.0),
            opacity: Param::new(1.0),
            crop_left: Param::new(0.0),
            crop_right: Param::new(0.0),
            crop_top: Param::new(0.0),
            crop_bottom: Param::new(0.0),
            crop_mode: CropMode::Trim,
            ken_burns_start: [0.0, 0.0, 1.0, 1.0],
            ken_burns_end: [0.1, 0.1, 0.8, 0.8],
            blend_mode: BlendMode::Normal,
            follow: None,
            flip_horizontal: false,
            flip_vertical: false,
            scale_lock: true,
            crop_aspect: CropAspect::Free,
            crop_lock: false,
        }
    }
}

impl VideoParams {
    pub fn params_mut(&mut self) -> [&mut Param; 12] {
        [
            &mut self.position_x,
            &mut self.position_y,
            &mut self.scale_x,
            &mut self.scale_y,
            &mut self.rotation,
            &mut self.anchor_x,
            &mut self.anchor_y,
            &mut self.opacity,
            &mut self.crop_left,
            &mut self.crop_right,
            &mut self.crop_top,
            &mut self.crop_bottom,
        ]
    }
}

// ---------------------------------------------------------------------------
// Color
// ---------------------------------------------------------------------------

/// One of the three color wheels: RGB offset plus a master (luma) amount.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ColorWheel {
    pub r: f32,
    pub g: f32,
    pub b: f32,
    pub master: f32,
}

impl ColorWheel {
    pub fn as_array(&self) -> [f32; 4] {
        [self.r, self.g, self.b, self.master]
    }
    pub fn is_neutral(&self) -> bool {
        self.r == 0.0 && self.g == 0.0 && self.b == 0.0 && self.master == 0.0
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LutRef {
    pub path: String,
    pub intensity: f32,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ColorParams {
    pub enabled: bool,
    /// Stops.
    pub exposure: Param,
    /// -1..1, 0 neutral: an offset on the displayed values (exposure is a gain in stops).
    #[serde(default)]
    pub brightness: Param,
    /// -1..1, 0 neutral.
    pub contrast: Param,
    /// 0..2, 1 neutral.
    pub saturation: Param,
    /// -1 (cool) .. 1 (warm).
    pub temperature: Param,
    /// -1 (green) .. 1 (magenta).
    pub tint: Param,
    /// Lift (shadows).
    pub shadows: ColorWheel,
    /// Gamma (midtones).
    pub midtones: ColorWheel,
    /// Gain (highlights).
    pub highlights: ColorWheel,
    pub lut: Option<LutRef>,
    /// How the source is encoded (converted to Rec.709 before grading).
    #[serde(default)]
    pub input_space: crate::color::ColorSpace,
    /// Encoding the primary and the correction nodes grade in.
    #[serde(default)]
    pub working: crate::color::WorkingSpace,
    /// Input LUT (e.g. a camera's log → Rec.709 LUT). Replaces the
    /// `input_space` conversion when set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_lut: Option<LutRef>,
    /// Stacked corrections applied after the primary, in order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub corrections: Vec<crate::color::ColorCorrection>,
    /// Node compositor wiring of the corrections (plus effects, media,
    /// mixers). `None`: the corrections run in series.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub graph: Option<crate::node_graph::NodeGraph>,
}

impl Default for ColorParams {
    fn default() -> Self {
        ColorParams {
            enabled: true,
            exposure: Param::new(0.0),
            brightness: Param::new(0.0),
            contrast: Param::new(0.0),
            saturation: Param::new(1.0),
            temperature: Param::new(0.0),
            tint: Param::new(0.0),
            shadows: ColorWheel::default(),
            midtones: ColorWheel::default(),
            highlights: ColorWheel::default(),
            lut: None,
            input_space: Default::default(),
            working: Default::default(),
            input_lut: None,
            corrections: Vec::new(),
            graph: None,
        }
    }
}

impl ColorParams {
    pub fn params_mut(&mut self) -> [&mut Param; 6] {
        [&mut self.exposure, &mut self.brightness, &mut self.contrast, &mut self.saturation, &mut self.temperature, &mut self.tint]
    }

    pub fn is_identity(&self) -> bool {
        !self.enabled
            || (!self.exposure.is_animated()
                && !self.brightness.is_animated()
                && self.brightness.value == 0.0
                && !self.contrast.is_animated()
                && !self.saturation.is_animated()
                && !self.temperature.is_animated()
                && !self.tint.is_animated()
                && self.exposure.value == 0.0
                && self.contrast.value == 0.0
                && self.saturation.value == 1.0
                && self.temperature.value == 0.0
                && self.tint.value == 0.0
                && self.shadows.is_neutral()
                && self.midtones.is_neutral()
                && self.highlights.is_neutral()
                && self.lut.is_none()
                && self.input_space == crate::color::ColorSpace::Rec709
                && self.input_lut.is_none()
                && self.corrections.iter().all(|n| n.is_identity())
                && self.graph.as_ref().is_none_or(|g| g.only_correctors()))
    }
}

// ---------------------------------------------------------------------------
// Audio
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum AudioRole {
    /// Dialogue / voiceover: the sidechain that music ducks under.
    #[default]
    Dialogue,
    Music,
    /// Sound effects (serialized as `Effects` for older projects).
    Effects,
    /// Gameplay / ambience: neither ducks nor triggers ducking.
    General,
}

impl AudioRole {
    pub const ALL: [AudioRole; 4] = [AudioRole::Dialogue, AudioRole::Music, AudioRole::Effects, AudioRole::General];
    pub fn label(self) -> &'static str {
        match self {
            AudioRole::Dialogue => "Dialogue / VO",
            AudioRole::Music => "Music",
            AudioRole::Effects => "SFX",
            AudioRole::General => "General",
        }
    }
    /// FCPXML `audioRole` name.
    pub fn fcp_role(self) -> &'static str {
        match self {
            AudioRole::Dialogue => "dialogue",
            AudioRole::Music => "music",
            AudioRole::Effects => "effects",
            AudioRole::General => "general",
        }
    }
    pub fn from_fcp_role(role: Option<&str>) -> AudioRole {
        match role.map(|r| r.split('.').next().unwrap_or(r).to_ascii_lowercase()).as_deref() {
            Some("music") => AudioRole::Music,
            Some("effects") => AudioRole::Effects,
            Some("general") => AudioRole::General,
            _ => AudioRole::Dialogue,
        }
    }
    /// Role for a freshly imported clip: voice recordings (or anything that
    /// looks like a mic / VO take) → Dialogue, other audio-only files → Music,
    /// gameplay / camera footage → General.
    pub fn guess(name: &str, has_video: bool) -> AudioRole {
        let n = name.to_ascii_lowercase();
        let words: Vec<&str> = n.split(|c: char| !c.is_ascii_alphanumeric()).filter(|w| !w.is_empty()).collect();
        let voice =
            words.iter().any(|w| matches!(*w, "vo" | "voice" | "voiceover" | "mic" | "microphone" | "narration" | "commentary" | "dialogue" | "dialog" | "podcast" | "recording"))
                || n.contains("voiceover")
                || n.contains("voice over")
                || n.contains("mic input");
        if voice {
            AudioRole::Dialogue
        } else if has_video {
            AudioRole::General
        } else {
            AudioRole::Music
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum FadeShape {
    Linear,
    #[default]
    SCurve,
    /// +3 dB equal-power.
    EqualPower,
}

impl FadeShape {
    pub const ALL: [FadeShape; 3] = [FadeShape::Linear, FadeShape::SCurve, FadeShape::EqualPower];
    pub fn label(self) -> &'static str {
        match self {
            FadeShape::Linear => "Linear",
            FadeShape::SCurve => "S-Curve",
            FadeShape::EqualPower => "+3dB",
        }
    }
    /// Gain for normalised fade progress `u` (0 = silent, 1 = full).
    pub fn gain(self, u: f32) -> f32 {
        let u = u.clamp(0.0, 1.0);
        match self {
            FadeShape::Linear => u,
            FadeShape::SCurve => u * u * (3.0 - 2.0 * u),
            FadeShape::EqualPower => (u * std::f32::consts::FRAC_PI_2).sin(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct EqSettings {
    pub enabled: bool,
    pub low_gain_db: f32,
    pub low_freq: f32,
    pub mid_gain_db: f32,
    pub mid_freq: f32,
    pub mid_q: f32,
    pub high_gain_db: f32,
    pub high_freq: f32,
}

impl Default for EqSettings {
    fn default() -> Self {
        EqSettings { enabled: false, low_gain_db: 0.0, low_freq: 120.0, mid_gain_db: 0.0, mid_freq: 1_000.0, mid_q: 0.9, high_gain_db: 0.0, high_freq: 8_000.0 }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct DuckingSettings {
    /// "Auto-Duck Under Voice": duck this clip under dialogue.
    pub enabled: bool,
    /// Attenuation applied while dialogue is present (positive dB, 3..30).
    pub amount_db: f32,
    /// Dialogue level that triggers ducking.
    pub threshold_db: f32,
    pub attack_ms: f32,
    pub release_ms: f32,
    /// Start ducking this long *before* the voice starts.
    #[serde(default = "DuckingSettings::default_lookahead")]
    pub lookahead_ms: f32,
    /// Stay ducked this long after the voice stops (bridges gaps between words).
    #[serde(default = "DuckingSettings::default_hold")]
    pub hold_ms: f32,
}

impl Default for DuckingSettings {
    fn default() -> Self {
        DuckingSettings {
            enabled: false,
            amount_db: DuckingSettings::DEFAULT_AMOUNT_DB,
            threshold_db: -35.0,
            attack_ms: 120.0,
            release_ms: 380.0,
            lookahead_ms: 100.0,
            hold_ms: 220.0,
        }
    }
}

impl DuckingSettings {
    pub const DEFAULT_AMOUNT_DB: f32 = 14.0;
    pub const AMOUNT_RANGE_DB: std::ops::RangeInclusive<f32> = 3.0..=30.0;

    fn default_lookahead() -> f32 {
        100.0
    }
    fn default_hold() -> f32 {
        220.0
    }

    /// Clamp hand-edited / deserialized values into sane ranges.
    pub fn sanitized(mut self) -> Self {
        let fix = |v: f32, lo: f32, hi: f32, d: f32| if v.is_finite() { v.clamp(lo, hi) } else { d };
        let d = DuckingSettings::default();
        self.amount_db = fix(self.amount_db.abs(), 3.0, 30.0, d.amount_db);
        self.threshold_db = fix(self.threshold_db, -80.0, 0.0, d.threshold_db);
        self.attack_ms = fix(self.attack_ms, 1.0, 2000.0, d.attack_ms);
        self.release_ms = fix(self.release_ms, 10.0, 5000.0, d.release_ms);
        self.lookahead_ms = fix(self.lookahead_ms, 0.0, 500.0, d.lookahead_ms);
        self.hold_ms = fix(self.hold_ms, 0.0, 2000.0, d.hold_ms);
        self
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AudioParams {
    pub volume_db: Param,
    /// -1 (left) .. 1 (right).
    pub pan: Param,
    pub fade_in: Time,
    pub fade_out: Time,
    pub fade_shape: FadeShape,
    /// 6-band parametric EQ + HPF/LPF (older 3-band projects load into it).
    pub eq: crate::audio_fx::ParametricEq,
    pub role: AudioRole,
    pub ducking: DuckingSettings,
    pub muted: bool,
    /// Normalise integrated loudness to -14 LUFS (streaming target).
    #[serde(default)]
    pub loudness_normalize: bool,
    /// Gain applied while `loudness_normalize` is on (-14 LUFS minus the
    /// measured loudness of the source).
    #[serde(default)]
    pub loudness_gain_db: f32,
    /// Vocal clarity chain: 80 Hz high-pass, presence boost, soft limiter.
    #[serde(default)]
    pub vocal_clarity: bool,
    /// Keyframeable underwater / muffled low-pass filter sweep (e.g. 20 kHz -> 350 Hz before kill).
    #[serde(default)]
    pub lowpass_enabled: bool,
    #[serde(default = "default_lowpass_hz")]
    pub lowpass_hz: Param,
    #[serde(default = "default_lowpass_q")]
    pub lowpass_q: f32,
    /// Sub-bass impact punch (`80 Hz` low-shelf boost on hits/drops).
    #[serde(default)]
    pub sub_bass_punch: bool,
    #[serde(default = "default_sub_bass_db")]
    pub sub_bass_db: f32,
    /// Legacy per-clip flag (kept for project compatibility); the master bus
    /// now always runs a −1 dBFS soft-knee limiter.
    #[serde(default = "default_true")]
    pub loudness_limiter: bool,
    /// Preserve audio pitch during montage speed ramps via granular overlap-add.
    #[serde(default = "default_true")]
    pub preserve_pitch: bool,
    /// Automatically mute audio on extreme slow-motion segments (`< 30%` speed).
    #[serde(default)]
    pub mute_extreme_slowmo: bool,
    /// J-cut: audio starts this long *before* the clip's video (split edit).
    #[serde(default)]
    pub audio_lead: Time,
    /// L-cut: audio keeps playing this long *after* the clip's video ends.
    #[serde(default)]
    pub audio_tail: Time,
    /// Expander/gate, compressor, de-esser, delay, reverb.
    #[serde(default)]
    pub fx: crate::audio_fx::AudioFx,
    /// Spectral noise reduction (rendered ahead of playback).
    #[serde(default)]
    pub noise_reduction: crate::audio_fx::NoiseReductionSettings,
    /// Voice isolation (rendered ahead of playback).
    #[serde(default)]
    pub voice_isolation: crate::audio_fx::VoiceIsolationSettings,
    /// Dialogue leveler (automatic gain riding).
    #[serde(default)]
    pub leveler: crate::audio_fx::LevelerSettings,
    /// Audio Crossfade on the cut before this clip (a fade-in without a neighbour).
    #[serde(default, skip_serializing_if = "time_is_zero")]
    pub crossfade_in: Time,
    /// Audio Crossfade on the cut after this clip (a fade-out without a neighbour).
    #[serde(default, skip_serializing_if = "time_is_zero")]
    pub crossfade_out: Time,
    /// User bus ([`crate::audio_fx::UserBus::id`]) this clip is sent to
    /// instead of straight to its role bus. Gone / `None` = the role bus.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bus: Option<u64>,
}

fn default_lowpass_hz() -> Param {
    Param::new(20_000.0)
}
fn default_lowpass_q() -> f32 {
    0.707
}
fn default_sub_bass_db() -> f32 {
    6.0
}

impl Default for AudioParams {
    fn default() -> Self {
        AudioParams {
            volume_db: Param::new(0.0),
            pan: Param::new(0.0),
            fade_in: Time::ZERO,
            fade_out: Time::ZERO,
            fade_shape: FadeShape::SCurve,
            eq: crate::audio_fx::ParametricEq::default(),
            role: AudioRole::Dialogue,
            ducking: DuckingSettings::default(),
            muted: false,
            loudness_normalize: false,
            loudness_gain_db: 0.0,
            vocal_clarity: false,
            lowpass_enabled: false,
            lowpass_hz: default_lowpass_hz(),
            lowpass_q: default_lowpass_q(),
            sub_bass_punch: false,
            sub_bass_db: default_sub_bass_db(),
            loudness_limiter: true,
            preserve_pitch: true,
            mute_extreme_slowmo: false,
            audio_lead: Time::ZERO,
            audio_tail: Time::ZERO,
            fx: Default::default(),
            noise_reduction: Default::default(),
            voice_isolation: Default::default(),
            leveler: Default::default(),
            crossfade_in: Time::ZERO,
            crossfade_out: Time::ZERO,
            bus: None,
        }
    }
}

impl AudioParams {
    /// One-click underwater muffled pre-kill sweep: `20 kHz -> 350 Hz` into `peak_time`,
    /// then snaps sharply back open to `20 kHz` on the kill / beat drop.
    pub fn apply_pre_kill_underwater_sweep(&mut self, peak_time: Time) {
        self.apply_underwater_sweep(peak_time, Time::from_millis(600));
    }
    /// The underwater sweep over `len` before `peak_time`: a pre-kill muffle at
    /// 0.6 s, or a build into a drop over a bar or more. The muffle reaches its
    /// floor ~37% of the way before the peak (220 ms of a 600 ms sweep) and
    /// holds there, so a long build stays underwater until the hit.
    pub fn apply_underwater_sweep(&mut self, peak_time: Time, len: Time) {
        self.lowpass_enabled = true;
        self.lowpass_q = 1.2;
        let len = len.max(Time::from_millis(100));
        let peak = peak_time.max(Time::from_millis(120));
        let start = (peak - len).max(Time::ZERO);
        let hold = Time::from_secs_f64((len.as_secs_f64() * 0.37).min(1.0));
        let dip = (peak - hold).max(start + Time::from_millis(20));
        let hold_end = (peak - Time::from_millis(25)).max(dip + Time::from_millis(10));
        let mut c = crate::keyframe::Curve::new();
        c.insert(crate::keyframe::Keyframe::new(start, 20_000.0).with_interp(Interpolation::EaseOut));
        c.insert(crate::keyframe::Keyframe::new(dip, 350.0).with_interp(Interpolation::Linear));
        c.insert(crate::keyframe::Keyframe::new(hold_end, 350.0).with_interp(Interpolation::Hold));
        c.insert(crate::keyframe::Keyframe::new(peak, 20_000.0).with_interp(Interpolation::Linear));
        self.lowpass_hz = Param { value: 20_000.0, curve: Some(c) };
    }
    /// Linear gain at clip-local time `t` for a clip of `duration`, including fades.
    pub fn gain_at(&self, t: Time, duration: Time) -> f32 {
        if self.muted {
            return 0.0;
        }
        let mut db = self.volume_db.at(t) as f32;
        if self.loudness_normalize {
            db += self.loudness_gain_db.clamp(-40.0, 30.0);
        }
        let mut g = db_to_gain(db);
        // Fades span the audible extent, which split edits (J/L cuts) widen
        // beyond the video's [0, duration].
        let (lead, tail) = (self.audio_lead.max(Time::ZERO), self.audio_tail.max(Time::ZERO));
        let from_head = t + lead;
        if self.fade_in.is_positive() && from_head < self.fade_in {
            g *= self.fade_shape.gain(from_head.as_secs_f64() as f32 / self.fade_in.as_secs_f64() as f32);
        }
        let remain = duration + tail - t;
        if self.fade_out.is_positive() && remain < self.fade_out {
            g *= self.fade_shape.gain(remain.as_secs_f64() as f32 / self.fade_out.as_secs_f64() as f32);
        }
        g
    }
}

pub(crate) fn default_true() -> bool {
    true
}

fn is_true(v: &bool) -> bool {
    *v
}

fn is_default<T: Default + PartialEq>(v: &T) -> bool {
    *v == T::default()
}

pub fn db_to_gain(db: f32) -> f32 {
    if db <= -96.0 {
        0.0
    } else {
        10f32.powf(db / 20.0)
    }
}

pub fn gain_to_db(g: f32) -> f32 {
    if g <= 1e-6 {
        -120.0
    } else {
        20.0 * g.log10()
    }
}

// ---------------------------------------------------------------------------
// Titles
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum TitleStyle {
    #[default]
    Basic,
    LowerThird,
    Centered,
}

impl TitleStyle {
    pub const ALL: [TitleStyle; 3] = [TitleStyle::Basic, TitleStyle::LowerThird, TitleStyle::Centered];
    pub fn label(self) -> &'static str {
        match self {
            TitleStyle::Basic => "Basic Title",
            TitleStyle::LowerThird => "Lower Third",
            TitleStyle::Centered => "Centered",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TitleParams {
    pub text: String,
    pub subtitle: String,
    pub style: TitleStyle,
    pub font_family: String,
    /// Font size in pixels at 1080p (scaled to the output height).
    pub font_size: f32,
    /// Extra letter spacing in pixels at 1080p.
    pub tracking: f32,
    pub color: [f32; 4],
    pub shadow_enabled: bool,
    pub shadow_color: [f32; 4],
    pub shadow_offset: [f32; 2],
    pub shadow_blur: f32,
    pub pill_enabled: bool,
    pub pill_color: [f32; 4],
    pub pill_padding: f32,
    pub pill_radius: f32,
    /// Normalised text anchor in the frame (0..1).
    pub position: [f32; 2],
    #[serde(default)]
    pub animation: TextAnimation,
    /// Length of the in (and out) animation, seconds.
    #[serde(default = "default_anim_duration")]
    pub anim_duration: f32,
    #[serde(default)]
    pub outline_enabled: bool,
    #[serde(default = "default_outline_color")]
    pub outline_color: [f32; 4],
    /// Outline width in pixels at 1080p.
    #[serde(default = "default_outline_width")]
    pub outline_width: f32,
    /// Colour of the active word (word-highlight animation).
    #[serde(default = "default_highlight_color")]
    pub highlight_color: [f32; 4],
    /// Explicit per-word `[start, end]` seconds relative to the clip (captions).
    #[serde(default)]
    pub word_times: Vec<[f32; 2]>,
    /// The pill backs only the active word and glides between words.
    #[serde(default)]
    pub word_pill: bool,
    /// Word-highlight: colour of words not spoken yet (`None` = text colour).
    #[serde(default)]
    pub dim_color: Option<[f32; 4]>,
    /// A vector shape drawn instead of the text (fill = `color`; outline,
    /// shadow and animations apply as for text).
    #[serde(default)]
    pub shape: Option<ShapeSpec>,
    /// Rolling credits / crawl: the text block scrolls through the frame
    /// over the clip's duration.
    #[serde(default)]
    pub roll: RollMode,
}

/// Scroll direction of a rolling title.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum RollMode {
    #[default]
    None,
    /// Credits: bottom to top.
    Up,
    /// Crawl: right to left.
    Left,
}

impl RollMode {
    pub const ALL: [RollMode; 3] = [RollMode::None, RollMode::Up, RollMode::Left];
    pub fn label(self) -> &'static str {
        match self {
            RollMode::None => "Static",
            RollMode::Up => "Roll (credits)",
            RollMode::Left => "Crawl",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ShapeKind {
    Rectangle,
    RoundedRect,
    Ellipse,
    Triangle,
    Star,
    Line,
}

impl ShapeKind {
    pub const ALL: [ShapeKind; 6] = [ShapeKind::Rectangle, ShapeKind::RoundedRect, ShapeKind::Ellipse, ShapeKind::Triangle, ShapeKind::Star, ShapeKind::Line];
    pub fn label(self) -> &'static str {
        match self {
            ShapeKind::Rectangle => "Rectangle",
            ShapeKind::RoundedRect => "Rounded Rectangle",
            ShapeKind::Ellipse => "Ellipse",
            ShapeKind::Triangle => "Triangle",
            ShapeKind::Star => "Star",
            ShapeKind::Line => "Line",
        }
    }
}

/// A generator shape: sizes are pixels at 1080p, centred on the title position.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ShapeSpec {
    pub kind: ShapeKind,
    /// Width, height (a line's height is its thickness).
    pub size: [f32; 2],
    /// Corner radius (rounded rectangle).
    pub radius: f32,
    /// Star points.
    pub points: u32,
    /// Star inner radius as a fraction of the outer one.
    pub inner: f32,
    /// Rotation, degrees clockwise.
    pub rotation: f32,
}

impl ShapeSpec {
    pub fn new(kind: ShapeKind) -> Self {
        let size = match kind {
            ShapeKind::Line => [600.0, 12.0],
            ShapeKind::Rectangle | ShapeKind::RoundedRect => [600.0, 300.0],
            _ => [360.0, 360.0],
        };
        ShapeSpec { kind, size, radius: 40.0, points: 5, inner: 0.45, rotation: 0.0 }
    }
}

fn default_anim_duration() -> f32 {
    0.6
}
fn default_outline_color() -> [f32; 4] {
    [0.0, 0.0, 0.0, 1.0]
}
fn default_outline_width() -> f32 {
    4.0
}
fn default_highlight_color() -> [f32; 4] {
    crate::captions::HIGHLIGHT_YELLOW
}

impl Default for TitleParams {
    fn default() -> Self {
        TitleParams {
            text: "Title".into(),
            subtitle: String::new(),
            style: TitleStyle::Basic,
            font_family: "Sans".into(),
            font_size: 96.0,
            tracking: 0.0,
            color: [1.0, 1.0, 1.0, 1.0],
            shadow_enabled: true,
            shadow_color: [0.0, 0.0, 0.0, 0.6],
            shadow_offset: [4.0, 4.0],
            shadow_blur: 6.0,
            pill_enabled: false,
            pill_color: [0.04, 0.52, 1.0, 0.85],
            pill_padding: 24.0,
            pill_radius: 18.0,
            position: [0.5, 0.5],
            animation: TextAnimation::None,
            anim_duration: default_anim_duration(),
            outline_enabled: false,
            outline_color: default_outline_color(),
            outline_width: default_outline_width(),
            highlight_color: default_highlight_color(),
            word_times: Vec::new(),
            word_pill: false,
            dim_color: None,
            shape: None,
            roll: RollMode::None,
        }
    }
}

impl TitleParams {
    pub fn lower_third(name: &str, subtitle: &str) -> Self {
        TitleParams {
            text: name.into(),
            subtitle: subtitle.into(),
            style: TitleStyle::LowerThird,
            font_size: 64.0,
            pill_enabled: true,
            position: [0.08, 0.82],
            ..Default::default()
        }
    }

    /// Built-in title templates (name, params).
    pub fn templates() -> Vec<(&'static str, TitleParams)> {
        let shape = |kind: ShapeKind, color: [f32; 4]| TitleParams { text: String::new(), shape: Some(ShapeSpec::new(kind)), color, shadow_enabled: false, ..Default::default() };
        vec![
            ("Basic Title", TitleParams::default()),
            ("Lower Third", TitleParams::lower_third("Name", "Title")),
            (
                "Bold Heading",
                TitleParams {
                    text: "HEADLINE".into(),
                    font_family: crate::captions::HEAVY_FONT.into(),
                    font_size: 140.0,
                    tracking: 6.0,
                    outline_enabled: true,
                    animation: TextAnimation::PopBounce,
                    ..Default::default()
                },
            ),
            (
                "Boxed Caption",
                TitleParams {
                    text: "Caption text".into(),
                    font_size: 56.0,
                    pill_enabled: true,
                    pill_color: [0.0, 0.0, 0.0, 0.7],
                    shadow_enabled: false,
                    position: [0.5, 0.86],
                    ..Default::default()
                },
            ),
            ("Typewriter", TitleParams { text: "Once upon a time…".into(), font_size: 72.0, animation: TextAnimation::Typewriter, anim_duration: 2.0, ..Default::default() }),
            (
                "Rolling Credits",
                TitleParams {
                    text: "THE END\n\nDirector | Name\nProducer | Name\nEditor | Name\nMusic | Name\n\nThanks for watching".into(),
                    font_size: 54.0,
                    roll: RollMode::Up,
                    shadow_enabled: false,
                    ..Default::default()
                },
            ),
            (
                "News Crawl",
                TitleParams {
                    text: "BREAKING  •  Headline one  •  Headline two  •  Headline three".into(),
                    font_size: 48.0,
                    roll: RollMode::Left,
                    pill_enabled: true,
                    pill_color: [0.75, 0.05, 0.05, 0.92],
                    pill_radius: 0.0,
                    position: [0.5, 0.9],
                    shadow_enabled: false,
                    ..Default::default()
                },
            ),
            ("Rectangle", shape(ShapeKind::Rectangle, [0.04, 0.52, 1.0, 1.0])),
            ("Rounded Box", shape(ShapeKind::RoundedRect, [1.0, 1.0, 1.0, 0.9])),
            ("Circle", shape(ShapeKind::Ellipse, [1.0, 0.8, 0.1, 1.0])),
            ("Triangle", shape(ShapeKind::Triangle, [0.2, 0.85, 0.4, 1.0])),
            ("Star", shape(ShapeKind::Star, [1.0, 0.85, 0.1, 1.0])),
            ("Line", shape(ShapeKind::Line, [1.0, 1.0, 1.0, 1.0])),
        ]
    }
}

// ---------------------------------------------------------------------------
// Transitions
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum TransitionKind {
    #[default]
    CrossDissolve,
    DipToBlack,
    DipToWhite,
    WhipPan,
    Slide,
    Push,
    LumaWipe,
    ZoomPunch,
    /// Straight soft-edged wipe in the transition's direction.
    Wipe,
    /// Circle opening from the centre.
    Iris,
    /// Sweep clockwise from 12 o'clock.
    ClockWipe,
    /// Two doors opening from the centre (horizontal for Left / Right).
    BarnDoor,
    /// Zoom blur into A, cut, zoom blur out of B.
    CrossZoom,
    /// Dissolve through a blur.
    BlurDissolve,
}

impl TransitionKind {
    pub const ALL: [TransitionKind; 14] = [
        TransitionKind::CrossDissolve,
        TransitionKind::DipToBlack,
        TransitionKind::DipToWhite,
        TransitionKind::WhipPan,
        TransitionKind::Slide,
        TransitionKind::Push,
        TransitionKind::LumaWipe,
        TransitionKind::ZoomPunch,
        TransitionKind::Wipe,
        TransitionKind::Iris,
        TransitionKind::ClockWipe,
        TransitionKind::BarnDoor,
        TransitionKind::CrossZoom,
        TransitionKind::BlurDissolve,
    ];
    pub fn label(self) -> &'static str {
        match self {
            TransitionKind::CrossDissolve => "Cross Dissolve",
            TransitionKind::DipToBlack => "Dip to Black",
            TransitionKind::DipToWhite => "Dip to White",
            TransitionKind::WhipPan => "Whip Pan",
            TransitionKind::Slide => "Slide",
            TransitionKind::Push => "Push",
            TransitionKind::LumaWipe => "Luma Wipe",
            TransitionKind::ZoomPunch => "Zoom Punch",
            TransitionKind::Wipe => "Wipe",
            TransitionKind::Iris => "Iris",
            TransitionKind::ClockWipe => "Clock Wipe",
            TransitionKind::BarnDoor => "Barn Door",
            TransitionKind::CrossZoom => "Cross Zoom",
            TransitionKind::BlurDissolve => "Blur Dissolve",
        }
    }
    pub fn from_label(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|k| k.label().eq_ignore_ascii_case(s))
    }
    pub fn shader_index(self) -> u32 {
        self as u32
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Direction {
    #[default]
    Left,
    Right,
    Up,
    Down,
}

impl Direction {
    pub const ALL: [Direction; 4] = [Direction::Left, Direction::Right, Direction::Up, Direction::Down];
    pub fn label(self) -> &'static str {
        match self {
            Direction::Left => "Left",
            Direction::Right => "Right",
            Direction::Up => "Up",
            Direction::Down => "Down",
        }
    }
    /// Unit vector in normalised frame space (y down).
    pub fn vector(self) -> [f32; 2] {
        match self {
            Direction::Left => [-1.0, 0.0],
            Direction::Right => [1.0, 0.0],
            Direction::Up => [0.0, -1.0],
            Direction::Down => [0.0, 1.0],
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Transition {
    pub kind: TransitionKind,
    pub duration: Time,
    pub easing: Interpolation,
    pub direction: Direction,
    pub softness: f32,
}

impl Transition {
    pub fn new(kind: TransitionKind, duration: Time) -> Self {
        Transition { kind, duration, easing: Interpolation::Linear, direction: Direction::Left, softness: 0.1 }
    }
}

// ---------------------------------------------------------------------------
// Clip
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CompoundClip {
    pub spine: Vec<Clip>,
    /// A secondary storyline (FCP `<spine lane=…>`): a connected container
    /// whose clips ripple among themselves and render inline (transparent
    /// where it has gaps). Its length always equals its contents'.
    #[serde(default, skip_serializing_if = "is_false")]
    pub storyline: bool,
    /// Set on a multicam clip: all its angles (`spine` is the shown one).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub multicam: Option<Box<crate::multicam::Multicam>>,
}

fn is_false(b: &bool) -> bool {
    !*b
}

fn time_is_zero(t: &Time) -> bool {
    t.is_zero()
}

impl CompoundClip {
    pub fn new(spine: Vec<Clip>) -> Self {
        CompoundClip { spine, storyline: false, multicam: None }
    }

    pub fn storyline(spine: Vec<Clip>) -> Self {
        CompoundClip { spine, storyline: true, multicam: None }
    }

    pub fn duration(&self) -> Time {
        self.spine.iter().map(|c| c.duration).sum()
    }

    /// The clips heard: the heard angle of a multicam clip, else the spine.
    pub fn audio_spine(&self) -> &[Clip] {
        self.multicam.as_deref().and_then(|m| m.audio_spine()).unwrap_or(&self.spine)
    }

    /// Roles of the audio heard inside, in order of first appearance. A
    /// compound's sound routes by these, not by the compound's own role.
    pub fn heard_roles(&self) -> Vec<AudioRole> {
        fn walk(c: &Clip, out: &mut Vec<AudioRole>) {
            match &c.kind {
                ClipKind::Compound(comp) => comp.audio_spine().iter().for_each(|x| walk(x, out)),
                _ if c.has_audio() && !out.contains(&c.audio.role) => out.push(c.audio.role),
                _ => {}
            }
            c.connected.iter().for_each(|x| walk(x, out));
        }
        let mut out = Vec::new();
        self.audio_spine().iter().for_each(|c| walk(c, &mut out));
        out
    }

    /// Give every clip inside (every multicam angle too) `role`.
    pub fn set_roles(&mut self, role: AudioRole) {
        fn walk(c: &mut Clip, role: AudioRole) {
            c.audio.role = role;
            if let ClipKind::Compound(comp) = &mut c.kind {
                comp.set_roles(role);
            }
            c.connected.iter_mut().for_each(|x| walk(x, role));
        }
        self.spine.iter_mut().for_each(|c| walk(c, role));
        if let Some(m) = &mut self.multicam {
            m.angles.iter_mut().flat_map(|a| a.spine.iter_mut()).for_each(|c| walk(c, role));
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum ClipKind {
    Asset {
        asset: AssetId,
    },
    /// Slug / placeholder that renders black and silence.
    Gap,
    Title(Box<TitleParams>),
    /// Applies its color grade to everything composited beneath it.
    Adjustment,
    Compound(Box<CompoundClip>),
}

impl ClipKind {
    pub fn is_gap(&self) -> bool {
        matches!(self, ClipKind::Gap)
    }
}

/// Which streams of the clip's media are used.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct AvFlags {
    pub video: bool,
    pub audio: bool,
}

impl Default for AvFlags {
    fn default() -> Self {
        AvFlags { video: true, audio: true }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Clip {
    pub id: ClipId,
    pub name: String,
    pub kind: ClipKind,
    /// Start point within the source media.
    pub source_in: Time,
    /// Length on the timeline.
    pub duration: Time,
    /// Total length of the source media, if bounded.
    #[serde(default)]
    pub media_duration: Option<Time>,
    /// 0 = primary storyline; >0 connected above; <0 connected below.
    #[serde(default)]
    pub lane: i32,
    /// For connected clips: start relative to the parent clip's start.
    #[serde(default)]
    pub offset: Time,
    #[serde(default)]
    pub connected: Vec<Clip>,
    #[serde(default)]
    pub av: AvFlags,
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub video: VideoParams,
    #[serde(default)]
    pub color: ColorParams,
    #[serde(default)]
    pub audio: AudioParams,
    #[serde(default)]
    pub retime: Option<Retime>,
    #[serde(default)]
    pub transition_in: Option<Transition>,
    #[serde(default)]
    pub transition_out: Option<Transition>,
    /// Stackable GPU effects, applied in order after the colour grade.
    #[serde(default)]
    pub effects: Vec<Effect>,
    /// How frames are synthesised when the source frame rate is lower than
    /// the timeline's (e.g. 24 fps on a 120 fps timeline). Retimed clips use
    /// their retime's quality instead.
    #[serde(default)]
    pub rate_conform: FrameBlendMode,
    /// The single preset slot of projects saved before presets stacked per
    /// category; moved into [`Clip::preset_bases`] by the next preset action.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preset_base: Option<Box<crate::presets::PresetBase>>,
    /// What each category's montage preset changed, so a preset replaces
    /// the previous one of its own category and stacks on the others.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub preset_bases: std::collections::BTreeMap<crate::presets::PresetCategory, crate::presets::PresetBase>,
    /// Connected clips: the connection point, measured from the clip's own
    /// start (Ctrl+Alt+Click moves it). The parent is the primary storyline
    /// clip under `offset + anchor`.
    #[serde(default, skip_serializing_if = "time_is_zero")]
    pub anchor: Time,
    /// Audition: the alternate takes. This clip is the active pick.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub audition: Vec<Clip>,
    /// Point / region / planar tracks measured on this clip's source media.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tracks: Vec<crate::motion::Track>,
    /// Camera stabilization (analysed camera path + smoothing settings).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stabilization: Option<crate::motion::Stabilization>,
    /// Linked audio/video pair (see `links.rs`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub link: Option<u64>,
}

impl Clip {
    pub fn new(name: impl Into<String>, kind: ClipKind, duration: Time) -> Self {
        Clip {
            id: ClipId::new(),
            name: name.into(),
            kind,
            source_in: Time::ZERO,
            duration,
            media_duration: None,
            lane: 0,
            offset: Time::ZERO,
            connected: Vec::new(),
            av: AvFlags::default(),
            enabled: true,
            video: VideoParams::default(),
            color: ColorParams::default(),
            audio: AudioParams::default(),
            retime: None,
            transition_in: None,
            transition_out: None,
            effects: Vec::new(),
            rate_conform: FrameBlendMode::FloorFrame,
            preset_base: None,
            preset_bases: Default::default(),
            anchor: Time::ZERO,
            audition: Vec::new(),
            tracks: Vec::new(),
            stabilization: None,
            link: None,
        }
    }

    /// A secondary storyline (connected container clip).
    pub fn is_storyline(&self) -> bool {
        matches!(&self.kind, ClipKind::Compound(c) if c.storyline)
    }

    /// The connection point, clamped inside the clip.
    pub fn anchor_clamped(&self) -> Time {
        self.anchor.clamp(Time::ZERO, (self.duration - Time(1)).max(Time::ZERO))
    }

    /// Where a connected clip attaches, relative to its parent's start.
    pub fn connection_offset(&self) -> Time {
        self.offset + self.anchor_clamped()
    }

    /// A clip covering the whole of `asset`.
    pub fn from_asset(asset: &MediaAsset) -> Self {
        let (lo, hi) = asset.bounds();
        let mut c = Clip::new(asset.name.clone(), ClipKind::Asset { asset: asset.id }, hi - lo);
        c.source_in = lo;
        c.media_duration = asset.clip_media_duration();
        c.av = AvFlags { video: asset.has_video(), audio: asset.has_audio() };
        if asset.has_audio() {
            c.audio.role = AudioRole::guess(&asset.name, asset.has_video());
            if c.audio.role == AudioRole::Music {
                c.audio.ducking.enabled = true;
            }
        }
        c
    }

    /// A sub-range of an asset.
    pub fn from_asset_range(asset: &MediaAsset, source_in: Time, duration: Time) -> Self {
        let mut c = Clip::from_asset(asset);
        c.source_in = source_in;
        c.duration = duration;
        c
    }

    pub fn gap(duration: Time) -> Self {
        let mut c = Clip::new("Gap", ClipKind::Gap, duration);
        c.av = AvFlags { video: false, audio: false };
        c
    }

    pub fn title(params: TitleParams, duration: Time) -> Self {
        let name = if params.text.is_empty() { "Title".to_string() } else { params.text.clone() };
        let mut c = Clip::new(name, ClipKind::Title(Box::new(params)), duration);
        c.av = AvFlags { video: true, audio: false };
        c
    }

    pub fn adjustment(duration: Time) -> Self {
        let mut c = Clip::new("Adjustment Layer", ClipKind::Adjustment, duration);
        c.av = AvFlags { video: true, audio: false };
        c
    }

    pub fn with_lane(mut self, lane: i32) -> Self {
        self.lane = lane;
        self
    }

    pub fn is_gap(&self) -> bool {
        self.kind.is_gap()
    }

    pub fn asset_id(&self) -> Option<AssetId> {
        match self.kind {
            ClipKind::Asset { asset } => Some(asset),
            _ => None,
        }
    }

    pub fn has_video(&self) -> bool {
        self.av.video && !self.is_gap()
    }

    pub fn has_audio(&self) -> bool {
        self.av.audio && matches!(self.kind, ClipKind::Asset { .. } | ClipKind::Compound(_))
    }

    /// Source time displayed at clip-local timeline offset `local`.
    pub fn source_time(&self, local: Time) -> Time {
        match &self.retime {
            Some(r) => self.source_in + r.map(local),
            None => self.source_in + local,
        }
    }

    /// Clip-local offset at which this clip shows source time `source`, or
    /// `None` when that moment is outside the clip.
    pub fn local_for_source(&self, source: Time) -> Option<Time> {
        let local = match &self.retime {
            Some(r) => r.inverse(source - self.source_in)?,
            None => source - self.source_in,
        };
        // the end itself is outside the clip; allow one tick of rounding slack
        (local >= Time::ZERO - Time(1) && local < self.duration).then_some(local.max(Time::ZERO))
    }

    /// Current playback speed at clip-local `local` (1.0 when not retimed).
    pub fn speed_at(&self, local: Time) -> f64 {
        self.retime.as_ref().map(|r| r.speed_at(local)).unwrap_or(1.0)
    }

    /// Range of source time this clip consumes `(start, end)`.
    pub fn source_range(&self) -> (Time, Time) {
        match &self.retime {
            Some(r) => {
                let (lo, hi) = r.source_span();
                (self.source_in + lo, self.source_in + hi)
            }
            None => (self.source_in, self.source_in + self.duration),
        }
    }

    /// The transition on the cut between this clip and `next`: this clip's
    /// end transition, else `next`'s start transition.
    pub fn cut_transition<'a>(&'a self, next: &'a Clip) -> Option<&'a Transition> {
        self.transition_out.as_ref().or(next.transition_in.as_ref())
    }

    /// New ids for everything inside this clip except its connected clips.
    fn remint_contents(&mut self) {
        if let ClipKind::Compound(comp) = &mut self.kind {
            comp.spine.iter_mut().for_each(Clip::remint_ids);
        }
        self.audition.iter_mut().for_each(Clip::remint_ids);
    }

    /// New ids for this clip and everything attached to or nested in it (copies).
    pub fn remint_ids(&mut self) {
        self.id = ClipId::new();
        self.connected.iter_mut().for_each(Clip::remint_ids);
        self.remint_contents();
    }

    pub fn end_offset(&self) -> Time {
        self.offset + self.duration
    }

    /// Effective J/L-cut extension `(lead, tail)` of this clip's audio,
    /// clamped to the media that actually exists before / after the used
    /// range. Retimed and non-media clips cannot be split.
    pub fn audio_split(&self) -> (Time, Time) {
        if self.retime.is_some() || !matches!(self.kind, ClipKind::Asset { .. }) {
            return (Time::ZERO, Time::ZERO);
        }
        let lead = self.audio.audio_lead.max(Time::ZERO).min(self.source_in);
        let mut tail = self.audio.audio_tail.max(Time::ZERO);
        if let Some(md) = self.media_duration {
            tail = tail.min((md - self.source_in - self.duration).max(Time::ZERO));
        }
        (lead, tail)
    }

    /// Every parameter shown in the keyframe lane and the graph editor, with
    /// its display name (built-ins, then each effect's parameters).
    pub fn animatable_params(&self) -> Vec<(std::borrow::Cow<'static, str>, &Param)> {
        let (v, col, a) = (&self.video, &self.color, &self.audio);
        let refs: [&Param; 20] = [
            &v.opacity,
            &v.position_x,
            &v.position_y,
            &v.scale_x,
            &v.scale_y,
            &v.rotation,
            &v.anchor_x,
            &v.anchor_y,
            &v.crop_left,
            &v.crop_right,
            &v.crop_top,
            &v.crop_bottom,
            &col.exposure,
            &col.brightness,
            &col.contrast,
            &col.saturation,
            &col.temperature,
            &col.tint,
            &a.volume_db,
            &a.pan,
        ];
        let mut out: Vec<_> = ANIMATABLE_NAMES.iter().zip(refs).map(|(n, p)| (std::borrow::Cow::Borrowed(*n), p)).collect();
        for e in &self.effects {
            for (d, p) in e.kind.params().iter().zip(&e.params) {
                out.push((std::borrow::Cow::Owned(format!("{}: {}", e.kind.label(), d.name)), p));
            }
        }
        out
    }

    /// [`Clip::animatable_params`], mutable (same order and names).
    pub fn animatable_params_mut(&mut self) -> Vec<(std::borrow::Cow<'static, str>, &mut Param)> {
        let (v, col, a) = (&mut self.video, &mut self.color, &mut self.audio);
        let refs: [&mut Param; 20] = [
            &mut v.opacity,
            &mut v.position_x,
            &mut v.position_y,
            &mut v.scale_x,
            &mut v.scale_y,
            &mut v.rotation,
            &mut v.anchor_x,
            &mut v.anchor_y,
            &mut v.crop_left,
            &mut v.crop_right,
            &mut v.crop_top,
            &mut v.crop_bottom,
            &mut col.exposure,
            &mut col.brightness,
            &mut col.contrast,
            &mut col.saturation,
            &mut col.temperature,
            &mut col.tint,
            &mut a.volume_db,
            &mut a.pan,
        ];
        let mut out: Vec<_> = ANIMATABLE_NAMES.iter().zip(refs).map(|(n, p)| (std::borrow::Cow::Borrowed(*n), p)).collect();
        for e in &mut self.effects {
            let label = e.kind.label();
            for (d, p) in e.kind.params().iter().zip(&mut e.params) {
                out.push((std::borrow::Cow::Owned(format!("{label}: {}", d.name)), p));
            }
        }
        out
    }

    /// Visit every keyframable parameter of the clip.
    /// Move every keyframe (parameters and colour nodes) by `dt`.
    pub fn shift_keys(&mut self, dt: Time) {
        self.for_each_param_mut(|p| p.shift(dt));
        self.color.corrections.iter_mut().for_each(|n| n.shift_keys(dt));
        self.effects.iter_mut().filter_map(|e| e.mask.as_mut()).for_each(|m| m.shift_keys(dt));
        self.effects.iter_mut().for_each(|e| e.trigger += dt);
    }

    pub fn for_each_param_mut(&mut self, mut f: impl FnMut(&mut Param)) {
        for p in self.video.params_mut() {
            f(p);
        }
        for p in self.color.params_mut() {
            f(p);
        }
        f(&mut self.audio.volume_db);
        f(&mut self.audio.pan);
        f(&mut self.audio.lowpass_hz);
        for e in &mut self.effects {
            for p in &mut e.params {
                f(p);
            }
        }
        if let Some(g) = &mut self.color.graph {
            g.for_each_param_mut(&mut f);
        }
    }

    /// Frame-synthesis quality for this clip (retime quality when retimed).
    pub fn frame_quality(&self) -> FrameBlendMode {
        self.retime.as_ref().map(|r| r.blend).unwrap_or(self.rate_conform)
    }

    /// Split into `(left, right)` at clip-local `at`. `left` keeps this id.
    pub fn split(&self, at: Time) -> Option<(Clip, Clip)> {
        if at <= Time::ZERO || at >= self.duration {
            return None;
        }
        let mut left = self.clone();
        let mut right = self.clone();
        right.id = ClipId::new();
        // both halves hold a copy of the contents: one of them needs its own ids
        right.remint_contents();
        left.duration = at;
        right.duration = self.duration - at;

        match &self.retime {
            Some(r) => {
                let (l, rr) = r.split_at(at);
                left.retime = Some(l);
                right.retime = Some(rr);
            }
            None => right.source_in = self.source_in + at,
        }

        left.for_each_param_mut(|p| p.keep_left(at));
        right.for_each_param_mut(|p| p.keep_right(at));
        left.color.corrections.iter_mut().for_each(|n| n.keep_left(at));
        right.color.corrections.iter_mut().for_each(|n| n.keep_right(at));
        left.effects.iter_mut().filter_map(|e| e.mask.as_mut()).for_each(|m| m.keep_left(at));
        right.effects.iter_mut().filter_map(|e| e.mask.as_mut()).for_each(|m| m.keep_right(at));
        right.effects.iter_mut().for_each(|e| e.trigger -= at);

        left.anchor = if self.anchor < at { self.anchor } else { Time::ZERO };
        right.anchor = if self.anchor >= at { self.anchor - at } else { Time::ZERO };
        left.connected = self.connected.iter().filter(|c| c.connection_offset() < at).cloned().collect();
        right.connected = self
            .connected
            .iter()
            .filter(|c| c.connection_offset() >= at)
            .cloned()
            .map(|mut c| {
                c.offset -= at;
                c
            })
            .collect();

        left.transition_out = None;
        right.transition_in = None;
        left.audio.fade_out = Time::ZERO;
        right.audio.fade_in = Time::ZERO;
        left.audio.crossfade_out = Time::ZERO;
        right.audio.crossfade_in = Time::ZERO;
        Some((left, right))
    }

    /// Recursively collect the ids of this clip and everything nested in it.
    pub fn collect_ids(&self, out: &mut Vec<ClipId>) {
        out.push(self.id);
        for c in &self.connected {
            c.collect_ids(out);
        }
        if let ClipKind::Compound(comp) = &self.kind {
            for c in &comp.spine {
                c.collect_ids(out);
            }
        }
    }

    /// Every asset this clip, its connected clips, nested clips and
    /// audition takes use.
    pub fn collect_assets(&self, out: &mut std::collections::HashSet<AssetId>) {
        if let Some(a) = self.asset_id() {
            out.insert(a);
        }
        out.extend(self.color.graph.iter().flat_map(|g| g.media()));
        for c in self.connected.iter().chain(self.audition.iter()) {
            c.collect_assets(out);
        }
        if let ClipKind::Compound(comp) = &self.kind {
            for c in &comp.spine {
                c.collect_assets(out);
            }
        }
    }

    /// Highest id in this clip, its nested clips and their audition takes
    /// (so freshly generated ids never collide after a load).
    pub fn max_id(&self) -> u64 {
        let own = self.id.0;
        let connected = self.connected.iter().map(Clip::max_id).max().unwrap_or(0);
        let takes = self.audition.iter().map(Clip::max_id).max().unwrap_or(0);
        let inner = match &self.kind {
            ClipKind::Compound(comp) => comp.spine.iter().map(Clip::max_id).max().unwrap_or(0),
            _ => 0,
        };
        // link ids come from the same counter
        own.max(connected).max(takes).max(inner).max(self.link.unwrap_or(0))
    }
}

#[cfg(test)]
mod sweep_tests {
    use super::*;

    #[test]
    fn long_underwater_sweep_holds_the_muffle_until_the_drop() {
        let mut a = AudioParams::default();
        a.apply_underwater_sweep(Time::from_secs(10), Time::from_secs(4));
        let hz = |s: f64| a.lowpass_hz.at(Time::from_secs_f64(s));
        assert!(hz(5.9) > 19_000.0, "open before the build: {}", hz(5.9));
        assert!(hz(7.5) < 15_000.0, "closing during the build: {}", hz(7.5));
        assert!((hz(9.5) - 350.0).abs() < 1.0, "underwater just before the drop: {}", hz(9.5));
        assert!(hz(10.05) > 19_000.0, "open on the drop: {}", hz(10.05));
        // the classic one-click sweep keeps its 0.6 s shape
        let mut b = AudioParams::default();
        b.apply_pre_kill_underwater_sweep(Time::from_secs(1));
        assert!((b.lowpass_hz.at(Time::from_millis(800)) - 350.0).abs() < 1.0);
        assert!(b.lowpass_hz.at(Time::from_millis(350)) > 19_000.0);
    }
}
