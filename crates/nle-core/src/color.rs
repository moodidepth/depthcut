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
//! A node's numeric values (wheels, sliders, mix, window geometry) can be
//! keyframed as a whole ([`NodeKey`], like Resolve's dynamic keyframes);
//! curves, qualifier ranges and the window's shape stay fixed.

use crate::clip::ColorWheel;
use crate::time::Time;
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
    /// Follow a track of the clip (see `Clip::tracks`); the shape is drawn
    /// at the track's reference frame.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub track: Option<u64>,
    /// Bézier tangents per point as `[in dx, in dy, out dx, out dy]`
    /// (offsets from the point). When there is one per point the outline is
    /// a cubic Bézier spline and `smooth` is ignored.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub handles: Vec<[f32; 4]>,
    /// Rotoscoping: the polygon's shape at clip-local times. Between keys
    /// every point (and tangent) moves in a straight line.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub shape_keys: Vec<ShapeKey>,
}

/// Mask points and Bézier handles at one time.
pub type ShapePose = (Vec<[f32; 2]>, Vec<[f32; 4]>);

/// A polygon mask's points (and Bézier tangents) at one time.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ShapeKey {
    pub time: Time,
    pub points: Vec<[f32; 2]>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub handles: Vec<[f32; 4]>,
}

impl Default for ShapeMask {
    fn default() -> Self {
        ShapeMask {
            shape: MaskShape::Ellipse,
            center: [0.5, 0.5],
            size: [0.3, 0.3],
            rotation: 0.0,
            feather: 0.05,
            invert: false,
            points: Vec::new(),
            smooth: false,
            track: None,
            handles: Vec::new(),
            shape_keys: Vec::new(),
        }
    }
}

fn cubic(a: [f32; 2], b: [f32; 2], c: [f32; 2], d: [f32; 2], t: f32) -> [f32; 2] {
    let u = 1.0 - t;
    let (w0, w1, w2, w3) = (u * u * u, 3.0 * u * u * t, 3.0 * u * t * t, t * t * t);
    [w0 * a[0] + w1 * b[0] + w2 * c[0] + w3 * d[0], w0 * a[1] + w1 * b[1] + w2 * c[1] + w3 * d[1]]
}

/// Bézier tangents that reproduce the Catmull-Rom spline through `p`
/// (closed), or zero (sharp corners) when `smooth` is false.
fn tangents_for(p: &[[f32; 2]], smooth: bool) -> Vec<[f32; 4]> {
    let n = p.len();
    (0..n)
        .map(|i| {
            if !smooth || n < 3 {
                return [0.0; 4];
            }
            let (a, b) = (p[(i + n - 1) % n], p[(i + 1) % n]);
            let t = [(b[0] - a[0]) / 6.0, (b[1] - a[1]) / 6.0];
            [-t[0], -t[1], t[0], t[1]]
        })
        .collect()
}

impl ShapeMask {
    pub fn new(shape: MaskShape) -> Self {
        let mut m = ShapeMask { shape, ..Default::default() };
        if shape == MaskShape::Polygon {
            m.points = vec![[0.35, 0.3], [0.65, 0.3], [0.7, 0.7], [0.3, 0.7]];
        }
        m
    }

    /// True when every point carries Bézier tangents.
    pub fn is_bezier(&self) -> bool {
        !self.points.is_empty() && self.handles.len() == self.points.len()
    }

    /// The polygon outline the GPU tests against: the vertices, or the
    /// flattened spline when Bézier / `smooth` (at most [`MAX_MASK_POINTS`]).
    pub fn outline(&self) -> Vec<[f32; 2]> {
        let p = &self.points;
        if self.is_bezier() && p.len() >= 3 {
            let h = &self.handles;
            let n = p.len().min(MAX_MASK_POINTS);
            let per = (MAX_MASK_POINTS / n).max(1);
            let mut out = Vec::with_capacity(n * per);
            for i in 0..n {
                let j = (i + 1) % n;
                let (a, d) = (p[i], p[j]);
                let b = [a[0] + h[i][2], a[1] + h[i][3]];
                let c = [d[0] + h[j][0], d[1] + h[j][1]];
                for s in 0..per {
                    out.push(cubic(a, b, c, d, s as f32 / per as f32));
                }
            }
            return out;
        }
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
    /// Give every point Bézier tangents, keeping the current outline
    /// (smooth points get Catmull-Rom tangents, sharp ones zero tangents).
    /// Applies to every shape key too.
    pub fn convert_to_bezier(&mut self) {
        if self.is_bezier() {
            return;
        }
        self.handles = tangents_for(&self.points, self.smooth);
        for k in &mut self.shape_keys {
            k.handles = tangents_for(&k.points, self.smooth);
        }
        self.smooth = false;
    }

    /// Drop the Bézier tangents (straight edges again).
    pub fn clear_bezier(&mut self) {
        self.handles.clear();
        self.shape_keys.iter_mut().for_each(|k| k.handles.clear());
    }

    /// Insert a point before index `i` (wrapping). Bézier masks get a zero
    /// tangent; every shape key gets the midpoint of its own edge so the
    /// keys keep matching point counts.
    pub fn insert_point(&mut self, i: usize, p: [f32; 2]) {
        let i = i.min(self.points.len());
        self.points.insert(i, p);
        if self.handles.len() + 1 == self.points.len() {
            self.handles.insert(i, [0.0; 4]);
        }
        for k in &mut self.shape_keys {
            let n = k.points.len();
            if n == 0 {
                continue;
            }
            let (a, b) = (k.points[(i + n - 1) % n], k.points[i % n]);
            k.points.insert(i.min(n), [(a[0] + b[0]) * 0.5, (a[1] + b[1]) * 0.5]);
            if k.handles.len() == n {
                k.handles.insert(i.min(n), [0.0; 4]);
            }
        }
    }

    /// Remove point `i` from the shape and every key.
    pub fn remove_point(&mut self, i: usize) {
        if i >= self.points.len() {
            return;
        }
        self.points.remove(i);
        if i < self.handles.len() {
            self.handles.remove(i);
        }
        for k in &mut self.shape_keys {
            if i < k.points.len() {
                k.points.remove(i);
            }
            if i < k.handles.len() {
                k.handles.remove(i);
            }
        }
    }

    /// Points and tangents at clip-local `t` (None without shape keys).
    pub fn shape_at(&self, t: Time) -> Option<ShapePose> {
        let k = &self.shape_keys;
        let i = k.iter().position(|x| x.time > t);
        let one = |s: &ShapeKey| (s.points.clone(), s.handles.clone());
        match i {
            _ if k.is_empty() => None,
            Some(0) => Some(one(&k[0])),
            None => Some(one(&k[k.len() - 1])),
            Some(i) => {
                let (a, b) = (&k[i - 1], &k[i]);
                if a.points.len() != b.points.len() {
                    return Some(one(a));
                }
                let span = (b.time - a.time).as_secs_f64();
                let u = if span > 0.0 { ((t - a.time).as_secs_f64() / span).clamp(0.0, 1.0) as f32 } else { 1.0 };
                let pts = a.points.iter().zip(&b.points).map(|(p, q)| [lerp(p[0], q[0], u), lerp(p[1], q[1], u)]).collect();
                let hs = if a.handles.len() == b.handles.len() {
                    a.handles.iter().zip(&b.handles).map(|(p, q)| std::array::from_fn(|j| lerp(p[j], q[j], u))).collect()
                } else {
                    a.handles.clone()
                };
                Some((pts, hs))
            }
        }
    }

    /// This mask as it renders at clip-local `t`.
    pub fn at(&self, t: Time) -> ShapeMask {
        let mut m = self.clone();
        if let Some((p, h)) = self.shape_at(t) {
            m.points = p;
            m.handles = h;
        }
        m.shape_keys.clear();
        m
    }

    pub fn shape_key_near(&self, t: Time, tolerance: Time) -> Option<usize> {
        self.shape_keys.iter().position(|k| (k.time - t).abs() <= tolerance)
    }

    /// Store the current points / tangents as the shape key at `t`.
    pub fn set_shape_key(&mut self, t: Time, tolerance: Time) {
        let key = ShapeKey { time: t, points: self.points.clone(), handles: self.handles.clone() };
        match self.shape_key_near(t, tolerance) {
            Some(i) => self.shape_keys[i] = ShapeKey { time: self.shape_keys[i].time, ..key },
            None => {
                let at = self.shape_keys.partition_point(|k| k.time < t);
                self.shape_keys.insert(at, key);
            }
        }
    }

    pub fn remove_shape_key_near(&mut self, t: Time, tolerance: Time) -> bool {
        match self.shape_key_near(t, tolerance) {
            Some(i) => {
                self.shape_keys.remove(i);
                true
            }
            None => false,
        }
    }

    fn key_at(&self, at: Time) -> Option<ShapeKey> {
        self.shape_at(at).map(|(points, handles)| ShapeKey { time: at, points, handles })
    }

    /// The part before `at` (blade).
    pub fn keep_left(&mut self, at: Time) {
        if let Some(k) = self.key_at(at) {
            self.shape_keys.retain(|x| x.time <= at);
            if self.shape_keys.last().map(|x| x.time) != Some(at) {
                self.shape_keys.push(k);
            }
        }
    }

    /// The part after `at`, re-based to start at zero (blade).
    pub fn keep_right(&mut self, at: Time) {
        if let Some(k) = self.key_at(at) {
            self.shape_keys.retain(|x| x.time >= at);
            if self.shape_keys.first().map(|x| x.time) != Some(at) {
                self.shape_keys.insert(0, k);
            }
            self.shift_keys(Time::ZERO - at);
        }
    }

    pub fn shift_keys(&mut self, dt: Time) {
        for k in &mut self.shape_keys {
            k.time += dt;
        }
    }

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
pub fn new_node_id() -> u64 {
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
    /// Keyframes (clip-local times, sorted). Empty = the values above.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub keys: Vec<NodeKey>,
}

/// The keyframable values of a node.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct NodeValues {
    pub mix: f32,
    pub exposure: f32,
    pub temperature: f32,
    pub tint: f32,
    pub lift: ColorWheel,
    pub gamma: ColorWheel,
    pub gain: ColorWheel,
    pub offset: ColorWheel,
    pub contrast: f32,
    pub pivot: f32,
    pub saturation: f32,
    pub log_shadows: ColorWheel,
    pub log_midtones: ColorWheel,
    pub log_highlights: ColorWheel,
    pub low_range: f32,
    pub high_range: f32,
    /// Window centre x / y, size x / y, rotation, feather.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window: Option<[f32; 6]>,
}

/// A node's values at one time.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct NodeKey {
    pub time: Time,
    pub values: NodeValues,
}

fn lerp(a: f32, b: f32, u: f32) -> f32 {
    a + (b - a) * u
}

fn lerp_wheel(a: ColorWheel, b: ColorWheel, u: f32) -> ColorWheel {
    ColorWheel { r: lerp(a.r, b.r, u), g: lerp(a.g, b.g, u), b: lerp(a.b, b.b, u), master: lerp(a.master, b.master, u) }
}

impl NodeValues {
    pub fn of(n: &ColorCorrection) -> NodeValues {
        NodeValues {
            mix: n.mix,
            exposure: n.exposure,
            temperature: n.temperature,
            tint: n.tint,
            lift: n.lift,
            gamma: n.gamma,
            gain: n.gain,
            offset: n.offset,
            contrast: n.contrast,
            pivot: n.pivot,
            saturation: n.saturation,
            log_shadows: n.log_shadows,
            log_midtones: n.log_midtones,
            log_highlights: n.log_highlights,
            low_range: n.low_range,
            high_range: n.high_range,
            window: n.mask.as_ref().map(|m| [m.center[0], m.center[1], m.size[0], m.size[1], m.rotation, m.feather]),
        }
    }

    pub fn apply(&self, n: &mut ColorCorrection) {
        n.mix = self.mix;
        n.exposure = self.exposure;
        n.temperature = self.temperature;
        n.tint = self.tint;
        n.lift = self.lift;
        n.gamma = self.gamma;
        n.gain = self.gain;
        n.offset = self.offset;
        n.contrast = self.contrast;
        n.pivot = self.pivot;
        n.saturation = self.saturation;
        n.log_shadows = self.log_shadows;
        n.log_midtones = self.log_midtones;
        n.log_highlights = self.log_highlights;
        n.low_range = self.low_range;
        n.high_range = self.high_range;
        if let (Some(m), Some(w)) = (n.mask.as_mut(), self.window) {
            m.center = [w[0], w[1]];
            m.size = [w[2], w[3]];
            m.rotation = w[4];
            m.feather = w[5];
        }
    }

    /// Linear blend from `a` (u = 0) to `b` (u = 1).
    pub fn lerp(a: &NodeValues, b: &NodeValues, u: f32) -> NodeValues {
        let f = |x: f32, y: f32| lerp(x, y, u);
        let w = |x: ColorWheel, y: ColorWheel| lerp_wheel(x, y, u);
        NodeValues {
            mix: f(a.mix, b.mix),
            exposure: f(a.exposure, b.exposure),
            temperature: f(a.temperature, b.temperature),
            tint: f(a.tint, b.tint),
            lift: w(a.lift, b.lift),
            gamma: w(a.gamma, b.gamma),
            gain: w(a.gain, b.gain),
            offset: w(a.offset, b.offset),
            contrast: f(a.contrast, b.contrast),
            pivot: f(a.pivot, b.pivot),
            saturation: f(a.saturation, b.saturation),
            log_shadows: w(a.log_shadows, b.log_shadows),
            log_midtones: w(a.log_midtones, b.log_midtones),
            log_highlights: w(a.log_highlights, b.log_highlights),
            low_range: f(a.low_range, b.low_range),
            high_range: f(a.high_range, b.high_range),
            window: match (a.window, b.window) {
                (Some(x), Some(y)) => Some(std::array::from_fn(|i| lerp(x[i], y[i], u))),
                (x, y) => x.or(y),
            },
        }
    }
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
            keys: Vec::new(),
        }
    }
}

impl ColorCorrection {
    pub fn named(name: impl Into<String>) -> Self {
        ColorCorrection { name: name.into(), ..Default::default() }
    }

    /// A copy with its own id (pasting, duplicating).
    pub fn duplicate(&self) -> ColorCorrection {
        ColorCorrection { id: new_node_id(), ..self.clone() }
    }

    /// The values at clip-local time `t` (between keys: a linear blend;
    /// before the first / after the last: that key).
    pub fn values_at(&self, t: Time) -> NodeValues {
        let k = &self.keys;
        match k.iter().position(|x| x.time > t) {
            _ if k.is_empty() => NodeValues::of(self),
            Some(0) => k[0].values,
            None => k[k.len() - 1].values,
            Some(i) => {
                let (a, b) = (&k[i - 1], &k[i]);
                let span = (b.time - a.time).as_secs_f64();
                let u = if span > 0.0 { ((t - a.time).as_secs_f64() / span) as f32 } else { 1.0 };
                NodeValues::lerp(&a.values, &b.values, u.clamp(0.0, 1.0))
            }
        }
    }

    /// This node as it renders at `t`.
    pub fn at(&self, t: Time) -> ColorCorrection {
        let mut n = self.clone();
        if !self.keys.is_empty() {
            self.values_at(t).apply(&mut n);
        }
        if let Some(m) = n.mask.as_mut().filter(|m| !m.shape_keys.is_empty()) {
            *m = m.at(t);
        }
        n
    }

    pub fn key_near(&self, t: Time, tolerance: Time) -> Option<usize> {
        self.keys.iter().position(|k| (k.time - t).abs() <= tolerance)
    }

    /// Store the node's current values as the key at `t` (replacing one
    /// within `tolerance`).
    pub fn set_key(&mut self, t: Time, tolerance: Time) {
        let values = NodeValues::of(self);
        match self.key_near(t, tolerance) {
            Some(i) => self.keys[i].values = values,
            None => {
                let at = self.keys.partition_point(|k| k.time < t);
                self.keys.insert(at, NodeKey { time: t, values });
            }
        }
    }

    pub fn remove_key_near(&mut self, t: Time, tolerance: Time) -> bool {
        match self.key_near(t, tolerance) {
            Some(i) => {
                self.keys.remove(i);
                true
            }
            None => false,
        }
    }

    /// Drop the keys, keeping the values at `t`.
    pub fn clear_keys(&mut self, t: Time) {
        let v = self.values_at(t);
        v.apply(self);
        self.keys.clear();
    }

    /// The part before `at` (blade).
    pub fn keep_left(&mut self, at: Time) {
        if let Some(m) = &mut self.mask {
            m.keep_left(at);
        }
        if !self.keys.is_empty() {
            let v = self.values_at(at);
            self.keys.retain(|k| k.time <= at);
            if self.keys.last().map(|k| k.time) != Some(at) {
                self.keys.push(NodeKey { time: at, values: v });
            }
        }
    }

    /// The part after `at`, re-based to start at zero (blade).
    pub fn keep_right(&mut self, at: Time) {
        if let Some(m) = &mut self.mask {
            m.keep_right(at);
        }
        if !self.keys.is_empty() {
            let v = self.values_at(at);
            self.keys.retain(|k| k.time >= at);
            if self.keys.first().map(|k| k.time) != Some(at) {
                self.keys.insert(0, NodeKey { time: at, values: v });
            }
            for k in &mut self.keys {
                k.time -= at;
            }
        }
    }

    pub fn shift_keys(&mut self, dt: Time) {
        for k in &mut self.keys {
            k.time += dt;
        }
        if let Some(m) = &mut self.mask {
            m.shift_keys(dt);
        }
    }

    /// True when the node leaves every pixel unchanged.
    pub fn is_identity(&self) -> bool {
        if !self.keys.is_empty() {
            return !self.enabled || self.keys.iter().all(|k| self.at(k.time).without_keys().is_identity());
        }
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

    fn without_keys(mut self) -> ColorCorrection {
        self.keys.clear();
        self
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
    fn bezier_masks_keep_the_spline_and_shape_keys_interpolate() {
        let aspect = 16.0 / 9.0;
        let mut p = ShapeMask::new(MaskShape::Polygon);
        p.points = vec![[0.25, 0.25], [0.75, 0.25], [0.75, 0.75], [0.25, 0.75]];
        p.feather = 0.0;
        p.smooth = true;
        let spline = p.outline();
        p.convert_to_bezier();
        assert!(p.is_bezier() && !p.smooth);
        // Catmull-Rom tangents reproduce the same curve
        let bez = p.outline();
        assert_eq!(spline.len(), bez.len());
        for (a, b) in spline.iter().zip(&bez) {
            assert!((a[0] - b[0]).abs() < 1e-4 && (a[1] - b[1]).abs() < 1e-4, "{a:?} vs {b:?}");
        }
        // zero tangents: straight edges
        p.handles.iter_mut().for_each(|h| *h = [0.0; 4]);
        assert!(p.alpha([0.5, 0.21], aspect) < 0.01);
        assert!(p.alpha([0.26, 0.26], aspect) > 0.99);
        // a long out tangent on the top-left point bows the top edge upwards
        p.handles[0] = [0.0, 0.0, 0.2, -0.2];
        assert!(p.alpha([0.35, 0.21], aspect) > 0.99);

        // roto: two keys, the shape moves in a straight line between them
        let s = Time::from_secs;
        let mut m = ShapeMask::new(MaskShape::Polygon);
        let tol = Time::from_millis(10);
        m.set_shape_key(s(0), tol);
        let start = m.points.clone();
        for q in &mut m.points {
            q[0] += 0.2;
        }
        m.set_shape_key(s(2), tol);
        assert_eq!(m.shape_keys.len(), 2);
        let mid = m.at(s(1));
        assert!((mid.points[0][0] - (start[0][0] + 0.1)).abs() < 1e-5);
        assert!(mid.shape_keys.is_empty());
        assert_eq!(m.at(s(5)).points[0][0], start[0][0] + 0.2);
        // inserting a point keeps every key the same length
        m.insert_point(1, [0.5, 0.2]);
        assert!(m.shape_keys.iter().all(|k| k.points.len() == 5));
        m.remove_point(1);
        assert!(m.shape_keys.iter().all(|k| k.points.len() == 4));
        // blade: the right half starts where the left half ended
        let (mut l, mut r) = (m.clone(), m.clone());
        l.keep_left(s(1));
        r.keep_right(s(1));
        assert_eq!(l.at(s(1)).points, mid.points);
        assert_eq!(r.at(Time::ZERO).points, mid.points);
        assert_eq!(r.at(s(1)).points, m.at(s(2)).points);
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
    fn node_keys_blend_and_survive_a_blade() {
        let s = Time::from_secs;
        let tol = Time::from_millis(1);
        let mut n = ColorCorrection { mask: Some(ShapeMask::default()), ..Default::default() };
        n.set_key(s(0), tol);
        n.exposure = 2.0;
        n.lift.r = 0.4;
        n.mask.as_mut().unwrap().center = [0.9, 0.5];
        n.set_key(s(4), tol);
        assert_eq!(n.keys.len(), 2);
        let mid = n.at(s(1));
        assert!((mid.exposure - 0.5).abs() < 1e-5 && (mid.lift.r - 0.1).abs() < 1e-5);
        assert!((mid.mask.as_ref().unwrap().center[0] - 0.6).abs() < 1e-5, "the window moves");
        assert_eq!(n.at(s(9)).exposure, 2.0, "held after the last key");
        assert_eq!(n.at(Time::ZERO - s(1)).exposure, 0.0, "held before the first key");
        assert!(!n.is_identity());
        // re-keying near an existing key replaces it
        n.exposure = 3.0;
        n.set_key(s(4) + Time(1), tol);
        assert_eq!(n.keys.len(), 2);
        assert_eq!(n.at(s(4)).exposure, 3.0);
        // blade at 2 s: both halves hold the value there
        let (mut l, mut r) = (n.clone(), n.clone());
        l.keep_left(s(2));
        r.keep_right(s(2));
        assert!((l.at(s(2)).exposure - 1.5).abs() < 1e-5);
        assert!((r.at(Time::ZERO).exposure - 1.5).abs() < 1e-5);
        assert!((r.at(s(2)).exposure - 3.0).abs() < 1e-5);
        // keys round-trip through JSON; nodes without keys don't write any
        let j = serde_json::to_string(&n).unwrap();
        let back: ColorCorrection = serde_json::from_str(&j).unwrap();
        assert_eq!(back.keys, n.keys);
        assert!(!serde_json::to_string(&ColorCorrection::default()).unwrap().contains("keys"));
        n.clear_keys(s(1));
        assert!(n.keys.is_empty() && (n.exposure - 0.75).abs() < 1e-5);
    }

    #[test]
    fn ffmpeg_transfer_tags_map_to_input_spaces() {
        assert_eq!(ColorSpace::from_ffmpeg_transfer("smpte2084"), Some(ColorSpace::Rec2100Pq));
        assert_eq!(ColorSpace::from_ffmpeg_transfer("arib-std-b67"), Some(ColorSpace::Rec2100Hlg));
        assert_eq!(ColorSpace::from_ffmpeg_transfer("bt709"), Some(ColorSpace::Rec709));
        assert_eq!(ColorSpace::from_ffmpeg_transfer("unknown"), None);
    }
}
