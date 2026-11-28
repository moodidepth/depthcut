//! Authentic CS2 & Valorant Pro Montage Preset Library.
//!
//! Pre-populated fragmovie presets across RSMB Motion Blur, Cubic Bezier
//! Velocity Curves, Sapphire-style Impact Shakes, Valorant/CS2 Color Correction
//! & Signature FX, and Whip-Pan / Zoom-Punch transitions, plus user preset
//! persistence under `presets/user/`.

use crate::clip::{Clip, ColorParams, ColorWheel, Direction, Transition, TransitionKind};
use crate::effects::{Effect, EffectKind};
use crate::keyframe::{Curve, Interpolation, Keyframe, Param};
use crate::speed::Retime;
use crate::time::{FrameRate, Time};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum PresetCategory {
    RsmbMotionBlur,
    VelocityGraph,
    ShakeAndImpact,
    ColorAndSignatureFx,
    Transitions,
}

impl PresetCategory {
    pub const ALL: [PresetCategory; 5] =
        [PresetCategory::RsmbMotionBlur, PresetCategory::VelocityGraph, PresetCategory::ShakeAndImpact, PresetCategory::ColorAndSignatureFx, PresetCategory::Transitions];

    pub fn label(self) -> &'static str {
        match self {
            PresetCategory::RsmbMotionBlur => "RSMB & Motion Blur",
            PresetCategory::VelocityGraph => "Velocity Curves",
            PresetCategory::ShakeAndImpact => "S_Shake & Impact",
            PresetCategory::ColorAndSignatureFx => "CS2 / Valorant CC & FX",
            PresetCategory::Transitions => "Transitions",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ColorPreset {
    pub exposure: f64,
    pub contrast: f64,
    pub saturation: f64,
    pub temperature: f64,
    pub tint: f64,
    pub shadows: ColorWheel,
    pub midtones: ColorWheel,
    pub highlights: ColorWheel,
}

impl Default for ColorPreset {
    fn default() -> Self {
        Self {
            exposure: 0.0,
            contrast: 0.0,
            saturation: 1.0,
            temperature: 0.0,
            tint: 0.0,
            shadows: ColorWheel::default(),
            midtones: ColorWheel::default(),
            highlights: ColorWheel::default(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MontagePreset {
    pub name: String,
    pub category: PresetCategory,
    #[serde(default)]
    pub game_tag: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub velocity_preset: Option<String>,
    #[serde(default)]
    pub color: Option<ColorPreset>,
    #[serde(default)]
    pub effects: Vec<Effect>,
    #[serde(default)]
    pub transition_in: Option<Transition>,
    #[serde(default)]
    pub freeze_frames: Option<i64>,
    #[serde(default)]
    pub zoom_push_scale: Option<f64>,
}

/// Most effects a preset file may carry (guards against corrupt files).
pub const MAX_PRESET_EFFECTS: usize = 16;

/// A clip field's value before and after the last preset touched it.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Change<T> {
    pub before: T,
    pub after: T,
}

impl<T: Clone + PartialEq> Change<T> {
    fn same(v: T) -> Self {
        Change { before: v.clone(), after: v }
    }
    /// Value to go back to: `before` while `current` is still what the
    /// preset set; if the user edited it since, `current` wins.
    fn revert(&self, current: &T) -> T {
        if *current == self.after {
            self.before.clone()
        } else {
            current.clone()
        }
    }
}

/// What the last montage preset changed on a clip (see [`Clip::preset_base`]).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PresetBase {
    pub name: String,
    pub retime: Change<Option<Retime>>,
    pub duration: Change<Time>,
    pub source_in: Change<Time>,
    #[serde(default)]
    pub preserve_pitch: Change<bool>,
    pub color: Change<ColorParams>,
    pub transition_in: Change<Option<Transition>>,
    pub scale_x: Change<Param>,
    pub scale_y: Change<Param>,
    /// User effects of the same kind that preset effects replaced.
    #[serde(default)]
    pub displaced_effects: Vec<Effect>,
}

impl PresetBase {
    fn capture(c: &Clip) -> Self {
        PresetBase {
            name: String::new(),
            retime: Change::same(c.retime.clone()),
            duration: Change::same(c.duration),
            source_in: Change::same(c.source_in),
            preserve_pitch: Change::same(c.audio.preserve_pitch),
            color: Change::same(c.color.clone()),
            transition_in: Change::same(c.transition_in.clone()),
            scale_x: Change::same(c.video.scale_x.clone()),
            scale_y: Change::same(c.video.scale_y.clone()),
            displaced_effects: Vec::new(),
        }
    }
}

/// Undo the last preset on `clip`; returns the clip's pre-preset state.
fn revert_preset(clip: &mut Clip) -> PresetBase {
    if let Some(b) = clip.preset_base.take() {
        // timing reverts as a unit, and only if untouched since: a blade or
        // trim after the preset makes the current timing the new base
        if clip.retime == b.retime.after && clip.duration == b.duration.after && clip.source_in == b.source_in.after {
            clip.retime = b.retime.before.clone();
            clip.duration = b.duration.before;
            clip.source_in = b.source_in.before;
            clip.audio.preserve_pitch = b.preserve_pitch.revert(&clip.audio.preserve_pitch);
        }
        clip.color = b.color.revert(&clip.color);
        clip.transition_in = b.transition_in.revert(&clip.transition_in);
        clip.video.scale_x = b.scale_x.revert(&clip.video.scale_x);
        clip.video.scale_y = b.scale_y.revert(&clip.video.scale_y);
        clip.effects.retain(|e| !e.from_preset);
        for e in b.displaced_effects {
            if !clip.effects.iter().any(|x| x.id == e.id) {
                clip.effects.push(e);
            }
        }
    } else {
        clip.effects.retain(|e| !e.from_preset);
    }
    PresetBase::capture(clip)
}

fn anim_param(default: f64, keys: &[(f64, f64, Interpolation)]) -> Param {
    let mut curve = Curve::new();
    for &(secs, val, interp) in keys {
        curve.insert(Keyframe::new(Time::from_secs_f64(secs), val).with_interp(interp));
    }
    Param { value: default, curve: Some(curve) }
}

fn effect_with(kind: EffectKind, updates: &[(usize, Param)]) -> Effect {
    let mut e = Effect::new(kind);
    for (idx, p) in updates {
        if let Some(slot) = e.params.get_mut(*idx) {
            *slot = p.clone();
        }
    }
    e.normalize();
    e
}

fn shift_param(p: &mut Param, dt: Time) {
    if !dt.is_zero() {
        p.shift(dt);
    }
}

impl MontagePreset {
    /// All 16 built-in CS2 & Valorant pro montage presets.
    pub fn built_in() -> Vec<MontagePreset> {
        let f60 = |n: i64| Time::from_frames(n, FrameRate::FPS_60).as_secs_f64();
        vec![
            // 1. RSMB & High-FPS Motion Blur Presets
            MontagePreset {
                name: "RSMB Clean Flick (Valorant/CS2)".into(),
                category: PresetCategory::RsmbMotionBlur,
                game_tag: "Valorant / CS2".into(),
                description: "Blur Amount 0.25, Sensitivity 82%, 16 samples — smooths fast mouse flicks while keeping HUD/crosshair razor-sharp.".into(),
                velocity_preset: None,
                color: None,
                effects: vec![effect_with(
                    EffectKind::MotionBlur,
                    &[(0, Param::new(180.0)), (1, Param::new(16.0)), (2, Param::new(0.5)), (3, Param::new(1.0)), (4, Param::new(0.25)), (5, Param::new(82.0))],
                )],
                transition_in: None,
                freeze_frames: None,
                zoom_push_scale: None,
            },
            MontagePreset {
                name: "RSMB 180° Cinematic".into(),
                category: PresetCategory::RsmbMotionBlur,
                game_tag: "Universal".into(),
                description: "Blur Amount 0.50, Sensitivity 72%, 24 samples — rich filmic blur for slow-mo cinematics and HLAE freecam shots.".into(),
                velocity_preset: None,
                color: None,
                effects: vec![effect_with(
                    EffectKind::MotionBlur,
                    &[(0, Param::new(180.0)), (1, Param::new(24.0)), (2, Param::new(0.35)), (3, Param::new(1.0)), (4, Param::new(0.50)), (5, Param::new(72.0))],
                )],
                transition_in: None,
                freeze_frames: None,
                zoom_push_scale: None,
            },
            // 2. Velocity Graph Presets (Cubic Bezier Time-Remapping)
            MontagePreset {
                name: "Valorant Flow Sync (Fast-In / Kill-Hold / Snap-Out)".into(),
                category: PresetCategory::VelocityGraph,
                game_tag: "Valorant".into(),
                description: "Ramps 280% during strafe/peek -> sharp Bezier drop to 25% held 10 frames on kill banner -> 340% cut exit.".into(),
                velocity_preset: Some("Valorant Flow Sync".into()),
                color: None,
                effects: Vec::new(),
                transition_in: None,
                freeze_frames: None,
                zoom_push_scale: None,
            },
            MontagePreset {
                name: "CS2 AWP / Deagle Double-Pump".into(),
                category: PresetCategory::VelocityGraph,
                game_tag: "CS2".into(),
                description: "220% lead-in -> 18% shot 1 impact (8 frames) -> 300% bolt-cycle whip -> 30% second kill -> 100%.".into(),
                velocity_preset: Some("CS2 AWP / Deagle Double-Pump".into()),
                color: None,
                effects: Vec::new(),
                transition_in: None,
                freeze_frames: None,
                zoom_push_scale: None,
            },
            MontagePreset {
                name: "Clean Skill Showcase (Pro Gameplay)".into(),
                category: PresetCategory::VelocityGraph,
                game_tag: "CS2 / Valorant".into(),
                description: "Keeps 100% native speed through aim flick -> smooth Bezier ease to 45% right after kill -> 100% exit.".into(),
                velocity_preset: Some("Clean Skill Showcase".into()),
                color: None,
                effects: Vec::new(),
                transition_in: None,
                freeze_frames: None,
                zoom_push_scale: None,
            },
            MontagePreset {
                name: "Bullet-Time Kill Ramp".into(),
                category: PresetCategory::VelocityGraph,
                game_tag: "Universal".into(),
                description: "Fast 220% entry -> deep 12% bullet-time hold w/ Weighted Accumulation -> 240% exit.".into(),
                velocity_preset: Some("Bullet-Time Kill Ramp".into()),
                color: None,
                effects: Vec::new(),
                transition_in: None,
                freeze_frames: None,
                zoom_push_scale: None,
            },
            MontagePreset {
                name: "Smooth Ease".into(),
                category: PresetCategory::VelocityGraph,
                game_tag: "Universal".into(),
                description: "Smooth 160% -> 35% -> 160% S-curve speed ramp.".into(),
                velocity_preset: Some("Smooth Ease".into()),
                color: None,
                effects: Vec::new(),
                transition_in: None,
                freeze_frames: None,
                zoom_push_scale: None,
            },
            // 3. Sapphire-Style Shake & Impact Combos
            MontagePreset {
                name: "Hard Kill Impact (S_Shake Decay)".into(),
                category: PresetCategory::ShakeAndImpact,
                game_tag: "Valorant / CS2".into(),
                description: "42px high-freq S_Shake w/ 1.08x Z-punch, 2.2° tilt, 0.75 MoBlur, Mirror wrap decaying over 14 frames + 18% white flash + 6px RGB split.".into(),
                velocity_preset: None,
                color: None,
                effects: vec![
                    effect_with(
                        EffectKind::CameraShake,
                        &[
                            (0, Param::new(0.85)),
                            (1, Param::new(10.0)),
                            (2, Param::new(0.0)),
                            (3, Param::new(0.0)),
                            (4, Param::new(1.0)),
                            (5, Param::new(11.0)),
                            (6, Param::new(12.0)),
                            (7, Param::new(1.08)),
                            (8, Param::new(2.2)),
                            (9, Param::new(0.75)),
                            (10, Param::new(0.0)),
                            (11, Param::new(6.0)),
                        ],
                    ),
                    effect_with(EffectKind::ImpactFlash, &[(0, Param::new(0.18)), (1, Param::new(24.0))]),
                    effect_with(
                        EffectKind::RgbSplitGlitch,
                        &[(0, anim_param(6.0, &[(0.0, 6.0, Interpolation::EaseOut), (f60(12), 0.0, Interpolation::Hold)])), (2, Param::new(0.85))],
                    ),
                ],
                transition_in: None,
                freeze_frames: None,
                zoom_push_scale: None,
            },
            MontagePreset {
                name: "Pre-Drop Build-Up Riser Shake".into(),
                category: PresetCategory::ShakeAndImpact,
                game_tag: "Universal".into(),
                description: "Ramps X/Y amplitude 0px -> 12px and frequency 10 Hz -> 28 Hz over 18 frames into the beat drop or sniper shot.".into(),
                velocity_preset: None,
                color: None,
                effects: vec![effect_with(
                    EffectKind::CameraShake,
                    &[
                        (0, anim_param(0.32, &[(0.0, 0.0, Interpolation::EaseIn), (f60(18), 0.34, Interpolation::Linear)])),
                        (1, anim_param(6.0, &[(0.0, 3.3, Interpolation::EaseIn), (f60(18), 9.4, Interpolation::Linear)])),
                        (4, Param::new(0.0)),
                        (8, Param::new(0.8)),
                        (10, Param::new(0.0)),
                    ],
                )],
                transition_in: None,
                freeze_frames: None,
                zoom_push_scale: None,
            },
            MontagePreset {
                name: "Slow-Mo Cinematic Hover".into(),
                category: PresetCategory::ShakeAndImpact,
                game_tag: "Universal".into(),
                description: "Gentle organic handheld wave drift (3.5px @ 1.8 Hz, 0.4° tilt) so slow-motion shots stay alive.".into(),
                velocity_preset: None,
                color: None,
                effects: vec![effect_with(
                    EffectKind::CameraShake,
                    &[(0, Param::new(0.10)), (1, Param::new(1.8)), (2, Param::new(0.14)), (4, Param::new(0.0)), (8, Param::new(0.4)), (10, Param::new(0.0))],
                )],
                transition_in: None,
                freeze_frames: None,
                zoom_push_scale: None,
            },
            // 4. CS2 & Valorant Color Correction & Signature FX
            MontagePreset {
                name: "Valorant Vibrant Clean CC".into(),
                category: PresetCategory::ColorAndSignatureFx,
                game_tag: "Valorant".into(),
                description: "+12% contrast, shadow lift -6%, highlight roll-off -10%, +18% vibrance, teal shadows/warm highlights + soft Screen glow (0.80 / 32px / 0.25) that lifts ability VFX without clipping them.".into(),
                velocity_preset: None,
                color: Some(ColorPreset {
                    exposure: 0.05,
                    contrast: 0.12,
                    saturation: 1.18,
                    temperature: 0.04,
                    tint: 0.0,
                    shadows: ColorWheel { r: -0.03, g: -0.01, b: 0.02, master: -0.06 },
                    midtones: ColorWheel::default(),
                    highlights: ColorWheel { r: 0.04, g: 0.02, b: -0.02, master: -0.10 },
                }),
                effects: vec![effect_with(EffectKind::Glow, &[(0, Param::new(0.80)), (1, Param::new(32.0)), (2, Param::new(0.25)), (6, Param::new(1.0))])],
                transition_in: None,
                freeze_frames: None,
                zoom_push_scale: None,
            },
            MontagePreset {
                name: "CS2 Moody Fragmovie CC".into(),
                category: PresetCategory::ColorAndSignatureFx,
                game_tag: "CS2".into(),
                description: "Crushed blacks (-10%), desaturated greens (-20%), warm amber/gold muzzle pop + 35mm Halation (0.28) + 20% vignette.".into(),
                velocity_preset: None,
                color: Some(ColorPreset {
                    exposure: -0.05,
                    contrast: 0.16,
                    saturation: 0.86,
                    temperature: 0.10,
                    tint: -0.12,
                    shadows: ColorWheel { r: -0.02, g: -0.02, b: 0.0, master: -0.10 },
                    midtones: ColorWheel { r: 0.02, g: 0.01, b: -0.02, master: 0.0 },
                    highlights: ColorWheel { r: 0.09, g: 0.05, b: -0.04, master: 0.04 },
                }),
                effects: vec![
                    effect_with(EffectKind::Halation, &[(0, Param::new(0.72)), (1, Param::new(28.0)), (2, Param::new(0.28)), (3, Param::new(0.85))]),
                    effect_with(EffectKind::ImpactFlash, &[(0, Param::new(0.0)), (5, Param::new(0.20)), (6, Param::new(1.0))]),
                ],
                transition_in: None,
                freeze_frames: None,
                zoom_push_scale: None,
            },
            MontagePreset {
                name: "Operator / AWP Scope Pulse".into(),
                category: PresetCategory::ColorAndSignatureFx,
                game_tag: "CS2 / Valorant".into(),
                description: "5-frame scope-in radial zoom-blur (0.35), chromatic aberration (8px), and 35% vignette that snaps cleanly to 0 on shot.".into(),
                velocity_preset: None,
                color: None,
                effects: vec![
                    effect_with(
                        EffectKind::ZoomBlur,
                        &[(2, anim_param(0.35, &[(0.0, 0.0, Interpolation::EaseIn), (f60(5), 0.35, Interpolation::Hold), (f60(6), 0.0, Interpolation::Hold)]))],
                    ),
                    effect_with(
                        EffectKind::RgbSplitGlitch,
                        &[
                            (0, anim_param(8.0, &[(0.0, 0.0, Interpolation::EaseIn), (f60(5), 8.0, Interpolation::Hold), (f60(6), 0.0, Interpolation::Hold)])),
                            (2, Param::new(0.90)),
                        ],
                    ),
                    effect_with(
                        EffectKind::ImpactFlash,
                        &[
                            (0, Param::new(0.0)),
                            (5, anim_param(0.35, &[(0.0, 0.35, Interpolation::EaseIn), (f60(5), 0.35, Interpolation::Hold), (f60(6), 0.0, Interpolation::Hold)])),
                            (6, anim_param(1.12, &[(0.0, 1.12, Interpolation::EaseIn), (f60(6), 1.0, Interpolation::Hold)])),
                        ],
                    ),
                ],
                transition_in: None,
                freeze_frames: None,
                zoom_push_scale: None,
            },
            MontagePreset {
                name: "Ace Final-Kill Freeze Outline".into(),
                category: PresetCategory::ColorAndSignatureFx,
                game_tag: "Valorant / CS2".into(),
                description: "10-frame final-kill showcase w/ Sobel neon/white character silhouette outline (0.85) + smooth 1.0x -> 1.12x Z-push.".into(),
                velocity_preset: None,
                color: None,
                effects: vec![effect_with(
                    EffectKind::NeonOutline,
                    &[
                        (0, Param::new(0.85)),
                        (1, Param::new(0.22)),
                        (2, Param::new(2.0)),
                        (3, Param::new(0.92)),
                        (4, Param::new(0.98)),
                        (5, Param::new(1.0)),
                        (6, Param::new(0.25)),
                    ],
                )],
                transition_in: None,
                freeze_frames: Some(10),
                zoom_push_scale: Some(1.12),
            },
            // 5. Transition Presets
            MontagePreset {
                name: "Whip-Pan Directional Blur (8 frames)".into(),
                category: PresetCategory::Transitions,
                game_tag: "Universal".into(),
                description: "8-frame high-speed directional Whip-Pan transition w/ motion blur.".into(),
                velocity_preset: None,
                color: None,
                effects: Vec::new(),
                transition_in: Some(Transition {
                    kind: TransitionKind::WhipPan,
                    duration: Time::from_frames(8, FrameRate::FPS_60),
                    easing: Interpolation::EaseInOut,
                    direction: Direction::Right,
                    softness: 0.1,
                }),
                freeze_frames: None,
                zoom_push_scale: None,
            },
            MontagePreset {
                name: "Sniper Zoom-In Punch (10 frames)".into(),
                category: PresetCategory::Transitions,
                game_tag: "CS2 / Valorant".into(),
                description: "10-frame exponential 1.0x -> 2.4x Z-zoom punch w/ radial blur and RGB split peak at cut.".into(),
                velocity_preset: None,
                color: None,
                effects: Vec::new(),
                transition_in: Some(Transition {
                    kind: TransitionKind::ZoomPunch,
                    duration: Time::from_frames(10, FrameRate::FPS_60),
                    easing: Interpolation::EaseIn,
                    direction: Direction::Left,
                    softness: 0.1,
                }),
                freeze_frames: None,
                zoom_push_scale: None,
            },
        ]
    }

    pub fn find(name: &str) -> Option<MontagePreset> {
        let q = name.trim().to_lowercase();
        let all = Self::built_in();
        all.iter().find(|p| p.name.to_lowercase() == q).cloned().or_else(|| all.into_iter().find(|p| p.name.to_lowercase().contains(&q)))
    }

    /// Clamp every value to the range the UI / renderer supports, so a
    /// hand-edited or corrupt preset file can never push a clip out of bounds.
    pub fn sanitize(&mut self) {
        fn fin(v: f64, lo: f64, hi: f64, def: f64) -> f64 {
            if v.is_finite() {
                v.clamp(lo, hi)
            } else {
                def
            }
        }
        fn wheel(w: &mut ColorWheel) {
            for c in [&mut w.r, &mut w.g, &mut w.b, &mut w.master] {
                *c = if c.is_finite() { c.clamp(-1.0, 1.0) } else { 0.0 };
            }
        }
        self.name = self.name.trim().chars().take(96).collect();
        if self.name.is_empty() {
            self.name = "Preset".into();
        }
        if let Some(c) = &mut self.color {
            c.exposure = fin(c.exposure, -4.0, 4.0, 0.0);
            c.contrast = fin(c.contrast, -1.0, 1.0, 0.0);
            c.saturation = fin(c.saturation, 0.0, 2.0, 1.0);
            c.temperature = fin(c.temperature, -1.0, 1.0, 0.0);
            c.tint = fin(c.tint, -1.0, 1.0, 0.0);
            wheel(&mut c.shadows);
            wheel(&mut c.midtones);
            wheel(&mut c.highlights);
        }
        self.effects.truncate(MAX_PRESET_EFFECTS);
        for e in &mut self.effects {
            e.normalize();
            let defs = e.kind.params();
            for (p, d) in e.params.iter_mut().zip(defs.iter()) {
                p.value = fin(p.value, d.min, d.max, d.default);
                if let Some(c) = p.curve.as_mut() {
                    for k in &mut c.keys {
                        k.value = fin(k.value, d.min, d.max, d.default);
                    }
                }
            }
        }
        if let Some(t) = &mut self.transition_in {
            t.duration = t.duration.clamp(Time::from_millis(1), Time::from_secs(10));
            t.softness = if t.softness.is_finite() { t.softness.clamp(0.0, 1.0) } else { 0.0 };
        }
        self.freeze_frames = self.freeze_frames.map(|f| f.clamp(1, 600));
        self.zoom_push_scale = self.zoom_push_scale.filter(|z| z.is_finite()).map(|z| z.clamp(0.25, 4.0));
    }

    /// Remove whatever the last preset applied to `clip` (no-op without one).
    pub fn clear_from_clip(clip: &mut Clip) {
        revert_preset(clip);
    }

    /// Apply this preset to `clip` at clip-local offset `at_local`.
    ///
    /// Idempotent: whatever a previous preset set on this clip is reverted
    /// first (from [`Clip::preset_base`]), so applying presets repeatedly
    /// replaces instead of stacking effects, multiplying speed or scale.
    /// Values the user edited after the last preset are kept.
    pub fn apply_to_clip(&self, clip: &mut Clip, at_local: Time) {
        let mut preset = self.clone();
        preset.sanitize();
        let base = revert_preset(clip);
        let at_local = at_local.clamp(Time::ZERO, clip.duration);
        let mut applied = PresetBase { name: preset.name.clone(), ..base.clone() };

        // same-kind user effects are parked in the base (restored by the
        // next revert) so a preset shake never doubles a user shake
        if !preset.effects.is_empty() {
            let (displaced, kept): (Vec<Effect>, Vec<Effect>) = std::mem::take(&mut clip.effects).into_iter().partition(|e| preset.effects.iter().any(|t| t.kind == e.kind));
            clip.effects = kept;
            applied.displaced_effects = displaced;
        }
        if let Some(vp) = &preset.velocity_preset {
            // from the un-retimed clip length, never from a previous ramp
            if let Some(r) = Retime::velocity_preset(vp, base.duration.before) {
                clip.duration = r.timeline_duration();
                clip.retime = Some(r);
                // speed ramps keep pitch (crossfaded WSOLA stretch in the mixer)
                clip.audio.preserve_pitch = true;
            }
        }
        if let Some(cp) = &preset.color {
            clip.color = ColorParams {
                enabled: true,
                exposure: Param::new(cp.exposure),
                contrast: Param::new(cp.contrast),
                saturation: Param::new(cp.saturation),
                temperature: Param::new(cp.temperature),
                tint: Param::new(cp.tint),
                shadows: cp.shadows,
                midtones: cp.midtones,
                highlights: cp.highlights,
                // keep the clip's LUT, colour management and correction nodes
                ..clip.color.clone()
            };
        }
        for template in &preset.effects {
            let mut e = Effect::new(template.kind);
            e.params = template.params.clone();
            e.enabled = template.enabled;
            e.from_preset = true;
            e.trigger = at_local;
            e.normalize();
            if at_local.is_positive() {
                for p in &mut e.params {
                    shift_param(p, at_local);
                }
            }
            clip.effects.push(e);
        }
        if let Some(tr) = &preset.transition_in {
            clip.transition_in = Some(tr.clone());
        }
        if let (Some(frames), Some(target)) = (preset.freeze_frames, preset.zoom_push_scale) {
            let end_t = (at_local + Time::from_frames(frames.max(1), FrameRate::FPS_60)).min(clip.duration);
            if end_t > at_local {
                // multiply the pre-preset scale on both axes: a uniform clip
                // stays uniform, a non-uniform one keeps its aspect ratio
                let key = |b: f64| anim_param(b, &[(at_local.as_secs_f64(), b, Interpolation::EaseOut), (end_t.as_secs_f64(), b * target, Interpolation::Hold)]);
                clip.video.scale_x = key(clip.video.scale_x.value);
                clip.video.scale_y = key(clip.video.scale_y.value);
            }
        }

        applied.retime.after = clip.retime.clone();
        applied.duration.after = clip.duration;
        applied.source_in.after = clip.source_in;
        applied.preserve_pitch.after = clip.audio.preserve_pitch;
        applied.color.after = clip.color.clone();
        applied.transition_in.after = clip.transition_in.clone();
        applied.scale_x.after = clip.video.scale_x.clone();
        applied.scale_y.after = clip.video.scale_y.clone();
        clip.preset_base = Some(Box::new(applied));
    }

    pub fn filename_slug(name: &str) -> String {
        let mut s = String::new();
        let mut prev_dash = false;
        for ch in name.chars() {
            if ch.is_ascii_alphanumeric() {
                s.push(ch.to_ascii_lowercase());
                prev_dash = false;
            } else if !prev_dash && !s.is_empty() {
                s.push('_');
                prev_dash = true;
            }
        }
        let s = s.trim_end_matches('_').to_string();
        if s.is_empty() {
            "preset".into()
        } else {
            s
        }
    }

    pub fn from_clip(name: impl Into<String>, category: PresetCategory, clip: &Clip) -> Self {
        let color = if clip.color.is_identity() {
            None
        } else {
            Some(ColorPreset {
                exposure: clip.color.exposure.value,
                contrast: clip.color.contrast.value,
                saturation: clip.color.saturation.value,
                temperature: clip.color.temperature.value,
                tint: clip.color.tint.value,
                shadows: clip.color.shadows,
                midtones: clip.color.midtones,
                highlights: clip.color.highlights,
            })
        };
        MontagePreset {
            name: name.into(),
            category,
            game_tag: "Custom".into(),
            description: format!("User preset from '{}'", clip.name),
            velocity_preset: None,
            color,
            effects: clip.effects.clone(),
            transition_in: clip.transition_in.clone(),
            freeze_frames: None,
            zoom_push_scale: None,
        }
    }

    pub fn save_user_preset(&self, user_dir: &Path) -> std::io::Result<PathBuf> {
        std::fs::create_dir_all(user_dir)?;
        let path = user_dir.join(format!("{}.json", Self::filename_slug(&self.name)));
        let json = serde_json::to_string_pretty(self).map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        std::fs::write(&path, json)?;
        Ok(path)
    }

    pub fn load_user_presets(user_dir: &Path) -> Vec<MontagePreset> {
        let mut out = Vec::new();
        let Ok(entries) = std::fs::read_dir(user_dir) else { return out };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) == Some("json") {
                if let Ok(text) = std::fs::read_to_string(&path) {
                    if let Ok(mut p) = serde_json::from_str::<MontagePreset>(&text) {
                        p.sanitize();
                        out.push(p);
                    }
                }
            }
        }
        out.sort_by(|a, b| a.name.cmp(&b.name));
        out
    }

    pub fn export_built_ins_to_dir(dir: &Path) -> std::io::Result<Vec<PathBuf>> {
        std::fs::create_dir_all(dir)?;
        std::fs::create_dir_all(dir.join("user"))?;
        let mut written = Vec::new();
        for p in Self::built_in() {
            let path = dir.join(format!("{}.json", Self::filename_slug(&p.name)));
            let json = serde_json::to_string_pretty(&p).map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
            std::fs::write(&path, json)?;
            written.push(path);
        }
        Ok(written)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clip::Clip;

    #[test]
    fn all_16_builtin_presets_round_trip_and_apply() {
        let presets = MontagePreset::built_in();
        assert_eq!(presets.len(), 16);
        for p in &presets {
            let json = serde_json::to_string_pretty(p).unwrap();
            let back: MontagePreset = serde_json::from_str(&json).unwrap();
            assert_eq!(*p, back);
            let mut c = Clip::gap(Time::from_secs(3));
            p.apply_to_clip(&mut c, Time::from_millis(500));
            for e in &c.effects {
                assert_eq!(e.params.len(), e.kind.params().len());
            }
        }
        assert!(MontagePreset::find("Hard Kill Impact").is_some());
        assert!(MontagePreset::find("Valorant Vibrant Clean CC").is_some());
        assert!(MontagePreset::find("Operator / AWP Scope Pulse").is_some());
        assert!(MontagePreset::find("RSMB Clean Flick").is_some());
    }

    #[test]
    fn a_preset_at_the_playhead_fires_its_hit_there() {
        let p = MontagePreset::find("Hard Kill Impact").unwrap();
        let mut c = Clip::gap(Time::from_secs(20));
        p.apply_to_clip(&mut c, Time::from_secs(12));
        let flash = c.effects.iter().find(|e| e.kind == EffectKind::ImpactFlash).unwrap();
        assert_eq!(flash.trigger, Time::from_secs(12));
        assert_eq!(flash.resolve(Time::from_secs(12)).elapsed, 0.0);
        assert!((flash.resolve(Time::from_millis(12_250)).elapsed - 0.25).abs() < 1e-4);
        assert!(flash.resolve(Time::from_secs(11)).elapsed < 0.0, "not fired yet");

        // the trigger stays on the same frame through a split and a head trim
        let (_, right) = c.split(Time::from_secs(10)).unwrap();
        let flash = right.effects.iter().find(|e| e.kind == EffectKind::ImpactFlash).unwrap();
        assert_eq!(flash.trigger, Time::from_secs(2));
        let mut trimmed = c.clone();
        trimmed.shift_keys(Time::ZERO - Time::from_secs(5));
        assert_eq!(trimmed.effects.iter().find(|e| e.kind == EffectKind::ImpactFlash).unwrap().trigger, Time::from_secs(7));

        // projects saved before triggers existed fire from the clip start
        let mut json = serde_json::to_value(Effect::new(EffectKind::ImpactFlash)).unwrap();
        assert!(json.get("trigger").is_none(), "a zero trigger is not written");
        json.as_object_mut().unwrap().remove("trigger");
        let old: Effect = serde_json::from_value(json).unwrap();
        assert_eq!(old.trigger, Time::ZERO);
    }

    #[test]
    fn reapplying_presets_replaces_instead_of_stacking() {
        let presets = MontagePreset::built_in();
        let with_fx: Vec<_> = presets.iter().filter(|p| !p.effects.is_empty()).collect();
        assert!(with_fx.len() >= 2);
        let mut c = Clip::gap(Time::from_secs(3));
        let user = Effect::new(EffectKind::FilmGrain);
        if !with_fx.iter().any(|p| p.effects.iter().any(|e| e.kind == user.kind)) {
            c.effects.push(user.clone());
        }
        let n_user = c.effects.len();
        let a = with_fx[0];
        for _ in 0..3 {
            a.apply_to_clip(&mut c, Time::ZERO);
        }
        assert_eq!(c.effects.len(), n_user + a.effects.len(), "same preset 3× must not stack");
        let b = with_fx[1];
        b.apply_to_clip(&mut c, Time::ZERO);
        assert_eq!(c.effects.len(), n_user + b.effects.len(), "switching presets replaces the previous one");
        if n_user == 1 {
            assert!(c.effects.iter().any(|e| e.id == user.id), "user effects survive");
        }
    }

    fn velocity_preset() -> MontagePreset {
        MontagePreset::built_in().into_iter().find(|p| p.velocity_preset.is_some()).expect("a built-in speed-ramp preset")
    }

    #[test]
    fn every_preset_pair_applies_idempotently() {
        let all = MontagePreset::built_in();
        let fresh = Clip::gap(Time::from_secs(3));
        for a in &all {
            let mut once = fresh.clone();
            a.apply_to_clip(&mut once, Time::from_millis(400));
            let mut thrice = once.clone();
            a.apply_to_clip(&mut thrice, Time::from_millis(400));
            a.apply_to_clip(&mut thrice, Time::from_millis(400));
            let strip = |c: &Clip| {
                let mut c = c.clone();
                c.effects.iter_mut().for_each(|e| e.id = 0);
                c
            };
            assert_eq!(strip(&once), strip(&thrice), "'{}' re-applied must be identical", a.name);
            for b in &all {
                let mut ab = once.clone();
                b.apply_to_clip(&mut ab, Time::from_millis(400));
                let mut only_b = fresh.clone();
                b.apply_to_clip(&mut only_b, Time::from_millis(400));
                assert_eq!(strip(&ab).preset_base.map(|p| p.name), strip(&only_b).preset_base.map(|p| p.name));
                let (x, y) = (strip(&ab), strip(&only_b));
                assert_eq!(
                    (x.duration, x.retime, x.color, x.transition_in, x.video),
                    (y.duration, y.retime, y.color, y.transition_in, y.video),
                    "'{}' then '{}' must equal '{}' alone",
                    a.name,
                    b.name,
                    b.name
                );
                assert_eq!(x.effects.len(), y.effects.len());
            }
        }
    }

    #[test]
    fn speed_ramp_never_compounds_and_reverts_timing() {
        let vp = velocity_preset();
        let mut c = Clip::gap(Time::from_secs(4));
        let base_dur = c.duration;
        vp.apply_to_clip(&mut c, Time::ZERO);
        let d1 = c.duration;
        assert!(c.retime.is_some() && c.audio.preserve_pitch);
        for _ in 0..4 {
            vp.apply_to_clip(&mut c, Time::ZERO);
        }
        assert_eq!(c.duration, d1, "re-applying a speed ramp must not multiply speed");
        MontagePreset::clear_from_clip(&mut c);
        assert_eq!((c.duration, c.retime.is_none(), c.preset_base.is_none()), (base_dur, true, true));
    }

    #[test]
    fn zoom_push_keeps_aspect_ratio_and_user_scale() {
        let zp = MontagePreset::built_in().into_iter().find(|p| p.zoom_push_scale.is_some()).expect("zoom preset");
        let mut c = Clip::gap(Time::from_secs(3));
        c.video.scale_x = Param::new(0.5);
        c.video.scale_y = Param::new(0.5);
        zp.apply_to_clip(&mut c, Time::from_millis(200));
        zp.apply_to_clip(&mut c, Time::from_millis(200));
        let target = zp.zoom_push_scale.unwrap();
        for ms in [0, 200, 250, 400, 2000] {
            let t = Time::from_millis(ms);
            let (x, y) = (c.video.scale_x.at(t), c.video.scale_y.at(t));
            assert!((x - y).abs() < 1e-9, "uniform scale stays uniform at {ms} ms ({x} vs {y})");
            assert!(x >= 0.5 - 1e-9 && x <= 0.5 * target + 1e-9);
        }
        // non-uniform clips keep their ratio
        let mut n = Clip::gap(Time::from_secs(3));
        n.video.scale_x = Param::new(1.0);
        n.video.scale_y = Param::new(0.5);
        zp.apply_to_clip(&mut n, Time::ZERO);
        let t = Time::from_secs(2);
        assert!((n.video.scale_x.at(t) / n.video.scale_y.at(t) - 2.0).abs() < 1e-9);
        MontagePreset::clear_from_clip(&mut n);
        assert_eq!((n.video.scale_x, n.video.scale_y), (Param::new(1.0), Param::new(0.5)));
    }

    #[test]
    fn user_edits_after_a_preset_survive_the_next_one() {
        let all = MontagePreset::built_in();
        let cc = all.iter().find(|p| p.color.is_some()).unwrap();
        let tr = all.iter().find(|p| p.transition_in.is_some() && p.color.is_none()).unwrap();
        let mut c = Clip::gap(Time::from_secs(3));
        cc.apply_to_clip(&mut c, Time::ZERO);
        c.color.exposure = Param::new(1.25); // user tweak
        tr.apply_to_clip(&mut c, Time::ZERO);
        assert_eq!(c.color.exposure.value, 1.25);
    }

    #[test]
    fn corrupt_preset_values_are_clamped() {
        let mut p = MontagePreset::find("Valorant Vibrant Clean CC").unwrap();
        let c = p.color.as_mut().unwrap();
        c.exposure = 99.0;
        c.saturation = f64::NAN;
        c.shadows.r = -50.0;
        p.zoom_push_scale = Some(1e9);
        p.freeze_frames = Some(-5);
        p.effects.push(Effect::new(EffectKind::FilmGrain));
        p.effects.last_mut().unwrap().params[0].value = 1e12;
        let json = serde_json::to_string(&p).unwrap().replace("NaN", "null");
        let dir = std::env::temp_dir().join(format!("rcp-preset-clamp-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("bad.json"), json).unwrap();
        let mut p2 = p.clone();
        p2.sanitize();
        let c = p2.color.as_ref().unwrap();
        assert_eq!((c.exposure, c.saturation, c.shadows.r), (4.0, 1.0, -1.0));
        assert_eq!((p2.zoom_push_scale, p2.freeze_frames), (Some(4.0), Some(1)));
        let grain = p2.effects.last().unwrap();
        let def = &grain.kind.params()[0];
        assert!(grain.params[0].value <= def.max);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn preset_application_is_one_undo_step() {
        use crate::command::{Command, History};
        use crate::timeline::Timeline;
        let mut tl = Timeline::default();
        let clip = Clip::gap(Time::from_secs(4));
        let id = clip.id;
        tl.append(clip).unwrap();
        let before = tl.clone();
        let mut h = History::default();
        let mut c = tl.clip(id).cloned().unwrap();
        velocity_preset().apply_to_clip(&mut c, Time::ZERO);
        h.execute_coalesced(&mut tl, Command::ReplaceClip { label: "Apply Preset", clip: Box::new(c) }, None).unwrap();
        assert!(tl.clip(id).unwrap().retime.is_some() && tl.clip(id).unwrap().preset_base.is_some());
        assert!(h.undo(&mut tl));
        assert_eq!(tl.clip(id), before.clip(id));
        assert_eq!(tl.extent(), before.extent());
    }

    #[test]
    fn user_presets_save_and_load_from_disk() {
        let dir = std::env::temp_dir().join(format!("argon-presets-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let written = MontagePreset::export_built_ins_to_dir(&dir).unwrap();
        assert_eq!(written.len(), 16);
        let p = MontagePreset::find("Hard Kill Impact").unwrap();
        let saved = p.save_user_preset(&dir.join("user")).unwrap();
        assert!(saved.exists());
        let loaded = MontagePreset::load_user_presets(&dir.join("user"));
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].name, p.name);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    #[ignore]
    fn write_repo_presets() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("..").join("..").join("presets");
        let paths = MontagePreset::export_built_ins_to_dir(&dir).unwrap();
        assert_eq!(paths.len(), 16);
    }
}
