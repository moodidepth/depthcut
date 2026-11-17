//! Timeline evaluation: resolve what should be on screen (and in the
//! speakers) at a given time into a flat, renderer-friendly description.

use crate::clip::{AudioParams, BlendMode, Clip, ClipKind, Direction, LutRef, TitleParams, TransitionKind};
use crate::effects::{resolve_stack, EffectKind, ResolvedEffect};
use crate::text_anim::{title_anim, TitleAnim};
use crate::ids::{AssetId, ClipId};
use crate::speed::{FrameBlendMode, Retime};
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
    /// Ken Burns source rectangle `[x, y, w, h]` (full frame when unused).
    pub source_rect: [f32; 4],
    pub blend: BlendMode,
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
            source_rect: [0.0, 0.0, 1.0, 1.0],
            blend: BlendMode::Normal,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct ResolvedColor {
    pub enabled: bool,
    pub exposure: f32,
    pub contrast: f32,
    pub saturation: f32,
    pub temperature: f32,
    pub tint: f32,
    pub lift: [f32; 4],
    pub gamma: [f32; 4],
    pub gain: [f32; 4],
    pub lut: Option<LutRef>,
}

impl Default for ResolvedColor {
    fn default() -> Self {
        ResolvedColor {
            enabled: false,
            exposure: 0.0,
            contrast: 0.0,
            saturation: 1.0,
            temperature: 0.0,
            tint: 0.0,
            lift: [0.0; 4],
            gamma: [0.0; 4],
            gain: [0.0; 4],
            lut: None,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum LayerSource {
    Media { asset: AssetId, source_time: Time, speed: f64, blend: FrameBlendMode },
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

#[derive(Clone, Debug, PartialEq)]
pub enum RenderNode {
    Layer(Layer),
    /// `None` on either side means black.
    Transition { from: Option<Box<Layer>>, to: Option<Box<Layer>>, transition: ResolvedTransition },
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
    ResolvedVideo {
        position: [v.position_x.at_f32(t), v.position_y.at_f32(t)],
        scale: [v.scale_x.at_f32(t), v.scale_y.at_f32(t)],
        rotation: v.rotation.at_f32(t),
        anchor: [v.anchor_x.at_f32(t), v.anchor_y.at_f32(t)],
        opacity: v.opacity.at_f32(t).clamp(0.0, 1.0),
        crop: [
            v.crop_left.at_f32(t).clamp(0.0, 1.0),
            v.crop_right.at_f32(t).clamp(0.0, 1.0),
            v.crop_top.at_f32(t).clamp(0.0, 1.0),
            v.crop_bottom.at_f32(t).clamp(0.0, 1.0),
        ],
        source_rect,
        blend: v.blend_mode,
    }
}

fn resolve_color(c: &Clip, t: Time) -> ResolvedColor {
    let k = &c.color;
    ResolvedColor {
        enabled: !k.is_identity(),
        exposure: k.exposure.at_f32(t),
        contrast: k.contrast.at_f32(t),
        saturation: k.saturation.at_f32(t),
        temperature: k.temperature.at_f32(t),
        tint: k.tint.at_f32(t),
        lift: k.shadows.as_array(),
        gamma: k.midtones.as_array(),
        gain: k.highlights.as_array(),
        lut: k.lut.clone(),
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
            LayerSource::Media {
                asset: *asset,
                source_time: st,
                speed: c.speed_at(local),
                blend: c.frame_quality(),
            }
        }
        ClipKind::Title(p) => LayerSource::Title(p.clone(), title_anim(p, local, c.duration)),
        ClipKind::Adjustment => LayerSource::Solid([0.0; 4]),
        ClipKind::Compound(comp) => {
            let inner_t = c.source_time(local);
            LayerSource::Compound(Box::new(evaluate_spine(&comp.spine, inner_t)))
        }
    };
    Some(Layer {
        clip: c.id,
        lane,
        source,
        video: resolve_video(c, local),
        color: resolve_color(c, local),
        adjustment: matches!(c.kind, ClipKind::Adjustment),
        effects: resolve_stack(&c.effects, local),
    })
}

fn transition_progress(tr: &crate::clip::Transition, elapsed: Time) -> ResolvedTransition {
    let u = if tr.duration.is_positive() { elapsed.as_secs_f64() / tr.duration.as_secs_f64() } else { 1.0 };
    ResolvedTransition {
        kind: tr.kind,
        direction: tr.direction,
        softness: tr.softness,
        progress: tr.easing.ease(u.clamp(0.0, 1.0)) as f32,
    }
}

fn spine_node(spine: &[Clip], starts: &[Time], t: Time) -> Option<RenderNode> {
    let i = (0..spine.len()).find(|&i| t >= starts[i] && t < starts[i] + spine[i].duration)?;
    let is_real = |j: usize| spine.get(j).is_some_and(|c| !c.is_gap());

    // Cut transitions (j → j+1) whose window covers t.
    for j in [i.wrapping_sub(1), i] {
        let Some(a) = spine.get(j) else { continue };
        let Some(tr) = &a.transition_out else { continue };
        if !a.enabled || a.is_gap() {
            continue;
        }
        let end_a = starts[j] + a.duration;
        if is_real(j + 1) {
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
    let mut below: Vec<(i32, Time, Layer)> = Vec::new();
    let mut above: Vec<(i32, Time, Layer)> = Vec::new();
    for (i, p) in spine.iter().enumerate() {
        for cc in &p.connected {
            let s = starts[i] + cc.offset;
            if t >= s && t < s + cc.duration {
                if let Some(l) = clip_layer(cc, t - s, cc.lane) {
                    if cc.lane < 0 {
                        below.push((cc.lane, s, l));
                    } else {
                        above.push((cc.lane, s, l));
                    }
                }
            }
        }
    }
    below.sort_by_key(|(lane, s, _)| (*lane, *s));
    above.sort_by_key(|(lane, s, _)| (*lane, *s));

    let mut nodes = Vec::new();
    nodes.extend(below.into_iter().map(|(_, _, l)| RenderNode::Layer(l)));
    if let Some(n) = spine_node(spine, &starts, t) {
        nodes.push(n);
    }
    nodes.extend(above.into_iter().map(|(_, _, l)| RenderNode::Layer(l)));
    Composition::new(t, nodes)
}

/// The timeline at `t` with the active caption on top (no previous frame).
fn evaluate_frame(timeline: &Timeline, t: Time) -> Composition {
    let mut comp = evaluate_spine(&timeline.spine, t);
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
    pub local_origin: Time,
    /// Duration of the owning clip (for fade computation).
    pub clip_duration: Time,
    pub lane: i32,
    pub extra_gain_db: f32,
}

impl AudioSpan {
    pub fn end(&self) -> Time {
        self.start + self.duration
    }
    /// Source time at absolute timeline time `t`.
    pub fn source_time(&self, t: Time) -> Time {
        let local = t - self.local_origin;
        match &self.retime {
            Some(r) => self.source_in + r.map(local),
            None => self.source_in + local,
        }
    }
    /// Linear gain at absolute time `t` (volume keyframes + fades).
    pub fn gain_at(&self, t: Time) -> f32 {
        self.params.gain_at(t - self.local_origin, self.clip_duration) * crate::clip::db_to_gain(self.extra_gain_db)
    }
    pub fn pan_at(&self, t: Time) -> f32 {
        self.params.pan.at_f32(t - self.local_origin).clamp(-1.0, 1.0)
    }
}

fn collect_audio(clips: &[Clip], offset: Time, window: (Time, Time), extra_db: f32, out: &mut Vec<AudioSpan>) {
    let mut acc = Time::ZERO;
    for p in clips {
        let start = offset + acc;
        push_clip_audio(p, start, 0, window, extra_db, out);
        for cc in &p.connected {
            push_clip_audio(cc, start + cc.offset, cc.lane, window, extra_db, out);
        }
        acc += p.duration;
    }
}

fn push_clip_audio(c: &Clip, start: Time, lane: i32, window: (Time, Time), extra_db: f32, out: &mut Vec<AudioSpan>) {
    if !c.enabled || !c.av.audio || c.audio.muted {
        return;
    }
    // J/L split edits widen the audible range beyond the video edges.
    let (lead, tail) = c.audio_split();
    let s = (start - lead).max(window.0);
    let e = (start + c.duration + tail).min(window.1);
    if e <= s {
        return;
    }
    match &c.kind {
        ClipKind::Asset { asset } => {
            let mut params = c.audio.clone();
            params.audio_lead = lead;
            params.audio_tail = tail;
            out.push(AudioSpan {
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
                extra_gain_db: extra_db,
            })
        }
        ClipKind::Compound(comp) if c.retime.is_none() => {
            let inner_offset = start - c.source_in;
            let gain = extra_db + c.audio.volume_db.value as f32;
            collect_audio(&comp.spine, inner_offset, (s, e), gain, out);
        }
        _ => {}
    }
}

/// Flatten every audible clip into absolute-time spans.
pub fn audio_spans(timeline: &Timeline) -> Vec<AudioSpan> {
    let mut out = Vec::new();
    collect_audio(&timeline.spine, Time::ZERO, (Time::ZERO, Time::MAX), 0.0, &mut out);
    out.sort_by_key(|s| (s.start, s.lane));
    out
}
