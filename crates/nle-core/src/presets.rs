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

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
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

/// What a category's last montage preset changed on a clip (see
/// [`Clip::preset_bases`]).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PresetBase {
    pub name: String,
    /// When it was applied relative to the clip's other categories (higher =
    /// later): presets come off newest first.
    #[serde(default)]
    pub order: u32,
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
            order: 0,
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

/// A preset applied after `removed` recorded `removed`'s result as its own
/// "before"; once `removed` is gone, point it at what `removed` replaced.
fn rebase<T: Clone + PartialEq>(later: &mut Change<T>, removed: &Change<T>) {
    if removed.before != removed.after && later.before == removed.after {
        if later.after == later.before {
            later.after = removed.before.clone();
        }
        later.before = removed.before.clone();
    }
}

impl PresetBase {
    fn rebase_onto(&mut self, removed: &PresetBase) {
        rebase(&mut self.retime, &removed.retime);
        rebase(&mut self.duration, &removed.duration);
        rebase(&mut self.source_in, &removed.source_in);
        rebase(&mut self.preserve_pitch, &removed.preserve_pitch);
        rebase(&mut self.color, &removed.color);
        rebase(&mut self.transition_in, &removed.transition_in);
        rebase(&mut self.scale_x, &removed.scale_x);
        rebase(&mut self.scale_y, &removed.scale_y);
    }
}

/// Category of a preset saved in the single slot projects used before
/// presets stacked: a built-in's own, else guessed from what it changed.
fn legacy_category(b: &PresetBase, clip: &Clip) -> PresetCategory {
    let q = b.name.trim().to_lowercase();
    if let Some(p) = MontagePreset::built_in().into_iter().find(|p| p.name.to_lowercase() == q) {
        return p.category;
    }
    let timing = b.retime.before != b.retime.after || b.duration.before != b.duration.after;
    let look = b.color.before != b.color.after || b.scale_x.before != b.scale_x.after || b.scale_y.before != b.scale_y.after;
    let mut fx = clip.effects.iter().filter(|e| e.from_preset && e.preset_category.is_none()).peekable();
    if timing {
        PresetCategory::VelocityGraph
    } else if look {
        PresetCategory::ColorAndSignatureFx
    } else if b.transition_in.before != b.transition_in.after {
        PresetCategory::Transitions
    } else if fx.peek().is_some() && fx.all(|e| e.kind == EffectKind::MotionBlur) {
        PresetCategory::RsmbMotionBlur
    } else {
        PresetCategory::ShakeAndImpact
    }
}

fn next_order(clip: &Clip) -> u32 {
    clip.preset_bases.values().map(|b| b.order).max().unwrap_or(0) + 1
}

/// Move the single preset slot of an older project into its category.
fn migrate_legacy(clip: &mut Clip) {
    if let Some(b) = clip.preset_base.take() {
        let cat = legacy_category(&b, clip);
        for e in clip.effects.iter_mut().filter(|e| e.from_preset && e.preset_category.is_none()) {
            e.preset_category = Some(cat);
        }
        let base = PresetBase { order: next_order(clip), ..*b };
        clip.preset_bases.insert(cat, base);
    }
}

/// Take category `cat`'s preset off `clip`; returns the clip's state
/// without it. Other categories' presets and the user's edits stay.
fn revert_category(clip: &mut Clip, cat: PresetCategory) -> PresetBase {
    migrate_legacy(clip);
    // preset effects whose preset record is gone (older projects): any
    // preset clears those, as it always did
    clip.effects.retain(|e| !(e.from_preset && e.preset_category.is_none()));
    if let Some(b) = clip.preset_bases.remove(&cat) {
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
        clip.effects.retain(|e| e.preset_category != Some(cat));
        for later in clip.preset_bases.values_mut().filter(|o| o.order > b.order) {
            later.rebase_onto(&b);
        }
        for e in b.displaced_effects {
            if clip.effects.iter().any(|x| x.id == e.id) {
                continue;
            }
            // another category's preset holds that kind now: park the user's
            // effect with it until that one comes off too
            let owner = clip.effects.iter().find(|x| x.kind == e.kind).and_then(|x| x.preset_category);
            match owner.and_then(|o| clip.preset_bases.get_mut(&o)) {
                Some(ob) => ob.displaced_effects.push(e),
                None => clip.effects.push(e),
            }
        }
    }
    PresetBase::capture(clip)
}

/// One montage preset on a clip, as the preset badges show it.
#[derive(Clone, Debug, PartialEq)]
pub struct AppliedPreset {
    pub category: PresetCategory,
    pub name: String,
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

    /// Remove every montage preset from `clip`, newest first. Returns
    /// whether there was any.
    pub fn clear_from_clip(clip: &mut Clip) -> bool {
        migrate_legacy(clip);
        let had = !clip.preset_bases.is_empty() || clip.effects.iter().any(|e| e.from_preset);
        let mut stack: Vec<(u32, PresetCategory)> = clip.preset_bases.iter().map(|(c, b)| (b.order, *c)).collect();
        stack.sort_unstable();
        for (_, cat) in stack.into_iter().rev() {
            revert_category(clip, cat);
        }
        clip.effects.retain(|e| !e.from_preset);
        had
    }

    /// Remove only category `cat`'s preset from `clip`; the other
    /// categories stay. Returns whether that category had one.
    pub fn clear_category(clip: &mut Clip, cat: PresetCategory) -> bool {
        migrate_legacy(clip);
        let had = clip.preset_bases.contains_key(&cat);
        revert_category(clip, cat);
        had
    }

    /// The presets on `clip`, in the order they were applied.
    pub fn applied(clip: &Clip) -> Vec<AppliedPreset> {
        let mut out: Vec<(u32, AppliedPreset)> = clip.preset_bases.iter().map(|(c, b)| (b.order, AppliedPreset { category: *c, name: b.name.clone() })).collect();
        if let Some(b) = &clip.preset_base {
            out.push((0, AppliedPreset { category: legacy_category(b, clip), name: b.name.clone() }));
        }
        out.sort_by_key(|(o, _)| *o);
        out.into_iter().map(|(_, p)| p).collect()
    }

    /// Apply this preset to `clip` at clip-local offset `at_local`.
    ///
    /// Presets stack per [`PresetCategory`]: whatever this category's
    /// previous preset set is reverted first (from [`Clip::preset_bases`]),
    /// so re-applying replaces instead of stacking effects, multiplying
    /// speed or scale, while other categories' presets stay. Values the user
    /// edited since are kept.
    pub fn apply_to_clip(&self, clip: &mut Clip, at_local: Time) {
        let mut preset = self.clone();
        preset.sanitize();
        let cat = preset.category;
        let base = revert_category(clip, cat);
        let at_local = at_local.clamp(Time::ZERO, clip.duration);
        let mut applied = PresetBase { name: preset.name.clone(), order: next_order(clip), ..base.clone() };

        // same-kind user effects are parked in the base (restored by the
        // next revert) so a preset shake never doubles a user shake; other
        // categories' preset effects stay theirs
        if !preset.effects.is_empty() {
            let (displaced, kept): (Vec<Effect>, Vec<Effect>) =
                std::mem::take(&mut clip.effects).into_iter().partition(|e| e.preset_category.is_none() && preset.effects.iter().any(|t| t.kind == e.kind));
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
            e.preset_category = Some(cat);
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
        clip.preset_bases.insert(cat, applied);
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
        assert!(MontagePreset::find("Hard Kill Impact (S_Shake Decay)").is_some());
        assert!(MontagePreset::find("Valorant Vibrant Clean CC").is_some());
        assert!(MontagePreset::find("Operator / AWP Scope Pulse").is_some());
        assert!(MontagePreset::find("RSMB Clean Flick").is_some());
    }

    #[test]
    fn a_preset_at_the_playhead_fires_its_hit_there() {
        let p = MontagePreset::find("Hard Kill Impact (S_Shake Decay)").unwrap();
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
        let a = MontagePreset::find("Hard Kill Impact (S_Shake Decay)").unwrap();
        let b = MontagePreset::find("Pre-Drop Build-Up Riser Shake").unwrap();
        assert_eq!(a.category, b.category);
        let mut c = Clip::gap(Time::from_secs(3));
        let user = Effect::new(EffectKind::FilmGrain);
        assert!(![&a, &b].iter().any(|p| p.effects.iter().any(|e| e.kind == user.kind)));
        c.effects.push(user.clone());
        for _ in 0..3 {
            a.apply_to_clip(&mut c, Time::ZERO);
        }
        assert_eq!(c.effects.len(), 1 + a.effects.len(), "same preset 3× must not stack");
        b.apply_to_clip(&mut c, Time::ZERO);
        assert_eq!(c.effects.len(), 1 + b.effects.len(), "a preset replaces the previous one of its category");
        assert!(c.effects.iter().any(|e| e.id == user.id), "user effects survive");
        assert_eq!(MontagePreset::applied(&c), vec![AppliedPreset { category: b.category, name: b.name.clone() }]);
    }

    fn strip(c: &Clip) -> Clip {
        let mut c = c.clone();
        c.effects.iter_mut().for_each(|e| e.id = 0);
        c
    }

    /// What a clip looks and plays like, without bookkeeping (effect ids,
    /// stack order).
    fn look(c: &Clip) -> (Time, Time, Option<Retime>, ColorParams, Option<Transition>, crate::clip::VideoParams, Vec<Effect>) {
        let c = strip(c);
        (c.duration, c.source_in, c.retime, c.color, c.transition_in, c.video, c.effects)
    }

    fn names(c: &Clip) -> Vec<String> {
        MontagePreset::applied(c).into_iter().map(|p| p.name).collect()
    }

    fn velocity_preset() -> MontagePreset {
        MontagePreset::built_in().into_iter().find(|p| p.velocity_preset.is_some()).expect("a built-in speed-ramp preset")
    }

    #[test]
    fn every_preset_pair_applies_idempotently() {
        let all = MontagePreset::built_in();
        let fresh = Clip::gap(Time::from_secs(3));
        let at = Time::from_millis(400);
        for a in &all {
            let mut once = fresh.clone();
            a.apply_to_clip(&mut once, at);
            let mut thrice = once.clone();
            a.apply_to_clip(&mut thrice, at);
            a.apply_to_clip(&mut thrice, at);
            assert_eq!(strip(&once), strip(&thrice), "'{}' re-applied must be identical", a.name);
            for b in &all {
                let mut ab = once.clone();
                b.apply_to_clip(&mut ab, at);
                let mut only_b = fresh.clone();
                b.apply_to_clip(&mut only_b, at);
                if a.category == b.category {
                    // same category: B replaces A
                    assert_eq!(names(&ab), vec![b.name.clone()]);
                    assert_eq!(look(&ab), look(&only_b), "'{}' then '{}' must equal '{}' alone", a.name, b.name, b.name);
                    continue;
                }
                // different categories stack, and each comes off on its own
                assert_eq!(names(&ab), vec![a.name.clone(), b.name.clone()], "'{}' + '{}' must stack", a.name, b.name);
                assert_eq!(ab.effects.len(), once.effects.len() + only_b.effects.len(), "'{}' + '{}' keeps both effect sets", a.name, b.name);
                let mut ab_again = ab.clone();
                b.apply_to_clip(&mut ab_again, at);
                assert_eq!(strip(&ab_again), strip(&ab), "'{}' re-applied on top of '{}' must be identical", b.name, a.name);
                let mut minus_a = ab.clone();
                assert!(MontagePreset::clear_category(&mut minus_a, a.category));
                assert_eq!(look(&minus_a), look(&only_b), "'{}' + '{}' without '{}' must equal '{}' alone", a.name, b.name, a.name, b.name);
                assert_eq!(names(&minus_a), vec![b.name.clone()]);
                let mut minus_b = ab.clone();
                assert!(MontagePreset::clear_category(&mut minus_b, b.category));
                assert_eq!(look(&minus_b), look(&once), "'{}' + '{}' without '{}' must equal '{}' alone", a.name, b.name, b.name, a.name);
                let mut none = ab.clone();
                assert!(MontagePreset::clear_from_clip(&mut none));
                assert_eq!(look(&none), look(&fresh));
                assert!(none.preset_bases.is_empty() && none.preset_base.is_none());
            }
        }
    }

    #[test]
    fn a_full_montage_stack_survives_and_comes_apart_cleanly() {
        let p = |n: &str| MontagePreset::find(n).unwrap_or_else(|| panic!("{n}"));
        let stack =
            [p("RSMB 180° Cinematic"), p("CS2 Moody Fragmovie CC"), p("Hard Kill Impact (S_Shake Decay)"), p("Bullet-Time Kill Ramp"), p("Sniper Zoom-In Punch (10 frames)")];
        let cats: std::collections::BTreeSet<_> = stack.iter().map(|p| p.category).collect();
        assert_eq!(cats.len(), 5, "one preset per category");
        let mut fresh = Clip::gap(Time::from_secs(4));
        let grain = Effect::new(EffectKind::FilmGrain);
        let shake = Effect::new(EffectKind::CameraShake); // Hard Kill Impact brings its own
        fresh.effects.extend([grain.clone(), shake.clone()]);
        let mut c = fresh.clone();
        for (i, preset) in stack.iter().enumerate() {
            preset.apply_to_clip(&mut c, Time::from_millis(500));
            if i == 1 {
                c.color.exposure = Param::new(0.75); // the user's tweak on top of the grade
            }
        }
        assert_eq!(names(&c), stack.iter().map(|p| p.name.clone()).collect::<Vec<_>>(), "all five stay");
        assert!(c.effects.iter().any(|e| e.id == grain.id), "the user's grain stays");
        assert!(!c.effects.iter().any(|e| e.id == shake.id), "the user's shake gives way to the preset's");
        let preset_fx: usize = stack.iter().map(|p| p.effects.len()).sum();
        assert_eq!(c.effects.len(), 1 + preset_fx);
        assert!(c.retime.is_some() && c.transition_in.is_some());
        assert!(c.effects.iter().any(|e| e.kind == EffectKind::MotionBlur && e.preset_category == Some(PresetCategory::RsmbMotionBlur)));

        // re-applying a middle one keeps the stack and the effect count
        let mut again = c.clone();
        stack[2].apply_to_clip(&mut again, Time::from_millis(500));
        assert_eq!(again.preset_bases.len(), 5);
        assert_eq!(look(&again), look(&c));
        let mut thrice = again.clone();
        stack[2].apply_to_clip(&mut thrice, Time::from_millis(500));
        assert_eq!(strip(&thrice), strip(&again), "re-applying the newest is strictly idempotent");

        // clearing the grade keeps the rest, and the user's tweak
        let mut no_cc = c.clone();
        assert!(MontagePreset::clear_category(&mut no_cc, PresetCategory::ColorAndSignatureFx));
        assert!(!MontagePreset::clear_category(&mut no_cc, PresetCategory::ColorAndSignatureFx), "nothing left to clear");
        assert_eq!(no_cc.preset_bases.len(), 4);
        assert_eq!(no_cc.effects.len(), 1 + preset_fx - stack[1].effects.len());
        assert_eq!((no_cc.duration, &no_cc.retime, &no_cc.transition_in), (c.duration, &c.retime, &c.transition_in));
        assert_eq!(no_cc.color.exposure.value, 0.75);

        // clearing the impact gives the user's shake back
        let mut no_impact = c.clone();
        MontagePreset::clear_category(&mut no_impact, PresetCategory::ShakeAndImpact);
        assert!(no_impact.effects.iter().any(|e| e.id == shake.id));
        assert_eq!(no_impact.preset_bases.len(), 4);

        // everything off: the clip is back, except what the user did
        let mut bare = c.clone();
        assert!(MontagePreset::clear_from_clip(&mut bare));
        assert!(!MontagePreset::clear_from_clip(&mut bare));
        let mut ids: Vec<u64> = bare.effects.iter().map(|e| e.id).collect();
        ids.sort_unstable();
        let mut want = vec![grain.id, shake.id];
        want.sort_unstable();
        assert_eq!(ids, want);
        assert_eq!((bare.duration, &bare.retime, &bare.transition_in, &bare.video), (fresh.duration, &fresh.retime, &fresh.transition_in, &fresh.video));
        assert_eq!(bare.color.exposure.value, 0.75);
        assert!(bare.preset_bases.is_empty());
    }

    #[test]
    fn clearing_an_older_preset_keeps_a_newer_one_over_the_same_field() {
        // a user preset may set a field another category also sets
        let cc = MontagePreset::find("Valorant Vibrant Clean CC").unwrap();
        let mut graded_wipe = MontagePreset::find("Whip-Pan Directional Blur (8 frames)").unwrap();
        graded_wipe.color = MontagePreset::find("CS2 Moody Fragmovie CC").unwrap().color;
        let fresh = Clip::gap(Time::from_secs(3));
        let mut only_wipe = fresh.clone();
        graded_wipe.apply_to_clip(&mut only_wipe, Time::ZERO);

        let mut c = fresh.clone();
        cc.apply_to_clip(&mut c, Time::ZERO);
        graded_wipe.apply_to_clip(&mut c, Time::ZERO);
        let mut x = c.clone();
        MontagePreset::clear_category(&mut x, cc.category);
        assert_eq!(x.color, only_wipe.color, "the newer grade stays");
        MontagePreset::clear_category(&mut x, graded_wipe.category);
        assert_eq!(x.color, fresh.color, "and comes off to the original, not to the older preset's");
        let mut y = c.clone();
        MontagePreset::clear_category(&mut y, graded_wipe.category);
        MontagePreset::clear_category(&mut y, cc.category);
        assert_eq!(y.color, fresh.color);
    }

    #[test]
    fn projects_from_before_stacking_load_and_migrate() {
        let impact = MontagePreset::find("Hard Kill Impact (S_Shake Decay)").unwrap();
        let mut c = Clip::gap(Time::from_secs(3));
        c.effects.push(Effect::new(EffectKind::FilmGrain));
        impact.apply_to_clip(&mut c, Time::ZERO);
        // as the single-slot format stored it
        let mut legacy = c.clone();
        let b = legacy.preset_bases.remove(&impact.category).unwrap();
        legacy.preset_base = Some(Box::new(PresetBase { order: 0, ..b }));
        legacy.effects.iter_mut().for_each(|e| e.preset_category = None);
        let json = serde_json::to_string(&legacy).unwrap();
        assert!(json.contains("\"preset_base\"") && !json.contains("preset_bases") && !json.contains("preset_category"));
        let mut old: Clip = serde_json::from_str(&json).unwrap();
        assert_eq!(names(&old), vec![impact.name.clone()], "badges show the old preset before any edit");

        let mut stacked = old.clone();
        MontagePreset::find("CS2 Moody Fragmovie CC").unwrap().apply_to_clip(&mut stacked, Time::ZERO);
        assert_eq!(names(&stacked), vec![impact.name.clone(), "CS2 Moody Fragmovie CC".to_string()]);
        assert_eq!(stacked.effects.len(), c.effects.len() + MontagePreset::find("CS2 Moody Fragmovie CC").unwrap().effects.len());
        assert!(stacked.preset_base.is_none());

        let riser = MontagePreset::find("Pre-Drop Build-Up Riser Shake").unwrap();
        riser.apply_to_clip(&mut old, Time::ZERO);
        assert_eq!(names(&old), vec![riser.name.clone()]);
        assert_eq!(old.effects.len(), 1 + riser.effects.len(), "the old preset is replaced, not doubled");

        // a renamed user preset is placed by what it changed
        let mut ramp = Clip::gap(Time::from_secs(3));
        velocity_preset().apply_to_clip(&mut ramp, Time::ZERO);
        let b = ramp.preset_bases.remove(&PresetCategory::VelocityGraph).unwrap();
        ramp.preset_base = Some(Box::new(PresetBase { name: "my ramp".into(), ..b }));
        assert_eq!(MontagePreset::applied(&ramp)[0].category, PresetCategory::VelocityGraph);
        assert!(MontagePreset::clear_category(&mut ramp, PresetCategory::VelocityGraph));
        assert!(ramp.retime.is_none() && ramp.preset_base.is_none());
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
        assert_eq!((c.duration, c.retime.is_none(), c.preset_bases.is_empty()), (base_dur, true, true));
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
        assert!(tl.clip(id).unwrap().retime.is_some() && tl.clip(id).unwrap().preset_bases.contains_key(&PresetCategory::VelocityGraph));
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
        let p = MontagePreset::find("Hard Kill Impact (S_Shake Decay)").unwrap();
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
