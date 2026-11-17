//! Keyframed zoom punches — the montage "hit" zoom on a kill or beat.
//!
//! A punch writes the *same* relative curve to `scale_x` and `scale_y`, so
//! the zoom is always uniform (the clip's own aspect, including any
//! non-uniform user scale, is preserved exactly) and happens about the
//! clip's anchor. The keys live on the clip itself, so preview and export
//! evaluate the identical transform through `eval` → `layer_to_output`.
//!
//! * **Punch** (`Z`): 1.0× at `t−60 ms` → peak (default 1.18×) with
//!   [`Interpolation::SnapPunch`] at `t+80 ms` → hold → back to 1.0× with
//!   `EaseInOut`, landing at `t+650 ms`.
//! * **Hard punch** (`Shift+Z`): 1.0× → 1.16× `SnapPunch` over 110 ms, then
//!   holds.
//!
//! Re-punching the same spot replaces the previous punch instead of
//! stacking on top of it.

use crate::clip::Clip;
use crate::keyframe::{Curve, Interpolation, Keyframe, Param};
use crate::time::Time;
use serde::{Deserialize, Serialize};

pub const INTENSITY_MIN: f64 = 1.05;
pub const INTENSITY_MAX: f64 = 1.50;
pub const DEFAULT_INTENSITY: f64 = 1.18;
pub const HARD_INTENSITY: f64 = 1.16;

/// Punch timeline, in milliseconds relative to the hit.
const PRE_MS: i64 = 60;
const PEAK_MS: i64 = 80;
const RELEASE_START_MS: i64 = 300;
const END_MS: i64 = 650;
const HARD_RISE_MS: i64 = 110;

/// Where the zoom is centred, in the clip's fitted layer space
/// (`0,0` = layer centre, `±0.5` = edges, y down).
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub enum PunchAnchor {
    /// The point of the clip currently shown at the centre of the frame.
    #[default]
    Center,
    /// Upper third — where a face / facecam usually sits.
    TopCenterFace,
    /// The source frame's centre: an FPS crosshair, wherever the clip has
    /// been moved or reframed to.
    Crosshair,
    /// Picked by clicking on the preview.
    Custom { x: f64, y: f64 },
}

impl PunchAnchor {
    pub const PRESETS: [PunchAnchor; 3] = [PunchAnchor::Center, PunchAnchor::TopCenterFace, PunchAnchor::Crosshair];

    pub fn label(self) -> &'static str {
        match self {
            PunchAnchor::Center => "Center",
            PunchAnchor::TopCenterFace => "Top-Center / Face",
            PunchAnchor::Crosshair => "Crosshair",
            PunchAnchor::Custom { .. } => "Custom (click preview)",
        }
    }

    /// Anchor in layer space. `position` is the clip's position (fractions
    /// of the frame) and `frame_per_layer` the frame size divided by the
    /// fitted layer size per axis (1,1 when the clip fills the frame).
    pub fn resolve(self, position: [f64; 2], frame_per_layer: [f64; 2]) -> [f64; 2] {
        let a = match self {
            // The output point at frame centre is `centre + A` = frame centre,
            // i.e. A = −P (in pixels) → in layer units −P·frame/layer.
            PunchAnchor::Center => [-position[0] * frame_per_layer[0], -position[1] * frame_per_layer[1]],
            PunchAnchor::TopCenterFace => [0.0, -0.25],
            PunchAnchor::Crosshair => [0.0, 0.0],
            PunchAnchor::Custom { x, y } => [x, y],
        };
        [a[0].clamp(-0.5, 0.5), a[1].clamp(-0.5, 0.5)]
    }
}

/// Clamp a (possibly deserialized / user-typed) intensity.
pub fn clamp_intensity(k: f64) -> f64 {
    if k.is_finite() {
        k.clamp(INTENSITY_MIN, INTENSITY_MAX)
    } else {
        DEFAULT_INTENSITY
    }
}

/// The relative zoom keys (time offset from the hit, multiplier, ease of
/// the segment that *starts* at this key).
fn punch_shape(intensity: f64, hard: bool) -> Vec<(Time, f64, Interpolation)> {
    if hard {
        vec![(Time::ZERO, 1.0, Interpolation::SnapPunch), (Time::from_millis(HARD_RISE_MS), HARD_INTENSITY, Interpolation::Hold)]
    } else {
        vec![
            (Time::from_millis(-PRE_MS), 1.0, Interpolation::SnapPunch),
            (Time::from_millis(PEAK_MS), intensity, Interpolation::Hold),
            (Time::from_millis(RELEASE_START_MS), intensity, Interpolation::EaseInOut),
            (Time::from_millis(END_MS), 1.0, Interpolation::Linear),
        ]
    }
}

/// Time span (clip-local, clamped to the clip) a punch at `at` occupies.
pub fn punch_span(clip: &Clip, at: Time, hard: bool) -> (Time, Time) {
    let (lo, hi) = if hard { (Time::ZERO, Time::from_millis(HARD_RISE_MS)) } else { (Time::from_millis(-PRE_MS), Time::from_millis(END_MS)) };
    ((at + lo).clamp(Time::ZERO, clip.duration), (at + hi).clamp(Time::ZERO, clip.duration))
}

/// Write one axis of the punch: keys are `base(t) × multiplier`, where
/// `base` is the parameter's value with any previous punch in the span removed.
fn punch_param(p: &mut Param, at: Time, span: (Time, Time), shape: &[(Time, f64, Interpolation)], hard: bool) {
    // Drop any keys inside the span (a previous punch here) so re-punching is idempotent.
    if let Some(c) = p.curve.as_mut() {
        c.keys.retain(|k| k.time < span.0 || k.time > span.1);
    }
    let base = p.clone();
    let curve = p.curve.get_or_insert_with(Curve::new);
    for &(dt, mul, interp) in shape {
        let t = (at + dt).clamp(span.0, span.1);
        curve.insert(Keyframe::new(t, base.at(t) * mul).with_interp(interp));
    }
    if hard {
        // A hard punch holds to the end of the clip: re-assert the peak on
        // any later key so a following part of the curve doesn't drop back.
        let peak = shape.last().map(|s| s.1).unwrap_or(1.0);
        for k in curve.keys.iter_mut().filter(|k| k.time > span.1) {
            k.value = base.at(k.time) * peak;
        }
    }
    // A key before the punch keeps its old ease, which would bend the run-up;
    // make the segment into the punch flat.
    if let Some(i) = curve.keys.iter().rposition(|k| k.time < span.0) {
        if curve.keys[i].interp == Interpolation::Bezier {
            curve.keys[i].interp = Interpolation::Linear;
        }
    }
}

/// Apply a zoom punch at clip-local time `at`. Writes identical relative
/// keys to `scale_x` and `scale_y` and sets the anchor. Returns `false` (and
/// leaves the clip untouched) when `at` lies outside the clip.
pub fn apply_punch(clip: &mut Clip, at: Time, intensity: f64, hard: bool, anchor: [f64; 2]) -> bool {
    if clip.is_gap() || at < Time::ZERO || at > clip.duration {
        return false;
    }
    let intensity = clamp_intensity(intensity);
    let shape = punch_shape(intensity, hard);
    let span = punch_span(clip, at, hard);
    punch_param(&mut clip.video.scale_x, at, span, &shape, hard);
    punch_param(&mut clip.video.scale_y, at, span, &shape, hard);
    let ax = if anchor[0].is_finite() { anchor[0].clamp(-0.5, 0.5) } else { 0.0 };
    let ay = if anchor[1].is_finite() { anchor[1].clamp(-0.5, 0.5) } else { 0.0 };
    clip.video.anchor_x = Param::new(ax);
    clip.video.anchor_y = Param::new(ay);
    true
}

/// Remove every transform keyframe near `t` (clip-local). Returns whether any was removed.
pub fn remove_keys_near(clip: &mut Clip, t: Time, tol: Time) -> bool {
    let mut any = false;
    clip.for_each_param_mut(|p| {
        if let Some(c) = p.curve.as_mut() {
            while c.remove_near(t, tol) {
                any = true;
            }
            if c.keys.is_empty() {
                p.curve = None;
            }
        }
    });
    any
}

/// Set the ease of every keyframe near `t` (clip-local).
pub fn set_interp_near(clip: &mut Clip, t: Time, tol: Time, interp: Interpolation) -> bool {
    let mut any = false;
    clip.for_each_param_mut(|p| {
        if let Some(c) = p.curve.as_mut() {
            if let Some(i) = c.key_near(t, tol) {
                c.keys[i].interp = interp;
                any = true;
            }
        }
    });
    any
}

/// The ease shared by the keys near `t`, if they agree.
pub fn interp_near(clip: &Clip, t: Time, tol: Time) -> Option<Interpolation> {
    let mut found: Option<Interpolation> = None;
    let mut c = clip.clone();
    let mut mixed = false;
    c.for_each_param_mut(|p| {
        if let Some(cv) = &p.curve {
            if let Some(i) = cv.key_near(t, tol) {
                let it = cv.keys[i].interp;
                match found {
                    None => found = Some(it),
                    Some(f) if f != it => mixed = true,
                    _ => {}
                }
            }
        }
    });
    if mixed {
        None
    } else {
        found
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keyframe::Interpolation;

    fn clip() -> Clip {
        Clip::adjustment(Time::from_secs(4))
    }

    fn ms(v: i64) -> Time {
        Time::from_millis(v)
    }

    #[test]
    fn snap_punch_is_the_spec_bezier() {
        let e = |u| Interpolation::SnapPunch.ease(u);
        assert!((e(0.0) - 0.0).abs() < 1e-9);
        assert!((e(1.0) - 1.0).abs() < 1e-9);
        // cubic-bezier(0.16, 1, 0.3, 1): most of the move happens early and it never overshoots.
        assert!(e(0.25) > 0.75, "{}", e(0.25));
        assert!(e(0.5) > 0.93, "{}", e(0.5));
        let mut prev = 0.0;
        for i in 0..=200 {
            let v = e(i as f64 / 200.0);
            assert!(v >= prev - 1e-12 && v <= 1.0 + 1e-9, "u={} v={v}", i as f64 / 200.0);
            prev = v;
        }
    }

    #[test]
    fn ease_out_back_overshoots_then_settles() {
        let e = |u| Interpolation::EaseOutBack.ease(u);
        assert!((e(0.0)).abs() < 1e-9 && (e(1.0) - 1.0).abs() < 1e-9);
        let peak = (0..=100).map(|i| e(i as f64 / 100.0)).fold(0.0, f64::max);
        assert!(peak > 1.05 && peak < 1.15, "{peak}");
    }

    #[test]
    fn punch_hits_the_spec_keys() {
        let mut c = clip();
        let at = Time::from_secs(1);
        assert!(apply_punch(&mut c, at, DEFAULT_INTENSITY, false, [0.0, 0.0]));
        let s = &c.video.scale_x;
        assert!((s.at(at - ms(60)) - 1.0).abs() < 1e-9);
        assert!((s.at(at + ms(80)) - 1.18).abs() < 1e-9);
        assert!((s.at(at + ms(200)) - 1.18).abs() < 1e-9, "holds");
        assert!((s.at(at + ms(650)) - 1.0).abs() < 1e-9);
        assert!((s.at(Time::from_secs(3)) - 1.0).abs() < 1e-9);
        // SnapPunch: most of the zoom lands within the first third of the rise.
        let early = s.at(at - ms(60) + ms(47));
        assert!(early > 1.0 + 0.75 * 0.18, "{early}");
        let k = &s.curve.as_ref().unwrap().keys;
        assert_eq!(k[0].interp, Interpolation::SnapPunch);
    }

    #[test]
    fn scale_stays_uniform_and_keeps_user_aspect() {
        let mut c = clip();
        c.video.scale_x = Param::new(1.2);
        c.video.scale_y = Param::new(0.9);
        let at = Time::from_millis(1500);
        apply_punch(&mut c, at, 1.3, false, [0.1, -0.2]);
        for i in 0..=400 {
            let t = Time::from_millis(i * 10);
            let (sx, sy) = (c.video.scale_x.at(t), c.video.scale_y.at(t));
            assert!((sx / sy - 1.2 / 0.9).abs() < 1e-9, "t={t:?} {sx} {sy}");
        }
        assert_eq!(c.video.anchor_x.at(at), 0.1);
        assert_eq!(c.video.anchor_y.at(at), -0.2);
    }

    #[test]
    fn re_punching_the_same_spot_does_not_stack() {
        let mut once = clip();
        apply_punch(&mut once, Time::from_secs(2), 1.25, false, [0.0, 0.0]);
        let mut twice = once.clone();
        apply_punch(&mut twice, Time::from_secs(2), 1.25, false, [0.0, 0.0]);
        assert_eq!(once.video.scale_x, twice.video.scale_x);
        assert!((twice.video.scale_y.at(Time::from_millis(2080)) - 1.25).abs() < 1e-9);
    }

    #[test]
    fn hard_punch_rises_in_110ms_and_holds() {
        let mut c = clip();
        let at = Time::from_secs(1);
        apply_punch(&mut c, at, DEFAULT_INTENSITY, true, [0.0, 0.0]);
        let s = &c.video.scale_y;
        assert!((s.at(at) - 1.0).abs() < 1e-9);
        assert!((s.at(at + ms(110)) - HARD_INTENSITY).abs() < 1e-9);
        assert!((s.at(Time::from_millis(3900)) - HARD_INTENSITY).abs() < 1e-9);
    }

    #[test]
    fn punch_is_clamped_to_the_clip_and_intensity_range() {
        let mut c = clip();
        assert!(!apply_punch(&mut c, Time::from_secs(9), 1.2, false, [0.0, 0.0]));
        assert!(apply_punch(&mut c, Time::from_millis(10), 9.0, false, [3.0, f64::NAN]));
        let keys = &c.video.scale_x.curve.as_ref().unwrap().keys;
        assert!(keys.iter().all(|k| k.time >= Time::ZERO && k.time <= c.duration));
        assert!(keys.iter().all(|k| k.value <= INTENSITY_MAX + 1e-9));
        assert_eq!((c.video.anchor_x.value, c.video.anchor_y.value), (0.5, 0.0));
        assert_eq!(clamp_intensity(f64::NAN), DEFAULT_INTENSITY);
    }

    #[test]
    fn keys_can_be_re_eased_and_deleted() {
        let mut c = clip();
        let at = Time::from_secs(1);
        apply_punch(&mut c, at, 1.2, false, [0.0, 0.0]);
        let tol = ms(5);
        assert!(set_interp_near(&mut c, at + ms(80), tol, Interpolation::EaseOutBack));
        assert_eq!(interp_near(&c, at + ms(80), tol), Some(Interpolation::EaseOutBack));
        assert!(remove_keys_near(&mut c, at + ms(80), tol));
        assert_eq!(c.video.scale_x.curve.as_ref().unwrap().keys.len(), 3);
        for t in [-60, 300, 650] {
            remove_keys_near(&mut c, at + ms(t), tol);
        }
        assert!(!c.video.scale_x.is_animated());
    }

    #[test]
    fn anchor_presets_resolve_in_layer_space() {
        assert_eq!(PunchAnchor::Crosshair.resolve([0.2, 0.1], [1.0, 1.0]), [0.0, 0.0]);
        assert_eq!(PunchAnchor::Center.resolve([0.2, -0.1], [1.0, 1.0]), [-0.2, 0.1]);
        assert_eq!(PunchAnchor::Center.resolve([0.4, 0.0], [2.0, 1.0]), [-0.5, 0.0]);
        assert!(PunchAnchor::TopCenterFace.resolve([0.0, 0.0], [1.0, 1.0])[1] < 0.0);
    }
}
