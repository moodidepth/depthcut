//! Timeline evaluation: resolve what should be on screen (and in the
//! speakers) at a given time into a flat, renderer-friendly description.

use crate::clip::{AudioParams, BlendMode, Clip, ClipKind, Direction, FadeShape, LutRef, TitleParams, TransitionKind};
use crate::effects::{EffectKind, ResolvedEffect};
use crate::ids::{AssetId, ClipId};
use crate::speed::{FrameBlendMode, Retime};
use crate::text_anim::{title_anim, TitleAnim};
use crate::time::Time;
use crate::timeline::Timeline;

#[derive(Clone, Debug, PartialEq)]
pub struct ResolvedVideo {
    pub position: [f32; 2],
    pub scale: [f32; 2],
    pub rotation: f32,
    pub anchor: [f32; 2],
    pub opacity: f32,
    /// left, right, top, bottom (fractions of the source frame)
    pub crop: [f32; 4],
    /// Mirrored horizontally / vertically inside the cropped box.
    pub flip: [bool; 2],
    /// Ken Burns source rectangle `[x, y, w, h]` (full frame when unused).
    pub source_rect: [f32; 4],
    pub blend: BlendMode,
    /// Stabilization warp: output uv -> source uv (applied before grading).
    pub stabilize: Option<crate::motion::Mat3>,
}

impl Default for ResolvedVideo {
    fn default() -> Self {
        ResolvedVideo {
            position: [0.0, 0.0],
            scale: [1.0, 1.0],
            rotation: 0.0,
            anchor: [0.0, 0.0],
            opacity: 1.0,
            crop: [0.0; 4],
            flip: [false; 2],
            source_rect: [0.0, 0.0, 1.0, 1.0],
            blend: BlendMode::Normal,
            stabilize: None,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct ResolvedColor {
    pub enabled: bool,
    pub exposure: f32,
    pub brightness: f32,
    pub contrast: f32,
    pub saturation: f32,
    pub temperature: f32,
    pub tint: f32,
    pub lift: [f32; 4],
    pub gamma: [f32; 4],
    pub gain: [f32; 4],
    pub lut: Option<LutRef>,
    pub input_space: crate::color::ColorSpace,
    pub working: crate::color::WorkingSpace,
    pub input_lut: Option<LutRef>,
    /// Enabled correction nodes in order (identity ones included, so a
    /// node's qualifier can still be previewed).
    pub nodes: Vec<crate::color::ColorCorrection>,
}

impl Default for ResolvedColor {
    fn default() -> Self {
        ResolvedColor {
            enabled: false,
            exposure: 0.0,
            brightness: 0.0,
            contrast: 0.0,
            saturation: 1.0,
            temperature: 0.0,
            tint: 0.0,
            lift: [0.0; 4],
            gamma: [0.0; 4],
            gain: [0.0; 4],
            lut: None,
            input_space: Default::default(),
            working: Default::default(),
            input_lut: None,
            nodes: Vec::new(),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum LayerSource {
    Media {
        asset: AssetId,
        source_time: Time,
        speed: f64,
        blend: FrameBlendMode,
    },
    /// A title with its animation state at this instant.
    Title(Box<TitleParams>, TitleAnim),
    Solid([f32; 4]),
    Compound(Box<Composition>),
}

#[derive(Clone, Debug, PartialEq)]
pub struct Layer {
    pub clip: ClipId,
    pub lane: i32,
    pub source: LayerSource,
    pub video: ResolvedVideo,
    pub color: ResolvedColor,
    /// Adjustment layer: grade the composite beneath instead of drawing.
    pub adjustment: bool,
    /// Effect stack (enabled, non-trivial effects, in order).
    pub effects: Vec<ResolvedEffect>,
}

/// Clip id used for caption layers (never collides with real clips).
pub const CAPTION_CLIP_ID: ClipId = ClipId(u64::MAX - 1);

impl Layer {
    /// A plain layer with default transform, no grade and no effects.
    pub fn new(clip: ClipId, lane: i32, source: LayerSource) -> Self {
        Layer { clip, lane, source, video: ResolvedVideo::default(), color: ResolvedColor::default(), adjustment: false, effects: Vec::new() }
    }

    pub fn has_effect(&self, kind: EffectKind) -> bool {
        self.effects.iter().any(|e| e.kind == kind)
    }

    /// Media layers that need the *next* source frame too (in-between frame
    /// synthesis or motion vectors).
    pub fn needs_next_frame(&self) -> bool {
        match &self.source {
            LayerSource::Media { blend, .. } => *blend != FrameBlendMode::FloorFrame || self.has_effect(EffectKind::MotionBlur),
            _ => false,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct ResolvedTransition {
    pub kind: TransitionKind,
    pub direction: Direction,
    pub softness: f32,
    /// Eased progress 0 → 1.
    pub progress: f32,
}

// Layer is the common case and is built every frame; boxing it would add an
// allocation per layer per frame to save memory only on rare transitions.
#[allow(clippy::large_enum_variant)]
#[derive(Clone, Debug, PartialEq)]
pub enum RenderNode {
    Layer(Layer),
    /// `None` on either side means black.
    Transition {
        from: Option<Box<Layer>>,
        to: Option<Box<Layer>>,
        transition: ResolvedTransition,
    },
}

/// Everything visible at one instant, bottom to top.
#[derive(Clone, Debug, PartialEq, Default)]
pub struct Composition {
    pub time: Time,
    pub nodes: Vec<RenderNode>,
    /// The same timeline one frame earlier — present only when an adjustment
    /// layer needs motion vectors (vector motion blur of the composite).
    pub prev: Option<Box<Composition>>,
}

impl Composition {
    pub fn new(time: Time, nodes: Vec<RenderNode>) -> Self {
        Composition { time, nodes, prev: None }
    }

    /// Every layer (including both sides of transitions), bottom to top.
    pub fn layers(&self) -> Vec<&Layer> {
        let mut out = Vec::new();
        for n in &self.nodes {
            match n {
                RenderNode::Layer(l) => out.push(l),
                RenderNode::Transition { from, to, .. } => {
                    out.extend(from.as_deref());
                    out.extend(to.as_deref());
                }
            }
        }
        out
    }

    /// Whether an adjustment layer applies motion blur to the composite.
    pub fn needs_previous_frame(&self) -> bool {
        self.layers().iter().any(|l| l.adjustment && l.has_effect(EffectKind::MotionBlur))
    }

    /// Media `(asset, source_time)` whose *next* frame is needed as well.
    pub fn next_frame_requests(&self) -> Vec<(AssetId, Time)> {
        let mut out = Vec::new();
        for l in self.layers() {
            match &l.source {
                LayerSource::Media { asset, source_time, .. } if l.needs_next_frame() => out.push((*asset, *source_time)),
                LayerSource::Compound(c) => out.extend(c.next_frame_requests()),
                _ => {}
            }
        }
        if let Some(p) = &self.prev {
            out.extend(p.next_frame_requests());
        }
        out
    }

    /// All media `(asset, source_time)` pairs needed to render this frame.
    pub fn media_requests(&self) -> Vec<(AssetId, Time)> {
        let mut out = Vec::new();
        fn layer(l: &Layer, out: &mut Vec<(AssetId, Time)>) {
            match &l.source {
                LayerSource::Media { asset, source_time, .. } => out.push((*asset, *source_time)),
                LayerSource::Compound(c) => out.extend(c.media_requests()),
                _ => {}
            }
        }
        for n in &self.nodes {
            match n {
                RenderNode::Layer(l) => layer(l, &mut out),
                RenderNode::Transition { from, to, .. } => {
                    if let Some(f) = from {
                        layer(f, &mut out);
                    }
                    if let Some(t) = to {
                        layer(t, &mut out);
                    }
                }
            }
        }
        if let Some(p) = &self.prev {
            out.extend(p.media_requests());
        }
        out
    }
}

/// The most two opposite crop edges may remove together.
pub const MAX_CROP_SUM: f32 = 0.999;

/// An opposite pair of crop edges that always leaves part of the picture.
fn crop_pair(a: f32, b: f32) -> (f32, f32) {
    let (a, b) = (a.clamp(0.0, 1.0), b.clamp(0.0, 1.0));
    let k = if a + b > MAX_CROP_SUM { MAX_CROP_SUM / (a + b) } else { 1.0 };
    (a * k, b * k)
}

fn resolve_video(c: &Clip, t: Time) -> ResolvedVideo {
    let v = &c.video;
    let source_rect = match v.crop_mode {
        crate::clip::CropMode::Trim => [0.0, 0.0, 1.0, 1.0],
        crate::clip::CropMode::KenBurns => {
            let u = if c.duration.is_positive() { (t.as_secs_f64() / c.duration.as_secs_f64()).clamp(0.0, 1.0) as f32 } else { 0.0 };
            let e = crate::keyframe::Interpolation::EaseInOut.ease(u as f64) as f32;
            let (a, b) = (v.ken_burns_start, v.ken_burns_end);
            [a[0] + (b[0] - a[0]) * e, a[1] + (b[1] - a[1]) * e, a[2] + (b[2] - a[2]) * e, a[3] + (b[3] - a[3]) * e]
        }
    };
    let (left, right) = crop_pair(v.crop_left.at_f32(t), v.crop_right.at_f32(t));
    let (top, bottom) = crop_pair(v.crop_top.at_f32(t), v.crop_bottom.at_f32(t));
    ResolvedVideo {
        position: [v.position_x.at_f32(t), v.position_y.at_f32(t)],
        scale: [v.scale_x.at_f32(t), v.scale_y.at_f32(t)],
        rotation: v.rotation.at_f32(t),
        anchor: [v.anchor_x.at_f32(t), v.anchor_y.at_f32(t)],
        opacity: v.opacity.at_f32(t).clamp(0.0, 1.0),
        crop: [left, right, top, bottom],
        flip: [v.flip_horizontal, v.flip_vertical],
        source_rect,
        blend: v.blend_mode,
        stabilize: None,
    }
}

fn resolve_color(c: &Clip, t: Time) -> ResolvedColor {
    let k = &c.color;
    ResolvedColor {
        enabled: !k.is_identity(),
        exposure: k.exposure.at_f32(t),
        brightness: k.brightness.at_f32(t),
        contrast: k.contrast.at_f32(t),
        saturation: k.saturation.at_f32(t),
        temperature: k.temperature.at_f32(t),
        tint: k.tint.at_f32(t),
        lift: k.shadows.as_array(),
        gamma: k.midtones.as_array(),
        gain: k.highlights.as_array(),
        lut: k.lut.clone(),
        input_space: k.input_space,
        working: k.working,
        input_lut: k.input_lut.clone(),
        nodes: if k.enabled { k.corrections.iter().filter(|n| n.enabled).map(|n| n.at(t)).filter(|n| n.mix > 0.0).collect() } else { Vec::new() },
    }
}

/// Build the layer for `clip` at clip-local time `local`.
pub fn clip_layer(c: &Clip, local: Time, lane: i32) -> Option<Layer> {
    if !c.enabled || !c.has_video() {
        return None;
    }
    let source = match &c.kind {
        ClipKind::Gap => return None,
        ClipKind::Asset { asset } => {
            let mut st = c.source_time(local).max(Time::ZERO);
            if let Some(md) = c.media_duration {
                st = st.min((md - Time(1)).max(Time::ZERO));
            }
            LayerSource::Media { asset: *asset, source_time: st, speed: c.speed_at(local), blend: c.frame_quality() }
        }
        ClipKind::Title(p) => LayerSource::Title(p.clone(), title_anim(p, local, c.duration)),
        ClipKind::Adjustment => LayerSource::Solid([0.0; 4]),
        ClipKind::Compound(comp) => {
            let inner_t = c.source_time(local);
            LayerSource::Compound(Box::new(evaluate_spine(&comp.spine, inner_t)))
        }
    };
    // Source seconds drive tracks and stabilization (they survive trims and retimes).
    let src = matches!(c.kind, ClipKind::Asset { .. }).then(|| c.source_time(local).as_secs_f64());
    let mut video = resolve_video(c, local);
    let mut color = resolve_color(c, local);
    if let Some(s) = src {
        video.stabilize = c.stabilization.as_ref().and_then(|st| st.correction(s));
        for n in &mut color.nodes {
            if let Some(m) = &mut n.mask {
                *m = tracked_mask(c, m, s);
            }
        }
    }
    Some(Layer { clip: c.id, lane, source, video, color, adjustment: matches!(c.kind, ClipKind::Adjustment), effects: resolve_effects(c, local, src) })
}

/// `m` moved along its track (unchanged without one).
fn tracked_mask(c: &Clip, m: &crate::color::ShapeMask, src_secs: f64) -> crate::color::ShapeMask {
    match m.track.and_then(|id| c.tracks.iter().find(|t| t.id == id)) {
        Some(t) => t.move_mask(m, src_secs),
        None => m.clone(),
    }
}

/// Smart-reframe pan (-1..1) that centres the reframe window on `x` (0..1).
pub fn reframe_pan_for(x: f32, layout: usize, top_four_three: bool, src_aspect: f32) -> Option<f32> {
    let rect_aspect = match layout {
        0 => {
            if top_four_three {
                4.0 / 3.0
            } else {
                1.6
            }
        }
        2 => 9.0 / 16.0,
        _ => return None,
    };
    let w = rect_aspect / src_aspect;
    (w < 0.999).then(|| ((2.0 * x - 1.0) / (1.0 - w)).clamp(-1.0, 1.0))
}

/// The effect stack at `local`, with masks and smart-reframe pans following
/// the clip's tracks.
fn resolve_effects(c: &Clip, local: Time, src: Option<f64>) -> Vec<ResolvedEffect> {
    c.effects
        .iter()
        .filter(|e| e.enabled)
        .map(|e| {
            let mut r = e.resolve(local);
            if let Some(s) = src {
                if let Some(m) = e.mask.as_ref().filter(|m| m.track.is_some()) {
                    r.mask = Some(tracked_mask(c, m, s).resolve());
                }
                if e.kind == EffectKind::GamingReframe916 {
                    if let Some(t) = e.track.and_then(|id| c.tracks.iter().find(|t| t.id == id)) {
                        let (layout, top43) = (r.get("Layout") as usize, r.get("Top Aspect") > 0.5);
                        if let Some(pan) = reframe_pan_for(t.point_at(s)[0], layout, top43, t.aspect) {
                            if let Some(i) = e.kind.params().iter().position(|d| d.name == "Pan X") {
                                r.values[i] = pan;
                            }
                        }
                    }
                }
            }
            r
        })
        .filter(|r| !r.is_noop())
        .collect()
}

/// Move layers whose clips follow a track on another clip.
fn apply_follows(timeline: &Timeline, comp: &mut Composition) {
    fn each(l: &mut Layer, timeline: &Timeline, t: Time) {
        let Some(f) = timeline.clip(l.clip).and_then(|c| c.video.follow.clone()) else { return };
        // the stored clip id may be stale (duplicates, pastes): fall back to the track's owner
        let owner = timeline.clip(f.clip).filter(|c| c.tracks.iter().any(|t| t.id == f.track)).map(|c| c.id).or_else(|| timeline.track_owner(f.track));
        let Some(owner) = owner else { return };
        let (Some(target), Some(start)) = (timeline.clip(owner), timeline.clip_start(owner)) else { return };
        let Some(track) = target.tracks.iter().find(|tr| tr.id == f.track) else { return };
        let local = (t - start).max(Time::ZERO).min(target.duration);
        let h = track.at(crate::motion::source_secs(target, local));
        let p0 = [0.5 + l.video.position[0], 0.5 + l.video.position[1]];
        let p1 = crate::motion::apply(&h, p0);
        l.video.position = [p1[0] - 0.5, p1[1] - 0.5];
        if f.transform {
            let (s, rot) = crate::motion::local_similarity(&h, p0, track.aspect);
            l.video.scale = [l.video.scale[0] * s, l.video.scale[1] * s];
            l.video.rotation += rot.to_degrees();
        }
    }
    let t = comp.time;
    for n in &mut comp.nodes {
        match n {
            RenderNode::Layer(l) => each(l, timeline, t),
            RenderNode::Transition { from, to, .. } => {
                for l in [from, to].into_iter().flatten() {
                    each(l, timeline, t);
                }
            }
        }
    }
}

fn transition_progress(tr: &crate::clip::Transition, elapsed: Time) -> ResolvedTransition {
    let u = if tr.duration.is_positive() { elapsed.as_secs_f64() / tr.duration.as_secs_f64() } else { 1.0 };
    ResolvedTransition { kind: tr.kind, direction: tr.direction, softness: tr.softness, progress: tr.easing.ease(u.clamp(0.0, 1.0)) as f32 }
}

fn spine_node(spine: &[Clip], starts: &[Time], t: Time) -> Option<RenderNode> {
    let i = (0..spine.len()).find(|&i| t >= starts[i] && t < starts[i] + spine[i].duration)?;
    let is_real = |j: usize| spine.get(j).is_some_and(|c| !c.is_gap());

    // Cut transitions (j → j+1) whose window covers t.
    for j in [i.wrapping_sub(1), i] {
        let Some(a) = spine.get(j) else { continue };
        if !a.enabled || a.is_gap() {
            continue;
        }
        let at_cut = is_real(j + 1);
        let Some(tr) = (if at_cut { a.cut_transition(&spine[j + 1]) } else { a.transition_out.as_ref() }) else { continue };
        let end_a = starts[j] + a.duration;
        if at_cut {
            let half = tr.duration.div_i64(2);
            let (w0, w1) = (end_a - half, end_a + (tr.duration - half));
            if t >= w0 && t < w1 {
                let b = &spine[j + 1];
                let from = clip_layer(a, t - starts[j], 0).map(Box::new);
                let to = clip_layer(b, t - end_a, 0).map(Box::new);
                return Some(RenderNode::Transition { from, to, transition: transition_progress(tr, t - w0) });
            }
        } else if j == i {
            // transition at the clip end → to black
            let w0 = end_a - tr.duration;
            if t >= w0 {
                let from = clip_layer(a, t - starts[j], 0).map(Box::new);
                return Some(RenderNode::Transition { from, to: None, transition: transition_progress(tr, t - w0) });
            }
        }
    }
    let c = &spine[i];
    // Transition at the clip start ← from black.
    if let Some(tr) = &c.transition_in {
        if i == 0 || !is_real(i - 1) {
            let local = t - starts[i];
            if local < tr.duration {
                let to = clip_layer(c, local, 0).map(Box::new);
                return Some(RenderNode::Transition { from: None, to, transition: transition_progress(tr, local) });
            }
        }
    }
    clip_layer(c, t - starts[i], 0).map(RenderNode::Layer)
}

/// Evaluate a storyline (with its connected clips) at time `t`.
pub fn evaluate_spine(spine: &[Clip], t: Time) -> Composition {
    let mut starts = Vec::with_capacity(spine.len());
    let mut acc = Time::ZERO;
    for c in spine {
        starts.push(acc);
        acc += c.duration;
    }
    let mut below: Vec<(i32, Time, RenderNode)> = Vec::new();
    let mut above: Vec<(i32, Time, RenderNode)> = Vec::new();
    for (i, p) in spine.iter().enumerate() {
        for cc in &p.connected {
            let s = starts[i] + cc.offset;
            if t >= s && t < s + cc.duration {
                if let Some(n) = connected_node(cc, t - s) {
                    if cc.lane < 0 {
                        below.push((cc.lane, s, n));
                    } else {
                        above.push((cc.lane, s, n));
                    }
                }
            }
        }
    }
    below.sort_by_key(|(lane, s, _)| (*lane, *s));
    above.sort_by_key(|(lane, s, _)| (*lane, *s));

    let mut nodes = Vec::new();
    nodes.extend(below.into_iter().map(|(_, _, n)| n));
    if let Some(n) = spine_node(spine, &starts, t) {
        nodes.push(n);
    }
    nodes.extend(above.into_iter().map(|(_, _, n)| n));
    Composition::new(t, nodes)
}

/// A connected clip at clip-local time `local`. A secondary storyline
/// renders inline: its current clip (or transition) composites straight
/// onto the layers below, so its gaps are transparent and its transitions
/// mix with what is underneath instead of with black.
fn connected_node(cc: &Clip, local: Time) -> Option<RenderNode> {
    let ClipKind::Compound(comp) = &cc.kind else {
        return clip_layer(cc, local, cc.lane).map(RenderNode::Layer);
    };
    if !comp.storyline || cc.retime.is_some() {
        return clip_layer(cc, local, cc.lane).map(RenderNode::Layer);
    }
    if !cc.enabled {
        return None;
    }
    let mut starts = Vec::with_capacity(comp.spine.len());
    let mut acc = Time::ZERO;
    for c in &comp.spine {
        starts.push(acc);
        acc += c.duration;
    }
    let lane = cc.lane;
    let relane = |l: &mut Layer| l.lane = lane;
    let clear = || Some(Box::new(Layer::new(cc.id, lane, LayerSource::Solid([0.0; 4]))));
    Some(match spine_node(&comp.spine, &starts, cc.source_in + local)? {
        RenderNode::Layer(mut l) => {
            relane(&mut l);
            RenderNode::Layer(l)
        }
        RenderNode::Transition { mut from, mut to, transition } => {
            from.as_deref_mut().map(relane);
            to.as_deref_mut().map(relane);
            // the open side of a storyline transition is what lies beneath
            RenderNode::Transition { from: from.or_else(clear), to: to.or_else(clear), transition }
        }
    })
}

fn node_lane(n: &RenderNode) -> i32 {
    match n {
        RenderNode::Layer(l) => l.lane,
        RenderNode::Transition { from, to, .. } => from.as_ref().or(to.as_ref()).map_or(0, |l| l.lane),
    }
}

/// The timeline at `t` with the active caption on top (no previous frame).
fn evaluate_frame(timeline: &Timeline, t: Time) -> Composition {
    let mut comp = evaluate_spine(&timeline.spine, t);
    if timeline.tracks.iter().any(|s| s.hidden) {
        comp.nodes.retain(|n| crate::tracks::lane_visible(&timeline.tracks, node_lane(n)));
    }
    apply_follows(timeline, &mut comp);
    if timeline.captions.enabled {
        let ct = timeline.captions.caption_time(t);
        if let Some((_, seg)) = timeline.captions.segment_at(ct) {
            let p = timeline.captions.title_params(seg);
            let anim = title_anim(&p, ct - seg.start, seg.duration());
            comp.nodes.push(RenderNode::Layer(Layer::new(CAPTION_CLIP_ID, i32::MAX, LayerSource::Title(Box::new(p), anim))));
        }
    }
    comp
}

/// Evaluate the whole timeline at `t`.
pub fn evaluate(timeline: &Timeline, t: Time) -> Composition {
    let mut comp = evaluate_frame(timeline, t);
    if comp.needs_previous_frame() {
        let f = timeline.format.rate.frame_duration();
        if t >= f {
            comp.prev = Some(Box::new(evaluate_frame(timeline, t - f)));
        }
    }
    comp
}

// ---------------------------------------------------------------------------
// Audio
// ---------------------------------------------------------------------------

/// A compound clip around an [`AudioSpan`]: its envelopes, and its time
/// mapping when it is retimed.
#[derive(Clone, Debug, PartialEq)]
pub struct AudioContainer {
    pub params: AudioParams,
    /// The compound's start, in the time it is placed in.
    pub origin: Time,
    pub duration: Time,
    pub source_in: Time,
    pub retime: Option<Retime>,
    /// Audible range of the contents, in the compound's inner time.
    pub window: (Time, Time),
}

impl AudioContainer {
    /// `t` in the time the contents are placed in (the same unless retimed).
    fn enter(&self, t: Time) -> Time {
        match &self.retime {
            Some(r) => self.source_in + r.map(t - self.origin),
            None => t,
        }
    }
}

/// One side of an Audio Crossfade, in the clip's local time.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Crossfade {
    pub from: Time,
    pub to: Time,
    pub shape: FadeShape,
}

impl Crossfade {
    fn rising(&self, local: Time) -> f32 {
        self.shape.gain(((local - self.from).as_secs_f64() / (self.to - self.from).as_secs_f64()) as f32)
    }
    fn falling(&self, local: Time) -> f32 {
        self.shape.gain(((self.to - local).as_secs_f64() / (self.to - self.from).as_secs_f64()) as f32)
    }
}

/// Stereo balance gains `(left, right)` of a pan position.
fn balance(pan: f32) -> (f32, f32) {
    let p = pan.clamp(-1.0, 1.0);
    ((1.0 - p).min(1.0), (1.0 + p).min(1.0))
}

/// A contiguous stretch of audio from one asset, in timeline coordinates.
#[derive(Clone, Debug, PartialEq)]
pub struct AudioSpan {
    pub clip: ClipId,
    pub asset: AssetId,
    pub start: Time,
    pub duration: Time,
    pub source_in: Time,
    pub retime: Option<Retime>,
    /// Keyframes are relative to `local_origin` (the owning clip's start).
    pub params: AudioParams,
    /// In the innermost retimed container's time, else in timeline time.
    pub local_origin: Time,
    /// Duration of the owning clip (for fade computation).
    pub clip_duration: Time,
    pub lane: i32,
    /// Enclosing compound clips, outermost first.
    pub containers: Vec<AudioContainer>,
    pub crossfade_in: Option<Crossfade>,
    pub crossfade_out: Option<Crossfade>,
}

impl AudioSpan {
    pub fn end(&self) -> Time {
        self.start + self.duration
    }
    /// Clip-local time at absolute timeline time `t`.
    pub fn local_time(&self, t: Time) -> Time {
        self.containers.iter().fold(t, |t, c| c.enter(t)) - self.local_origin
    }
    /// Source time at absolute timeline time `t`.
    pub fn source_time(&self, t: Time) -> Time {
        let local = self.local_time(t);
        match &self.retime {
            Some(r) => self.source_in + r.map(local),
            None => self.source_in + local,
        }
    }
    /// Whether this clip or a compound around it changes speed.
    pub fn is_retimed(&self) -> bool {
        self.retime.is_some() || self.containers.iter().any(|c| c.retime.is_some())
    }
    /// Playback speed of the source at absolute time `t`, through every retime.
    pub fn speed_at(&self, mut t: Time) -> f64 {
        let mut speed = 1.0;
        for c in &self.containers {
            if let Some(r) = &c.retime {
                speed *= r.speed_at((t - c.origin).max(Time::ZERO));
            }
            t = c.enter(t);
        }
        speed * self.retime.as_ref().map_or(1.0, |r| r.speed_at((t - self.local_origin).max(Time::ZERO)))
    }
    /// The timeline time pitch-preserving grains may not reach back past.
    pub fn stretch_origin(&self) -> Time {
        self.containers.iter().find(|c| c.retime.is_some()).map_or(self.local_origin, |c| c.origin)
    }
    /// Linear gain at absolute time `t`: the volume and fades of the clip and
    /// of every compound around it.
    pub fn gain_at(&self, mut t: Time) -> f32 {
        let mut gain = 1.0;
        for c in &self.containers {
            gain *= c.params.gain_at(t - c.origin, c.duration);
            t = c.enter(t);
            if c.retime.is_some() && (t < c.window.0 || t >= c.window.1) {
                return 0.0;
            }
        }
        let local = t - self.local_origin;
        if let Some(x) = &self.crossfade_in {
            gain *= x.rising(local);
        }
        if let Some(x) = &self.crossfade_out {
            gain *= x.falling(local);
        }
        gain * self.params.gain_at(local, self.clip_duration)
    }
    /// Stereo balance gains `(left, right)` at absolute time `t`.
    pub fn balance_at(&self, mut t: Time) -> (f32, f32) {
        let (mut l, mut r) = (1.0, 1.0);
        for c in &self.containers {
            let (cl, cr) = balance(c.params.pan.at_f32(t - c.origin));
            l *= cl;
            r *= cr;
            t = c.enter(t);
        }
        let (cl, cr) = balance(self.params.pan.at_f32(t - self.local_origin));
        (l * cl, r * cr)
    }
    /// Net pan at absolute time `t` (-1 left .. 1 right).
    pub fn pan_at(&self, t: Time) -> f32 {
        let (l, r) = self.balance_at(t);
        r - l
    }
}

/// How a clip's audible edges change for Audio Crossfades.
#[derive(Clone, Copy, Default)]
struct Edges {
    /// Media played beyond the clip before its start / after its end.
    before: Time,
    after: Time,
    fade_in: Option<Crossfade>,
    fade_out: Option<Crossfade>,
}

fn can_sound(c: &Clip) -> bool {
    c.enabled && c.has_audio() && !c.audio.muted
}

/// Spare media `(before, after)` a clip can play for a crossfade.
fn handles(c: &Clip) -> (Time, Time) {
    if c.retime.is_some() || !matches!(c.kind, ClipKind::Asset { .. }) {
        return (Time::ZERO, Time::ZERO);
    }
    let after = c.media_duration.map_or(Time::ZERO, |md| md - c.source_in - c.duration);
    (c.source_in.max(Time::ZERO), after.max(Time::ZERO))
}

/// Audio Crossfades of `placed` clips. A crossfade centres on the cut and
/// overlaps as far as both clips have spare media; with none, the first
/// fades out into the cut and the second fades in from it. With no touching
/// neighbour (or beside a J/L edit) it is a fade at the audible edge.
fn crossfade_edges(placed: &[(&Clip, Time, i32)]) -> Vec<Edges> {
    let mut out = vec![Edges::default(); placed.len()];
    if !placed.iter().any(|(c, ..)| c.audio.crossfade_in.is_positive() || c.audio.crossfade_out.is_positive()) {
        return out;
    }
    let mut paired = vec![(false, false); placed.len()];
    for (i, &(a, a_start, lane)) in placed.iter().enumerate() {
        if !can_sound(a) || a.audio_split().1.is_positive() {
            continue;
        }
        let cut = a_start + a.duration;
        let Some(j) = placed.iter().position(|&(b, s, l)| l == lane && s == cut && can_sound(b)) else { continue };
        let b = placed[j].0;
        if b.audio_split().0.is_positive() {
            continue;
        }
        paired[i].1 = true;
        paired[j].0 = true;
        let wanted = if a.audio.crossfade_out.is_positive() { a.audio.crossfade_out } else { b.audio.crossfade_in };
        let d = wanted.min(a.duration).min(b.duration);
        if !d.is_positive() {
            continue;
        }
        let half = d.div_i64(2).max(Time(1));
        let (before, after) = (half.min(handles(b).0), half.min(handles(a).1));
        if before.is_zero() && after.is_zero() {
            let shape = FadeShape::EqualPower;
            out[i].fade_out = Some(Crossfade { from: a.duration - half, to: a.duration, shape });
            out[j].fade_in = Some(Crossfade { from: Time::ZERO, to: half, shape });
        } else {
            // the same recording continuing across the cut would gain 3 dB with equal power
            let continuous = a.asset_id().is_some() && a.asset_id() == b.asset_id() && b.source_in == a.source_in + a.duration;
            let shape = if continuous { FadeShape::Linear } else { FadeShape::EqualPower };
            out[i].after = after;
            out[i].fade_out = Some(Crossfade { from: a.duration - before, to: a.duration + after, shape });
            out[j].before = before;
            out[j].fade_in = Some(Crossfade { from: -before, to: after, shape });
        }
    }
    for (i, &(c, ..)) in placed.iter().enumerate() {
        let (lead, tail) = c.audio_split();
        let shape = FadeShape::EqualPower;
        if !paired[i].0 && c.audio.crossfade_in.is_positive() {
            out[i].fade_in = Some(Crossfade { from: -lead, to: c.audio.crossfade_in.min(c.duration) - lead, shape });
        }
        if !paired[i].1 && c.audio.crossfade_out.is_positive() {
            out[i].fade_out = Some(Crossfade { from: c.duration + tail - c.audio.crossfade_out.min(c.duration), to: c.duration + tail, shape });
        }
    }
    out
}

/// `tracks` are the lane controls (only the root timeline has them).
fn collect_audio(clips: &[Clip], offset: Time, window: (Time, Time), tracks: &[crate::tracks::TrackState], out: &mut Vec<AudioSpan>) {
    let mut placed: Vec<(&Clip, Time, i32)> = Vec::new();
    let mut start = offset;
    for p in clips {
        placed.push((p, start, 0));
        placed.extend(p.connected.iter().map(|cc| (cc, start + cc.offset, cc.lane)));
        start += p.duration;
    }
    let edges = crossfade_edges(&placed);
    for (&(c, start, lane), edge) in placed.iter().zip(&edges) {
        if crate::tracks::lane_audible(tracks, lane) {
            push_clip_audio(c, start, lane, window, edge, out);
        }
    }
}

fn push_clip_audio(c: &Clip, start: Time, lane: i32, window: (Time, Time), edge: &Edges, out: &mut Vec<AudioSpan>) {
    if !can_sound(c) {
        return;
    }
    // J/L split edits and crossfades widen the audible range beyond the video edges.
    let (lead, tail) = c.audio_split();
    let s = (start - lead - edge.before).max(window.0);
    let e = (start + c.duration + tail + edge.after).min(window.1);
    if e <= s {
        return;
    }
    let mut params = c.audio.clone();
    params.audio_lead = lead;
    params.audio_tail = tail;
    match &c.kind {
        ClipKind::Asset { asset } => out.push(AudioSpan {
            clip: c.id,
            asset: *asset,
            start: s,
            duration: e - s,
            source_in: c.source_in,
            retime: c.retime.clone(),
            params,
            local_origin: start,
            clip_duration: c.duration,
            lane,
            containers: Vec::new(),
            crossfade_in: edge.fade_in,
            crossfade_out: edge.fade_out,
        }),
        ClipKind::Compound(comp) => {
            let first = out.len();
            // the compound's own crossfades act as fades on its envelope
            if let Some(x) = edge.fade_in {
                params.fade_in = params.fade_in.max(x.to);
            }
            if let Some(x) = edge.fade_out {
                params.fade_out = params.fade_out.max(c.duration - x.from);
            }
            match &c.retime {
                // same clock as the timeline around it
                None => collect_audio(comp.audio_spine(), start - c.source_in, (s, e), &[], out),
                Some(_) => {
                    let range = c.source_range();
                    collect_audio(comp.audio_spine(), Time::ZERO, range, &[], out);
                }
            }
            for span in &mut out[first..] {
                let inner = (span.start, span.end());
                if let Some(r) = &c.retime {
                    // back to this level's time; `window` gates what can't be narrowed (reverse)
                    let outer = |t: Time| r.inverse(t - c.source_in).map(|local| start + local);
                    let a = outer(inner.0).unwrap_or(s).max(s);
                    let b = outer(inner.1).unwrap_or(e).min(e);
                    span.start = a;
                    span.duration = (b - a).max(Time::ZERO);
                }
                let container = AudioContainer { params: params.clone(), origin: start, duration: c.duration, source_in: c.source_in, retime: c.retime.clone(), window: inner };
                span.containers.insert(0, container);
            }
        }
        _ => {}
    }
}

/// Flatten every audible clip into absolute-time spans.
pub fn audio_spans(timeline: &Timeline) -> Vec<AudioSpan> {
    let mut out = Vec::new();
    collect_audio(&timeline.spine, Time::ZERO, (Time::ZERO, Time::MAX), &timeline.tracks, &mut out);
    out.sort_by_key(|s| (s.start, s.lane));
    out
}
