//! Stackable GPU effects ("ResolveFX") applied to clips and adjustment clips.
//!
//! Every effect is a [`EffectKind`] plus a list of keyframable [`Param`]s whose
//! meaning, range and default come from the kind's static [`ParamDef`] table.
//! The same table drives the inspector UI, keyframes, serialization and the
//! renderer's uniform packing, so adding a parameter is a one-line change.

use crate::keyframe::Param;
use crate::time::Time;
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicU64, Ordering};

/// Maximum number of scalar parameters an effect can have (packed into
/// three `vec4<f32>` uniforms on the GPU).
pub const MAX_EFFECT_PARAMS: usize = 12;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum EffectKind {
    /// Vector motion blur driven by dense optical flow.
    MotionBlur,
    /// 35 mm film halation: warm glow bleeding around highlights.
    Halation,
    /// Procedural handheld / impact camera shake (`S_Shake` style).
    CameraShake,
    /// Animated, luminance-weighted Cineon-style film grain.
    FilmGrain,
    /// Threshold bloom / Deep Glow.
    Glow,
    /// Linear (whip-pan) blur along an angle.
    DirectionalBlur,
    /// Radial (punch-in) blur from a centre point.
    ZoomBlur,
    /// YCbCr chroma keyer for green / blue screen.
    ChromaKey,
    /// Radial / directional RGB chromatic aberration & digital beat-drop glitch.
    RgbSplitGlitch,
    /// Exponential impact flash tint + sniper/scope radial zoom vignette.
    ImpactFlash,
    /// Sobel edge-detection neon / Saber character & weapon outline glow.
    NeonOutline,
    /// Stretch-free 9:16 gaming layouts (split stack / centre fit / full crop)
    /// with an aspect-locked killfeed PiP. Outputs a 9:16 layer.
    GamingReframe916,
    /// Separable RGBA Gaussian blur with independent horizontal / vertical radius.
    GaussianBlur,
    /// Rotational (spin) blur around a centre point.
    RadialBlur,
    /// Elliptical darkening (or brightening) towards the frame edges.
    Vignette,
    /// Luma unsharp mask with a noise threshold.
    Sharpen,
    /// Mosaic blocks or halftone-style dots.
    Pixelate,
    /// Brown–Conrady barrel / pincushion lens model with lateral chromatic aberration.
    LensDistortion,
    /// Hue rotation, saturation and lightness.
    HueSatLightness,
    /// Input / output black & white points with gamma, per channel or RGB.
    Levels,
    /// AI person / subject matte (local ONNX segmentation model): cut out
    /// the subject or the background, or show the matte.
    PersonMatte,
    /// AI depth map (local ONNX depth model): show depth, depth fog, or
    /// keep only a near / far range.
    DepthMap,
    /// Reflects one half (or a quadrant) of the frame onto the other.
    Mirror,
    /// Radial mirror segments around a centre.
    Kaleidoscope,
    /// Quantises each channel to a few levels.
    Posterize,
    /// Negative image (RGB) or inverted brightness (luma).
    Invert,
    /// Black & white, sepia or a two-colour duotone map.
    Tint,
    /// Sobel edges, emboss relief or a pencil-sketch look.
    FindEdges,
    /// Cinema bars for a chosen aspect ratio.
    Letterbox,
}

/// Which per-frame AI matte an effect reads (see `FrameSource::matte`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum MatteKind {
    Person,
    Depth,
}

impl MatteKind {
    pub const ALL: [MatteKind; 2] = [MatteKind::Person, MatteKind::Depth];

    pub fn label(self) -> &'static str {
        match self {
            MatteKind::Person => "Person Matte",
            MatteKind::Depth => "Depth Map",
        }
    }

    /// File-system friendly name (cache folders, model files).
    pub fn slug(self) -> &'static str {
        match self {
            MatteKind::Person => "person",
            MatteKind::Depth => "depth",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ParamKind {
    /// Continuous value.
    Number,
    /// Whole-number value (sample counts).
    Integer,
    /// One of the listed options; the value is the option index.
    Choice(&'static [&'static str]),
    /// One colour channel (0..1); three consecutive Color params form an RGB colour.
    Color,
}

/// Static description of one effect parameter.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ParamDef {
    pub name: &'static str,
    pub min: f64,
    pub max: f64,
    pub default: f64,
    pub kind: ParamKind,
    pub suffix: &'static str,
}

const fn num(name: &'static str, min: f64, max: f64, default: f64, suffix: &'static str) -> ParamDef {
    ParamDef { name, min, max, default, kind: ParamKind::Number, suffix }
}
const fn int(name: &'static str, min: f64, max: f64, default: f64) -> ParamDef {
    ParamDef { name, min, max, default, kind: ParamKind::Integer, suffix: "" }
}
const fn choice(name: &'static str, options: &'static [&'static str], default: f64) -> ParamDef {
    ParamDef { name, min: 0.0, max: (options.len() - 1) as f64, default, kind: ParamKind::Choice(options), suffix: "" }
}
const fn color(name: &'static str, default: f64) -> ParamDef {
    ParamDef { name, min: 0.0, max: 1.0, default, kind: ParamKind::Color, suffix: "" }
}

pub const QUALITY_OPTIONS: &[&str] = &["Fast", "Better"];

const MOTION_BLUR: &[ParamDef] = &[
    num("Shutter Angle", 0.0, 360.0, 180.0, "°"),
    int("Sample Count", 8.0, 32.0, 16.0),
    num("Motion Threshold", 0.0, 10.0, 0.5, " px"),
    choice("Quality", QUALITY_OPTIONS, 1.0),
    num("Blur Amount", 0.0, 1.5, 0.5, ""),
    num("Motion Sensitivity", 0.0, 100.0, 75.0, "%"),
];
const HALATION: &[ParamDef] =
    &[num("Threshold", 0.0, 1.0, 0.72, ""), num("Spread", 1.0, 120.0, 28.0, " px"), num("Strength", 0.0, 2.0, 0.65, ""), num("Warmth", 0.0, 1.0, 0.7, "")];
const CAMERA_SHAKE: &[ParamDef] = &[
    num("Motion Scale", 0.0, 1.0, 0.3, ""),
    num("Speed", 0.0, 10.0, 1.5, ""),
    num("Rotation", 0.0, 1.0, 0.25, ""),
    num("Directional Bias", -1.0, 1.0, 0.0, ""),
    choice("Mode", &["Handheld", "Impact"], 0.0),
    num("Seed", 0.0, 100.0, 7.0, ""),
    num("Decay Rate", 0.0, 30.0, 0.0, ""),
    num("Z-Zoom Punch", 1.0, 1.5, 1.0, "x"),
    num("Tilt Deg", 0.0, 15.0, 0.0, "°"),
    num("MoBlur Length", 0.0, 1.0, 0.0, ""),
    choice("Edge Wrap", &["Mirror", "Tile"], 0.0),
    num("RGB Split", 0.0, 30.0, 0.0, " px"),
];
const RGB_SPLIT_GLITCH: &[ParamDef] =
    &[num("Intensity", 0.0, 40.0, 6.0, " px"), num("Angle", 0.0, 360.0, 0.0, "°"), num("Edge Falloff", 0.0, 1.0, 0.5, ""), num("Glitch Burst", 0.0, 1.0, 0.0, "")];
const IMPACT_FLASH: &[ParamDef] = &[
    num("Flash Intensity", 0.0, 2.0, 0.35, ""),
    num("Flash Decay", 0.0, 40.0, 14.0, ""),
    color("Flash Red", 1.0),
    color("Flash Green", 1.0),
    color("Flash Blue", 1.0),
    num("Vignette Darken", 0.0, 1.0, 0.0, ""),
    num("Scope Zoom", 1.0, 2.0, 1.0, "x"),
];
const NEON_OUTLINE: &[ParamDef] = &[
    num("Intensity", 0.0, 2.0, 0.85, ""),
    num("Threshold", 0.0, 1.0, 0.25, ""),
    num("Thickness", 0.5, 4.0, 1.5, " px"),
    color("Edge Red", 0.25),
    color("Edge Green", 0.85),
    color("Edge Blue", 1.0),
    num("Base Dim", 0.0, 1.0, 0.15, ""),
];
/// Indices 0..=5 predate the layouts and keep their order for old projects.
pub const REFRAME_LAYOUTS: &[&str] = &["Split Stack", "Center Fit + Blur", "Full 9:16 Crop"];
const GAMING_REFRAME_916: &[ParamDef] = &[
    num("Killfeed Enabled", 0.0, 1.0, 1.0, ""),
    num("Killfeed Width", 0.20, 0.50, 0.34, ""),
    num("Killfeed Height", 0.10, 0.40, 0.22, ""),
    num("PiP Scale", 0.40, 0.95, 0.84, ""),
    num("PiP Y Offset", 0.0, 0.25, 0.04, ""),
    num("Corner Radius", 0.0, 0.08, 0.02, ""),
    choice("Layout", REFRAME_LAYOUTS, 0.0),
    num("Pan X", -1.0, 1.0, 0.0, ""),
    choice("Top Aspect", &["16:10", "4:3"], 0.0),
    num("Background Darken", 0.0, 0.95, 0.45, ""),
    num("Divider", 0.0, 16.0, 6.0, " px"),
    choice("Bottom Panel", &["HUD on Blur", "Cam Track (transparent)"], 0.0),
];
const FILM_GRAIN: &[ParamDef] =
    &[num("Grain Size", 0.5, 4.0, 1.2, ""), num("Strength", 0.0, 1.0, 0.25, ""), num("Saturation", 0.0, 1.0, 0.1, ""), choice("Stock", &["35mm", "16mm"], 0.0)];
/// Order is fixed for old projects: Threshold, Radius, Intensity (was
/// "Strength"), then the colour / blend parameters added later.
const GLOW: &[ParamDef] = &[
    num("Threshold", 0.0, 1.0, 0.65, ""),
    num("Radius", 1.0, 200.0, 36.0, " px"),
    num("Intensity", 0.0, 5.0, 0.6, ""),
    color("Glow Color R", 1.0),
    color("Glow Color G", 1.0),
    color("Glow Color B", 1.0),
    choice("Blend", &["Add", "Screen"], 0.0),
];
const DIRECTIONAL_BLUR: &[ParamDef] = &[num("Angle", 0.0, 360.0, 0.0, "°"), num("Length", 0.0, 300.0, 40.0, " px"), int("Sample Count", 8.0, 32.0, 16.0)];
const ZOOM_BLUR: &[ParamDef] = &[num("Center X", -0.5, 0.5, 0.0, ""), num("Center Y", -0.5, 0.5, 0.0, ""), num("Amount", 0.0, 1.0, 0.2, ""), int("Sample Count", 8.0, 32.0, 16.0)];
const CHROMA_KEY: &[ParamDef] = &[
    color("Key Red", 0.0),
    color("Key Green", 0.69),
    color("Key Blue", 0.25),
    num("Tolerance", 0.0, 1.0, 0.28, ""),
    num("Edge Feather", 0.0, 1.0, 0.12, ""),
    num("Spill Suppression", 0.0, 1.0, 0.6, ""),
];
const GAUSSIAN_BLUR: &[ParamDef] = &[num("Radius", 0.0, 200.0, 12.0, " px"), num("Horizontal", 0.0, 2.0, 1.0, "x"), num("Vertical", 0.0, 2.0, 1.0, "x")];
const RADIAL_BLUR: &[ParamDef] =
    &[num("Center X", -0.5, 0.5, 0.0, ""), num("Center Y", -0.5, 0.5, 0.0, ""), num("Angle", 0.0, 90.0, 8.0, "°"), int("Sample Count", 8.0, 64.0, 24.0)];
const VIGNETTE: &[ParamDef] = &[
    num("Amount", -1.0, 1.0, 0.5, ""),
    num("Size", 0.0, 1.5, 0.75, ""),
    num("Feather", 0.01, 1.0, 0.5, ""),
    num("Roundness", 0.0, 1.0, 0.0, ""),
    num("Center X", -0.5, 0.5, 0.0, ""),
    num("Center Y", -0.5, 0.5, 0.0, ""),
];
const SHARPEN: &[ParamDef] = &[num("Amount", 0.0, 5.0, 1.0, ""), num("Radius", 0.5, 10.0, 1.5, " px"), num("Threshold", 0.0, 0.2, 0.0, "")];
pub const PIXELATE_SHAPES: &[&str] = &["Square", "Dots"];
const PIXELATE: &[ParamDef] = &[num("Block Size", 1.0, 200.0, 24.0, " px"), choice("Shape", PIXELATE_SHAPES, 0.0)];
/// "Fit" scales the image so a barrel distortion never pulls the corners inside.
pub const LENS_EDGE_OPTIONS: &[&str] = &["Fit", "Transparent", "Mirror"];
const LENS_DISTORTION: &[ParamDef] = &[
    num("Distortion", -1.0, 1.0, 0.2, ""),
    num("Cubic", -1.0, 1.0, 0.0, ""),
    num("Zoom", 0.5, 2.0, 1.0, "x"),
    num("Chromatic Aberration", 0.0, 1.0, 0.0, ""),
    num("Center X", -0.5, 0.5, 0.0, ""),
    num("Center Y", -0.5, 0.5, 0.0, ""),
    choice("Edges", LENS_EDGE_OPTIONS, 0.0),
];
const HUE_SAT_LIGHTNESS: &[ParamDef] = &[num("Hue", -180.0, 180.0, 0.0, "°"), num("Saturation", -100.0, 100.0, 0.0, "%"), num("Lightness", -100.0, 100.0, 0.0, "%")];
pub const LEVELS_CHANNELS: &[&str] = &["RGB", "Red", "Green", "Blue"];
pub const PERSON_MATTE_OUTPUTS: &[&str] = &["Cut Out Subject", "Cut Out Background", "Show Matte"];
const PERSON_MATTE: &[ParamDef] = &[choice("Output", PERSON_MATTE_OUTPUTS, 0.0), num("Choke", -0.5, 0.5, 0.0, ""), num("Softness", 0.0, 0.5, 0.08, "")];
pub const DEPTH_OUTPUTS: &[&str] = &["Show Depth", "Depth Fog", "Keep Range"];
const DEPTH_MAP: &[ParamDef] = &[
    choice("Output", DEPTH_OUTPUTS, 0.0),
    num("Near", 0.0, 1.0, 1.0, ""),
    num("Far", 0.0, 1.0, 0.4, ""),
    num("Softness", 0.0, 0.5, 0.05, ""),
    num("Fog Amount", 0.0, 1.0, 0.7, ""),
    color("Fog Red", 0.75),
    color("Fog Green", 0.8),
    color("Fog Blue", 0.85),
];
pub const MIRROR_MODES: &[&str] = &["Left onto Right", "Right onto Left", "Top onto Bottom", "Bottom onto Top", "Quad"];
const MIRROR: &[ParamDef] = &[choice("Mode", MIRROR_MODES, 0.0), num("Center", 0.0, 1.0, 0.5, "")];
const KALEIDOSCOPE: &[ParamDef] = &[
    int("Segments", 2.0, 16.0, 6.0),
    num("Angle", 0.0, 360.0, 0.0, "°"),
    num("Center X", -0.5, 0.5, 0.0, ""),
    num("Center Y", -0.5, 0.5, 0.0, ""),
    num("Zoom", 0.25, 4.0, 1.0, "x"),
];
const POSTERIZE: &[ParamDef] = &[int("Levels", 2.0, 32.0, 6.0), num("Mix", 0.0, 1.0, 1.0, "")];
const INVERT: &[ParamDef] = &[num("Amount", 0.0, 1.0, 1.0, ""), choice("Channel", &["RGB", "Luma"], 0.0)];
pub const TINT_MODES: &[&str] = &["Black & White", "Sepia", "Duotone"];
const TINT: &[ParamDef] = &[
    choice("Mode", TINT_MODES, 0.0),
    num("Amount", 0.0, 1.0, 1.0, ""),
    color("Shadow Red", 0.08),
    color("Shadow Green", 0.05),
    color("Shadow Blue", 0.3),
    color("Highlight Red", 1.0),
    color("Highlight Green", 0.85),
    color("Highlight Blue", 0.5),
];
pub const EDGE_MODES: &[&str] = &["Edges", "Emboss", "Sketch"];
const FIND_EDGES: &[ParamDef] = &[choice("Mode", EDGE_MODES, 0.0), num("Strength", 0.0, 8.0, 2.0, ""), num("Mix", 0.0, 1.0, 1.0, "")];
pub const LETTERBOX_ASPECTS: &[&str] = &["2.39:1", "2.00:1", "1.85:1", "4:3", "1:1", "9:16"];
const LETTERBOX: &[ParamDef] =
    &[choice("Aspect", LETTERBOX_ASPECTS, 0.0), num("Opacity", 0.0, 1.0, 1.0, ""), color("Bar Red", 0.0), color("Bar Green", 0.0), color("Bar Blue", 0.0)];
const LEVELS: &[ParamDef] = &[
    num("Input Black", 0.0, 1.0, 0.0, ""),
    num("Input White", 0.0, 1.0, 1.0, ""),
    num("Gamma", 0.1, 10.0, 1.0, ""),
    num("Output Black", 0.0, 1.0, 0.0, ""),
    num("Output White", 0.0, 1.0, 1.0, ""),
    choice("Channel", LEVELS_CHANNELS, 0.0),
];

impl EffectKind {
    pub const ALL: [EffectKind; 29] = [
        EffectKind::MotionBlur,
        EffectKind::Halation,
        EffectKind::CameraShake,
        EffectKind::FilmGrain,
        EffectKind::Glow,
        EffectKind::DirectionalBlur,
        EffectKind::ZoomBlur,
        EffectKind::ChromaKey,
        EffectKind::RgbSplitGlitch,
        EffectKind::ImpactFlash,
        EffectKind::NeonOutline,
        EffectKind::GamingReframe916,
        EffectKind::GaussianBlur,
        EffectKind::RadialBlur,
        EffectKind::Vignette,
        EffectKind::Sharpen,
        EffectKind::Pixelate,
        EffectKind::LensDistortion,
        EffectKind::HueSatLightness,
        EffectKind::Levels,
        EffectKind::PersonMatte,
        EffectKind::DepthMap,
        EffectKind::Mirror,
        EffectKind::Kaleidoscope,
        EffectKind::Posterize,
        EffectKind::Invert,
        EffectKind::Tint,
        EffectKind::FindEdges,
        EffectKind::Letterbox,
    ];

    pub fn label(self) -> &'static str {
        match self {
            EffectKind::MotionBlur => "Vector Motion Blur",
            EffectKind::Halation => "Halation",
            EffectKind::CameraShake => "Camera Shake",
            EffectKind::FilmGrain => "Film Grain",
            EffectKind::Glow => "Glow",
            EffectKind::DirectionalBlur => "Directional Blur",
            EffectKind::ZoomBlur => "Zoom Blur",
            EffectKind::ChromaKey => "Chroma Key",
            EffectKind::RgbSplitGlitch => "RGB Split / Glitch",
            EffectKind::ImpactFlash => "Impact Flash / Scope",
            EffectKind::NeonOutline => "Neon / Saber Edge",
            EffectKind::GamingReframe916 => "Gaming 9:16 Reframe",
            EffectKind::GaussianBlur => "Gaussian Blur",
            EffectKind::RadialBlur => "Radial Blur",
            EffectKind::Vignette => "Vignette",
            EffectKind::Sharpen => "Sharpen",
            EffectKind::Pixelate => "Pixelate",
            EffectKind::LensDistortion => "Lens Distortion",
            EffectKind::HueSatLightness => "Hue / Saturation / Lightness",
            EffectKind::Levels => "Levels",
            EffectKind::PersonMatte => "Person Matte (AI)",
            EffectKind::DepthMap => "Depth Map (AI)",
            EffectKind::Mirror => "Mirror",
            EffectKind::Kaleidoscope => "Kaleidoscope",
            EffectKind::Posterize => "Posterize",
            EffectKind::Invert => "Invert",
            EffectKind::Tint => "Black & White / Tint",
            EffectKind::FindEdges => "Find Edges / Emboss",
            EffectKind::Letterbox => "Letterbox",
        }
    }

    pub fn category(self) -> &'static str {
        match self {
            EffectKind::MotionBlur | EffectKind::DirectionalBlur | EffectKind::ZoomBlur | EffectKind::GaussianBlur | EffectKind::RadialBlur => "Blur",
            EffectKind::Sharpen => "Sharpen",
            EffectKind::Vignette => "Light",
            EffectKind::Pixelate => "Stylize",
            EffectKind::LensDistortion => "Distort",
            EffectKind::HueSatLightness | EffectKind::Levels => "Color",
            EffectKind::PersonMatte | EffectKind::DepthMap => "AI Matte",
            EffectKind::Halation | EffectKind::FilmGrain => "Film Look",
            EffectKind::Glow => "Light",
            EffectKind::CameraShake => "Motion",
            EffectKind::ChromaKey => "Keying",
            EffectKind::RgbSplitGlitch | EffectKind::ImpactFlash => "Glitch & Impact",
            EffectKind::NeonOutline => "Stylize",
            EffectKind::GamingReframe916 => "Reframe",
            EffectKind::Mirror => "Distort",
            EffectKind::Kaleidoscope => "Distort",
            EffectKind::Posterize => "Stylize",
            EffectKind::Invert => "Color",
            EffectKind::Tint => "Color",
            EffectKind::FindEdges => "Stylize",
            EffectKind::Letterbox => "Stylize",
        }
    }

    pub fn params(self) -> &'static [ParamDef] {
        match self {
            EffectKind::MotionBlur => MOTION_BLUR,
            EffectKind::Halation => HALATION,
            EffectKind::CameraShake => CAMERA_SHAKE,
            EffectKind::FilmGrain => FILM_GRAIN,
            EffectKind::Glow => GLOW,
            EffectKind::DirectionalBlur => DIRECTIONAL_BLUR,
            EffectKind::ZoomBlur => ZOOM_BLUR,
            EffectKind::ChromaKey => CHROMA_KEY,
            EffectKind::RgbSplitGlitch => RGB_SPLIT_GLITCH,
            EffectKind::ImpactFlash => IMPACT_FLASH,
            EffectKind::NeonOutline => NEON_OUTLINE,
            EffectKind::GamingReframe916 => GAMING_REFRAME_916,
            EffectKind::GaussianBlur => GAUSSIAN_BLUR,
            EffectKind::RadialBlur => RADIAL_BLUR,
            EffectKind::Vignette => VIGNETTE,
            EffectKind::Sharpen => SHARPEN,
            EffectKind::Pixelate => PIXELATE,
            EffectKind::LensDistortion => LENS_DISTORTION,
            EffectKind::HueSatLightness => HUE_SAT_LIGHTNESS,
            EffectKind::Levels => LEVELS,
            EffectKind::PersonMatte => PERSON_MATTE,
            EffectKind::DepthMap => DEPTH_MAP,
            EffectKind::Mirror => MIRROR,
            EffectKind::Kaleidoscope => KALEIDOSCOPE,
            EffectKind::Posterize => POSTERIZE,
            EffectKind::Invert => INVERT,
            EffectKind::Tint => TINT,
            EffectKind::FindEdges => FIND_EDGES,
            EffectKind::Letterbox => LETTERBOX,
        }
    }

    /// Needs the next source frame (motion vectors).
    pub fn needs_motion(self) -> bool {
        self == EffectKind::MotionBlur
    }

    /// The AI matte this effect reads, if any.
    pub fn matte(self) -> Option<MatteKind> {
        match self {
            EffectKind::PersonMatte => Some(MatteKind::Person),
            EffectKind::DepthMap => Some(MatteKind::Depth),
            _ => None,
        }
    }
}

static NEXT_EFFECT_ID: AtomicU64 = AtomicU64::new(1);

fn new_effect_id() -> u64 {
    NEXT_EFFECT_ID.fetch_add(1, Ordering::Relaxed)
}

/// One instance of an effect in a clip's stack.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Effect {
    /// Stable id for UI state (reordering, collapsing). Not semantically relevant.
    #[serde(default = "new_effect_id")]
    pub id: u64,
    pub kind: EffectKind,
    #[serde(default = "crate::clip::default_true")]
    pub enabled: bool,
    /// One per [`EffectKind::params`] entry, in the same order.
    pub params: Vec<Param>,
    /// Added by a montage preset: re-applying any preset replaces these
    /// instead of stacking duplicates on top.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub from_preset: bool,
    /// Limits the effect to a shape (outside it the clip stays unaffected).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mask: Option<crate::color::ShapeMask>,
    /// Drive the effect's position from a clip track (Smart Reframe pan).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub track: Option<u64>,
    /// Clip-local instant a one-shot effect fires (a preset dropped at the
    /// playhead): shake decay and impact flash count their time from here.
    #[serde(default, skip_serializing_if = "is_zero_time")]
    pub trigger: Time,
}

fn is_zero_time(t: &Time) -> bool {
    t.is_zero()
}

impl Effect {
    pub fn new(kind: EffectKind) -> Self {
        Effect {
            id: new_effect_id(),
            kind,
            enabled: true,
            params: kind.params().iter().map(|d| Param::new(d.default)).collect(),
            from_preset: false,
            mask: None,
            track: None,
            trigger: Time::ZERO,
        }
    }

    /// A copy with its own id (pasting, duplicating).
    pub fn duplicate(&self) -> Effect {
        Effect { id: new_effect_id(), ..self.clone() }
    }

    /// Repair the parameter list after loading an older project (missing
    /// trailing parameters get their defaults, extra ones are dropped).
    pub fn normalize(&mut self) {
        let defs = self.kind.params();
        self.params.truncate(defs.len());
        for d in &defs[self.params.len()..] {
            self.params.push(Param::new(d.default));
        }
    }

    pub fn value(&self, name: &str, t: Time) -> Option<f32> {
        let i = self.kind.params().iter().position(|d| d.name == name)?;
        self.params.get(i).map(|p| p.at_f32(t))
    }

    /// Evaluate every parameter at clip-local `t`, clamped to its range.
    pub fn resolve(&self, t: Time) -> ResolvedEffect {
        let mut values = [0.0f32; MAX_EFFECT_PARAMS];
        for (i, d) in self.kind.params().iter().enumerate().take(MAX_EFFECT_PARAMS) {
            let v = self.params.get(i).map(|p| p.at(t)).unwrap_or(d.default).clamp(d.min, d.max);
            values[i] = match d.kind {
                ParamKind::Integer | ParamKind::Choice(_) => v.round() as f32,
                _ => v as f32,
            };
        }
        let elapsed = (t - self.trigger).as_secs_f64() as f32;
        ResolvedEffect { kind: self.kind, values, mask: self.mask.as_ref().map(|m| m.at(t).resolve()), elapsed }
    }
}

/// An effect with its parameters evaluated at one instant.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ResolvedEffect {
    pub kind: EffectKind,
    pub values: [f32; MAX_EFFECT_PARAMS],
    pub mask: Option<crate::color::ResolvedMask>,
    /// Seconds since the effect's trigger (negative before it fires).
    pub elapsed: f32,
}

impl ResolvedEffect {
    pub fn get(&self, name: &str) -> f32 {
        self.kind.params().iter().position(|d| d.name == name).map(|i| self.values[i]).unwrap_or(0.0)
    }

    /// True when the effect would leave the image unchanged.
    pub fn is_noop(&self) -> bool {
        let v = &self.values;
        match self.kind {
            EffectKind::MotionBlur => v[0] <= 0.0 || v[4] <= 0.0,
            EffectKind::Halation => v[2] <= 0.0,
            EffectKind::Glow => v[2] <= 0.0 || v[3].max(v[4]).max(v[5]) <= 0.0,
            EffectKind::CameraShake => v[0] <= 0.0 && v[2] <= 0.0 && v[7] <= 1.0 && v[8] <= 0.0 && v[11] <= 0.0,
            EffectKind::FilmGrain => v[1] <= 0.0,
            EffectKind::DirectionalBlur => v[1] <= 0.0,
            EffectKind::ZoomBlur => v[2] <= 0.0,
            EffectKind::ChromaKey => false,
            EffectKind::RgbSplitGlitch => v[0] <= 0.0 && v[3] <= 0.0,
            EffectKind::ImpactFlash => v[0] <= 0.0 && v[5] <= 0.0 && v[6] <= 1.0,
            EffectKind::NeonOutline => v[0] <= 0.0,
            EffectKind::GamingReframe916 => false,
            EffectKind::GaussianBlur => v[0] <= 0.0 || (v[1] <= 0.0 && v[2] <= 0.0),
            EffectKind::RadialBlur => v[2] <= 0.0,
            EffectKind::Vignette => v[0] == 0.0,
            EffectKind::Sharpen => v[0] <= 0.0,
            EffectKind::Pixelate => v[0] <= 1.0 && v[1] < 0.5,
            EffectKind::LensDistortion => v[0] == 0.0 && v[1] == 0.0 && v[2] == 1.0 && v[3] <= 0.0,
            EffectKind::HueSatLightness => v[0] == 0.0 && v[1] == 0.0 && v[2] == 0.0,
            EffectKind::Levels => v[0] <= 0.0 && v[1] >= 1.0 && v[2] == 1.0 && v[3] <= 0.0 && v[4] >= 1.0,
            EffectKind::PersonMatte | EffectKind::DepthMap => false,
            EffectKind::Mirror | EffectKind::Kaleidoscope => false,
            EffectKind::Posterize => v[1] <= 0.0,
            EffectKind::Invert => v[0] <= 0.0,
            EffectKind::Tint => v[1] <= 0.0,
            EffectKind::FindEdges => v[2] <= 0.0,
            EffectKind::Letterbox => v[1] <= 0.0,
        }
    }
}

/// Resolve the enabled, non-trivial effects of a stack.
pub fn resolve_stack(effects: &[Effect], t: Time) -> Vec<ResolvedEffect> {
    effects.iter().filter(|e| e.enabled).map(|e| e.resolve(t)).filter(|r| !r.is_noop()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_effect_fits_the_uniform_and_defaults_are_in_range() {
        for k in EffectKind::ALL {
            let defs = k.params();
            assert!(!defs.is_empty() && defs.len() <= MAX_EFFECT_PARAMS, "{k:?}");
            for d in defs {
                assert!(d.min <= d.default && d.default <= d.max, "{k:?} {}", d.name);
            }
            let e = Effect::new(k);
            assert_eq!(e.params.len(), defs.len());
            let r = e.resolve(Time::ZERO);
            assert_eq!(r.kind, k);
        }
    }

    #[test]
    fn resolve_clamps_rounds_and_animates() {
        let mut e = Effect::new(EffectKind::MotionBlur);
        e.params[0].set(Time::ZERO, 0.0);
        e.params[0].add_keyframe(Time::ZERO);
        e.params[0].curve.as_mut().unwrap().set_value_at(Time::from_secs(1), 360.0);
        e.params[1].value = 99.0; // above max
        let r = e.resolve(Time::from_millis(500));
        assert!((r.get("Shutter Angle") - 180.0).abs() < 1.0, "{}", r.get("Shutter Angle"));
        assert_eq!(r.get("Sample Count"), 32.0);
        // zero shutter is a no-op and gets dropped from the stack
        assert!(resolve_stack(&[e.clone()], Time::ZERO).is_empty());
        e.enabled = false;
        assert!(resolve_stack(&[e], Time::from_secs(1)).is_empty());
    }

    #[test]
    fn normalize_repairs_old_param_lists() {
        let mut e = Effect::new(EffectKind::CameraShake);
        e.params.truncate(2);
        e.normalize();
        assert_eq!(e.params.len(), EffectKind::CameraShake.params().len());
        assert_eq!(e.params[5].value, 7.0);
    }

    #[test]
    fn new_effects_defaults_are_active_and_neutral_settings_are_noops() {
        use EffectKind::*;
        // defaults must do something visible, otherwise "+ Add Effect" looks broken
        for k in [GaussianBlur, RadialBlur, Vignette, Sharpen, Pixelate, LensDistortion] {
            assert!(!Effect::new(k).resolve(Time::ZERO).is_noop(), "{k:?}");
        }
        // colour tools default to neutral (a no-op) until a slider moves
        for k in [HueSatLightness, Levels] {
            assert!(Effect::new(k).resolve(Time::ZERO).is_noop(), "{k:?}");
        }
        let mut e = Effect::new(GaussianBlur);
        e.params[0].value = 0.0;
        assert!(e.resolve(Time::ZERO).is_noop());
        let mut e = Effect::new(Levels);
        e.params[2].value = 1.8;
        assert!(!e.resolve(Time::ZERO).is_noop());
        let mut e = Effect::new(Vignette);
        e.params[0].value = -0.4; // white vignette is still an effect
        assert!(!e.resolve(Time::ZERO).is_noop());
        assert_eq!(EffectKind::ALL.len(), 29);
        for k in [Mirror, Kaleidoscope, Posterize, Invert, Tint, FindEdges, Letterbox] {
            assert!(!Effect::new(k).resolve(Time::ZERO).is_noop(), "{k:?} defaults do something");
        }
        for k in [PersonMatte, DepthMap] {
            assert!(!Effect::new(k).resolve(Time::ZERO).is_noop() && k.matte().is_some(), "{k:?}");
        }
        let mut seen = std::collections::HashSet::new();
        assert!(EffectKind::ALL.iter().all(|k| seen.insert(k.label())), "labels are unique");
    }

    #[test]
    fn serde_round_trip() {
        let e = Effect::new(EffectKind::ChromaKey);
        let s = serde_json::to_string(&e).unwrap();
        let back: Effect = serde_json::from_str(&s).unwrap();
        assert_eq!(e, back);
    }
}
