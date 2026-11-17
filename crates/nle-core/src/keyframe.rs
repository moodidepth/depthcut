//! Cubic Bézier keyframe curves and animatable parameters.

use crate::time::Time;
use serde::{Deserialize, Serialize};

/// Interpolation of the segment *leaving* a keyframe.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Interpolation {
    #[default]
    Linear,
    EaseIn,
    EaseOut,
    EaseInOut,
    /// Step: hold the value until the next keyframe.
    Hold,
    /// Free cubic Bézier using the keyframes' tangent handles.
    Bezier,
    /// Fast-in/fast-out `cubic-bezier(0.85, 0, 0.15, 1)`: the snappy CapCut zoom-punch / whip ease.
    Whip,
    /// Ease-out that overshoots ~10 % and settles (`cubic-bezier(0.34, 1.56, 0.64, 1)`).
    Overshoot,
    /// Ease-out bounce (three decaying rebounds).
    Bounce,
    /// Zoom-punch snap `cubic-bezier(0.16, 1, 0.3, 1)`: ~90 % of the move in
    /// the first third, then a long soft settle. Never overshoots.
    SnapPunch,
    /// Penner ease-out-back: overshoots ~10 % past the target, then settles.
    EaseOutBack,
}

impl Interpolation {
    pub const ALL: [Interpolation; 11] = [
        Interpolation::Linear,
        Interpolation::EaseIn,
        Interpolation::EaseOut,
        Interpolation::EaseInOut,
        Interpolation::Hold,
        Interpolation::Bezier,
        Interpolation::Whip,
        Interpolation::Overshoot,
        Interpolation::Bounce,
        Interpolation::SnapPunch,
        Interpolation::EaseOutBack,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Interpolation::Linear => "Linear",
            Interpolation::EaseIn => "Ease In",
            Interpolation::EaseOut => "Ease Out",
            Interpolation::EaseInOut => "Ease In-Out",
            Interpolation::Hold => "Hold",
            Interpolation::Bezier => "Bézier",
            Interpolation::Whip => "Whip",
            Interpolation::Overshoot => "Overshoot",
            Interpolation::Bounce => "Bounce",
            Interpolation::SnapPunch => "Snap Punch",
            Interpolation::EaseOutBack => "Ease Out Back",
        }
    }

    /// Normalised CSS-style control points `(x1, y1, x2, y2)` for preset eases.
    pub fn preset_control_points(self) -> Option<(f64, f64, f64, f64)> {
        match self {
            Interpolation::EaseIn => Some((0.42, 0.0, 1.0, 1.0)),
            Interpolation::EaseOut => Some((0.0, 0.0, 0.58, 1.0)),
            Interpolation::EaseInOut => Some((0.42, 0.0, 0.58, 1.0)),
            Interpolation::Whip => Some((0.85, 0.0, 0.15, 1.0)),
            Interpolation::Overshoot => Some((0.34, 1.56, 0.64, 1.0)),
            Interpolation::SnapPunch => Some((0.16, 1.0, 0.3, 1.0)),
            _ => None,
        }
    }

    /// Apply this easing to a normalised progress value in `[0, 1]`.
    pub fn ease(self, u: f64) -> f64 {
        let u = u.clamp(0.0, 1.0);
        match self {
            Interpolation::Linear | Interpolation::Bezier => u,
            Interpolation::Bounce => bounce_out(u),
            Interpolation::EaseOutBack => ease_out_back(u),
            Interpolation::Hold => {
                if u >= 1.0 {
                    1.0
                } else {
                    0.0
                }
            }
            other => {
                let (x1, y1, x2, y2) = other.preset_control_points().unwrap_or((0.0, 0.0, 1.0, 1.0));
                cubic_bezier_ease(x1, y1, x2, y2, u)
            }
        }
    }
}

/// Penner ease-out-back (`c1 = 1.70158`): 0 → ~1.10 → 1, exact at both ends.
pub fn ease_out_back(u: f64) -> f64 {
    const C1: f64 = 1.70158;
    const C3: f64 = C1 + 1.0;
    let v = u.clamp(0.0, 1.0) - 1.0;
    1.0 + C3 * v * v * v + C1 * v * v
}

/// Standard ease-out bounce (Penner): 1 at u = 1, three decaying rebounds.
pub fn bounce_out(u: f64) -> f64 {
    const N: f64 = 7.5625;
    const D: f64 = 2.75;
    let u = u.clamp(0.0, 1.0);
    if u < 1.0 / D {
        N * u * u
    } else if u < 2.0 / D {
        let v = u - 1.5 / D;
        N * v * v + 0.75
    } else if u < 2.5 / D {
        let v = u - 2.25 / D;
        N * v * v + 0.9375
    } else {
        let v = u - 2.625 / D;
        N * v * v + 0.984375
    }
}

/// A Bézier tangent handle. `dt` is a fraction of the adjacent segment's
/// duration (0..1) and `dv` is an absolute value offset.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Handle {
    pub dt: f64,
    pub dv: f64,
}

impl Default for Handle {
    fn default() -> Self {
        Handle { dt: 1.0 / 3.0, dv: 0.0 }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Keyframe {
    /// Clip-local time.
    pub time: Time,
    pub value: f64,
    pub interp: Interpolation,
    #[serde(default)]
    pub in_handle: Handle,
    #[serde(default)]
    pub out_handle: Handle,
}

impl Keyframe {
    pub fn new(time: Time, value: f64) -> Self {
        Keyframe { time, value, interp: Interpolation::Linear, in_handle: Handle::default(), out_handle: Handle::default() }
    }
    pub fn with_interp(mut self, interp: Interpolation) -> Self {
        self.interp = interp;
        self
    }
}

/// Evaluate a 1-D cubic Bézier with endpoints/control values at parameter `s`.
pub fn bezier1d(p0: f64, p1: f64, p2: f64, p3: f64, s: f64) -> f64 {
    let m = 1.0 - s;
    m * m * m * p0 + 3.0 * m * m * s * p1 + 3.0 * m * s * s * p2 + s * s * s * p3
}

fn bezier1d_deriv(p0: f64, p1: f64, p2: f64, p3: f64, s: f64) -> f64 {
    let m = 1.0 - s;
    3.0 * m * m * (p1 - p0) + 6.0 * m * s * (p2 - p1) + 3.0 * s * s * (p3 - p2)
}

/// Solve `x(s) = x` for a Bézier whose x control points are `(0, x1, x2, 1)`.
pub fn solve_bezier_param(x1: f64, x2: f64, x: f64) -> f64 {
    let x1 = x1.clamp(0.0, 1.0);
    let x2 = x2.clamp(0.0, 1.0);
    let x = x.clamp(0.0, 1.0);
    // Newton–Raphson
    let mut s = x;
    for _ in 0..8 {
        let err = bezier1d(0.0, x1, x2, 1.0, s) - x;
        if err.abs() < 1e-9 {
            return s;
        }
        let d = bezier1d_deriv(0.0, x1, x2, 1.0, s);
        if d.abs() < 1e-7 {
            break;
        }
        s = (s - err / d).clamp(0.0, 1.0);
    }
    // Bisection fallback (x(s) is monotonic for clamped controls)
    let (mut lo, mut hi) = (0.0f64, 1.0f64);
    s = x;
    for _ in 0..60 {
        let v = bezier1d(0.0, x1, x2, 1.0, s);
        if (v - x).abs() < 1e-10 {
            break;
        }
        if v < x {
            lo = s;
        } else {
            hi = s;
        }
        s = 0.5 * (lo + hi);
    }
    s
}

/// CSS `cubic-bezier(x1, y1, x2, y2)` easing evaluated at `x`.
pub fn cubic_bezier_ease(x1: f64, y1: f64, x2: f64, y2: f64, x: f64) -> f64 {
    if x <= 0.0 {
        return 0.0;
    }
    if x >= 1.0 {
        return 1.0;
    }
    let s = solve_bezier_param(x1, x2, x);
    bezier1d(0.0, y1, y2, 1.0, s)
}

/// A sorted list of keyframes.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Curve {
    pub keys: Vec<Keyframe>,
}

impl Curve {
    pub fn new() -> Self {
        Curve { keys: Vec::new() }
    }

    pub fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }

    /// Evaluate the curve at clip-local time `t`.
    pub fn eval(&self, t: Time) -> f64 {
        let keys = &self.keys;
        match keys.len() {
            0 => 0.0,
            1 => keys[0].value,
            _ => {
                if t <= keys[0].time {
                    return keys[0].value;
                }
                let last = keys[keys.len() - 1];
                if t >= last.time {
                    return last.value;
                }
                let i = keys.partition_point(|k| k.time <= t) - 1;
                eval_segment(&keys[i], &keys[i + 1], t)
            }
        }
    }

    /// Insert or replace (at the same time) a keyframe, keeping order.
    pub fn insert(&mut self, k: Keyframe) {
        match self.keys.binary_search_by(|e| e.time.cmp(&k.time)) {
            Ok(i) => self.keys[i] = k,
            Err(i) => self.keys.insert(i, k),
        }
    }

    /// Set the value at `t`, creating a keyframe that inherits the
    /// interpolation of the preceding key.
    pub fn set_value_at(&mut self, t: Time, value: f64) {
        match self.keys.binary_search_by(|e| e.time.cmp(&t)) {
            Ok(i) => self.keys[i].value = value,
            Err(i) => {
                let interp = if i > 0 { self.keys[i - 1].interp } else { Interpolation::Linear };
                self.keys.insert(i, Keyframe::new(t, value).with_interp(interp));
            }
        }
    }

    /// Remove a keyframe within `tolerance` of `t`.
    pub fn remove_near(&mut self, t: Time, tolerance: Time) -> bool {
        if let Some(i) = self.keys.iter().position(|k| (k.time - t).abs() <= tolerance) {
            self.keys.remove(i);
            true
        } else {
            false
        }
    }

    pub fn shift(&mut self, dt: Time) {
        for k in &mut self.keys {
            k.time += dt;
        }
    }

    /// Index of the keyframe nearest to `t` within `tolerance`.
    pub fn key_near(&self, t: Time, tolerance: Time) -> Option<usize> {
        self.keys
            .iter()
            .enumerate()
            .filter(|(_, k)| (k.time - t).abs() <= tolerance)
            .min_by_key(|(_, k)| (k.time - t).abs())
            .map(|(i, _)| i)
    }
}

fn eval_segment(k0: &Keyframe, k1: &Keyframe, t: Time) -> f64 {
    let span = (k1.time - k0.time).0 as f64;
    if span <= 0.0 {
        return k1.value;
    }
    let u = ((t - k0.time).0 as f64 / span).clamp(0.0, 1.0);
    let (v0, v1) = (k0.value, k1.value);
    match k0.interp {
        Interpolation::Hold => v0,
        Interpolation::Linear => v0 + (v1 - v0) * u,
        Interpolation::EaseIn
        | Interpolation::EaseOut
        | Interpolation::EaseInOut
        | Interpolation::Whip
        | Interpolation::Overshoot
        | Interpolation::Bounce
        | Interpolation::SnapPunch
        | Interpolation::EaseOutBack => v0 + (v1 - v0) * k0.interp.ease(u),
        Interpolation::Bezier => {
            let x1 = k0.out_handle.dt.clamp(0.0, 1.0);
            let x2 = (1.0 - k1.in_handle.dt).clamp(0.0, 1.0);
            let s = solve_bezier_param(x1, x2, u);
            bezier1d(v0, v0 + k0.out_handle.dv, v1 + k1.in_handle.dv, v1, s)
        }
    }
}

/// An animatable scalar parameter: a static value, optionally overridden by
/// a keyframe curve.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Param {
    pub value: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub curve: Option<Curve>,
}

impl Default for Param {
    fn default() -> Self {
        Param::new(0.0)
    }
}

impl From<f64> for Param {
    fn from(v: f64) -> Self {
        Param::new(v)
    }
}

impl Param {
    pub const fn new(value: f64) -> Self {
        Param { value, curve: None }
    }

    pub fn is_animated(&self) -> bool {
        self.curve.as_ref().is_some_and(|c| !c.is_empty())
    }

    pub fn at(&self, t: Time) -> f64 {
        match &self.curve {
            Some(c) if !c.is_empty() => c.eval(t),
            _ => self.value,
        }
    }

    pub fn at_f32(&self, t: Time) -> f32 {
        self.at(t) as f32
    }

    /// Set the value: writes a keyframe at `t` when animated, otherwise the static value.
    pub fn set(&mut self, t: Time, v: f64) {
        if self.is_animated() {
            self.curve.as_mut().expect("animated").set_value_at(t, v);
        } else {
            self.value = v;
        }
    }

    /// Add a keyframe at `t` holding the current value.
    pub fn add_keyframe(&mut self, t: Time) {
        let v = self.at(t);
        self.curve.get_or_insert_with(Curve::new).set_value_at(t, v);
    }

    pub fn clear_keyframes(&mut self) {
        if let Some(c) = &self.curve {
            if !c.is_empty() {
                self.value = c.keys[0].value;
            }
        }
        self.curve = None;
    }

    /// The part of this parameter before `at` (used by blade).
    pub fn keep_left(&mut self, at: Time) {
        if let Some(c) = self.curve.as_mut().filter(|c| !c.is_empty()) {
            let v = c.eval(at);
            c.keys.retain(|k| k.time <= at);
            if c.keys.last().map(|k| k.time) != Some(at) {
                c.keys.push(Keyframe::new(at, v));
            }
        }
    }

    /// The part of this parameter after `at`, re-based to start at zero.
    pub fn keep_right(&mut self, at: Time) {
        if let Some(c) = self.curve.as_mut().filter(|c| !c.is_empty()) {
            let v = c.eval(at);
            let interp = c.keys.iter().rev().find(|k| k.time <= at).map(|k| k.interp).unwrap_or_default();
            c.keys.retain(|k| k.time >= at);
            if c.keys.first().map(|k| k.time) != Some(at) {
                c.keys.insert(0, Keyframe::new(at, v).with_interp(interp));
            }
            c.shift(-at);
        }
    }

    /// Shift keyframes by `dt` (used when trimming a clip's head).
    pub fn shift(&mut self, dt: Time) {
        if let Some(c) = self.curve.as_mut() {
            c.shift(dt);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn linear_interpolation() {
        let mut c = Curve::new();
        c.insert(Keyframe::new(Time::ZERO, 0.0));
        c.insert(Keyframe::new(Time::from_secs(2), 10.0));
        assert!((c.eval(Time::from_secs(1)) - 5.0).abs() < 1e-9);
        assert_eq!(c.eval(Time::from_secs(-1)), 0.0);
        assert_eq!(c.eval(Time::from_secs(3)), 10.0);
    }

    #[test]
    fn ease_presets_are_monotonic_and_bounded() {
        for interp in [Interpolation::EaseIn, Interpolation::EaseOut, Interpolation::EaseInOut] {
            let mut prev = 0.0;
            for i in 0..=100 {
                let y = interp.ease(i as f64 / 100.0);
                assert!(y >= prev - 1e-9, "{interp:?} not monotonic");
                assert!((-1e-9..=1.0 + 1e-9).contains(&y));
                prev = y;
            }
        }
        // ease-in starts slower than linear, ease-out faster
        assert!(Interpolation::EaseIn.ease(0.25) < 0.25);
        assert!(Interpolation::EaseOut.ease(0.25) > 0.25);
        assert!((Interpolation::EaseInOut.ease(0.5) - 0.5).abs() < 1e-6);
    }
}
