//! Retiming: constant speed changes, smooth speed ramps, reverse playback and
//! freeze frames, expressed as a timeline-time → source-time mapping.
//!
//! A [`Retime`] is a sequence of segments laid out along *timeline* time. Each
//! segment plays at a constant `speed` (source seconds per timeline second;
//! `0` = freeze frame, negative = reverse) after an optional eased `ramp` from
//! the previous segment's speed. The source offset is the analytic integral
//! of the speed curve, so ramps are exact and continuous.

use crate::keyframe::{Curve, Interpolation, Keyframe};
use crate::time::{FrameRate, Time};
use serde::{Deserialize, Serialize};

/// How in-between source frames are synthesised when slowing footage down.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum FrameBlendMode {
    /// Nearest (floor) source frame — no blending.
    #[default]
    FloorFrame,
    /// Cross-fade the two neighbouring source frames.
    FrameBlend,
    /// GPU motion-vector compensated interpolation.
    OpticalFlow,
    /// HLAE-style multi-tap optical-flow weighted frame accumulation.
    WeightedAccumulation,
    /// High-quality bidirectional optical flow: full-resolution vectors,
    /// more refinement passes, vector-median outlier rejection and
    /// occlusion-aware blending (slower; for hero slow-motion shots).
    SpeedWarp,
}

impl FrameBlendMode {
    pub const ALL: [FrameBlendMode; 5] =
        [FrameBlendMode::FloorFrame, FrameBlendMode::FrameBlend, FrameBlendMode::OpticalFlow, FrameBlendMode::SpeedWarp, FrameBlendMode::WeightedAccumulation];
    pub fn label(self) -> &'static str {
        match self {
            FrameBlendMode::FloorFrame => "Normal",
            FrameBlendMode::FrameBlend => "Frame Blending",
            FrameBlendMode::OpticalFlow => "Optical Flow",
            FrameBlendMode::WeightedAccumulation => "Weighted Accum (HLAE)",
            FrameBlendMode::SpeedWarp => "Speed Warp (Better Flow)",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RetimeSegment {
    /// Length of this segment on the timeline.
    pub duration: Time,
    /// Source seconds per timeline second.
    pub speed: f64,
    /// Length of the smooth (smoothstep) transition from the previous speed.
    #[serde(default)]
    pub ramp: Time,
    /// Phase into the ramp at which this segment starts (non-zero only for
    /// segments produced by splitting in the middle of a ramp).
    #[serde(default)]
    pub ramp_offset: Time,
    /// Speed the ramp starts from, overriding the previous segment's speed
    /// (set when a segment is split in the middle of its ramp).
    #[serde(default)]
    pub ramp_from: Option<f64>,
}

impl RetimeSegment {
    pub fn new(duration: Time, speed: f64) -> Self {
        RetimeSegment { duration, speed, ramp: Time::ZERO, ramp_offset: Time::ZERO, ramp_from: None }
    }
}

/// Minimum and maximum velocity multiplier (`10%` to `800%`).
pub const MIN_VELOCITY: f64 = 0.10;
pub const MAX_VELOCITY: f64 = 8.00;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Retime {
    pub segments: Vec<RetimeSegment>,
    /// Source offset at timeline offset zero.
    #[serde(default)]
    pub start_offset: Time,
    /// Speed preceding the first segment (for ramps into segment 0).
    #[serde(default)]
    pub lead_speed: Option<f64>,
    #[serde(default)]
    pub blend: FrameBlendMode,
    /// Optional continuous speed-multiplier curve (`0.10..=8.00` = `10%..800%`)
    /// evaluated across clip-local timeline time (`Ctrl+R` velocity curve lane).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub velocity_curve: Option<Curve>,
    /// Source frames pinned to timeline times (velocity-lane pins). The
    /// curve was solved through them with [`Retime::pinned`]; a hand edit of
    /// the curve can leave them stale.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pins: Vec<VelocityPin>,
}

/// How the speed moves through a [`VelocityPin`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum PinShape {
    /// Even speed on both sides; the change between spans takes a frame.
    #[default]
    Linear,
    /// Rush into the pinned frame, slow right down through it, then pick up.
    FastInSlowKill,
    /// Glide in fast and snap to a near-hold just before the pinned frame.
    FlowSnap,
}

impl PinShape {
    pub const ALL: [PinShape; 3] = [PinShape::Linear, PinShape::FastInSlowKill, PinShape::FlowSnap];

    pub fn label(self) -> &'static str {
        match self {
            PinShape::Linear => "Linear",
            PinShape::FastInSlowKill => "Fast-In / Slow Kill",
            PinShape::FlowSnap => "Flow Snap",
        }
    }
}

/// A source frame pinned to a clip-local timeline time.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct VelocityPin {
    /// Clip-local timeline offset the frame lands on.
    pub timeline: Time,
    /// Source offset in the retime's source space (what [`Retime::map`] returns).
    pub source: Time,
    #[serde(default)]
    pub shape: PinShape,
}

/// What [`Retime::solve_velocity_for_anchors`] produced.
#[derive(Clone, Debug)]
pub struct VelocitySolve {
    pub retime: Retime,
    /// One line per pin that was dropped or span the speed range could not
    /// reach (its speed was clamped, so the pins after it may be off).
    pub warnings: Vec<String>,
    /// Largest `|map(pin.timeline) - pin.source|` over the kept pins, seconds.
    pub max_error: f64,
}

/// A velocity knot inside one solved span: scaled by the span's speed or
/// fixed (carrying the speed the previous span arrived with).
#[derive(Clone, Copy)]
enum Knot {
    Scaled(f64),
    Fixed(f64),
}

impl Knot {
    fn value(self, k: f64) -> f64 {
        match self {
            Knot::Scaled(g) => (k * g).clamp(MIN_VELOCITY, MAX_VELOCITY),
            Knot::Fixed(v) => v,
        }
    }
}

/// Knots (fraction of the span, knot) of the span leaving a `from` pin and
/// arriving at a `to` pin. `first` spans have no speed to carry in; `dt` is
/// the span length in seconds and `prev` the speed it starts with.
fn span_knots(from: PinShape, to: PinShape, first: bool, dt: f64, prev: f64) -> Vec<(f64, Knot)> {
    use Knot::{Fixed, Scaled};
    let mut k = match (from, first) {
        (PinShape::Linear, true) => vec![(0.0, Scaled(1.0))],
        (PinShape::Linear, false) => vec![(0.0, Fixed(prev)), ((1.0 / 120.0f64).min(0.05 * dt) / dt, Scaled(1.0))],
        (PinShape::FastInSlowKill, true) => vec![(0.0, Scaled(0.3)), (0.12, Scaled(0.3)), (0.3, Scaled(1.0))],
        (PinShape::FastInSlowKill, false) => vec![(0.0, Fixed(prev)), (0.12, Fixed(prev)), (0.3, Scaled(1.0))],
        (PinShape::FlowSnap, true) => vec![(0.0, Scaled(0.25)), (0.15, Scaled(0.25)), (0.22, Scaled(1.0))],
        (PinShape::FlowSnap, false) => vec![(0.0, Fixed(prev)), (0.15, Fixed(prev)), (0.22, Scaled(1.0))],
    };
    match to {
        PinShape::Linear => k.push((1.0, Scaled(1.0))),
        PinShape::FastInSlowKill => k.extend([(0.55, Scaled(1.0)), (1.0, Scaled(0.3))]),
        PinShape::FlowSnap => k.extend([(0.6, Scaled(1.0)), (0.78, Scaled(0.25)), (1.0, Scaled(0.25))]),
    }
    k
}

/// Exact integral of a piecewise-linear knot run (times in seconds).
fn knots_integral(knots: &[(f64, Knot)], k: f64) -> f64 {
    knots.windows(2).map(|w| 0.5 * (w[1].0 - w[0].0) * (w[0].1.value(k) + w[1].1.value(k))).sum()
}

/// Integral over `[0, u]` of the speed inside one segment, where the speed
/// eases from `a` to `b` over `r` seconds (smoothstep) and stays at `b`.
fn seg_integral(a: f64, b: f64, r: f64, u: f64) -> f64 {
    if r <= 0.0 {
        return b * u;
    }
    if u <= r {
        let s = u / r;
        a * u + (b - a) * r * (s * s * s - 0.5 * s * s * s * s)
    } else {
        r * (a + b) * 0.5 + b * (u - r)
    }
}

/// ∫₀ᵗ v(τ) dτ of a velocity curve, continuous and monotonic in `t`.
///
/// Integrates piecewise between keyframes with 3-point Gauss–Legendre
/// (interior nodes only), so a Hold step is never sampled on its jump and the
/// source position has no discontinuities or grid-rate slope kinks — those
/// were audible as clicks and 64 Hz pitch jitter on speed ramps.
fn integrate_velocity(curve: &Curve, t: f64) -> f64 {
    const NODES: [(f64, f64); 3] = [(-0.774_596_669_241_483_4, 5.0 / 9.0), (0.0, 8.0 / 9.0), (0.774_596_669_241_483_4, 5.0 / 9.0)];
    let v = |x: f64| curve.eval(Time::from_secs_f64(x)).clamp(MIN_VELOCITY, MAX_VELOCITY);
    // Fixed absolute grid (power of two, so grid points are exact): only the
    // final partial cell depends on `t`, which keeps the result continuous.
    let mut h = 1.0 / 32.0;
    while t / h > 1024.0 {
        h *= 2.0;
    }
    let mut acc = 0.0;
    let mut x = 0.0;
    let breaks = curve.keys.iter().map(|k| k.time.as_secs_f64()).filter(|&k| k > 0.0 && k < t).chain(std::iter::once(t));
    for b in breaks {
        while x < b {
            let next = (((x / h).floor() + 1.0) * h).min(b);
            let half = 0.5 * (next - x);
            let mid = x + half;
            acc += half * NODES.iter().map(|(n, w)| w * v(mid + n * half)).sum::<f64>();
            x = next;
        }
    }
    acc
}

fn seg_speed(a: f64, b: f64, r: f64, u: f64) -> f64 {
    if r <= 0.0 || u >= r {
        return b;
    }
    let s = (u / r).clamp(0.0, 1.0);
    a + (b - a) * (3.0 * s * s - 2.0 * s * s * s)
}

impl Retime {
    /// Constant-speed retime covering `source_duration` of media.
    /// Negative speeds play the same media in reverse.
    pub fn uniform(source_duration: Time, speed: f64) -> Retime {
        assert!(speed != 0.0, "use freeze_frame for zero speed");
        let dur = Time::from_secs_f64(source_duration.as_secs_f64() / speed.abs());
        Retime {
            segments: vec![RetimeSegment::new(dur.max(Time(1)), speed)],
            start_offset: if speed < 0.0 { source_duration } else { Time::ZERO },
            lead_speed: None,
            blend: FrameBlendMode::FloorFrame,
            velocity_curve: None,
            pins: Vec::new(),
        }
    }

    pub fn from_velocity_curve(duration: Time, curve: Curve, blend: FrameBlendMode) -> Self {
        Retime { segments: vec![RetimeSegment::new(duration.max(Time(1)), 1.0)], start_offset: Time::ZERO, lead_speed: None, blend, velocity_curve: Some(curve), pins: Vec::new() }
    }

    /// Solve a velocity curve over `duration` whose mapping puts every
    /// anchor's source frame on its timeline time, speed kept in
    /// `MIN_VELOCITY..=MAX_VELOCITY`.
    ///
    /// Each span between two anchors is a short run of linear knots shaped by
    /// the two anchors' [`PinShape`]s and scaled by one factor, found by
    /// bisection on the span's exact integral; the knots sit on `Time` ticks,
    /// so the solved mapping matches [`Retime::map_secs`] to rounding. A span
    /// the speed range cannot reach (too much or too little source, or the
    /// source going backwards) is clamped, named in `warnings`, and the next
    /// span starts from where the clamped one really ended. The anchors are
    /// stored as the retime's `pins`.
    pub fn solve_velocity_for_anchors(duration: Time, anchors: &[VelocityPin], blend: FrameBlendMode) -> VelocitySolve {
        let dur = duration.max(Time(1));
        let mut warnings = Vec::new();
        let mut sorted: Vec<VelocityPin> = Vec::with_capacity(anchors.len());
        let mut by_time = anchors.to_vec();
        by_time.sort_by_key(|p| p.timeline);
        for p in by_time {
            if p.timeline < Time::ZERO || p.timeline > dur {
                warnings.push(format!("pin at {:.3} s is outside the clip (0–{:.3} s); ignored", p.timeline.as_secs_f64(), dur.as_secs_f64()));
            } else if sorted.last().is_some_and(|q| (p.timeline - q.timeline).as_secs_f64() < 1e-3) {
                warnings.push(format!("pin at {:.3} s is within 1 ms of the pin before it; ignored", p.timeline.as_secs_f64()));
            } else {
                sorted.push(p);
            }
        }
        let mut curve = Curve::new();
        let start_offset = match sorted.as_slice() {
            [] => {
                curve.insert(Keyframe::new(Time::ZERO, 1.0));
                Time::ZERO
            }
            [p] => {
                curve.insert(Keyframe::new(Time::ZERO, 1.0));
                p.source - p.timeline
            }
            pins => {
                let mut reached = pins[0].source.as_secs_f64();
                let mut prev = 1.0;
                for (i, w) in pins.windows(2).enumerate() {
                    let (a, b) = (w[0], w[1]);
                    let span = (b.timeline - a.timeline).0;
                    let dt = (b.timeline - a.timeline).as_secs_f64();
                    // knots on Time ticks, strictly increasing
                    let mut knots: Vec<(Time, Knot)> = Vec::new();
                    for (frac, knot) in span_knots(a.shape, b.shape, i == 0, dt, prev) {
                        let t = if frac >= 1.0 { b.timeline } else { Time(a.timeline.0 + (span as f64 * frac).round() as i64) };
                        match knots.last_mut() {
                            Some(last) if t <= last.0 => {
                                if frac >= 1.0 {
                                    *last = (t, knot);
                                }
                            }
                            _ => knots.push((t, knot)),
                        }
                    }
                    let secs: Vec<(f64, Knot)> = knots.iter().map(|&(t, k)| (t.as_secs_f64(), k)).collect();
                    let min_g = secs.iter().filter_map(|(_, k)| if let Knot::Scaled(g) = k { Some(*g) } else { None }).fold(f64::INFINITY, f64::min);
                    let (mut lo, mut hi) = (0.0, MAX_VELOCITY / min_g.max(1e-3) * 1.001);
                    let want = b.source.as_secs_f64() - reached;
                    let (least, most) = (knots_integral(&secs, lo), knots_integral(&secs, hi));
                    let k = if want < least - 1e-6 || want > most + 1e-6 {
                        let what = if want < 0.0 { "goes backwards in the source".to_string() } else { format!("needs {:.2}×", want / dt) };
                        warnings.push(format!(
                            "span {:.2} s→{:.2} s (pin {}→{}) {what} (range {MIN_VELOCITY:.2}–{MAX_VELOCITY:.2}×); clamped",
                            a.timeline.as_secs_f64(),
                            b.timeline.as_secs_f64(),
                            i + 1,
                            i + 2
                        ));
                        if want < least {
                            lo
                        } else {
                            hi
                        }
                    } else {
                        for _ in 0..100 {
                            let mid = 0.5 * (lo + hi);
                            if knots_integral(&secs, mid) < want {
                                lo = mid;
                            } else {
                                hi = mid;
                            }
                        }
                        0.5 * (lo + hi)
                    };
                    reached += knots_integral(&secs, k);
                    for (j, &(t, knot)) in knots.iter().enumerate() {
                        // a span's first knot is the previous span's last key
                        if j > 0 || i == 0 {
                            curve.insert(Keyframe::new(t, knot.value(k)));
                        }
                    }
                    prev = knots.last().map_or(1.0, |&(_, knot)| knot.value(k));
                }
                // the curve holds its first value before its first key
                let v0 = curve.keys.first().map_or(1.0, |k| k.value);
                Time::from_secs_f64(pins[0].source.as_secs_f64() - v0 * pins[0].timeline.as_secs_f64())
            }
        };
        let mut retime = Retime::from_velocity_curve(dur, curve, blend);
        retime.start_offset = start_offset;
        let max_error = sorted.iter().map(|p| (retime.map_secs(p.timeline.as_secs_f64()) - p.source.as_secs_f64()).abs()).fold(0.0, f64::max);
        retime.pins = sorted;
        VelocitySolve { retime, warnings, max_error }
    }

    /// Re-solve this retime over `duration` so each pin's source frame lands
    /// on its timeline time, keeping the clip's first and last frames where
    /// they are now (unless a pin sits on an edge). Keeps the blend mode;
    /// stores `pins` (not the edges) as the new retime's pins.
    pub fn pinned(&self, duration: Time, pins: &[VelocityPin]) -> VelocitySolve {
        let edge = Time::from_secs_f64(1.0 / 240.0);
        let near = |t: Time| pins.iter().any(|p| (p.timeline - t).abs() <= edge);
        let mut anchors = pins.to_vec();
        for t in [Time::ZERO, duration] {
            if !near(t) {
                anchors.push(VelocityPin { timeline: t, source: self.map(t), shape: PinShape::Linear });
            }
        }
        let mut solve = Retime::solve_velocity_for_anchors(duration, &anchors, self.blend);
        solve.retime.pins.retain(|p| pins.contains(p));
        solve
    }

    /// Whether the mapping still puts `pin`'s frame on its time, within `tol`.
    pub fn pin_holds(&self, pin: &VelocityPin, tol: Time) -> bool {
        (self.map(pin.timeline) - pin.source).abs() <= tol
    }

    pub fn is_identity(&self) -> bool {
        if let Some(c) = &self.velocity_curve {
            if !c.is_empty() {
                return c.keys.iter().all(|k| (k.value - 1.0).abs() < 1e-6);
            }
        }
        self.lead_speed.is_none_or(|s| (s - 1.0).abs() < 1e-6) && self.segments.iter().all(|s| (s.speed - 1.0).abs() < 1e-6 && s.ramp.is_zero())
    }

    pub fn has_velocity_curve(&self) -> bool {
        self.velocity_curve.as_ref().is_some_and(|c| !c.is_empty())
    }

    /// Set or insert a velocity keyframe (multiplier `0.10..=8.00`).
    pub fn set_velocity_point(&mut self, t: Time, speed_mul: f64) {
        let clamped = speed_mul.clamp(MIN_VELOCITY, MAX_VELOCITY);
        let dur = self.timeline_duration();
        let base_speed = self.speed_at(Time::ZERO).clamp(MIN_VELOCITY, MAX_VELOCITY);
        let end_speed = self.speed_at(dur).clamp(MIN_VELOCITY, MAX_VELOCITY);
        let curve = self.velocity_curve.get_or_insert_with(|| {
            let mut c = Curve::new();
            c.insert(Keyframe::new(Time::ZERO, base_speed).with_interp(Interpolation::EaseInOut));
            if dur.is_positive() {
                c.insert(Keyframe::new(dur, end_speed).with_interp(Interpolation::EaseInOut));
            }
            c
        });
        curve.set_value_at(t.max(Time::ZERO), clamped);
    }

    pub fn for_each_curve_mut(&mut self, mut f: impl FnMut(&mut Curve)) {
        if let Some(c) = self.velocity_curve.as_mut() {
            f(c);
        }
    }

    pub fn out_duration(&self, _src_len: Time) -> Time {
        self.timeline_duration()
    }

    pub const PRESET_NAMES: [&'static str; 5] =
        ["Valorant Flow Sync (Fast-In / Kill-Hold / Snap-Out)", "CS2 AWP / Deagle Double-Pump", "Clean Skill Showcase", "Bullet-Time Kill Ramp", "Smooth Ease"];

    /// Build a one-click fragmovie velocity curve preset over `duration`.
    pub fn velocity_preset(name: &str, duration: Time) -> Option<Self> {
        let lower = name.to_lowercase();
        if lower.contains("valorant") || lower.contains("flow") {
            Some(Self::valorant_flow_sync(duration))
        } else if lower.contains("awp") || lower.contains("double") || lower.contains("cs2") {
            Some(Self::cs2_awp_double_pump(duration))
        } else if lower.contains("clean") || lower.contains("skill") || lower.contains("showcase") {
            Some(Self::clean_skill_showcase(duration))
        } else if lower.contains("bullet") {
            Some(Self::bullet_time_kill_ramp(duration))
        } else if lower.contains("smooth") || lower.contains("ease") {
            Some(Self::smooth_ease(duration))
        } else {
            None
        }
    }

    /// Valorant Flow Sync: 280% strafe/peek -> sharp Bezier drop to 25% held 10 frames -> 340% exit.
    pub fn valorant_flow_sync(duration: Time) -> Self {
        let d = duration.as_secs_f64().max(0.25);
        let hold = Time::from_frames(10, FrameRate::FPS_60).as_secs_f64().min(d * 0.35);
        let t1 = (d * 0.34).max(0.02);
        let t2 = (t1 + 0.06f64.min(d * 0.12)).min(d - hold - 0.04);
        let t3 = (t2 + hold).min(d * 0.72);
        let t4 = (t3 + 0.08f64.min(d * 0.14)).min(d - 0.01);
        let mut c = Curve::new();
        let mut k0 = Keyframe::new(Time::ZERO, 2.80).with_interp(Interpolation::EaseIn);
        k0.out_handle.dt = 0.6;
        c.insert(k0);
        let mut k1 = Keyframe::new(Time::from_secs_f64(t1), 2.80).with_interp(Interpolation::Bezier);
        k1.out_handle.dt = 0.18;
        k1.out_handle.dv = -2.2;
        c.insert(k1);
        c.insert(Keyframe::new(Time::from_secs_f64(t2), 0.25).with_interp(Interpolation::Hold));
        let mut k3 = Keyframe::new(Time::from_secs_f64(t3), 0.25).with_interp(Interpolation::Bezier);
        k3.out_handle.dt = 0.22;
        k3.out_handle.dv = 0.35;
        c.insert(k3);
        c.insert(Keyframe::new(Time::from_secs_f64(t4), 3.40).with_interp(Interpolation::EaseOut));
        c.insert(Keyframe::new(Time::from_secs_f64(d), 3.40).with_interp(Interpolation::Linear));
        Self::from_velocity_curve(duration, c, FrameBlendMode::OpticalFlow)
    }

    /// CS2 AWP / Deagle Double-Pump: 220% lead-in -> 18% shot1 (8f) -> 300% whip -> 30% shot2 -> 100%.
    pub fn cs2_awp_double_pump(duration: Time) -> Self {
        let d = duration.as_secs_f64().max(0.30);
        let hold1 = Time::from_frames(8, FrameRate::FPS_60).as_secs_f64().min(d * 0.20);
        let t_s1 = (d * 0.20).min(d * 0.25);
        let t_s1e = (t_s1 + hold1).min(d * 0.45);
        let t_mid = (t_s1e + (d - t_s1e) * 0.32).min(d * 0.65);
        let t_s2 = (t_mid + 0.05).min(d * 0.72);
        let t_s2e = (t_s2 + hold1).min(d * 0.90);
        let mut c = Curve::new();
        c.insert(Keyframe::new(Time::ZERO, 2.20).with_interp(Interpolation::EaseInOut));
        c.insert(Keyframe::new(Time::from_secs_f64(t_s1), 0.18).with_interp(Interpolation::Hold));
        c.insert(Keyframe::new(Time::from_secs_f64(t_s1e), 0.18).with_interp(Interpolation::EaseInOut));
        c.insert(Keyframe::new(Time::from_secs_f64(t_mid), 3.00).with_interp(Interpolation::EaseInOut));
        c.insert(Keyframe::new(Time::from_secs_f64(t_s2), 0.30).with_interp(Interpolation::Hold));
        c.insert(Keyframe::new(Time::from_secs_f64(t_s2e), 0.30).with_interp(Interpolation::EaseOut));
        c.insert(Keyframe::new(Time::from_secs_f64(d), 1.00).with_interp(Interpolation::Linear));
        Self::from_velocity_curve(duration, c, FrameBlendMode::OpticalFlow)
    }

    /// Clean Skill Showcase: 100% through aim flick -> 45% right after kill confirmation -> 100%.
    pub fn clean_skill_showcase(duration: Time) -> Self {
        let d = duration.as_secs_f64().max(0.20);
        let mut c = Curve::new();
        c.insert(Keyframe::new(Time::ZERO, 1.00).with_interp(Interpolation::Hold));
        c.insert(Keyframe::new(Time::from_secs_f64(d * 0.44), 1.00).with_interp(Interpolation::EaseInOut));
        c.insert(Keyframe::new(Time::from_secs_f64(d * 0.56), 0.45).with_interp(Interpolation::EaseInOut));
        c.insert(Keyframe::new(Time::from_secs_f64(d * 0.80), 0.45).with_interp(Interpolation::EaseInOut));
        c.insert(Keyframe::new(Time::from_secs_f64(d), 1.00).with_interp(Interpolation::Linear));
        Self::from_velocity_curve(duration, c, FrameBlendMode::OpticalFlow)
    }

    /// Bullet-Time Kill Ramp: fast 220% lead -> ultra-slow 12% bullet-time hold -> 240% snap-out.
    pub fn bullet_time_kill_ramp(duration: Time) -> Self {
        let d = duration.as_secs_f64().max(0.20);
        let mut c = Curve::new();
        c.insert(Keyframe::new(Time::ZERO, 2.20).with_interp(Interpolation::EaseInOut));
        c.insert(Keyframe::new(Time::from_secs_f64(d * 0.28), 0.12).with_interp(Interpolation::Hold));
        c.insert(Keyframe::new(Time::from_secs_f64(d * 0.68), 0.12).with_interp(Interpolation::EaseIn));
        c.insert(Keyframe::new(Time::from_secs_f64(d * 0.86), 2.40).with_interp(Interpolation::Linear));
        c.insert(Keyframe::new(Time::from_secs_f64(d), 2.40).with_interp(Interpolation::Linear));
        Self::from_velocity_curve(duration, c, FrameBlendMode::WeightedAccumulation)
    }

    /// Smooth Ease: gentle cinematic S-curve dip to 35% and back to 160%.
    pub fn smooth_ease(duration: Time) -> Self {
        let d = duration.as_secs_f64().max(0.20);
        let mut c = Curve::new();
        c.insert(Keyframe::new(Time::ZERO, 1.60).with_interp(Interpolation::EaseInOut));
        c.insert(Keyframe::new(Time::from_secs_f64(d * 0.50), 0.35).with_interp(Interpolation::EaseInOut));
        c.insert(Keyframe::new(Time::from_secs_f64(d), 1.60).with_interp(Interpolation::Linear));
        Self::from_velocity_curve(duration, c, FrameBlendMode::OpticalFlow)
    }

    pub fn timeline_duration(&self) -> Time {
        self.segments.iter().map(|s| s.duration).sum()
    }

    fn lead_for(&self, i: usize) -> f64 {
        if let Some(from) = self.segments[i].ramp_from {
            return from;
        }
        if i == 0 {
            self.lead_speed.unwrap_or(self.segments[0].speed)
        } else {
            self.segments[i - 1].speed
        }
    }

    /// Integral of one segment from its (possibly offset) start to local `u`.
    fn segment_integral(&self, i: usize, u: f64) -> f64 {
        let seg = &self.segments[i];
        let a = self.lead_for(i);
        let r = seg.ramp.as_secs_f64();
        let off = seg.ramp_offset.as_secs_f64();
        seg_integral(a, seg.speed, r, u + off) - seg_integral(a, seg.speed, r, off)
    }

    /// Map a clip-local timeline offset (seconds) to a source offset (seconds).
    pub fn map_secs(&self, t: f64) -> f64 {
        let mut acc = self.start_offset.as_secs_f64();
        if let Some(curve) = self.velocity_curve.as_ref().filter(|c| !c.is_empty()) {
            if t < 0.0 {
                return acc + self.speed_at_secs(0.0) * t;
            }
            if t == 0.0 {
                return acc;
            }
            return acc + integrate_velocity(curve, t);
        }
        if self.segments.is_empty() {
            return acc + t;
        }
        if t < 0.0 {
            // extrapolate before the clip using the initial speed (transition handles)
            return acc + self.speed_at_secs(0.0) * t;
        }
        let mut seg_start = 0.0;
        let last = self.segments.len() - 1;
        for i in 0..self.segments.len() {
            let d = self.segments[i].duration.as_secs_f64();
            let local = t - seg_start;
            if local < d || i == last {
                if local <= d {
                    return acc + self.segment_integral(i, local);
                }
                return acc + self.segment_integral(i, d) + self.segments[i].speed * (local - d);
            }
            acc += self.segment_integral(i, d);
            seg_start += d;
        }
        acc
    }

    /// Map a clip-local timeline offset to a source offset.
    pub fn map(&self, t: Time) -> Time {
        Time::from_secs_f64(self.map_secs(t.as_secs_f64()))
    }

    pub fn speed_at_secs(&self, t: f64) -> f64 {
        if let Some(curve) = self.velocity_curve.as_ref().filter(|c| !c.is_empty()) {
            return curve.eval(Time::from_secs_f64(t.max(0.0))).clamp(MIN_VELOCITY, MAX_VELOCITY);
        }
        let mut seg_start = 0.0;
        for i in 0..self.segments.len() {
            let seg = &self.segments[i];
            let d = seg.duration.as_secs_f64();
            if t < seg_start + d || i + 1 == self.segments.len() {
                let u = (t - seg_start).max(0.0) + seg.ramp_offset.as_secs_f64();
                return seg_speed(self.lead_for(i), seg.speed, seg.ramp.as_secs_f64(), u);
            }
            seg_start += d;
        }
        1.0
    }

    /// Instantaneous playback speed at a clip-local timeline offset.
    pub fn speed_at(&self, t: Time) -> f64 {
        self.speed_at_secs(t.as_secs_f64())
    }

    /// Timeline start offset of each segment.
    pub fn segment_starts(&self) -> Vec<Time> {
        let mut out = Vec::with_capacity(self.segments.len());
        let mut acc = Time::ZERO;
        for s in &self.segments {
            out.push(acc);
            acc += s.duration;
        }
        out
    }

    fn segment_at(&self, t: Time) -> Option<(usize, Time)> {
        let mut acc = Time::ZERO;
        for (i, s) in self.segments.iter().enumerate() {
            if t >= acc && t < acc + s.duration {
                return Some((i, t - acc));
            }
            acc += s.duration;
        }
        None
    }

    /// The range of source offsets touched by this retime (min, max).
    pub fn source_span(&self) -> (Time, Time) {
        let total = self.timeline_duration().as_secs_f64();
        let mut lo = f64::INFINITY;
        let mut hi = f64::NEG_INFINITY;
        let samples = 32 * self.segments.len().max(1);
        for i in 0..=samples {
            let v = self.map_secs(total * i as f64 / samples as f64);
            lo = lo.min(v);
            hi = hi.max(v);
        }
        (Time::from_secs_f64(lo), Time::from_secs_f64(hi))
    }

    /// Whether the mapping never goes backwards.
    pub fn is_monotonic(&self) -> bool {
        if self.has_velocity_curve() {
            return true;
        }
        self.lead_speed.unwrap_or(0.0) >= 0.0 && self.segments.iter().all(|s| s.speed >= 0.0)
    }

    /// Inverse mapping for monotonic retimes (bisection).
    pub fn inverse(&self, source_offset: Time) -> Option<Time> {
        if !self.is_monotonic() {
            return None;
        }
        let target = source_offset.as_secs_f64();
        let (mut lo, mut hi) = (0.0, self.timeline_duration().as_secs_f64());
        if target < self.map_secs(lo) - 1e-9 || target > self.map_secs(hi) + 1e-9 {
            return None;
        }
        for _ in 0..80 {
            let mid = 0.5 * (lo + hi);
            if self.map_secs(mid) < target {
                lo = mid;
            } else {
                hi = mid;
            }
        }
        Some(Time::from_secs_f64(hi))
    }

    /// Split the segment containing `at` into two (same speed); returns the
    /// index of the second half. No-op if `at` is already a boundary.
    pub fn split_segment(&mut self, at: Time) -> usize {
        let Some((i, local)) = self.segment_at(at) else {
            return self.segments.len();
        };
        if local.is_zero() {
            return i;
        }
        let seg = self.segments[i].clone();
        let lead = self.lead_for(i);
        let mut first = seg.clone();
        first.duration = local;
        let mut second = seg;
        second.duration -= local;
        second.ramp_offset += local;
        if second.ramp_offset >= second.ramp {
            second.ramp = Time::ZERO;
            second.ramp_offset = Time::ZERO;
            second.ramp_from = None;
        } else {
            second.ramp_from = Some(lead);
        }
        self.segments[i] = first;
        self.segments.insert(i + 1, second);
        i + 1
    }

    /// Split into `(left, right)` at clip-local timeline offset `at`.
    /// `right` starts exactly where `left` ends in source time.
    pub fn split_at(&self, at: Time) -> (Retime, Retime) {
        let mut tmp = self.clone();
        let idx = tmp.split_segment(at);
        let left_segments = tmp.segments[..idx].to_vec();
        let right_segments = tmp.segments[idx..].to_vec();
        let right_lead = if idx == 0 { tmp.lead_speed } else { Some(tmp.segments[idx - 1].speed) };
        let (left_curve, right_curve) = match &self.velocity_curve {
            Some(c) if !c.is_empty() => {
                let v = c.eval(at);
                let interp = c.keys.iter().rev().find(|k| k.time <= at).map(|k| k.interp).unwrap_or_default();
                let mut lc = c.clone();
                lc.keys.retain(|k| k.time <= at);
                if lc.keys.last().map(|k| k.time) != Some(at) {
                    lc.keys.push(Keyframe::new(at, v));
                }
                let mut rc = c.clone();
                rc.keys.retain(|k| k.time >= at);
                if rc.keys.first().map(|k| k.time) != Some(at) {
                    rc.keys.insert(0, Keyframe::new(at, v).with_interp(interp));
                }
                rc.shift(-at);
                (Some(lc), Some(rc))
            }
            _ => (None, None),
        };
        let left_pins = self.pins.iter().filter(|p| p.timeline <= at).copied().collect();
        let right_pins = self.pins.iter().filter(|p| p.timeline >= at).map(|p| VelocityPin { timeline: p.timeline - at, ..*p }).collect();
        let left = Retime { segments: left_segments, velocity_curve: left_curve, pins: left_pins, ..self.clone() };
        let right = Retime { segments: right_segments, start_offset: self.map(at), lead_speed: right_lead, blend: self.blend, velocity_curve: right_curve, pins: right_pins };
        (left, right)
    }

    /// Insert a freeze frame (hold) of `hold` length at clip-local `at`.
    pub fn freeze_frame(&mut self, at: Time, hold: Time) {
        let idx = self.split_segment(at);
        self.segments.insert(idx, RetimeSegment::new(hold, 0.0));
        // the segment after the hold starts at full speed (no ramp from 0)
        if let Some(next) = self.segments.get_mut(idx + 1) {
            next.ramp = Time::ZERO;
            next.ramp_offset = Time::ZERO;
            next.ramp_from = None;
        }
    }

    /// Change the speed of the segment containing `at`, preserving the amount
    /// of source media that segment covers.
    pub fn set_segment_speed(&mut self, index: usize, speed: f64) {
        let Some(seg) = self.segments.get_mut(index) else { return };
        if speed == 0.0 || seg.speed == 0.0 {
            seg.speed = speed;
            return;
        }
        let src = seg.duration.as_secs_f64() * seg.speed.abs();
        seg.duration = Time::from_secs_f64(src / speed.abs()).max(Time(1));
        seg.speed = speed;
    }

    /// Reverse playback direction of the whole retime, keeping the same media.
    pub fn reversed(&self) -> Retime {
        let total = self.timeline_duration();
        let end = self.map(total);
        let mut segs: Vec<RetimeSegment> = self.segments.iter().rev().map(|s| RetimeSegment::new(s.duration, -s.speed)).collect();
        // keep ramps attached to the boundary they describe
        for (i, s) in self.segments.iter().enumerate().skip(1) {
            let j = self.segments.len() - i;
            segs[j].ramp = s.ramp;
        }
        Retime { segments: segs, start_offset: end, lead_speed: None, blend: self.blend, velocity_curve: None, pins: Vec::new() }
    }

    /// Set the ease duration into segment `index`.
    pub fn set_ramp(&mut self, index: usize, ramp: Time) {
        if let Some(seg) = self.segments.get_mut(index) {
            seg.ramp = ramp.max(Time::ZERO);
            seg.ramp_offset = Time::ZERO;
            seg.ramp_from = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Velocity-curve source mapping must be continuous and track the speed
    /// exactly across Hold steps and Bezier ramps (no grid-rate kinks).
    #[test]
    fn velocity_map_is_continuous_across_hold_steps_and_presets() {
        let mut c = Curve::new();
        c.insert(Keyframe::new(Time::ZERO, 1.0).with_interp(Interpolation::Hold));
        c.insert(Keyframe::new(Time::from_millis(500), 0.5).with_interp(Interpolation::Hold));
        let r = Retime::from_velocity_curve(Time::from_secs(2), c, FrameBlendMode::FloorFrame);
        assert!((r.map_secs(0.5) - 0.5).abs() < 1e-9);
        assert!((r.map_secs(1.5) - 1.0).abs() < 1e-9);
        let dt = 1.0 / 48_000.0;
        for name in ["valorant", "awp", "clean", "bullet", "smooth"] {
            let r = Retime::velocity_preset(name, Time::from_secs(3)).unwrap();
            let mut prev = r.map_secs(0.0);
            for i in 1..(3.0 / dt) as usize {
                let t = i as f64 * dt;
                let s = r.map_secs(t);
                let expect = r.speed_at_secs(t - 0.5 * dt) * dt;
                // Bezier speed segments may be steep, so allow a modest slope
                // change per sample but never a jump.
                assert!(s > prev && (s - prev - expect).abs() < 0.02 * expect + 1e-7, "{name} @ {t:.5}: step {} vs speed {expect}", s - prev);
                prev = s;
            }
        }
    }

    #[test]
    fn integral_matches_numeric() {
        let (a, b, r) = (1.0, 0.25, 0.8);
        let mut acc = 0.0;
        let n = 100_000;
        let u = 1.3;
        for i in 0..n {
            let x = (i as f64 + 0.5) / n as f64 * u;
            acc += seg_speed(a, b, r, x) * u / n as f64;
        }
        assert!((acc - seg_integral(a, b, r, u)).abs() < 1e-6);
    }

    #[test]
    fn velocity_curve_presets_integrate_monotonically_and_split() {
        let dur = Time::from_secs(2);
        for name in Retime::PRESET_NAMES {
            let mut r = Retime::velocity_preset(name, dur).expect(name);
            assert!(r.has_velocity_curve());
            assert!(r.is_monotonic());
            assert!(!r.is_identity());
            let mut prev = -1.0;
            for step in 0..=100 {
                let t = step as f64 / 50.0;
                let s = r.speed_at_secs(t);
                assert!((MIN_VELOCITY..=MAX_VELOCITY).contains(&s), "{name}: {s}");
                let m = r.map_secs(t);
                assert!(m > prev, "{name} map_secs must be strictly increasing at {t}: {m} <= {prev}");
                prev = m;
            }
            let (l, rr) = r.split_at(Time::from_secs(1));
            assert_eq!(rr.start_offset, r.map(Time::from_secs(1)));
            assert!((l.map_secs(1.0) - rr.start_offset.as_secs_f64()).abs() < 0.1);
            r.set_velocity_point(Time::from_millis(500), 5.0);
            assert!((r.speed_at_secs(0.5) - 5.0).abs() < 1e-6);
        }
        let vf = Retime::valorant_flow_sync(dur);
        assert!(vf.speed_at_secs(0.05) > 2.0);
        assert!(vf.speed_at_secs(0.75) < 0.5);
        assert!(vf.speed_at_secs(1.9) > 2.5);
    }

    fn pin(t: f64, s: f64, shape: PinShape) -> VelocityPin {
        VelocityPin { timeline: Time::from_secs_f64(t), source: Time::from_secs_f64(s), shape }
    }

    fn assert_in_range(r: &Retime, dur: f64) {
        for i in 0..=2000 {
            let s = r.speed_at_secs(dur * i as f64 / 2000.0);
            assert!((MIN_VELOCITY - 1e-9..=MAX_VELOCITY + 1e-9).contains(&s), "speed {s} out of range");
        }
    }

    /// Several beats: every pinned frame lands on its beat within 1 ms, for
    /// every shape, and the speed stays in range.
    #[test]
    fn pinned_frames_land_on_their_beats_for_every_shape() {
        for shape in PinShape::ALL {
            // kills at source 0.9 / 2.1 / 3.4 / 4.0 onto beats 1.2 / 2.0 / 2.9 / 3.6 in a 4.5 s clip of 5 s media
            let pins = [pin(1.2, 0.9, shape), pin(2.0, 2.1, shape), pin(2.9, 3.4, shape), pin(3.6, 4.0, shape)];
            let base = Retime::from_velocity_curve(Time::from_secs_f64(4.5), Curve { keys: vec![Keyframe::new(Time::ZERO, 5.0 / 4.5)] }, FrameBlendMode::OpticalFlow);
            let solve = base.pinned(Time::from_secs_f64(4.5), &pins);
            assert!(solve.warnings.is_empty(), "{shape:?}: {:?}", solve.warnings);
            assert!(solve.max_error < 1e-3, "{shape:?}: max error {}", solve.max_error);
            let r = &solve.retime;
            for p in &pins {
                assert!((r.map_secs(p.timeline.as_secs_f64()) - p.source.as_secs_f64()).abs() < 1e-3, "{shape:?}: {p:?}");
                assert!(r.pin_holds(p, Time::from_millis(1)));
            }
            // the clip's first and last frames stay
            assert!((r.map_secs(0.0) - 0.0).abs() < 1e-6);
            assert!((r.map_secs(4.5) - 5.0).abs() < 1e-3, "{shape:?}: end {}", r.map_secs(4.5));
            assert_eq!(r.pins, pins.to_vec());
            assert_eq!(r.blend, FrameBlendMode::OpticalFlow);
            assert!(r.is_monotonic());
            assert_in_range(r, 4.5);
        }
        // the kill shape is slow on the kill and fast going in
        let s = Retime::solve_velocity_for_anchors(
            Time::from_secs(2),
            &[pin(0.0, 0.0, PinShape::Linear), pin(1.0, 1.0, PinShape::FastInSlowKill), pin(2.0, 2.0, PinShape::Linear)],
            FrameBlendMode::FloorFrame,
        );
        assert!(s.retime.speed_at_secs(1.0) < 0.6 && s.retime.speed_at_secs(0.5) > 1.0, "{} {}", s.retime.speed_at_secs(1.0), s.retime.speed_at_secs(0.5));
        assert!(s.max_error < 1e-3);
    }

    /// A pin the speed range cannot reach is clamped and the span named; the
    /// later pins still land when they can.
    #[test]
    fn impossible_pins_clamp_and_name_the_span() {
        // 10 s of source in 0.5 s needs 20x
        let pins = [pin(0.0, 0.0, PinShape::Linear), pin(0.5, 10.0, PinShape::Linear), pin(3.0, 12.0, PinShape::Linear)];
        let s = Retime::solve_velocity_for_anchors(Time::from_secs(3), &pins, FrameBlendMode::FloorFrame);
        assert_eq!(s.warnings.len(), 1, "{:?}", s.warnings);
        assert!(s.warnings[0].contains("0.50 s") && s.warnings[0].contains("pin 1→2") && s.warnings[0].contains("clamped"), "{}", s.warnings[0]);
        assert!(s.max_error > 1.0);
        assert_in_range(&s.retime, 3.0);
        assert!((s.retime.map_secs(0.5) - 4.0).abs() < 0.05, "8x for 0.5 s: {}", s.retime.map_secs(0.5));
        // the last span starts from where the clamped one ended and still lands
        assert!((s.retime.map_secs(3.0) - 12.0).abs() < 1e-3);
        // backwards
        let back = [pin(0.0, 2.0, PinShape::Linear), pin(1.0, 1.0, PinShape::Linear)];
        let s = Retime::solve_velocity_for_anchors(Time::from_secs(1), &back, FrameBlendMode::FloorFrame);
        assert!(s.warnings.iter().any(|w| w.contains("backwards")), "{:?}", s.warnings);
        assert!(s.retime.is_monotonic() && s.retime.map_secs(1.0) > 2.0);
        // out of range / duplicate pins are dropped with a note
        let s = Retime::solve_velocity_for_anchors(
            Time::from_secs(1),
            &[pin(0.2, 0.2, PinShape::Linear), pin(0.2, 0.3, PinShape::Linear), pin(4.0, 1.0, PinShape::Linear)],
            FrameBlendMode::FloorFrame,
        );
        assert_eq!(s.warnings.len(), 2, "{:?}", s.warnings);
        assert_eq!(s.retime.pins.len(), 1);
        assert!((s.retime.map_secs(0.2) - 0.2).abs() < 1e-6);
    }

    /// `inverse` finds each pinned frame at its beat, and a split keeps the
    /// mapping continuous with each pin on its own half.
    #[test]
    fn pinned_retime_inverts_and_splits() {
        let pins = [pin(0.8, 0.5, PinShape::FlowSnap), pin(1.6, 1.9, PinShape::FastInSlowKill), pin(2.5, 2.6, PinShape::Linear)];
        let r = Retime::uniform(Time::from_secs(3), 1.0).pinned(Time::from_secs(3), &pins).retime;
        for p in &pins {
            let t = r.inverse(p.source).expect("inverse").as_secs_f64();
            assert!((t - p.timeline.as_secs_f64()).abs() < 1e-3, "{p:?} -> {t}");
        }
        for i in 1..30 {
            let t = Time::from_secs_f64(i as f64 * 0.1);
            assert_eq!(r.map(r.inverse(r.map(t)).unwrap()), r.map(t));
        }
        let at = Time::from_secs_f64(1.2);
        let (l, rr) = r.split_at(at);
        assert_eq!(l.pins, vec![pins[0]]);
        assert_eq!(rr.pins.len(), 2);
        assert_eq!(rr.pins[0].timeline, pins[1].timeline - at);
        assert_eq!(rr.pins[0].source, pins[1].source);
        assert!((l.map_secs(1.2) - rr.map_secs(0.0)).abs() < 1e-6);
        for p in &rr.pins {
            assert!(rr.pin_holds(p, Time::from_millis(1)), "{p:?}: {}", rr.map_secs(p.timeline.as_secs_f64()));
        }
        assert!(l.pin_holds(&l.pins[0], Time::from_millis(1)));
        assert!(r.reversed().pins.is_empty());
    }

    #[test]
    fn retimes_without_pins_load_and_pins_round_trip() {
        let old = r#"{"segments":[{"duration":705600000,"speed":1.0}],"start_offset":0,"lead_speed":null,"blend":"FloorFrame"}"#;
        let r: Retime = serde_json::from_str(old).expect("old retime");
        assert!(r.pins.is_empty() && r.velocity_curve.is_none());
        assert!(!serde_json::to_string(&r).unwrap().contains("pins"), "empty pins are not written");
        let solved = r.pinned(Time::from_secs(1), &[pin(0.5, 0.4, PinShape::FlowSnap)]).retime;
        let back: Retime = serde_json::from_str(&serde_json::to_string(&solved).unwrap()).unwrap();
        assert_eq!(back, solved);
        let shapeless: VelocityPin = serde_json::from_str(r#"{"timeline":0,"source":0}"#).unwrap();
        assert_eq!(shapeless.shape, PinShape::Linear);
    }
}
