//! Motion analysis drivers: GPU dense optical flow + robust CPU fits from
//! `nle_core::motion`. Used for the point / region / planar trackers, the
//! camera stabilizer and scene-cut detection.
//!
//! Every function blocks on GPU readbacks, so callers run them on a worker
//! thread with their own [`Renderer`] (a `GpuContext` clones cheaply).

use crate::renderer::{FlowField, FlowQuality, Renderer};
use nle_core::motion::{self, Mat3, MotionModel, Pair, Stabilization, Track, TrackSample};
use nle_core::Time;
use nle_media::decode::VideoDecoder;

/// Analysis frames are decoded at most this size (flow runs at ≤ 960 wide).
pub const ANALYSIS_MAX: (u32, u32) = (960, 540);

/// Flow settings for analysis: denser than preview, without the Speed Warp
/// full-HD cost.
pub fn analysis_quality() -> FlowQuality {
    FlowQuality { max_width: 960, ..FlowQuality::high() }
}

/// An RGBA8 analysis frame.
#[derive(Clone)]
pub struct Frame {
    pub t: f64,
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

impl Frame {
    pub fn aspect(&self) -> f32 {
        self.width as f32 / self.height.max(1) as f32
    }

    /// Box-filtered luma at `w × h` (0..1).
    pub fn luma(&self, w: u32, h: u32) -> Vec<f32> {
        let (sw, sh) = (self.width.max(1), self.height.max(1));
        let mut out = vec![0.0f32; (w * h) as usize];
        for y in 0..h {
            let (y0, y1) = (y * sh / h, ((y + 1) * sh / h).max(y * sh / h + 1).min(sh));
            for x in 0..w {
                let (x0, x1) = (x * sw / w, ((x + 1) * sw / w).max(x * sw / w + 1).min(sw));
                let (mut acc, mut n) = (0.0f32, 0u32);
                for yy in y0..y1 {
                    for xx in x0..x1 {
                        let i = ((yy * sw + xx) * 4) as usize;
                        if let Some(p) = self.rgba.get(i..i + 3) {
                            acc += (0.2126 * p[0] as f32 + 0.7152 * p[1] as f32 + 0.0722 * p[2] as f32) / 255.0;
                            n += 1;
                        }
                    }
                }
                out[(y * w + x) as usize] = acc / n.max(1) as f32;
            }
        }
        out
    }
}

/// Decode the frame shown at source time `t` (seconds) for analysis.
pub fn decode(dec: &mut dyn VideoDecoder, t: f64) -> Option<Frame> {
    let (w, h, rgba) = dec.thumbnail_at(Time::from_secs_f64(t.max(0.0)), ANALYSIS_MAX.0, ANALYSIS_MAX.1).ok()?;
    Some(Frame { t, width: w, height: h, rgba })
}

/// Source times of every frame between `t0` and `t1` (inclusive), `step`
/// frames apart.
pub fn frame_times(dec: &dyn VideoDecoder, t0: f64, t1: f64, step: u32) -> Vec<f64> {
    let fps = dec.frame_rate().fps().max(1.0);
    let (f0, f1) = ((t0 * fps).round() as i64, (t1 * fps).floor() as i64);
    (f0..=f1).step_by(step.max(1) as usize).map(|f| (f as f64 + 0.25) / fps).collect()
}

/// Flow correspondences inside `region` (`[x, y, w, h]`, normalised; `None`
/// = whole frame), in aspect space (`x · aspect`, `y`). Flat texels are
/// skipped (their flow is unreliable: the aperture problem).
pub fn correspondences(field: &FlowField, luma: &[f32], region: Option<[f32; 4]>, aspect: f32, max_points: usize) -> Vec<Pair> {
    let (fw, fh) = (field.width.max(1), field.height.max(1));
    let r = region.unwrap_or([0.0, 0.0, 1.0, 1.0]);
    let (x0, y0) = ((r[0].max(0.0) * fw as f32) as u32, (r[1].max(0.0) * fh as f32) as u32);
    let (x1, y1) = (((r[0] + r[2]).min(1.0) * fw as f32).ceil() as u32, ((r[1] + r[3]).min(1.0) * fh as f32).ceil() as u32);
    if x1 <= x0 + 1 || y1 <= y0 + 1 {
        return Vec::new();
    }
    let area = ((x1 - x0) * (y1 - y0)) as f32;
    let step = ((area / max_points.max(16) as f32).sqrt().floor() as u32).max(1);
    let g = |x: u32, y: u32| luma.get((y.min(fh - 1) * fw + x.min(fw - 1)) as usize).copied().unwrap_or(0.0);
    let mut pts = Vec::new();
    let mut y = y0 + step / 2;
    while y < y1 {
        let mut x = x0 + step / 2;
        while x < x1 {
            let gx = g(x + 1, y) - g(x.saturating_sub(1), y);
            let gy = g(x, y + 1) - g(x, y.saturating_sub(1));
            if gx * gx + gy * gy > 4e-4 {
                let v = field.at(x, y);
                let p = [(x as f32 + 0.5) / fw as f32, (y as f32 + 0.5) / fh as f32];
                let q = [p[0] + v[0] / fw as f32, p[1] + v[1] / fh as f32];
                pts.push(([p[0] * aspect, p[1]], [q[0] * aspect, q[1]]));
            }
            x += step;
        }
        y += step;
    }
    pts
}

/// Motion `a → b` of the content inside `region` (normalised coordinates).
/// Returns the transform and the RANSAC inlier ratio.
pub fn estimate(r: &mut Renderer, a: &Frame, b: &Frame, region: Option<[f32; 4]>, model: MotionModel) -> Option<(Mat3, f32)> {
    let field = r.flow_field(&a.rgba, &b.rgba, a.width, a.height, analysis_quality());
    if field.data.is_empty() {
        return None;
    }
    let luma = a.luma(field.width, field.height);
    let aspect = a.aspect();
    let pairs = correspondences(&field, &luma, region, aspect, 1500);
    let min_pts = model.minimal() * 3;
    if pairs.len() < min_pts {
        return None;
    }
    // ~1.5 flow pixels of tolerance
    let thresh = 1.5 / field.height as f32;
    let (m, ratio) = motion::ransac(model, &pairs, thresh, 200)?;
    Some((motion::from_aspect(&m, aspect), ratio))
}

/// Bounding box of four points, clamped to the frame.
fn bbox(c: &[[f32; 2]; 4]) -> [f32; 4] {
    let (mut x0, mut y0, mut x1, mut y1) = (f32::MAX, f32::MAX, f32::MIN, f32::MIN);
    for p in c {
        x0 = x0.min(p[0]);
        y0 = y0.min(p[1]);
        x1 = x1.max(p[0]);
        y1 = y1.max(p[1]);
    }
    let (x0, y0) = (x0.clamp(0.0, 1.0), y0.clamp(0.0, 1.0));
    [x0, y0, (x1.clamp(0.0, 1.0) - x0).max(0.0), (y1.clamp(0.0, 1.0) - y0).max(0.0)]
}

/// Why a tracking run ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TrackEnd {
    /// Reached the requested end time.
    Done,
    /// The region lost its texture or left the frame.
    Lost,
    /// No more frames could be decoded.
    NoFrames,
    /// `progress` asked to stop.
    Cancelled,
}

/// Track `track` from source time `from` to `to` (either direction),
/// appending a sample per frame. The track must already have a sample at or
/// near `from` (its reference frame has the identity). `progress(done, total)`
/// returns `false` to cancel.
pub fn track(r: &mut Renderer, dec: &mut dyn VideoDecoder, track: &mut Track, from: f64, to: f64, mut progress: impl FnMut(usize, usize) -> bool) -> TrackEnd {
    let mut times = frame_times(dec, from.min(to), from.max(to), 1);
    if to < from {
        times.reverse();
    }
    let total = times.len();
    let Some(mut prev) = times.first().and_then(|&t| decode(dec, t)) else { return TrackEnd::NoFrames };
    let mut h = track.at(prev.t);
    for (i, &t) in times.iter().enumerate().skip(1) {
        if !progress(i, total) {
            return TrackEnd::Cancelled;
        }
        let Some(next) = decode(dec, t) else { return TrackEnd::NoFrames };
        let region = bbox(&track.corners_at(prev.t));
        if region[2] < 0.005 || region[3] < 0.005 {
            return TrackEnd::Lost;
        }
        let Some((d, ratio)) = estimate(r, &prev, &next, Some(region), track.model) else { return TrackEnd::Lost };
        if ratio < 0.25 {
            return TrackEnd::Lost;
        }
        h = motion::normalize(&motion::mul(&d, &h));
        track.insert(TrackSample { t: next.t, h });
        prev = next;
    }
    TrackEnd::Done
}

/// Per-frame camera motion and scene-change scores over a source range.
#[derive(Clone, Debug, Default)]
pub struct Analysis {
    /// Source times of the analysed frames.
    pub times: Vec<f64>,
    /// `deltas[i]`: camera motion from `times[i]` to `times[i + 1]`.
    pub deltas: Vec<Mat3>,
    /// `(times[i + 1], change score)`.
    pub scores: Vec<(f64, f32)>,
    pub aspect: f32,
}

/// Analyse `[t0, t1]` frame by frame: dominant (camera) motion with `model`
/// and scene-change scores. Frames whose motion cannot be estimated (cuts,
/// flat frames) get the identity.
pub fn analyze(r: &mut Renderer, dec: &mut dyn VideoDecoder, t0: f64, t1: f64, model: MotionModel, mut progress: impl FnMut(usize, usize) -> bool) -> Option<Analysis> {
    let times = frame_times(dec, t0, t1, 1);
    let total = times.len();
    let mut out = Analysis::default();
    let mut prev = decode(dec, *times.first()?)?;
    out.aspect = prev.aspect();
    out.times.push(prev.t);
    for (i, &t) in times.iter().enumerate().skip(1) {
        if !progress(i, total) {
            return None;
        }
        let Some(next) = decode(dec, t) else { break };
        let field = r.flow_field(&prev.rgba, &next.rgba, prev.width, prev.height, analysis_quality());
        let (fw, fh) = (field.width, field.height);
        let la = prev.luma(fw, fh);
        let lb = next.luma(fw, fh);
        let score = motion::change_score(&la, &lb, &field.data, fw as usize, fh as usize);
        let pairs = correspondences(&field, &la, None, out.aspect, 2000);
        let d = (score < 0.5)
            .then(|| motion::ransac(model, &pairs, 1.5 / fh.max(1) as f32, 200))
            .flatten()
            .filter(|(_, ratio)| *ratio > 0.3)
            .map_or(motion::IDENTITY, |(m, _)| motion::from_aspect(&m, out.aspect));
        out.deltas.push(d);
        out.scores.push((next.t, score));
        out.times.push(next.t);
        prev = next;
    }
    Some(out)
}

/// A stabilization built from an analysis (keeps `settings`' smoothing,
/// mode and crop options).
pub fn stabilization_from(a: &Analysis, settings: &Stabilization) -> Stabilization {
    let mut s = Stabilization { aspect: a.aspect, ..settings.clone() };
    s.set_path_from_deltas(&a.times, &a.deltas);
    s
}

/// Scene-cut times (source seconds) in an analysis: a cut lies just before
/// each returned frame time. `sensitivity` 0..1.
pub fn detect(a: &Analysis, sensitivity: f32) -> Vec<f64> {
    motion::detect_cuts(&a.scores, sensitivity, 0.3)
}
