//! Colour management and stacked colour corrections.
//!
//! The data here is part of each clip's [`ColorParams`](crate::clip::ColorParams):
//!
//! * [`ColorSpace`]: how the clip's decoded code values are encoded (Rec.709,
//!   sRGB, Rec.2100 PQ / HLG, camera log). The renderer converts them to
//!   Rec.709 display values (CST, with highlight roll-off for HDR and log).
//! * [`WorkingSpace`]: the encoding the wheels, contrast and curves operate in.
//!   That is either the legacy Rec.709 gamma 2.4 or an ACEScct log curve over
//!   ACES AP1 primaries.
//! * [`ColorCorrection`]: an additional correction "node" after the primary.
//!   Each node has primary wheels, log wheels, RGB / luma and hue curves, an
//!   HSL [`Qualifier`], a [`ShapeMask`] and a mix amount. Nodes apply in
//!   order, and each one keys on the output of the node before it.
//!
//! Node values are not keyframable; the primary's sliders still are.

use crate::clip::ColorWheel;
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicU64, Ordering};

// ---------------------------------------------------------------------------
// Colour spaces
// ---------------------------------------------------------------------------

/// Encoding of a clip's decoded RGB values (the input colour space).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ColorSpace {
    /// Rec.709 / BT.1886 (gamma 2.4). The default; no conversion.
    #[default]
    Rec709,
    /// sRGB piecewise curve, Rec.709 primaries (screen recordings, stills).
    Srgb,
    /// Linear light, Rec.709 primaries (EXR renders).
    Linear709,
    /// Rec.2100 PQ (SMPTE ST 2084), Rec.2020 primaries.
    Rec2100Pq,
    /// Rec.2100 HLG, Rec.2020 primaries.
    Rec2100Hlg,
    /// ARRI LogC3 (EI 800), ARRI Wide Gamut 3.
    ArriLogC3,
    /// Sony S-Log3, S-Gamut3.Cine.
    SonySLog3,
    /// Panasonic V-Log, V-Gamut.
    PanasonicVLog,
}

impl ColorSpace {
    pub const ALL: [ColorSpace; 8] = [
        ColorSpace::Rec709,
        ColorSpace::Srgb,
        ColorSpace::Linear709,
        ColorSpace::Rec2100Pq,
        ColorSpace::Rec2100Hlg,
        ColorSpace::ArriLogC3,
        ColorSpace::SonySLog3,
        ColorSpace::PanasonicVLog,
    ];

    pub fn label(self) -> &'static str {
        match self {
            ColorSpace::Rec709 => "Rec.709 (gamma 2.4)",
            ColorSpace::Srgb => "sRGB",
            ColorSpace::Linear709 => "Linear (Rec.709 primaries)",
            ColorSpace::Rec2100Pq => "Rec.2100 PQ (HDR10)",
            ColorSpace::Rec2100Hlg => "Rec.2100 HLG",
            ColorSpace::ArriLogC3 => "ARRI LogC3 / AWG3",
            ColorSpace::SonySLog3 => "Sony S-Log3 / S-Gamut3.Cine",
            ColorSpace::PanasonicVLog => "Panasonic V-Log / V-Gamut",
        }
    }

    /// Index used by the shaders (`colorspace.wgsl`).
    pub fn shader_index(self) -> u32 {
        self as u32
    }

    /// High dynamic range or scene-referred: converted with highlight roll-off.
    pub fn is_scene_or_hdr(self) -> bool {
        !matches!(self, ColorSpace::Rec709 | ColorSpace::Srgb)
    }

    /// Best guess from FFmpeg's `color_transfer` tag.
    pub fn from_ffmpeg_transfer(trc: &str) -> Option<ColorSpace> {
        Some(match trc {
            "smpte2084" => ColorSpace::Rec2100Pq,
            "arib-std-b67" => ColorSpace::Rec2100Hlg,
            "iec61966-2-1" => ColorSpace::Srgb,
            "linear" => ColorSpace::Linear709,
            "bt709" | "bt1886" | "smpte170m" | "bt470bg" | "gamma22" => ColorSpace::Rec709,
            _ => return None,
        })
    }
}

/// Encoding the grade operates in.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum WorkingSpace {
    /// Rec.709 gamma 2.4 display values (how RustCut graded before colour
    /// management, so old projects look the same).
    #[default]
    Rec709Gamma,
    /// ACEScct log curve over ACES AP1 primaries: wheels and contrast feel
    /// like a film-style log grade and keep highlight detail.
    AcesCct,
}

impl WorkingSpace {
    pub const ALL: [WorkingSpace; 2] = [WorkingSpace::Rec709Gamma, WorkingSpace::AcesCct];

    pub fn label(self) -> &'static str {
        match self {
            WorkingSpace::Rec709Gamma => "Rec.709 gamma 2.4",
            WorkingSpace::AcesCct => "ACEScct (log, AP1)",
        }
    }

    pub fn shader_index(self) -> u32 {
        self as u32
    }

    /// Contrast pivot (18 % grey in this encoding).
    pub fn pivot(self) -> f32 {
        match self {
            WorkingSpace::Rec709Gamma => 0.5,
            WorkingSpace::AcesCct => acescct_encode(0.18),
        }
    }
}

/// ACEScct encoding of a linear value (CPU mirror of the shader).
pub fn acescct_encode(x: f32) -> f32 {
    if x <= 0.0078125 {
        10.540_237 * x + 0.072_905_53
    } else {
        (x.log2() + 9.72) / 17.52
    }
}

pub fn acescct_decode(y: f32) -> f32 {
    if y <= 0.155_251_14 {
        (y - 0.072_905_53) / 10.540_237
    } else {
        (y * 17.52 - 9.72).exp2()
    }
}

// ---------------------------------------------------------------------------
// Curves
// ---------------------------------------------------------------------------

/// Samples per baked curve table (the GPU looks curves up in these).
pub const CURVE_SAMPLES: usize = 256;
/// Baked tables per correction: master, R, G, B, hue-vs-hue, hue-vs-sat, hue-vs-lum.
pub const CURVE_TABLES: usize = 7;

/// A tone curve through control points `(x, y)` in 0..1. No points =
/// identity; otherwise the curve runs flat beyond the first and last point
/// (add (0, 0) and (1, 1) to pin the ends). Interpolation is monotone cubic
/// (Fritsch–Carlson), so the curve never overshoots between points.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Curve {
    pub points: Vec<[f32; 2]>,
}

impl Curve {
    pub fn is_identity(&self) -> bool {
        self.points.is_empty() || (self.points.len() == 2 && self.points[0] == [0.0, 0.0] && self.points[1] == [1.0, 1.0])
    }

    /// Points sorted by x with duplicates removed.
    fn sorted(&self) -> Vec<[f32; 2]> {
        let mut p: Vec<[f32; 2]> = self.points.iter().map(|q| [q[0].clamp(0.0, 1.0), q[1]]).collect();
        p.sort_by(|a, b| a[0].total_cmp(&b[0]));
        p.dedup_by(|a, b| (a[0] - b[0]).abs() < 1e-5);
        p
    }

    pub fn eval(&self, x: f32) -> f32 {
        if self.points.is_empty() {
            return x;
        }
        monotone_cubic(&self.sorted(), x)
    }

    /// Insert a point (or move the one within `snap` of `x`).
    pub fn set_point(&mut self, x: f32, y: f32, snap: f32) -> usize {
        if self.points.is_empty() {
            self.points = vec![[0.0, 0.0], [1.0, 1.0]];
        }
        if let Some(i) = self.points.iter().position(|p| (p[0] - x).abs() <= snap) {
            self.points[i] = [x, y];
            return i;
        }
        self.points.push([x, y]);
        self.points.len() - 1
    }
}

fn monotone_cubic(p: &[[f32; 2]], x: f32) -> f32 {
    let n = p.len();
    if n == 1 || x <= p[0][0] {
        return p[0][1];
    }
    if x >= p[n - 1][0] {
        return p[n - 1][1];
    }
    // secant slopes and Fritsch–Carlson tangents
    let d: Vec<f32> = (0..n - 1).map(|i| (p[i + 1][1] - p[i][1]) / (p[i + 1][0] - p[i][0]).max(1e-6)).collect();
    let mut m = vec![0.0f32; n];
    m[0] = d[0];
    m[n - 1] = d[n - 2];
    for i in 1..n - 1 {
        m[i] = if d[i - 1] * d[i] <= 0.0 { 0.0 } else { (d[i - 1] + d[i]) * 0.5 };
    }
    for i in 0..n - 1 {
        if d[i] == 0.0 {
            m[i] = 0.0;
            m[i + 1] = 0.0;
        } else {
            let (a, b) = (m[i] / d[i], m[i + 1] / d[i]);
            let s = a * a + b * b;
            if s > 9.0 {
                let t = 3.0 / s.sqrt();
                m[i] = t * a * d[i];
                m[i + 1] = t * b * d[i];
            }
        }
    }
    let i = p.windows(2).position(|w| x < w[1][0]).unwrap_or(n - 2);
    let h = p[i + 1][0] - p[i][0];
    let t = (x - p[i][0]) / h;
    let (t2, t3) = (t * t, t * t * t);
    (2.0 * t3 - 3.0 * t2 + 1.0) * p[i][1] + (t3 - 2.0 * t2 + t) * h * m[i] + (-2.0 * t3 + 3.0 * t2) * p[i + 1][1] + (t3 - t2) * h * m[i + 1]
}

/// A periodic curve over hue (0..1 = red → yellow → green → … → red), as a
/// smooth Catmull-Rom loop through `(hue, value)` points. `value` is an
/// offset: 0 everywhere is neutral, so an empty curve does nothing.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct HueCurve {
    pub points: Vec<[f32; 2]>,
}

impl HueCurve {
    pub fn is_identity(&self) -> bool {
        self.points.iter().all(|p| p[1] == 0.0)
    }

    pub fn eval(&self, hue: f32) -> f32 {
        let mut p: Vec<[f32; 2]> = self.points.iter().map(|q| [q[0].rem_euclid(1.0), q[1]]).collect();
        match p.len() {
            0 => return 0.0,
            1 => return p[0][1],
            _ => {}
        }
        p.sort_by(|a, b| a[0].total_cmp(&b[0]));
        let n = p.len();
        let h = hue.rem_euclid(1.0);
        // segment [i, i+1] (wrapping) that contains h
        let i = p.iter().rposition(|q| q[0] <= h).unwrap_or(n - 1);
        let at = |k: isize| -> [f32; 2] {
            let kk = k.rem_euclid(n as isize) as usize;
            let wraps = k.div_euclid(n as isize) as f32;
            [p[kk][0] + wraps, p[kk][1]]
        };
        let i = i as isize;
        let (p0, p1, p2, p3) = (at(i - 1), at(i), at(i + 1), at(i + 2));
        let x = if h < p1[0] { h + 1.0 } else { h };
        let span = (p2[0] - p1[0]).max(1e-6);
        let t = ((x - p1[0]) / span).clamp(0.0, 1.0);
        // non-uniform spacing: scale the tangents by the neighbouring spans
        let m1 = (p2[1] - p0[1]) / (p2[0] - p0[0]).max(1e-6) * span;
        let m2 = (p3[1] - p1[1]) / (p3[0] - p1[0]).max(1e-6) * span;
        let (t2, t3) = (t * t, t * t * t);
        (2.0 * t3 - 3.0 * t2 + 1.0) * p1[1] + (t3 - 2.0 * t2 + t) * m1 + (-2.0 * t3 + 3.0 * t2) * p2[1] + (t3 - t2) * m2
    }
}

// ---------------------------------------------------------------------------
// Qualifier and masks
// ---------------------------------------------------------------------------

/// HSL qualifier: keys a hue / saturation / luma range (with soft edges) of
/// the node's input so the node only affects matching pixels.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Qualifier {
    pub use_hue: bool,
    /// Hue centre and full width, 0..1 of the hue circle.
    pub hue_center: f32,
    pub hue_width: f32,
    pub hue_soft: f32,
    pub use_sat: bool,
    pub sat_low: f32,
    pub sat_high: f32,
    pub sat_soft: f32,
    pub use_lum: bool,
    pub lum_low: f32,
    pub lum_high: f32,
    pub lum_soft: f32,
    /// Matte clean-up: averages the key over a neighbourhood (0 = off).
    pub denoise: f32,
    pub invert: bool,
}

impl Default for Qualifier {
    fn default() -> Self {
        // a skin-tone-ish starting point
        Qualifier {
            use_hue: true,
            hue_center: 0.07,
            hue_width: 0.1,
            hue_soft: 0.05,
            use_sat: true,
            sat_low: 0.1,
            sat_high: 1.0,
            sat_soft: 0.05,
            use_lum: true,
            lum_low: 0.05,
            lum_high: 0.95,
            lum_soft: 0.05,
            denoise: 0.0,
            invert: false,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum MaskShape {
    #[default]
    Ellipse,
    Rectangle,
    /// Closed polygon through `points`; `smooth` turns it into a Bézier
    /// spline (Catmull-Rom tangents) through the same points.
    Polygon,
}

impl MaskShape {
    pub const ALL: [MaskShape; 3] = [MaskShape::Ellipse, MaskShape::Rectangle, MaskShape::Polygon];

    pub fn label(self) -> &'static str {
        match self {
            MaskShape::Ellipse => "Ellipse",
            MaskShape::Rectangle => "Rectangle",
            MaskShape::Polygon => "Polygon",
        }
    }
}

/// Maximum vertices of a (flattened) polygon mask sent to the GPU.
pub const MAX_MASK_POINTS: usize = 64;

/// A feathered shape mask in frame coordinates (0..1 across and down).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ShapeMask {
    pub shape: MaskShape,
    /// Centre of an ellipse / rectangle (0..1 frame coordinates).
    pub center: [f32; 2],
    /// Half width and half height, as fractions of the frame height (so a
    /// circle stays round on any aspect).
    pub size: [f32; 2],
    /// Degrees, clockwise.
    pub rotation: f32,
    /// Soft edge width as a fraction of the frame height (centred on the edge).
    pub feather: f32,
    pub invert: bool,
    /// Polygon vertices (0..1 frame coordinates).
    #[serde(default)]
    pub points: Vec<[f32; 2]>,
    #[serde(default)]
    pub smooth: bool,
}

impl Default for ShapeMask {
    fn default() -> Self {
        ShapeMask { shape: MaskShape::Ellipse, center: [0.5, 0.5], size: [0.3, 0.3], rotation: 0.0, feather: 0.05, invert: false, points: Vec::new(), smooth: false }
    }
}

impl ShapeMask {
    pub fn new(shape: MaskShape) -> Self {
        let mut m = ShapeMask { shape, ..Default::default() };
        if shape == MaskShape::Polygon {
            m.points = vec![[0.35, 0.3], [0.65, 0.3], [0.7, 0.7], [0.3, 0.7]];
        }
        m
    }

    /// The polygon outline the GPU tests against: the vertices, or the
    /// flattened spline when `smooth` (at most [`MAX_MASK_POINTS`]).
    pub fn outline(&self) -> Vec<[f32; 2]> {
        let p = &self.points;
        if !self.smooth || p.len() < 3 {
            return p.iter().take(MAX_MASK_POINTS).copied().collect();
        }
        let n = p.len().min(MAX_MASK_POINTS);
        let per = (MAX_MASK_POINTS / n).max(1);
        let mut out = Vec::with_capacity(n * per);
        for i in 0..n {
            let (p0, p1, p2, p3) = (p[(i + n - 1) % n], p[i], p[(i + 1) % n], p[(i + 2) % n]);
            for s in 0..per {
                let t = s as f32 / per as f32;
                let (t2, t3) = (t * t, t * t * t);
                let f = |a: f32, b: f32, c: f32, d: f32| 0.5 * (2.0 * b + (-a + c) * t + (2.0 * a - 5.0 * b + 4.0 * c - d) * t2 + (-a + 3.0 * b - 3.0 * c + d) * t3);
                out.push([f(p0[0], p1[0], p2[0], p3[0]), f(p0[1], p1[1], p2[1], p3[1])]);
            }
        }
        out
    }

    /// Mask coverage 0..1 at frame coordinate `uv` for a `aspect` (w / h)
    /// frame. CPU mirror of `mask_alpha` in `mask.wgsl`.
    pub fn alpha(&self, uv: [f32; 2], aspect: f32) -> f32 {
        // work in height units centred on the frame
        let to = |q: [f32; 2]| [(q[0] - 0.5) * aspect, q[1] - 0.5];
        let p = to(uv);
        let d = match self.shape {
            MaskShape::Ellipse | MaskShape::Rectangle => {
                let c = to(self.center);
                let (s, co) = (-self.rotation.to_radians()).sin_cos();
                let q = [p[0] - c[0], p[1] - c[1]];
                let l = [q[0] * co - q[1] * s, q[0] * s + q[1] * co];
                let hs = [self.size[0].max(1e-4), self.size[1].max(1e-4)];
                if self.shape == MaskShape::Ellipse {
                    let k = ((l[0] / hs[0]).powi(2) + (l[1] / hs[1]).powi(2)).sqrt();
                    (k - 1.0) * hs[0].min(hs[1])
                } else {
                    let dx = l[0].abs() - hs[0];
                    let dy = l[1].abs() - hs[1];
                    (dx.max(0.0).powi(2) + dy.max(0.0).powi(2)).sqrt() + dx.max(dy).min(0.0)
                }
            }
            MaskShape::Polygon => {
                let o: Vec<[f32; 2]> = self.outline().into_iter().map(to).collect();
                polygon_sdf(&o, p)
            }
        };
        let a = feather_alpha(d, self.feather);
        if self.invert {
            1.0 - a
        } else {
            a
        }
    }
}

/// A [`ShapeMask`] flattened for the GPU (fixed size, `Copy`).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ResolvedMask {
    pub shape: MaskShape,
    pub center: [f32; 2],
    pub size: [f32; 2],
    pub rotation: f32,
    pub feather: f32,
    pub invert: bool,
    /// Used entries of `points` (polygon outline).
    pub count: u32,
    pub points: [[f32; 2]; MAX_MASK_POINTS],
}

impl ShapeMask {
    pub fn resolve(&self) -> ResolvedMask {
        let mut points = [[0.0; 2]; MAX_MASK_POINTS];
        let mut count = 0;
        if self.shape == MaskShape::Polygon {
            for (d, p) in points.iter_mut().zip(self.outline()) {
                *d = p;
                count += 1;
            }
        }
        ResolvedMask { shape: self.shape, center: self.center, size: self.size, rotation: self.rotation, feather: self.feather, invert: self.invert, count, points }
    }
}

/// Signed distance to a closed polygon (negative inside), even-odd rule.
fn polygon_sdf(v: &[[f32; 2]], p: [f32; 2]) -> f32 {
    if v.len() < 3 {
        return 1.0;
    }
    let mut d = f32::MAX;
    let mut inside = false;
    let mut j = v.len() - 1;
    for i in 0..v.len() {
        let (a, b) = (v[i], v[j]);
        let e = [b[0] - a[0], b[1] - a[1]];
        let w = [p[0] - a[0], p[1] - a[1]];
        let t = ((w[0] * e[0] + w[1] * e[1]) / (e[0] * e[0] + e[1] * e[1]).max(1e-12)).clamp(0.0, 1.0);
        let q = [w[0] - e[0] * t, w[1] - e[1] * t];
        d = d.min(q[0] * q[0] + q[1] * q[1]);
        if (a[1] > p[1]) != (b[1] > p[1]) && p[0] < (b[0] - a[0]) * (p[1] - a[1]) / (b[1] - a[1]) + a[0] {
            inside = !inside;
        }
        j = i;
    }
    let d = d.sqrt();
    if inside {
        -d
    } else {
        d
    }
}

/// Coverage from a signed distance: a smoothstep across `feather` centred
/// on the edge (a hard edge when `feather` is 0).
pub fn feather_alpha(d: f32, feather: f32) -> f32 {
    let f = feather.max(1e-4);
    let t = (0.5 - d / f).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

// ---------------------------------------------------------------------------
// Correction node
// ---------------------------------------------------------------------------

static NEXT_NODE_ID: AtomicU64 = AtomicU64::new(1);

/// Unique within a session and, being time-seeded, practically never equal
/// to the ids of nodes loaded from a project.
fn new_node_id() -> u64 {
    static BASE: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    let base = *BASE.get_or_init(|| std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| (d.as_nanos() as u64) << 12).unwrap_or(1 << 40));
    base.wrapping_add(NEXT_NODE_ID.fetch_add(1, Ordering::Relaxed))
}

fn one() -> f32 {
    1.0
}
fn low_range() -> f32 {
    0.333
}
fn high_range() -> f32 {
    0.667
}

/// One stacked correction (a "node").
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ColorCorrection {
    /// Stable id for UI state. Not semantically relevant.
    #[serde(default = "new_node_id")]
    pub id: u64,
    pub name: String,
    pub enabled: bool,
    /// How much of the correction is applied (0..1).
    #[serde(default = "one")]
    pub mix: f32,
    // primary
    pub exposure: f32,
    pub temperature: f32,
    pub tint: f32,
    pub lift: ColorWheel,
    pub gamma: ColorWheel,
    pub gain: ColorWheel,
    pub offset: ColorWheel,
    pub contrast: f32,
    /// Contrast pivot offset from the working space's 18 % grey.
    pub pivot: f32,
    #[serde(default = "one")]
    pub saturation: f32,
    // log wheels
    pub log_shadows: ColorWheel,
    pub log_midtones: ColorWheel,
    pub log_highlights: ColorWheel,
    #[serde(default = "low_range")]
    pub low_range: f32,
    #[serde(default = "high_range")]
    pub high_range: f32,
    // curves
    pub curve_master: Curve,
    pub curve_r: Curve,
    pub curve_g: Curve,
    pub curve_b: Curve,
    /// Hue shift in turns (−0.5..0.5).
    pub hue_vs_hue: HueCurve,
    /// Saturation change (−1..1: ×0 .. ×2).
    pub hue_vs_sat: HueCurve,
    /// Luma change of saturated colours (−1..1).
    pub hue_vs_lum: HueCurve,
    // isolation
    pub qualifier: Option<Qualifier>,
    pub mask: Option<ShapeMask>,
}

impl Default for ColorCorrection {
    fn default() -> Self {
        ColorCorrection {
            id: new_node_id(),
            name: String::new(),
            enabled: true,
            mix: 1.0,
            exposure: 0.0,
            temperature: 0.0,
            tint: 0.0,
            lift: ColorWheel::default(),
            gamma: ColorWheel::default(),
            gain: ColorWheel::default(),
            offset: ColorWheel::default(),
            contrast: 0.0,
            pivot: 0.0,
            saturation: 1.0,
            log_shadows: ColorWheel::default(),
            log_midtones: ColorWheel::default(),
            log_highlights: ColorWheel::default(),
            low_range: low_range(),
            high_range: high_range(),
            curve_master: Curve::default(),
            curve_r: Curve::default(),
            curve_g: Curve::default(),
            curve_b: Curve::default(),
            hue_vs_hue: HueCurve::default(),
            hue_vs_sat: HueCurve::default(),
            hue_vs_lum: HueCurve::default(),
            qualifier: None,
            mask: None,
        }
    }
}

impl ColorCorrection {
    pub fn named(name: impl Into<String>) -> Self {
        ColorCorrection { name: name.into(), ..Default::default() }
    }

    /// True when the node leaves every pixel unchanged.
    pub fn is_identity(&self) -> bool {
        !self.enabled
            || self.mix <= 0.0
            || (self.exposure == 0.0
                && self.temperature == 0.0
                && self.tint == 0.0
                && [&self.lift, &self.gamma, &self.gain, &self.offset, &self.log_shadows, &self.log_midtones, &self.log_highlights].iter().all(|w| w.is_neutral())
                && self.contrast == 0.0
                && self.saturation == 1.0
                && self.curves_identity())
    }

    pub fn curves_identity(&self) -> bool {
        [&self.curve_master, &self.curve_r, &self.curve_g, &self.curve_b].iter().all(|c| c.is_identity())
            && [&self.hue_vs_hue, &self.hue_vs_sat, &self.hue_vs_lum].iter().all(|c| c.is_identity())
    }

    /// The seven curve tables ([`CURVE_TABLES`] × [`CURVE_SAMPLES`]) the GPU
    /// samples: master, R, G, B (output for input i / 255), then hue-vs-hue,
    /// hue-vs-sat and hue-vs-lum (offset for hue i / 256).
    pub fn bake_curves(&self) -> Vec<f32> {
        let mut out = Vec::with_capacity(CURVE_TABLES * CURVE_SAMPLES);
        for c in [&self.curve_master, &self.curve_r, &self.curve_g, &self.curve_b] {
            out.extend((0..CURVE_SAMPLES).map(|i| c.eval(i as f32 / (CURVE_SAMPLES - 1) as f32)));
        }
        for (c, lim) in [(&self.hue_vs_hue, 0.5), (&self.hue_vs_sat, 1.0), (&self.hue_vs_lum, 1.0)] {
            out.extend((0..CURVE_SAMPLES).map(|i| c.eval(i as f32 / CURVE_SAMPLES as f32).clamp(-lim, lim)));
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn acescct_round_trips_and_matches_reference_points() {
        assert!((acescct_encode(0.18) - 0.4135884).abs() < 1e-5);
        assert!((acescct_encode(1.0) - 0.5547945).abs() < 1e-5);
        for x in [-0.01f32, 0.0, 0.001, 0.0078125, 0.05, 0.18, 1.0, 16.0] {
            let y = acescct_decode(acescct_encode(x));
            assert!((y - x).abs() < 1e-4 * x.abs().max(1.0), "{x} → {y}");
        }
        // continuous at the toe break
        assert!((acescct_encode(0.0078125 - 1e-6) - acescct_encode(0.0078125 + 1e-6)).abs() < 1e-4);
        assert!((WorkingSpace::AcesCct.pivot() - 0.4135884).abs() < 1e-5);
    }

    #[test]
    fn tone_curve_is_monotone_and_hits_its_points() {
        let c = Curve { points: vec![[0.0, 0.0], [0.25, 0.15], [0.75, 0.85], [1.0, 1.0]] };
        for p in &c.points {
            assert!((c.eval(p[0]) - p[1]).abs() < 1e-5);
        }
        let mut last = -1.0;
        for i in 0..=100 {
            let y = c.eval(i as f32 / 100.0);
            assert!(y >= last - 1e-6, "monotone");
            assert!((-1e-6..=1.0 + 1e-6).contains(&y), "no overshoot: {y}");
            last = y;
        }
        assert!(Curve::default().is_identity());
        assert_eq!(Curve::default().eval(0.3), 0.3);
        // flat beyond the end points
        let lifted = Curve { points: vec![[0.0, 0.1], [1.0, 0.9]] };
        assert!((lifted.eval(0.5) - 0.5).abs() < 1e-5);
        assert_eq!(lifted.eval(1.5), 0.9);
        let mut e = Curve::default();
        e.set_point(0.5, 0.6, 0.02);
        assert_eq!(e.points.len(), 3);
        e.set_point(0.51, 0.7, 0.02);
        assert_eq!(e.points.len(), 3, "a nearby point moves");
    }

    #[test]
    fn hue_curve_wraps_smoothly() {
        let c = HueCurve { points: vec![[0.0, 0.2], [0.33, 0.0], [0.66, 0.0]] };
        assert!((c.eval(0.0) - 0.2).abs() < 1e-5);
        assert!((c.eval(1.0) - 0.2).abs() < 1e-5);
        assert!((c.eval(0.995) - c.eval(0.005)).abs() < 0.02, "continuous across red");
        assert!(c.eval(0.5).abs() < 0.05);
        assert!(HueCurve::default().is_identity());
        assert_eq!(HueCurve { points: vec![[0.4, 0.1]] }.eval(0.9), 0.1);
    }

    #[test]
    fn masks_cover_the_inside_and_feather_the_edge() {
        let aspect = 16.0 / 9.0;
        let e = ShapeMask { feather: 0.0, ..ShapeMask::default() };
        assert!(e.alpha([0.5, 0.5], aspect) > 0.99);
        assert!(e.alpha([0.02, 0.02], aspect) < 0.01);
        // circle of radius 0.3 frame heights: round on a 16:9 frame
        let r_x = 0.3 / aspect;
        assert!(e.alpha([0.5 + r_x * 0.95, 0.5], aspect) > 0.99);
        assert!(e.alpha([0.5 + r_x * 1.05, 0.5], aspect) < 0.01);
        let soft = ShapeMask { feather: 0.2, ..ShapeMask::default() };
        let edge = soft.alpha([0.5 + r_x, 0.5], aspect);
        assert!((edge - 0.5).abs() < 0.02, "half coverage on the edge: {edge}");
        let inv = ShapeMask { invert: true, ..e.clone() };
        assert!(inv.alpha([0.5, 0.5], aspect) < 0.01);
        // rotated rectangle
        let r = ShapeMask { shape: MaskShape::Rectangle, size: [0.4, 0.05], rotation: 90.0, feather: 0.0, ..ShapeMask::default() };
        assert!(r.alpha([0.5, 0.5 + 0.3], aspect) > 0.99, "long axis is vertical after 90°");
        assert!(r.alpha([0.5 + 0.3 / aspect, 0.5], aspect) < 0.01);
        // polygon (square) and its smoothed version
        let mut p = ShapeMask::new(MaskShape::Polygon);
        p.points = vec![[0.25, 0.25], [0.75, 0.25], [0.75, 0.75], [0.25, 0.75]];
        p.feather = 0.0;
        assert!(p.alpha([0.5, 0.5], aspect) > 0.99);
        assert!(p.alpha([0.1, 0.5], aspect) < 0.01);
        assert!(p.alpha([0.26, 0.26], aspect) > 0.99, "sharp corner kept");
        p.smooth = true;
        assert!(p.outline().len() <= MAX_MASK_POINTS && p.outline().len() >= 16);
        assert!(p.alpha([0.5, 0.5], aspect) > 0.99);
        // the spline passes through the vertices and bulges out between them
        // (Catmull-Rom over a square reaches y = 0.1875 mid-edge)
        assert!(p.alpha([0.5, 0.21], aspect) > 0.99, "smooth outline bulges past the straight edge");
        p.smooth = false;
        assert!(p.alpha([0.5, 0.21], aspect) < 0.01);
    }

    #[test]
    fn node_identity_and_curve_tables() {
        let n = ColorCorrection::default();
        assert!(n.is_identity());
        let t = n.bake_curves();
        assert_eq!(t.len(), CURVE_TABLES * CURVE_SAMPLES);
        assert!((t[128] - 128.0 / 255.0).abs() < 1e-6, "identity master table");
        assert!(t[4 * CURVE_SAMPLES..].iter().all(|&v| v == 0.0), "neutral hue tables");
        let mut s = ColorCorrection { saturation: 0.5, ..Default::default() };
        assert!(!s.is_identity());
        s.mix = 0.0;
        assert!(s.is_identity());
        let q = ColorCorrection { qualifier: Some(Qualifier::default()), ..Default::default() };
        assert!(q.is_identity(), "a qualifier alone changes nothing");
        // old projects (no new fields) deserialize to neutral values
        let old: ColorCorrection = serde_json::from_str(r#"{"name":"x","enabled":true}"#).unwrap();
        assert_eq!(old.mix, 1.0);
        assert_eq!(old.saturation, 1.0);
        assert!(old.is_identity());
    }

    #[test]
    fn ffmpeg_transfer_tags_map_to_input_spaces() {
        assert_eq!(ColorSpace::from_ffmpeg_transfer("smpte2084"), Some(ColorSpace::Rec2100Pq));
        assert_eq!(ColorSpace::from_ffmpeg_transfer("arib-std-b67"), Some(ColorSpace::Rec2100Hlg));
        assert_eq!(ColorSpace::from_ffmpeg_transfer("bt709"), Some(ColorSpace::Rec709));
        assert_eq!(ColorSpace::from_ffmpeg_transfer("unknown"), None);
    }
}
