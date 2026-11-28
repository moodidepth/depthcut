//! Clip-level layer masks and track mattes.
//!
//! A [`VideoMask`] cuts a clip's own alpha (in the clip's source frame,
//! before its transform) so lower tracks show through; several masks combine
//! in order by their [`MaskMode`]. A [`TrackMatte`] takes a clip's alpha from
//! its neighbour layer instead (its alpha, luma or AI subject matte), which
//! gives "text behind the subject" with one setting.

use serde::{Deserialize, Serialize};

use crate::color::{MaskShape, ResolvedMask, ShapeMask};
use crate::keyframe::Param;
use crate::time::Time;

/// Masks per clip (the GPU pass has a fixed array).
pub const MAX_CLIP_MASKS: usize = 8;

/// How a mask combines with the masks above it in the list.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum MaskMode {
    /// Union: adds this shape to the visible area.
    #[default]
    Add,
    /// Cuts this shape out of the visible area.
    Subtract,
    /// Keeps only where this shape overlaps the visible area.
    Intersect,
}

impl MaskMode {
    pub const ALL: [MaskMode; 3] = [MaskMode::Add, MaskMode::Subtract, MaskMode::Intersect];

    pub fn label(self) -> &'static str {
        match self {
            MaskMode::Add => "Add",
            MaskMode::Subtract => "Subtract",
            MaskMode::Intersect => "Intersect",
        }
    }

    /// Index used by the GPU pass.
    pub fn index(self) -> u32 {
        self as u32
    }

    /// `acc` combined with coverage `m`.
    pub fn apply(self, acc: f32, m: f32) -> f32 {
        match self {
            MaskMode::Add => acc + m - acc * m,
            MaskMode::Subtract => acc * (1.0 - m),
            MaskMode::Intersect => acc * m,
        }
    }

    /// Coverage before the first mask: nothing for Add, everything otherwise
    /// (so a lone Subtract mask cuts a hole and a lone Intersect keeps the
    /// shape).
    pub fn start(self) -> f32 {
        if self == MaskMode::Add {
            0.0
        } else {
            1.0
        }
    }
}

fn yes() -> bool {
    true
}
fn one() -> Param {
    Param::new(1.0)
}
fn ones() -> [Param; 2] {
    [one(), one()]
}
fn default_feather() -> Param {
    Param::new(0.02)
}

/// A shape mask on a clip's alpha with a keyframable transform.
///
/// The base geometry (and its invert, roto shape keys and tracking) lives in
/// `shape`; `shape.feather` is unused, `feather` is.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct VideoMask {
    #[serde(default = "yes")]
    pub enabled: bool,
    #[serde(default)]
    pub mode: MaskMode,
    #[serde(default)]
    pub shape: ShapeMask,
    /// Moves the shape (fractions of the frame width / height).
    #[serde(default)]
    pub offset: [Param; 2],
    /// Scales the shape about its centre.
    #[serde(default = "ones")]
    pub scale: [Param; 2],
    /// Degrees clockwise, about the shape's centre.
    #[serde(default)]
    pub rotation: Param,
    /// Soft edge width in frame heights.
    #[serde(default = "default_feather")]
    pub feather: Param,
    /// Grows (positive) or shrinks the shape, in frame heights.
    #[serde(default)]
    pub expand: Param,
    #[serde(default = "one")]
    pub opacity: Param,
}

impl Default for VideoMask {
    fn default() -> Self {
        VideoMask {
            enabled: true,
            mode: MaskMode::Add,
            shape: ShapeMask::default(),
            offset: Default::default(),
            scale: ones(),
            rotation: Param::default(),
            feather: default_feather(),
            expand: Param::default(),
            opacity: one(),
        }
    }
}

/// Names of a mask's animatable params, in [`VideoMask::params`] order.
pub const MASK_PARAM_NAMES: [&str; 8] = ["Offset X", "Offset Y", "Scale X", "Scale Y", "Rotation", "Feather", "Expand", "Opacity"];

impl VideoMask {
    pub fn new(shape: MaskShape) -> Self {
        let mut m = VideoMask::default();
        m.shape.shape = shape;
        if shape == MaskShape::Polygon {
            m.shape.points = vec![[0.3, 0.3], [0.7, 0.3], [0.7, 0.7], [0.3, 0.7]];
        }
        m
    }

    pub fn params(&self) -> [&Param; 8] {
        [&self.offset[0], &self.offset[1], &self.scale[0], &self.scale[1], &self.rotation, &self.feather, &self.expand, &self.opacity]
    }

    pub fn params_mut(&mut self) -> [&mut Param; 8] {
        let [ox, oy] = &mut self.offset;
        let [sx, sy] = &mut self.scale;
        [ox, oy, sx, sy, &mut self.rotation, &mut self.feather, &mut self.expand, &mut self.opacity]
    }

    /// The mask at clip-local `t` (roto keys applied, no tracking).
    pub fn resolve(&self, t: Time) -> ResolvedClipMask {
        self.resolve_shape(t, &self.shape.at(t))
    }

    /// The mask at `t` with `shape` as its (posed, tracked) base geometry.
    pub fn resolve_shape(&self, t: Time, shape: &ShapeMask) -> ResolvedClipMask {
        let mut mask = shape.resolve();
        mask.feather = self.feather.at_f32(t).max(0.0);
        mask.expand = self.expand.at_f32(t);
        ResolvedClipMask {
            mask,
            mode: self.mode,
            opacity: self.opacity.at_f32(t).clamp(0.0, 1.0),
            offset: [self.offset[0].at_f32(t), self.offset[1].at_f32(t)],
            scale: [self.scale[0].at_f32(t), self.scale[1].at_f32(t)],
            rotation: self.rotation.at_f32(t),
        }
    }

    /// The part before `at` (blade): the shape's roto keys (params are
    /// handled with the clip's other params).
    pub fn keep_left(&mut self, at: Time) {
        self.shape.keep_left(at);
    }

    pub fn keep_right(&mut self, at: Time) {
        self.shape.keep_right(at);
    }

    pub fn shift_keys(&mut self, dt: Time) {
        self.shape.shift_keys(dt);
    }
}

/// A clip mask at one instant: base shape plus its transform. The final
/// geometry depends on the layer's aspect ([`Self::for_aspect`]).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ResolvedClipMask {
    pub mask: ResolvedMask,
    pub mode: MaskMode,
    pub opacity: f32,
    pub offset: [f32; 2],
    pub scale: [f32; 2],
    pub rotation: f32,
}

/// The centre a polygon is scaled and rotated about (its bounding box's).
pub fn polygon_pivot(points: &[[f32; 2]]) -> [f32; 2] {
    if points.is_empty() {
        return [0.5, 0.5];
    }
    let (mut lo, mut hi) = ([f32::MAX; 2], [f32::MIN; 2]);
    for p in points {
        for k in 0..2 {
            lo[k] = lo[k].min(p[k]);
            hi[k] = hi[k].max(p[k]);
        }
    }
    [(lo[0] + hi[0]) * 0.5, (lo[1] + hi[1]) * 0.5]
}

/// Scale `p` by `scale` and rotate it `rotation` degrees clockwise about
/// `pivot`, then move it by `offset` (frame coordinates on an `aspect`
/// frame; scaling and rotation in height units so shapes keep proportions).
pub fn transform_point(p: [f32; 2], pivot: [f32; 2], offset: [f32; 2], scale: [f32; 2], rotation: f32, aspect: f32) -> [f32; 2] {
    let a = aspect.max(1e-4);
    let d = [(p[0] - pivot[0]) * a * scale[0], (p[1] - pivot[1]) * scale[1]];
    let (s, c) = rotation.to_radians().sin_cos();
    let r = [d[0] * c - d[1] * s, d[0] * s + d[1] * c];
    [pivot[0] + offset[0] + r[0] / a, pivot[1] + offset[1] + r[1]]
}

/// Inverse of [`transform_point`] (for dragging polygon points on screen).
pub fn untransform_point(q: [f32; 2], pivot: [f32; 2], offset: [f32; 2], scale: [f32; 2], rotation: f32, aspect: f32) -> [f32; 2] {
    let a = aspect.max(1e-4);
    let r = [(q[0] - pivot[0] - offset[0]) * a, q[1] - pivot[1] - offset[1]];
    let (s, c) = (-rotation).to_radians().sin_cos();
    let d = [r[0] * c - r[1] * s, r[0] * s + r[1] * c];
    let sc = |v: f32| if v.abs() < 1e-4 { 1e-4f32.copysign(v) } else { v };
    [pivot[0] + d[0] / (a * sc(scale[0])), pivot[1] + d[1] / sc(scale[1])]
}

impl ResolvedClipMask {
    /// The transformed shape on a layer of `aspect` (w / h).
    pub fn for_aspect(&self, aspect: f32) -> ResolvedMask {
        let mut m = self.mask;
        match m.shape {
            MaskShape::Ellipse | MaskShape::Rectangle => {
                m.center = [m.center[0] + self.offset[0], m.center[1] + self.offset[1]];
                m.size = [m.size[0] * self.scale[0].abs(), m.size[1] * self.scale[1].abs()];
                m.rotation += self.rotation;
            }
            MaskShape::Polygon => {
                let n = (m.count as usize).min(m.points.len());
                let pivot = polygon_pivot(&m.points[..n]);
                for p in &mut m.points[..n] {
                    *p = transform_point(*p, pivot, self.offset, self.scale, self.rotation, aspect);
                }
            }
        }
        m
    }

    /// This mask's own coverage × opacity at `uv`.
    pub fn coverage(&self, uv: [f32; 2], aspect: f32) -> f32 {
        self.for_aspect(aspect).coverage(uv, aspect) * self.opacity
    }
}

/// Combined coverage of a clip's masks at `uv` (CPU mirror of
/// `clip_mask.wgsl`). No masks → fully visible.
pub fn combine(masks: &[ResolvedClipMask], uv: [f32; 2], aspect: f32) -> f32 {
    let Some(first) = masks.first() else { return 1.0 };
    masks.iter().fold(first.mode.start(), |acc, m| m.mode.apply(acc, m.coverage(uv, aspect)))
}

/// Which neighbour a [`TrackMatte`] reads.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum MatteSource {
    /// The layer directly beneath; it stays visible (text behind the
    /// subject of the footage it sits on).
    #[default]
    Below,
    /// The layer directly above; it is used up and not drawn (classic
    /// After Effects track matte).
    Above,
}

impl MatteSource {
    pub const ALL: [MatteSource; 2] = [MatteSource::Below, MatteSource::Above];

    pub fn label(self) -> &'static str {
        match self {
            MatteSource::Below => "Layer Below",
            MatteSource::Above => "Layer Above",
        }
    }
}

/// What part of the matte layer becomes this clip's alpha.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum MatteChannel {
    /// The matte layer's alpha (shape masks, titles, cut-outs).
    #[default]
    Alpha,
    /// Its brightness.
    Luma,
    /// Its AI person matte (needs "Generate Person Matte" on that clip's
    /// media; no matte means no subject).
    Subject,
}

impl MatteChannel {
    pub const ALL: [MatteChannel; 3] = [MatteChannel::Alpha, MatteChannel::Luma, MatteChannel::Subject];

    pub fn label(self) -> &'static str {
        match self {
            MatteChannel::Alpha => "Alpha",
            MatteChannel::Luma => "Luma",
            MatteChannel::Subject => "Subject (AI)",
        }
    }

    pub fn index(self) -> u32 {
        self as u32
    }
}

/// Take this clip's alpha from a neighbouring layer.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrackMatte {
    #[serde(default)]
    pub source: MatteSource,
    #[serde(default)]
    pub channel: MatteChannel,
    /// Show the clip where the matte is empty instead (behind the subject).
    #[serde(default)]
    pub invert: bool,
}

impl TrackMatte {
    /// The "behind the subject" preset: subject of the layer below, inverted.
    pub fn behind_subject() -> Self {
        TrackMatte { source: MatteSource::Below, channel: MatteChannel::Subject, invert: true }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ellipse(center: [f32; 2], r: f32) -> VideoMask {
        let mut m = VideoMask::new(MaskShape::Ellipse);
        m.shape.center = center;
        m.shape.size = [r, r];
        m.feather = Param::new(0.0);
        m
    }

    #[test]
    fn old_json_without_new_fields_loads_with_defaults() {
        let m: VideoMask = serde_json::from_str("{}").unwrap();
        assert_eq!(m, VideoMask::default());
        assert!(m.enabled);
        assert_eq!(m.scale[0].value, 1.0);
        assert_eq!(m.opacity.value, 1.0);
        let t: TrackMatte = serde_json::from_str("{}").unwrap();
        assert_eq!(t, TrackMatte::default());
        let back: VideoMask = serde_json::from_str(&serde_json::to_string(&ellipse([0.2, 0.3], 0.1)).unwrap()).unwrap();
        assert_eq!(back, ellipse([0.2, 0.3], 0.1));
    }

    #[test]
    fn coverage_is_full_inside_and_empty_outside() {
        let m = ellipse([0.5, 0.5], 0.2).resolve(Time::ZERO);
        assert_eq!(m.coverage([0.5, 0.5], 1.0), 1.0);
        assert_eq!(m.coverage([0.9, 0.5], 1.0), 0.0);
        assert_eq!(combine(&[], [0.9, 0.5], 1.0), 1.0, "no masks keep everything");
    }

    #[test]
    fn feather_falls_off_monotonically_across_the_edge() {
        let mut v = ellipse([0.5, 0.5], 0.2);
        v.feather = Param::new(0.1);
        let m = v.resolve(Time::ZERO);
        let xs: Vec<f32> = (0..=20).map(|i| m.coverage([0.6 + i as f32 * 0.01, 0.5], 1.0)).collect();
        assert!(xs.windows(2).all(|w| w[1] <= w[0] + 1e-6), "{xs:?}");
        assert!((m.coverage([0.7, 0.5], 1.0) - 0.5).abs() < 0.01, "half at the edge");
        assert!(xs.iter().any(|&a| a > 0.05 && a < 0.95), "a soft ramp");
    }

    #[test]
    fn expand_grows_the_shape_and_invert_flips_it() {
        let mut v = ellipse([0.5, 0.5], 0.2);
        assert_eq!(v.resolve(Time::ZERO).coverage([0.75, 0.5], 1.0), 0.0);
        v.expand = Param::new(0.1);
        assert_eq!(v.resolve(Time::ZERO).coverage([0.75, 0.5], 1.0), 1.0);
        v.shape.invert = true;
        assert_eq!(v.resolve(Time::ZERO).coverage([0.75, 0.5], 1.0), 0.0);
        assert_eq!(v.resolve(Time::ZERO).coverage([0.98, 0.5], 1.0), 1.0);
    }

    #[test]
    fn modes_combine_in_order() {
        let a = ellipse([0.3, 0.5], 0.15).resolve(Time::ZERO);
        let b = ellipse([0.7, 0.5], 0.15).resolve(Time::ZERO);
        let left = [0.3, 0.5];
        let right = [0.7, 0.5];
        assert_eq!(combine(&[a, b], left, 1.0), 1.0);
        assert_eq!(combine(&[a, b], right, 1.0), 1.0);
        let sub = ResolvedClipMask { mode: MaskMode::Subtract, ..a };
        assert_eq!(combine(&[sub], left, 1.0), 0.0, "a lone subtract cuts a hole");
        assert_eq!(combine(&[sub], right, 1.0), 1.0);
        let big = ellipse([0.5, 0.5], 0.45).resolve(Time::ZERO);
        let inter = ResolvedClipMask { mode: MaskMode::Intersect, ..a };
        assert_eq!(combine(&[big, inter], left, 1.0), 1.0);
        assert_eq!(combine(&[big, inter], [0.5, 0.5], 1.0), 0.0);
        let half = ResolvedClipMask { opacity: 0.5, ..a };
        assert_eq!(combine(&[half], left, 1.0), 0.5);
    }

    #[test]
    fn keyframed_transform_moves_ellipse_and_polygon() {
        let mut v = ellipse([0.5, 0.5], 0.1);
        v.offset[0].add_keyframe(Time::ZERO);
        v.offset[0].set(Time::from_secs_f64(1.0), 0.3);
        let r = v.resolve(Time::from_secs_f64(1.0));
        assert_eq!(r.coverage([0.8, 0.5], 1.0), 1.0);
        assert_eq!(r.coverage([0.5, 0.5], 1.0), 0.0);
        assert_eq!(v.resolve(Time::ZERO).coverage([0.5, 0.5], 1.0), 1.0);

        let mut p = VideoMask::new(MaskShape::Polygon);
        p.feather = Param::new(0.0);
        p.scale = [Param::new(0.5), Param::new(0.5)];
        let r = p.resolve(Time::ZERO);
        // the 0.3..0.7 square shrinks about its centre to 0.4..0.6
        assert_eq!(r.coverage([0.5, 0.5], 16.0 / 9.0), 1.0);
        assert_eq!(r.coverage([0.35, 0.5], 16.0 / 9.0), 0.0);
        p.scale = [one(), one()];
        p.rotation = Param::new(45.0);
        let r = p.resolve(Time::ZERO);
        // a diamond on a square frame: its corner reaches 0.5 ± 0.2·√2 on x
        assert_eq!(r.coverage([0.75, 0.5], 1.0), 1.0);
        assert_eq!(r.coverage([0.32, 0.32], 1.0), 0.0);
    }

    #[test]
    fn untransform_inverts_transform() {
        let (piv, off, sc, rot, a) = ([0.4, 0.6], [0.05, -0.1], [1.5, 0.7], 33.0, 16.0 / 9.0);
        let p = [0.21, 0.77];
        let q = transform_point(p, piv, off, sc, rot, a);
        let back = untransform_point(q, piv, off, sc, rot, a);
        assert!((back[0] - p[0]).abs() < 1e-5 && (back[1] - p[1]).abs() < 1e-5, "{back:?}");
    }

    #[test]
    fn clip_masks_and_track_matte_survive_eval_split_shift_and_paste() {
        use crate::attributes::{self, AttributeSet, KeyTiming};
        use crate::clip::{Clip, TitleParams};
        use crate::eval::{evaluate, RenderNode};
        use crate::timeline::Timeline;

        let mut clip = Clip::title(TitleParams::default(), Time::from_secs(4));
        let mut m = VideoMask::new(MaskShape::Rectangle);
        m.offset[0].add_keyframe(Time::ZERO);
        m.offset[0].set(Time::from_secs(2), 0.25);
        m.feather = Param::new(0.05);
        m.expand = Param::new(0.02);
        clip.video.masks.push(m);
        clip.video.track_matte = Some(TrackMatte::behind_subject());

        // round-trip JSON & ensure clip without masks/track_matte loads cleanly
        let json = serde_json::to_string(&clip).unwrap();
        let back: Clip = serde_json::from_str(&json).unwrap();
        assert_eq!(back.video.masks, clip.video.masks);
        assert_eq!(back.video.track_matte, Some(TrackMatte::behind_subject()));

        let plain = Clip::title(TitleParams::default(), Time::from_secs(2));
        let plain_json = serde_json::to_string(&plain).unwrap();
        assert!(!plain_json.contains("masks") && !plain_json.contains("track_matte"));

        // animatable_params includes the mask's 8 params
        assert!(clip.animatable_params().iter().any(|(n, _)| n == "Mask 1: Offset X"));
        assert!(clip.animatable_params().iter().any(|(n, _)| n == "Mask 1: Feather"));

        // split keeps keyframes on each side relative to its start
        let (left, right) = clip.split(Time::from_secs(1)).unwrap();
        assert!((left.video.masks[0].offset[0].at(Time::from_secs(1)) - 0.125).abs() < 1e-6);
        assert!((right.video.masks[0].offset[0].at(Time::ZERO) - 0.125).abs() < 1e-6);
        assert!((right.video.masks[0].offset[0].at(Time::from_secs(1)) - 0.25).abs() < 1e-6);

        // paste compositing carries masks + track_matte (stretching keys)
        let dst = Clip::title(TitleParams::default(), Time::from_secs(8));
        let pasted = attributes::apply(&clip, &dst, &AttributeSet { compositing: true, ..AttributeSet::NONE }, KeyTiming::Stretch);
        assert_eq!(pasted.video.track_matte, Some(TrackMatte::behind_subject()));
        assert!((pasted.video.masks[0].offset[0].at(Time::from_secs(4)) - 0.25).abs() < 1e-6);

        // evaluate resolves masks and track matte onto the layer
        let mut tl = Timeline::default();
        tl.append(clip).unwrap();
        let comp = evaluate(&tl, Time::from_secs(2));
        let RenderNode::Layer(l) = &comp.nodes[0] else { panic!("expected layer") };
        assert_eq!(l.masks.len(), 1);
        assert!((l.masks[0].offset[0] - 0.25).abs() < 1e-6);
        assert_eq!(l.track_matte, Some(TrackMatte::behind_subject()));
    }
}
