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

/// A tangent handle's position in (time, value) space.
pub type HandlePoint = (Time, f64);

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
        self.keys.iter().enumerate().filter(|(_, k)| (k.time - t).abs() <= tolerance).min_by_key(|(_, k)| (k.time - t).abs()).map(|(i, _)| i)
    }

    // ----- graph editor ---------------------------------------------------

    /// Move key `i` to (`t`, `value`). The time is kept at least `gap`
    /// inside its neighbours so the order never changes. Returns the
    /// time actually used.
    pub fn move_key(&mut self, i: usize, t: Time, value: f64, gap: Time) -> Time {
        let Some(k) = self.keys.get(i) else { return t };
        let mut t = t;
        if i > 0 {
            t = t.max(self.keys[i - 1].time + gap);
        }
        if i + 1 < self.keys.len() {
            t = t.min(self.keys[i + 1].time - gap);
        }
        if i > 0 && i + 1 < self.keys.len() && self.keys[i - 1].time + gap > self.keys[i + 1].time - gap {
            t = k.time;
        }
        self.keys[i].time = t;
        self.keys[i].value = value;
        t
    }

    /// Graph positions of key `i`'s (in, out) tangent handles: the in
    /// handle exists when the segment into the key is Bézier, the out
    /// handle when the segment out of it is.
    pub fn handle_points(&self, i: usize) -> (Option<HandlePoint>, Option<HandlePoint>) {
        let k = &self.keys;
        let Some(key) = k.get(i) else { return (None, None) };
        let inh = (i > 0 && k[i - 1].interp == Interpolation::Bezier).then(|| {
            let span = (key.time - k[i - 1].time).0 as f64;
            (key.time - Time((span * key.in_handle.dt) as i64), key.value + key.in_handle.dv)
        });
        let outh = (i + 1 < k.len() && key.interp == Interpolation::Bezier).then(|| {
            let span = (k[i + 1].time - key.time).0 as f64;
            (key.time + Time((span * key.out_handle.dt) as i64), key.value + key.out_handle.dv)
        });
        (inh, outh)
    }

    fn spans(&self, i: usize) -> (f64, f64) {
        let k = &self.keys;
        let before = if i > 0 { (k[i].time - k[i - 1].time).0 as f64 } else { 0.0 };
        let after = if i + 1 < k.len() { (k[i + 1].time - k[i].time).0 as f64 } else { 0.0 };
        (before, after)
    }

    /// Drag key `i`'s out handle to graph point (`t`, `v`). The segment
    /// after the key becomes Bézier. With `mirror`, the in handle turns to
    /// keep the same slope (a smooth key).
    pub fn set_out_handle(&mut self, i: usize, t: Time, v: f64, mirror: bool) {
        if i + 1 >= self.keys.len() {
            return;
        }
        let (before, after) = self.spans(i);
        let k = &mut self.keys[i];
        k.interp = Interpolation::Bezier;
        k.out_handle.dt = ((t - k.time).0 as f64 / after.max(1.0)).clamp(0.0, 1.0);
        k.out_handle.dv = v - k.value;
        if mirror && before > 0.0 && k.out_handle.dt > 1e-6 {
            let slope = k.out_handle.dv / (k.out_handle.dt * after);
            k.in_handle.dv = -slope * k.in_handle.dt * before;
        }
    }

    /// Drag key `i`'s in handle to graph point (`t`, `v`). The segment
    /// into the key becomes Bézier. `mirror` as for the out handle.
    pub fn set_in_handle(&mut self, i: usize, t: Time, v: f64, mirror: bool) {
        if i == 0 || i >= self.keys.len() {
            return;
        }
        let (before, after) = self.spans(i);
        self.keys[i - 1].interp = Interpolation::Bezier;
        let k = &mut self.keys[i];
        k.in_handle.dt = ((k.time - t).0 as f64 / before.max(1.0)).clamp(0.0, 1.0);
        k.in_handle.dv = v - k.value;
        if mirror && after > 0.0 && k.in_handle.dt > 1e-6 {
            let slope = -k.in_handle.dv / (k.in_handle.dt * before);
            k.out_handle.dv = slope * k.out_handle.dt * after;
        }
    }

    /// Give key `i` smooth Bézier tangents along the line through its
    /// neighbours (both adjoining segments become Bézier).
    pub fn auto_smooth(&mut self, i: usize) {
        let n = self.keys.len();
        if i >= n || n < 2 {
            return;
        }
        let (a, b) = (self.keys[i.saturating_sub(1)], self.keys[(i + 1).min(n - 1)]);
        let dt = (b.time - a.time).0 as f64;
        let slope = if dt > 0.0 { (b.value - a.value) / dt } else { 0.0 };
        let (before, after) = self.spans(i);
        let k = &mut self.keys[i];
        k.in_handle = Handle { dt: 1.0 / 3.0, dv: -slope * before / 3.0 };
        k.out_handle = Handle { dt: 1.0 / 3.0, dv: slope * after / 3.0 };
        if i + 1 < n {
            k.interp = Interpolation::Bezier;
        }
        if i > 0 {
            self.keys[i - 1].interp = Interpolation::Bezier;
        }
    }

    /// Flat tangents on key `i` (ease in and out of it).
    pub fn flatten(&mut self, i: usize) {
        let n = self.keys.len();
        if i >= n {
            return;
        }
        let k = &mut self.keys[i];
        k.in_handle.dv = 0.0;
        k.out_handle.dv = 0.0;
        if i + 1 < n {
            k.interp = Interpolation::Bezier;
        }
        if i > 0 {
            self.keys[i - 1].interp = Interpolation::Bezier;
        }
    }

    /// Lowest and highest value the curve reaches (keys, handles and a
    /// sampled pass over the segments).
    pub fn value_range(&self) -> Option<(f64, f64)> {
        let (first, last) = (self.keys.first()?, self.keys.last()?);
        let mut lo = f64::INFINITY;
        let mut hi = f64::NEG_INFINITY;
        let mut add = |v: f64| {
            lo = lo.min(v);
            hi = hi.max(v);
        };
        for (i, k) in self.keys.iter().enumerate() {
            add(k.value);
            let (a, b) = self.handle_points(i);
            for (_, v) in a.into_iter().chain(b) {
                add(v);
            }
        }
        let span = (last.time - first.time).0;
        for s in 0..=64 {
            add(self.eval(first.time + Time(span * s / 64)));
        }
        Some((lo, hi))
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
    fn graph_editor_moves_keys_and_shapes_handles() {
        let s = Time::from_secs;
        let gap = Time::from_millis(40);
        let mut c = Curve::new();
        for (t, v) in [(0, 0.0), (2, 10.0), (4, 0.0)] {
            c.insert(Keyframe::new(s(t), v));
        }
        // a key cannot pass its neighbours
        assert_eq!(c.move_key(1, s(9), 12.0, gap), s(4) - gap);
        assert_eq!(c.keys[1].value, 12.0);
        c.move_key(1, s(2), 10.0, gap);
        assert_eq!(c.keys.iter().map(|k| k.time).collect::<Vec<_>>(), vec![s(0), s(2), s(4)]);

        // auto smooth on a ramp keeps it a ramp
        let mut ramp = Curve::new();
        for (t, v) in [(0, 0.0), (1, 1.0), (2, 2.0)] {
            ramp.insert(Keyframe::new(s(t), v));
        }
        (0..3).for_each(|i| ramp.auto_smooth(i));
        assert_eq!(ramp.keys[0].interp, Interpolation::Bezier);
        assert_eq!(ramp.keys[1].interp, Interpolation::Bezier);
        assert!((ramp.eval(Time::from_millis(1500)) - 1.5).abs() < 0.02, "{}", ramp.eval(Time::from_millis(1500)));

        // flat tangents on the peak: the curve does not overshoot it
        c.flatten(1);
        let (lo, hi) = c.value_range().unwrap();
        assert!(lo >= -1e-6 && hi <= 10.0 + 1e-6, "{lo} {hi}");
        assert!(c.eval(Time::from_millis(1900)) > 9.5, "eases into the peak");

        // dragging the out handle up makes the curve rise above the key after it
        let (_, out) = c.handle_points(1);
        let (ot, _) = out.unwrap();
        c.set_out_handle(1, ot, 16.0, true);
        assert!(c.eval(Time::from_millis(2300)) > 10.0);
        // mirrored: the in handle points the other way (below the key)
        let (inh, _) = c.handle_points(1);
        assert!(inh.unwrap().1 < 10.0);
        // the handle lands where it was dragged
        let (_, out) = c.handle_points(1);
        assert!((out.unwrap().1 - 16.0).abs() < 1e-9);
        // broken handles: the in handle moves alone
        let before = c.keys[1].out_handle;
        c.set_in_handle(1, s(1), 3.0, false);
        assert_eq!(c.keys[1].out_handle, before);
        assert!((c.handle_points(1).0.unwrap().1 - 3.0).abs() < 1e-9);
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
