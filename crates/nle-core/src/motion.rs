//! Motion tracking, planar tracking, stabilization and scene-cut picking:
//! the data model and the CPU maths. The GPU only measures motion (dense
//! optical flow, `nle_render::motion`); everything here is deterministic and
//! unit-tested.
//!
//! Geometry is in normalised frame coordinates (`0..1` on both axes, origin
//! top-left, y down). Fits and decompositions run in *aspect space*
//! (`x · aspect`, `y`) so rotations stay rigid on non-square frames.

use crate::clip::Clip;
use crate::color::ShapeMask;
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicU64, Ordering};

/// Row-major 3×3 matrix acting on `(x, y, 1)`.
pub type Mat3 = [f32; 9];

pub const IDENTITY: Mat3 = [1.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0];

pub fn mul(a: &Mat3, b: &Mat3) -> Mat3 {
    let mut o = [0.0f32; 9];
    for r in 0..3 {
        for c in 0..3 {
            o[r * 3 + c] = (0..3).map(|k| a[r * 3 + k] as f64 * b[k * 3 + c] as f64).sum::<f64>() as f32;
        }
    }
    o
}

pub fn invert(m: &Mat3) -> Option<Mat3> {
    let m: [f64; 9] = m.map(|v| v as f64);
    let c00 = m[4] * m[8] - m[5] * m[7];
    let c01 = m[5] * m[6] - m[3] * m[8];
    let c02 = m[3] * m[7] - m[4] * m[6];
    let det = m[0] * c00 + m[1] * c01 + m[2] * c02;
    if det.abs() < 1e-12 || !det.is_finite() {
        return None;
    }
    let d = 1.0 / det;
    Some(
        [
            c00 * d,
            (m[2] * m[7] - m[1] * m[8]) * d,
            (m[1] * m[5] - m[2] * m[4]) * d,
            c01 * d,
            (m[0] * m[8] - m[2] * m[6]) * d,
            (m[2] * m[3] - m[0] * m[5]) * d,
            c02 * d,
            (m[1] * m[6] - m[0] * m[7]) * d,
            (m[0] * m[4] - m[1] * m[3]) * d,
        ]
        .map(|v| v as f32),
    )
}

pub fn apply(m: &Mat3, p: [f32; 2]) -> [f32; 2] {
    let w = m[6] * p[0] + m[7] * p[1] + m[8];
    let w = if w.abs() < 1e-9 { 1e-9 } else { w };
    [(m[0] * p[0] + m[1] * p[1] + m[2]) / w, (m[3] * p[0] + m[4] * p[1] + m[5]) / w]
}

/// Scale so `m[8] == 1` (homographies are defined up to scale).
pub fn normalize(m: &Mat3) -> Mat3 {
    if m[8].abs() < 1e-12 {
        return *m;
    }
    m.map(|v| v / m[8])
}

pub fn translation(dx: f32, dy: f32) -> Mat3 {
    [1.0, 0.0, dx, 0.0, 1.0, dy, 0.0, 0.0, 1.0]
}

fn scaling(sx: f32, sy: f32) -> Mat3 {
    [sx, 0.0, 0.0, 0.0, sy, 0.0, 0.0, 0.0, 1.0]
}

/// Uniform scale `s` and clockwise rotation `angle` (radians) about `c`.
pub fn similarity_about(c: [f32; 2], s: f32, angle: f32, shift: [f32; 2]) -> Mat3 {
    let (sn, cs) = angle.sin_cos();
    let r = [s * cs, -s * sn, 0.0, s * sn, s * cs, 0.0, 0.0, 0.0, 1.0];
    mul(&translation(c[0] + shift[0], c[1] + shift[1]), &mul(&r, &translation(-c[0], -c[1])))
}

/// Normalised-coordinate matrix → aspect space.
pub fn to_aspect(m: &Mat3, aspect: f32) -> Mat3 {
    mul(&scaling(aspect, 1.0), &mul(m, &scaling(1.0 / aspect, 1.0)))
}

/// Aspect-space matrix → normalised coordinates.
pub fn from_aspect(m: &Mat3, aspect: f32) -> Mat3 {
    mul(&scaling(1.0 / aspect, 1.0), &mul(m, &scaling(aspect, 1.0)))
}

/// Local uniform scale and clockwise rotation (radians) of `m` at `p`
/// (normalised coordinates), measured in aspect space.
pub fn local_similarity(m: &Mat3, p: [f32; 2], aspect: f32) -> (f32, f32) {
    let a = to_aspect(m, aspect);
    let q = [p[0] * aspect, p[1]];
    let e = 1e-3;
    let o = apply(&a, q);
    let x = apply(&a, [q[0] + e, q[1]]);
    let y = apply(&a, [q[0], q[1] + e]);
    let (j00, j10) = ((x[0] - o[0]) / e, (x[1] - o[1]) / e);
    let (j01, j11) = ((y[0] - o[0]) / e, (y[1] - o[1]) / e);
    let det = j00 * j11 - j01 * j10;
    (det.abs().sqrt(), j10.atan2(j00))
}

// ---------------------------------------------------------------------------
// Fitting
// ---------------------------------------------------------------------------

/// What a fit may express.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
pub enum MotionModel {
    /// Position only (point tracker).
    #[default]
    Translation,
    /// Position, uniform scale and rotation (region tracker, Similarity stabilizer).
    Similarity,
    /// Full perspective (planar tracker, Perspective stabilizer).
    Perspective,
}

impl MotionModel {
    pub const ALL: [MotionModel; 3] = [MotionModel::Translation, MotionModel::Similarity, MotionModel::Perspective];

    /// Points needed for one hypothesis.
    pub fn label(self) -> &'static str {
        match self {
            MotionModel::Translation => "Translation (Point)",
            MotionModel::Similarity => "Similarity (Region)",
            MotionModel::Perspective => "Perspective (Planar)",
        }
    }

    pub fn minimal(self) -> usize {
        match self {
            MotionModel::Translation => 1,
            MotionModel::Similarity => 2,
            MotionModel::Perspective => 4,
        }
    }
}

/// `(from, to)` correspondence.
pub type Pair = ([f32; 2], [f32; 2]);

pub fn fit_translation(pairs: &[Pair]) -> Option<Mat3> {
    if pairs.is_empty() {
        return None;
    }
    let n = pairs.len() as f64;
    let dx = pairs.iter().map(|(a, b)| (b[0] - a[0]) as f64).sum::<f64>() / n;
    let dy = pairs.iter().map(|(a, b)| (b[1] - a[1]) as f64).sum::<f64>() / n;
    Some(translation(dx as f32, dy as f32))
}

/// Least-squares similarity (closed form, Umeyama without reflection).
pub fn fit_similarity(pairs: &[Pair]) -> Option<Mat3> {
    if pairs.len() < 2 {
        return None;
    }
    let n = pairs.len() as f64;
    let (mut ax, mut ay, mut bx, mut by) = (0.0, 0.0, 0.0, 0.0);
    for (a, b) in pairs {
        ax += a[0] as f64;
        ay += a[1] as f64;
        bx += b[0] as f64;
        by += b[1] as f64;
    }
    let (ax, ay, bx, by) = (ax / n, ay / n, bx / n, by / n);
    let (mut sxx, mut num_a, mut num_b) = (0.0, 0.0, 0.0);
    for (a, b) in pairs {
        let (x, y) = (a[0] as f64 - ax, a[1] as f64 - ay);
        let (u, v) = (b[0] as f64 - bx, b[1] as f64 - by);
        sxx += x * x + y * y;
        num_a += x * u + y * v;
        num_b += x * v - y * u;
    }
    if sxx < 1e-12 {
        return None;
    }
    let (a, b) = (num_a / sxx, num_b / sxx);
    let tx = bx - (a * ax - b * ay);
    let ty = by - (b * ax + a * ay);
    Some([a, -b, tx, b, a, ty, 0.0, 0.0, 1.0].map(|v| v as f32))
}

/// Least-squares homography (normalised DLT with `h33 = 1`).
pub fn fit_homography(pairs: &[Pair]) -> Option<Mat3> {
    if pairs.len() < 4 {
        return None;
    }
    // Hartley normalisation of both point sets
    let norm = |pts: &mut dyn Iterator<Item = [f32; 2]>| -> [f64; 9] {
        let v: Vec<[f64; 2]> = pts.map(|p| [p[0] as f64, p[1] as f64]).collect();
        let n = v.len() as f64;
        let cx = v.iter().map(|p| p[0]).sum::<f64>() / n;
        let cy = v.iter().map(|p| p[1]).sum::<f64>() / n;
        let d = v.iter().map(|p| ((p[0] - cx).powi(2) + (p[1] - cy).powi(2)).sqrt()).sum::<f64>() / n;
        let s = if d > 1e-12 { std::f64::consts::SQRT_2 / d } else { 1.0 };
        [s, 0.0, -s * cx, 0.0, s, -s * cy, 0.0, 0.0, 1.0]
    };
    let ta = norm(&mut pairs.iter().map(|p| p.0));
    let tb = norm(&mut pairs.iter().map(|p| p.1));
    let tr = |t: &[f64; 9], p: [f32; 2]| [t[0] * p[0] as f64 + t[2], t[4] * p[1] as f64 + t[5]];
    // normal equations of the 2n × 8 system
    let mut ata = [[0.0f64; 8]; 8];
    let mut atb = [0.0f64; 8];
    for (a, b) in pairs {
        let [x, y] = tr(&ta, *a);
        let [u, v] = tr(&tb, *b);
        let rows = [([x, y, 1.0, 0.0, 0.0, 0.0, -u * x, -u * y], u), ([0.0, 0.0, 0.0, x, y, 1.0, -v * x, -v * y], v)];
        for (r, rhs) in rows {
            for i in 0..8 {
                atb[i] += r[i] * rhs;
                for j in 0..8 {
                    ata[i][j] += r[i] * r[j];
                }
            }
        }
    }
    let h = solve8(ata, atb)?;
    let hn = [h[0], h[1], h[2], h[3], h[4], h[5], h[6], h[7], 1.0];
    // denormalise: H = Tb⁻¹ · Hn · Ta
    let tb_inv = invert(&tb.map(|v| v as f32))?.map(|v| v as f64);
    let m = mul64(&tb_inv, &mul64(&hn, &ta));
    let m = m.map(|v| v as f32);
    let m = normalize(&m);
    m.iter().all(|v| v.is_finite()).then_some(m)
}

fn mul64(a: &[f64; 9], b: &[f64; 9]) -> [f64; 9] {
    let mut o = [0.0; 9];
    for r in 0..3 {
        for c in 0..3 {
            o[r * 3 + c] = (0..3).map(|k| a[r * 3 + k] * b[k * 3 + c]).sum();
        }
    }
    o
}

/// Gaussian elimination with partial pivoting.
fn solve8(mut a: [[f64; 8]; 8], mut b: [f64; 8]) -> Option<[f64; 8]> {
    for col in 0..8 {
        let piv = (col..8).max_by(|&i, &j| a[i][col].abs().total_cmp(&a[j][col].abs()))?;
        if a[piv][col].abs() < 1e-12 {
            return None;
        }
        a.swap(col, piv);
        b.swap(col, piv);
        for r in col + 1..8 {
            let f = a[r][col] / a[col][col];
            for c in col..8 {
                a[r][c] -= f * a[col][c];
            }
            b[r] -= f * b[col];
        }
    }
    let mut x = [0.0; 8];
    for r in (0..8).rev() {
        let s: f64 = (r + 1..8).map(|c| a[r][c] * x[c]).sum();
        x[r] = (b[r] - s) / a[r][r];
    }
    Some(x)
}

pub fn fit(model: MotionModel, pairs: &[Pair]) -> Option<Mat3> {
    match model {
        MotionModel::Translation => fit_translation(pairs),
        MotionModel::Similarity => fit_similarity(pairs),
        MotionModel::Perspective => fit_homography(pairs),
    }
}

/// Small deterministic PRNG (xorshift) so analysis results are reproducible.
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n.max(1) as u64) as usize
    }
}

/// Robust fit: RANSAC over minimal samples, then a least-squares refit on
/// the inliers (`threshold` in the pairs' units). Returns the model and the
/// inlier ratio.
pub fn ransac(model: MotionModel, pairs: &[Pair], threshold: f32, iterations: usize) -> Option<(Mat3, f32)> {
    let k = model.minimal();
    if pairs.len() < k {
        return None;
    }
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15 ^ pairs.len() as u64);
    let t2 = threshold * threshold;
    let count = |m: &Mat3| {
        pairs
            .iter()
            .filter(|(a, b)| {
                let p = apply(m, *a);
                (p[0] - b[0]).powi(2) + (p[1] - b[1]).powi(2) <= t2
            })
            .count()
    };
    let mut best: Option<(Mat3, usize)> = None;
    let mut sample = Vec::with_capacity(k);
    for _ in 0..iterations.max(1) {
        sample.clear();
        for _ in 0..k * 4 {
            if sample.len() == k {
                break;
            }
            let p = pairs[rng.below(pairs.len())];
            if !sample.iter().any(|q: &Pair| (q.0[0] - p.0[0]).abs() + (q.0[1] - p.0[1]).abs() < 1e-6) {
                sample.push(p);
            }
        }
        if sample.len() < k {
            continue;
        }
        let Some(m) = fit(model, &sample) else { continue };
        let n = count(&m);
        if best.as_ref().is_none_or(|b| n > b.1) {
            best = Some((m, n));
            if n == pairs.len() {
                break;
            }
        }
    }
    let (m, _) = best?;
    // refit on inliers twice (the set can grow after the first refit)
    let mut m = m;
    for _ in 0..2 {
        let inl: Vec<Pair> = pairs
            .iter()
            .copied()
            .filter(|(a, b)| {
                let p = apply(&m, *a);
                (p[0] - b[0]).powi(2) + (p[1] - b[1]).powi(2) <= t2
            })
            .collect();
        if inl.len() < k {
            break;
        }
        if let Some(r) = fit(model, &inl) {
            m = r;
        }
    }
    let ratio = count(&m) as f32 / pairs.len() as f32;
    Some((m, ratio))
}

// ---------------------------------------------------------------------------
// Tracks
// ---------------------------------------------------------------------------

static NEXT_TRACK_ID: AtomicU64 = AtomicU64::new(1);

pub fn new_track_id() -> u64 {
    static BASE: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    let base = *BASE.get_or_init(|| std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| (d.as_nanos() as u64) << 12).unwrap_or(1 << 41));
    base.wrapping_add(NEXT_TRACK_ID.fetch_add(1, Ordering::Relaxed))
}

/// Transform of the tracked region at one source instant.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct TrackSample {
    /// Source-media time in seconds (independent of trims and retiming).
    pub t: f64,
    /// Reference frame → this frame (normalised coordinates).
    pub h: Mat3,
}

/// A point, region or planar track on a clip's source media.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Track {
    pub id: u64,
    pub name: String,
    pub model: MotionModel,
    /// Tracked region `[x, y, w, h]` at the reference frame.
    pub region: [f32; 4],
    /// Source time (seconds) the region was drawn at.
    pub reference: f64,
    /// Source frame aspect (width / height) the track was measured on.
    pub aspect: f32,
    /// Sorted by `t`.
    pub samples: Vec<TrackSample>,
}

impl Default for Track {
    fn default() -> Self {
        Track { id: new_track_id(), name: "Track".into(), model: MotionModel::Translation, region: [0.4, 0.4, 0.2, 0.2], reference: 0.0, aspect: 16.0 / 9.0, samples: Vec::new() }
    }
}

impl Track {
    pub fn new(name: impl Into<String>, model: MotionModel, region: [f32; 4], reference: f64, aspect: f32) -> Self {
        Track { name: name.into(), model, region, reference, aspect, samples: vec![TrackSample { t: reference, h: IDENTITY }], ..Default::default() }
    }

    /// Reference → frame-at-`t` transform. Interpolates between samples and
    /// holds the first / last sample outside the tracked range.
    pub fn at(&self, t: f64) -> Mat3 {
        sample_path(&self.samples, t)
    }

    /// Region centre at `t`.
    pub fn point_at(&self, t: f64) -> [f32; 2] {
        let r = self.region;
        apply(&self.at(t), [r[0] + r[2] * 0.5, r[1] + r[3] * 0.5])
    }

    /// The region's four corners at `t` (clockwise from top-left).
    pub fn corners_at(&self, t: f64) -> [[f32; 2]; 4] {
        let r = self.region;
        let h = self.at(t);
        [[r[0], r[1]], [r[0] + r[2], r[1]], [r[0] + r[2], r[1] + r[3]], [r[0], r[1] + r[3]]].map(|p| apply(&h, p))
    }

    /// Insert or replace the sample at `t` (within half a millisecond).
    pub fn insert(&mut self, s: TrackSample) {
        match self.samples.iter().position(|x| (x.t - s.t).abs() < 5e-4) {
            Some(i) => self.samples[i] = s,
            None => {
                let i = self.samples.partition_point(|x| x.t < s.t);
                self.samples.insert(i, s);
            }
        }
    }

    /// Time span covered by samples.
    pub fn range(&self) -> Option<(f64, f64)> {
        Some((self.samples.first()?.t, self.samples.last()?.t))
    }

    /// `mask` moved along with the tracked region from the reference to `t`.
    pub fn move_mask(&self, mask: &ShapeMask, t: f64) -> ShapeMask {
        transform_mask(mask, &self.at(t), self.aspect, self.model)
    }
}

/// Interpolated path sample (holds at both ends).
pub fn sample_path(samples: &[TrackSample], t: f64) -> Mat3 {
    let Some(first) = samples.first() else { return IDENTITY };
    if t <= first.t {
        return first.h;
    }
    let last = samples.last().unwrap();
    if t >= last.t {
        return last.h;
    }
    let i = samples.partition_point(|s| s.t <= t);
    let (a, b) = (&samples[i - 1], &samples[i]);
    let u = ((t - a.t) / (b.t - a.t).max(1e-9)) as f32;
    let (ha, hb) = (normalize(&a.h), normalize(&b.h));
    let mut o = [0.0; 9];
    for k in 0..9 {
        o[k] = ha[k] + (hb[k] - ha[k]) * u;
    }
    o
}

/// Apply a reference → current transform to a shape mask. Ellipses and
/// rectangles follow the local similarity of `h` (position, scale,
/// rotation); polygon points follow `h` exactly (planar corner pin).
pub fn transform_mask(mask: &ShapeMask, h: &Mat3, aspect: f32, model: MotionModel) -> ShapeMask {
    let mut m = mask.clone();
    m.center = apply(h, mask.center);
    if model != MotionModel::Translation {
        let (s, rot) = local_similarity(h, mask.center, aspect);
        m.size = [mask.size[0] * s, mask.size[1] * s];
        m.rotation = mask.rotation + rot.to_degrees();
        m.feather = mask.feather * s;
    }
    if !mask.points.is_empty() {
        m.points = if model == MotionModel::Translation {
            let d = [m.center[0] - mask.center[0], m.center[1] - mask.center[1]];
            mask.points.iter().map(|p| [p[0] + d[0], p[1] + d[1]]).collect()
        } else {
            mask.points.iter().map(|p| apply(h, *p)).collect()
        };
        // tangents ride along as offsets from their (moved) point
        if mask.is_bezier() && model != MotionModel::Translation {
            m.handles = mask
                .points
                .iter()
                .zip(&mask.handles)
                .zip(&m.points)
                .map(|((p, hd), q)| {
                    let i = apply(h, [p[0] + hd[0], p[1] + hd[1]]);
                    let o = apply(h, [p[0] + hd[2], p[1] + hd[3]]);
                    [i[0] - q[0], i[1] - q[1], o[0] - q[0], o[1] - q[1]]
                })
                .collect();
        }
    }
    m
}

/// Seconds of source media shown at clip-local `local` time.
pub fn source_secs(c: &Clip, local: crate::Time) -> f64 {
    c.source_time(local).as_secs_f64()
}

/// A connected clip (title, callout, …) that rides along with a track on
/// another clip. The follower's position is moved by the track's motion of
/// its own anchor point, assuming the tracked clip fills the frame.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Follow {
    pub clip: crate::ClipId,
    pub track: u64,
    /// Also follow the track's scale and rotation (region / planar tracks).
    #[serde(default)]
    pub transform: bool,
}

// ---------------------------------------------------------------------------
// Stabilization
// ---------------------------------------------------------------------------

/// Camera-shake removal: the measured camera path is smoothed and every
/// frame is warped from its real path onto the smooth one, then scaled up
/// just enough that no frame shows its edges (auto-crop).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Stabilization {
    pub enabled: bool,
    pub model: MotionModel,
    /// Gaussian smoothing of the camera path, in seconds (σ). `0` locks the
    /// camera completely (tripod).
    pub smoothing: f32,
    /// Lock the camera (ignore `smoothing`).
    pub tripod: bool,
    pub auto_crop: bool,
    /// Extra zoom on top of the auto-crop (1 = none).
    pub zoom: f32,
    /// Source aspect (width / height) of the analysis.
    pub aspect: f32,
    /// Cumulative camera motion: first analysed frame → frame at `t`.
    pub path: Vec<TrackSample>,
    /// Auto-crop zoom computed by [`Stabilization::refresh`].
    pub crop_scale: f32,
}

impl Default for Stabilization {
    fn default() -> Self {
        Stabilization {
            enabled: true,
            model: MotionModel::Similarity,
            smoothing: 0.8,
            tripod: false,
            auto_crop: true,
            zoom: 1.0,
            aspect: 16.0 / 9.0,
            path: Vec::new(),
            crop_scale: 1.0,
        }
    }
}

/// Similarity parameters about the frame centre in aspect space.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct SimParams {
    dx: f64,
    dy: f64,
    angle: f64,
    log_s: f64,
}

impl Stabilization {
    fn centre(&self) -> [f32; 2] {
        [self.aspect * 0.5, 0.5]
    }

    fn decompose(&self, h: &Mat3) -> SimParams {
        let a = to_aspect(h, self.aspect);
        let c = self.centre();
        let p = apply(&a, c);
        let (s, angle) = local_similarity(h, [0.5, 0.5], self.aspect);
        SimParams { dx: (p[0] - c[0]) as f64, dy: (p[1] - c[1]) as f64, angle: angle as f64, log_s: (s.max(1e-6) as f64).ln() }
    }

    fn compose(&self, p: SimParams) -> Mat3 {
        let (s, angle) = if self.model == MotionModel::Translation { (1.0, 0.0) } else { (p.log_s.exp() as f32, p.angle as f32) };
        from_aspect(&similarity_about(self.centre(), s, angle, [p.dx as f32, p.dy as f32]), self.aspect)
    }

    /// The smoothed camera path at sample `i`.
    fn smooth_at(&self, i: usize) -> Mat3 {
        let path = &self.path;
        if self.tripod || self.smoothing <= 0.0 {
            return path.first().map_or(IDENTITY, |s| s.h);
        }
        let t = path[i].t;
        let sigma = self.smoothing as f64;
        let reach = sigma * 3.0;
        let lo = path.partition_point(|s| s.t < t - reach);
        let hi = path.partition_point(|s| s.t <= t + reach);
        let mut wsum = 0.0;
        if self.model == MotionModel::Perspective {
            let mut acc = [0.0f64; 9];
            for s in &path[lo..hi] {
                let w = (-(s.t - t).powi(2) / (2.0 * sigma * sigma)).exp();
                let h = normalize(&s.h);
                for k in 0..9 {
                    acc[k] += w * h[k] as f64;
                }
                wsum += w;
            }
            return acc.map(|v| (v / wsum.max(1e-12)) as f32);
        }
        // Similarity / translation: average the decomposed parameters,
        // unwrapping angles around the centre sample.
        let base = self.decompose(&path[i].h);
        let mut acc = SimParams::default();
        for s in &path[lo..hi] {
            let w = (-(s.t - t).powi(2) / (2.0 * sigma * sigma)).exp();
            let mut p = self.decompose(&s.h);
            p.angle = base.angle + (p.angle - base.angle + std::f64::consts::PI).rem_euclid(std::f64::consts::TAU) - std::f64::consts::PI;
            acc.dx += w * p.dx;
            acc.dy += w * p.dy;
            acc.angle += w * p.angle;
            acc.log_s += w * p.log_s;
            wsum += w;
        }
        let w = wsum.max(1e-12);
        self.compose(SimParams { dx: acc.dx / w, dy: acc.dy / w, angle: acc.angle / w, log_s: acc.log_s / w })
    }

    /// The real path restricted to the stabilizer's model.
    fn real_at(&self, i: usize) -> Mat3 {
        let h = self.path[i].h;
        match self.model {
            MotionModel::Perspective => normalize(&h),
            _ => self.compose(self.decompose(&h)),
        }
    }

    /// Sampling matrix without the crop zoom: output position → source
    /// position at sample `i` (`P · S⁻¹`).
    fn warp_at(&self, i: usize) -> Mat3 {
        let s_inv = invert(&self.smooth_at(i)).unwrap_or(IDENTITY);
        mul(&self.real_at(i), &s_inv)
    }

    fn zoom_matrix(k: f32) -> Mat3 {
        // output → source: shrink about the centre by 1/k
        mul(&translation(0.5, 0.5), &mul(&scaling(1.0 / k, 1.0 / k), &translation(-0.5, -0.5)))
    }

    /// Smallest zoom ≥ 1 that keeps every warped frame covering the output.
    pub fn refresh(&mut self) {
        let warps: Vec<Mat3> = (0..self.path.len()).map(|i| self.warp_at(i)).collect();
        let covers = |k: f32| {
            let z = Self::zoom_matrix(k);
            warps.iter().all(|w| {
                let m = mul(w, &z);
                // sample the output border densely enough for perspective bowing
                (0..=8).all(|j| {
                    let f = j as f32 / 8.0;
                    [[f, 0.0], [f, 1.0], [0.0, f], [1.0, f]].iter().all(|p| {
                        let q = apply(&m, *p);
                        (-1e-4..=1.0 + 1e-4).contains(&q[0]) && (-1e-4..=1.0 + 1e-4).contains(&q[1])
                    })
                })
            })
        };
        let (mut lo, mut hi) = (1.0f32, 4.0f32);
        if covers(lo) {
            self.crop_scale = 1.0;
            return;
        }
        if !covers(hi) {
            self.crop_scale = hi;
            return;
        }
        for _ in 0..30 {
            let mid = 0.5 * (lo + hi);
            if covers(mid) {
                hi = mid;
            } else {
                lo = mid;
            }
        }
        self.crop_scale = hi;
    }

    /// Output-uv → source-uv sampling matrix at source time `t` (seconds),
    /// or `None` when there is nothing to do.
    pub fn correction(&self, t: f64) -> Option<Mat3> {
        if !self.enabled || self.path.len() < 2 {
            return None;
        }
        let i = self.path.partition_point(|s| s.t <= t);
        let w = if i == 0 {
            self.warp_at(0)
        } else if i >= self.path.len() {
            self.warp_at(self.path.len() - 1)
        } else {
            let (a, b) = (&self.path[i - 1], &self.path[i]);
            let u = ((t - a.t) / (b.t - a.t).max(1e-9)) as f32;
            let (wa, wb) = (normalize(&self.warp_at(i - 1)), normalize(&self.warp_at(i)));
            let mut o = [0.0; 9];
            for k in 0..9 {
                o[k] = wa[k] + (wb[k] - wa[k]) * u;
            }
            o
        };
        let k = if self.auto_crop { self.crop_scale.max(1.0) } else { 1.0 } * self.zoom.max(0.1);
        Some(mul(&w, &Self::zoom_matrix(k)))
    }

    /// Append frame-to-frame motions (`frame k → frame k+1`) measured at
    /// times `ts` (`ts.len() == deltas.len() + 1`) as the cumulative path.
    pub fn set_path_from_deltas(&mut self, ts: &[f64], deltas: &[Mat3]) {
        self.path.clear();
        let mut acc = IDENTITY;
        if let Some(&t0) = ts.first() {
            self.path.push(TrackSample { t: t0, h: acc });
        }
        for (d, &t) in deltas.iter().zip(ts.iter().skip(1)) {
            acc = normalize(&mul(d, &acc));
            self.path.push(TrackSample { t, h: acc });
        }
        self.refresh();
    }
}

// ---------------------------------------------------------------------------
// Scene cuts
// ---------------------------------------------------------------------------

/// Pick hard cuts from per-frame-pair change scores `(t_next, score)`: a
/// cut lies before `t_next` when its score stands out from the local median
/// by `ratio` and exceeds `floor`. `sensitivity` (0..1) lowers both.
pub fn detect_cuts(scores: &[(f64, f32)], sensitivity: f32, min_gap: f64) -> Vec<f64> {
    let s = sensitivity.clamp(0.0, 1.0);
    let floor = 0.30 - 0.22 * s;
    let ratio = 4.0 - 2.5 * s;
    let mut out: Vec<f64> = Vec::new();
    for i in 0..scores.len() {
        let (t, v) = scores[i];
        let lo = i.saturating_sub(8);
        let hi = (i + 9).min(scores.len());
        let mut neigh: Vec<f32> = (lo..hi).filter(|&j| j != i).map(|j| scores[j].1).collect();
        neigh.sort_by(f32::total_cmp);
        let median = neigh.get(neigh.len() / 2).copied().unwrap_or(0.0);
        // a local maximum only (a dissolve spreads; a cut spikes once)
        let is_peak = (lo..hi).all(|j| j == i || scores[j].1 <= v);
        if v >= floor && v >= ratio * median.max(0.01) && is_peak && out.last().is_none_or(|&p| t - p >= min_gap) {
            out.push(t);
        }
    }
    out
}

/// Change score between two luma images (same size, `0..1`), given the
/// forward flow `a → b` (pixels, same grid). Mixes the motion-compensated
/// residual (low for any motion the flow explains) with a histogram distance
/// (high when the content changes), so fast pans do not read as cuts.
pub fn change_score(a: &[f32], b: &[f32], flow: &[[f32; 2]], w: usize, h: usize) -> f32 {
    if a.len() < w * h || b.len() < w * h || w < 4 || h < 4 {
        return 0.0;
    }
    let bilinear = |img: &[f32], x: f32, y: f32| -> Option<f32> {
        if x < 0.0 || y < 0.0 || x > (w - 1) as f32 || y > (h - 1) as f32 {
            return None;
        }
        let (x0, y0) = (x.floor() as usize, y.floor() as usize);
        let (x1, y1) = ((x0 + 1).min(w - 1), (y0 + 1).min(h - 1));
        let (fx, fy) = (x - x0 as f32, y - y0 as f32);
        let top = img[y0 * w + x0] * (1.0 - fx) + img[y0 * w + x1] * fx;
        let bot = img[y1 * w + x0] * (1.0 - fx) + img[y1 * w + x1] * fx;
        Some(top * (1.0 - fy) + bot * fy)
    };
    let (mut err, mut n) = (0.0f64, 0usize);
    for y in 0..h {
        for x in 0..w {
            let v = flow.get(y * w + x).copied().unwrap_or([0.0; 2]);
            if let Some(bv) = bilinear(b, x as f32 + v[0], y as f32 + v[1]) {
                err += (a[y * w + x] - bv).abs() as f64;
                n += 1;
            }
        }
    }
    let residual = if n * 4 >= w * h { (err / n as f64) as f32 } else { 1.0 };
    let hist = |img: &[f32]| {
        let mut hh = [0.0f32; 32];
        for &v in &img[..w * h] {
            hh[((v.clamp(0.0, 1.0) * 31.999) as usize).min(31)] += 1.0;
        }
        let total = (w * h) as f32;
        hh.map(|c| c / total)
    };
    let (ha, hb) = (hist(a), hist(b));
    let hdist = 0.5 * ha.iter().zip(&hb).map(|(x, y)| (x - y).abs()).sum::<f32>();
    // residual of ~0.1 luma or a half-different histogram both read as a cut
    (residual / 0.10).min(1.5) * 0.5 + hdist.min(1.0) * 0.5
}

/// Timeline times at which to blade `clip` (placed at `start`) for source
/// cut times `cuts` (seconds), latest first so one clip id can be bladed
/// repeatedly (the left piece keeps the id).
pub fn cut_times_on_timeline(clip: &Clip, start: crate::Time, cuts: &[f64]) -> Vec<crate::Time> {
    let fd = 1e-3;
    let mut out: Vec<crate::Time> = Vec::new();
    // invert source_time by scanning: clips can be retimed
    let n = (clip.duration.as_secs_f64() / 0.004).ceil().max(1.0) as usize;
    let step = clip.duration.as_secs_f64() / n as f64;
    let mut prev = source_secs(clip, crate::Time::ZERO);
    for i in 1..=n {
        let local = crate::Time::from_secs_f64(step * i as f64);
        let cur = source_secs(clip, local);
        for &c in cuts {
            if (prev < c && c <= cur) || (cur <= c && c < prev) {
                let t = local - crate::Time::from_secs_f64(step * ((cur - c) / (cur - prev)).clamp(0.0, 1.0));
                if t.as_secs_f64() > fd && t.as_secs_f64() < clip.duration.as_secs_f64() - fd {
                    out.push(start + t);
                }
            }
        }
        prev = cur;
    }
    out.sort();
    out.dedup();
    out.reverse();
    out
}

/// One undoable command that blades `clip` at every detected scene cut.
/// `cuts` are source seconds (as returned by the analysis, a fraction into
/// the first frame after the cut); `media_fps` snaps them back to that
/// frame's start and the blade points onto the timeline's frame grid.
pub fn scene_cut_command(timeline: &crate::Timeline, clip: crate::ClipId, cuts: &[f64], media_fps: f64) -> Option<crate::Command> {
    let c = timeline.clip(clip)?;
    let start = timeline.clip_start(clip)?;
    let fps = media_fps.max(1.0);
    let snapped: Vec<f64> = cuts.iter().map(|&t| (t * fps + 1e-6).floor() / fps).collect();
    let rate = timeline.format.rate;
    let mut at: Vec<crate::Time> = cut_times_on_timeline(c, start, &snapped)
        .into_iter()
        .map(|t| crate::Time::from_frames(t.frames_round(rate), rate))
        .filter(|&t| t > start && t < start + c.duration)
        .collect();
    at.sort();
    at.dedup();
    at.reverse();
    if at.is_empty() {
        return None;
    }
    Some(crate::Command::Batch { label: "Blade at Scene Cuts", commands: at.into_iter().map(|at| crate::Command::BladeClip { id: clip, at }).collect() })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: &Mat3, b: &Mat3, eps: f32) -> bool {
        let (a, b) = (normalize(a), normalize(b));
        a.iter().zip(&b).all(|(x, y)| (x - y).abs() <= eps)
    }

    fn grid() -> Vec<[f32; 2]> {
        let mut v = Vec::new();
        for y in 0..7 {
            for x in 0..9 {
                v.push([0.1 + x as f32 * 0.1, 0.15 + y as f32 * 0.11]);
            }
        }
        v
    }

    #[test]
    fn matrix_inverse_and_aspect_round_trip() {
        let m = similarity_about([0.3, 0.6], 1.2, 0.3, [0.05, -0.02]);
        let i = invert(&m).unwrap();
        assert!(close(&mul(&m, &i), &IDENTITY, 1e-5));
        let a = from_aspect(&to_aspect(&m, 1.777), 1.777);
        assert!(close(&a, &m, 1e-5));
        let (s, r) = local_similarity(&from_aspect(&m, 1.777), [0.5, 0.5], 1.777);
        assert!((s - 1.2).abs() < 1e-3 && (r - 0.3).abs() < 1e-3, "{s} {r}");
    }

    #[test]
    fn fits_recover_exact_models() {
        let t = translation(0.03, -0.01);
        let s = similarity_about([0.5, 0.5], 0.9, -0.2, [0.02, 0.01]);
        let h = [1.05, 0.04, -0.02, -0.03, 0.97, 0.03, 0.08, -0.05, 1.0];
        for (model, m) in [(MotionModel::Translation, t), (MotionModel::Similarity, s), (MotionModel::Perspective, h)] {
            let pairs: Vec<Pair> = grid().into_iter().map(|p| (p, apply(&m, p))).collect();
            let f = fit(model, &pairs).unwrap();
            assert!(close(&f, &m, 1e-4), "{model:?}: {f:?} vs {m:?}");
        }
    }

    #[test]
    fn ransac_ignores_a_moving_subject() {
        let cam = similarity_about([0.5, 0.5], 1.01, 0.02, [0.01, 0.0]);
        let mut pairs: Vec<Pair> = grid().into_iter().map(|p| (p, apply(&cam, p))).collect();
        // 30 % of the points sit on a subject moving the other way
        let n = pairs.len() * 3 / 10;
        for p in pairs.iter_mut().take(n) {
            p.1 = [p.0[0] - 0.08, p.0[1] + 0.05];
        }
        let (m, ratio) = ransac(MotionModel::Similarity, &pairs, 0.004, 200).unwrap();
        assert!(close(&m, &cam, 2e-3), "{m:?}");
        assert!((ratio - 0.7).abs() < 0.05, "{ratio}");
        let (h, _) = ransac(MotionModel::Perspective, &pairs, 0.004, 300).unwrap();
        assert!(close(&h, &cam, 5e-3), "{h:?}");
    }

    #[test]
    fn track_interpolates_and_moves_masks() {
        use crate::color::MaskShape;
        let mut tr = Track::new("Face", MotionModel::Similarity, [0.4, 0.4, 0.2, 0.2], 1.0, 1.0);
        tr.insert(TrackSample { t: 2.0, h: similarity_about([0.5, 0.5], 2.0, std::f32::consts::FRAC_PI_2, [0.1, 0.0]) });
        assert_eq!(tr.at(0.0), IDENTITY, "holds before the first sample");
        let mid = tr.point_at(1.5);
        assert!((mid[0] - 0.55).abs() < 1e-4 && (mid[1] - 0.5).abs() < 1e-4, "{mid:?}");
        let mask = ShapeMask { center: [0.5, 0.5], size: [0.1, 0.05], ..ShapeMask::new(MaskShape::Rectangle) };
        let m = tr.move_mask(&mask, 2.0);
        assert!((m.center[0] - 0.6).abs() < 1e-4);
        assert!((m.size[0] - 0.2).abs() < 1e-3 && (m.rotation - 90.0).abs() < 0.1, "{m:?}");
        let poly = ShapeMask::new(MaskShape::Polygon);
        let pm = tr.move_mask(&poly, 2.0);
        assert_eq!(pm.points[0], apply(&tr.at(2.0), poly.points[0]));
    }

    #[test]
    fn stabilizer_cancels_shake_and_crops_just_enough() {
        // a slow pan with ±1 % jitter
        let ts: Vec<f64> = (0..120).map(|i| i as f64 / 30.0).collect();
        let jitter = |i: usize| if i.is_multiple_of(2) { 0.01 } else { -0.01 };
        let mut real = Vec::new();
        for i in 0..ts.len() {
            real.push(translation(0.002 * i as f32 + jitter(i), 0.5 * jitter(i)));
        }
        let deltas: Vec<Mat3> = (1..real.len()).map(|i| mul(&real[i], &invert(&real[i - 1]).unwrap())).collect();
        let mut st = Stabilization { model: MotionModel::Translation, smoothing: 0.3, ..Default::default() };
        st.set_path_from_deltas(&ts, &deltas);
        assert!(st.crop_scale > 1.0 && st.crop_scale < 1.2, "crop {}", st.crop_scale);
        // stabilized motion of the frame centre is smooth: the jitter is gone
        let centre = |i: usize| {
            let w = st.correction(ts[i]).unwrap();
            // where output centre samples the source, mapped back to world (frame 0)
            let src = apply(&w, [0.5, 0.5]);
            apply(&invert(&real[i]).unwrap(), src)
        };
        for i in 30..90 {
            let (a, b, c) = (centre(i - 1), centre(i), centre(i + 1));
            let accel = ((a[0] - 2.0 * b[0] + c[0]).powi(2) + (a[1] - 2.0 * b[1] + c[1]).powi(2)).sqrt();
            assert!(accel < 2e-3, "frame {i}: {accel}");
        }
        // every output corner stays inside the source frame
        for &t in &ts {
            let w = st.correction(t).unwrap();
            for p in [[0.0, 0.0], [1.0, 0.0], [0.0, 1.0], [1.0, 1.0]] {
                let q = apply(&w, p);
                assert!((-1e-3..=1.001).contains(&q[0]) && (-1e-3..=1.001).contains(&q[1]), "{t}: {q:?}");
            }
        }
        // tripod mode locks the world completely
        st.tripod = true;
        st.refresh();
        let w = st.correction(ts[50]).unwrap();
        let world = apply(&invert(&real[50]).unwrap(), apply(&w, [0.5, 0.5]));
        let w0 = st.correction(ts[0]).unwrap();
        let world0 = apply(&invert(&real[0]).unwrap(), apply(&w0, [0.5, 0.5]));
        assert!((world[0] - world0[0]).abs() < 1e-4 && (world[1] - world0[1]).abs() < 1e-4);
    }

    #[test]
    fn similarity_stabilizer_removes_roll() {
        let ts: Vec<f64> = (0..60).map(|i| i as f64 / 30.0).collect();
        let real: Vec<Mat3> = (0..60).map(|i| from_aspect(&similarity_about([0.888, 0.5], 1.0, if i % 2 == 0 { 0.02 } else { -0.02 }, [0.0, 0.0]), 1.777)).collect();
        let deltas: Vec<Mat3> = (1..real.len()).map(|i| mul(&real[i], &invert(&real[i - 1]).unwrap())).collect();
        let mut st = Stabilization { aspect: 1.777, smoothing: 0.5, ..Default::default() };
        st.set_path_from_deltas(&ts, &deltas);
        for i in 10..50 {
            let w = st.correction(ts[i]).unwrap();
            let world = mul(&invert(&real[i]).unwrap(), &w);
            let (_, roll) = local_similarity(&world, [0.5, 0.5], 1.777);
            assert!(roll.abs() < 2e-3, "frame {i}: roll {roll}");
        }
    }

    #[test]
    fn cuts_spike_out_of_noise_and_respect_min_gap() {
        let mut scores: Vec<(f64, f32)> = (0..100).map(|i| (i as f64 / 25.0, 0.04 + 0.01 * ((i * 7) % 5) as f32)).collect();
        scores[30].1 = 0.8;
        scores[31].1 = 0.5; // flash right after the cut
        scores[70].1 = 0.6;
        // a slow dissolve: broad, low hump
        for (k, i) in (45..55).enumerate() {
            scores[i].1 = 0.08 + 0.01 * k.min(9 - k) as f32;
        }
        let cuts = detect_cuts(&scores, 0.5, 0.5);
        assert_eq!(cuts, vec![scores[30].0, scores[70].0]);
    }

    #[test]
    fn change_score_ignores_explained_motion() {
        let (w, h) = (48, 32);
        let tex = |x: f32, y: f32| ((x * 0.4).sin() * (y * 0.3).cos() * 0.4 + 0.5).clamp(0.0, 1.0);
        let a: Vec<f32> = (0..w * h).map(|i| tex((i % w) as f32, (i / w) as f32)).collect();
        // b = a shifted right by 3 px
        let b: Vec<f32> = (0..w * h).map(|i| tex((i % w) as f32 - 3.0, (i / w) as f32)).collect();
        let flow = vec![[3.0, 0.0]; w * h];
        let moving = change_score(&a, &b, &flow, w, h);
        let other: Vec<f32> = (0..w * h).map(|i| 1.0 - a[i] * 0.5).collect();
        let cut = change_score(&a, &other, &vec![[0.0; 2]; w * h], w, h);
        assert!(moving < 0.05, "{moving}");
        assert!(cut > 0.3, "{cut}");
    }

    #[test]
    fn cut_times_map_through_source_offset() {
        let a = crate::asset::MediaAsset::new("x", "x.mp4", crate::Time::from_secs_f64(20.0));
        let c = Clip::from_asset_range(&a, crate::Time::from_secs_f64(5.0), crate::Time::from_secs_f64(10.0));
        let ts = cut_times_on_timeline(&c, crate::Time::from_secs_f64(2.0), &[6.0, 9.0, 30.0]);
        let secs: Vec<f64> = ts.iter().map(|t| (t.as_secs_f64() * 100.0).round() / 100.0).collect();
        assert_eq!(secs, vec![6.0, 3.0]);
    }
}
