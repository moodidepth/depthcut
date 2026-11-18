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
        }
    }

    pub fn from_velocity_curve(duration: Time, curve: Curve, blend: FrameBlendMode) -> Self {
        Retime { segments: vec![RetimeSegment::new(duration.max(Time(1)), 1.0)], start_offset: Time::ZERO, lead_speed: None, blend, velocity_curve: Some(curve) }
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
        let left = Retime { segments: left_segments, velocity_curve: left_curve, ..self.clone() };
        let right = Retime { segments: right_segments, start_offset: self.map(at), lead_speed: right_lead, blend: self.blend, velocity_curve: right_curve };
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
        Retime { segments: segs, start_offset: end, lead_speed: None, blend: self.blend, velocity_curve: None }
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
}
