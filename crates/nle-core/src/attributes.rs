//! Paste Attributes: copy chosen groups of settings (transform, crop,
//! compositing, colour, effects, audio, speed) from one clip onto others,
//! like Final Cut's Paste Attributes and Premiere's Paste Attributes dialog.
//! Also the colour-only shortcuts (copy / paste grade, grade from the
//! previous clip) the Color page uses.

use crate::clip::{Clip, ColorParams};
use crate::color::ColorCorrection;
use crate::command::Command;
use crate::keyframe::Param;
use crate::speed::Retime;
use crate::time::Time;
use serde::{Deserialize, Serialize};

/// Which groups to paste.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct AttributeSet {
    /// Position, scale, rotation, anchor, flips.
    pub transform: bool,
    /// Crop edges, crop mode, Ken Burns.
    pub crop: bool,
    /// Opacity and blend mode.
    pub compositing: bool,
    pub color: bool,
    pub effects: bool,
    /// Volume, pan, fades.
    pub volume: bool,
    /// EQ, filters and the audio enhancement switches.
    pub audio_effects: bool,
    /// Speed (as a constant rate; ramps aren't carried over).
    pub speed: bool,
}

impl Default for AttributeSet {
    fn default() -> Self {
        AttributeSet { transform: true, crop: true, compositing: true, color: true, effects: true, volume: true, audio_effects: true, speed: false }
    }
}

impl AttributeSet {
    pub const NONE: AttributeSet =
        AttributeSet { transform: false, crop: false, compositing: false, color: false, effects: false, volume: false, audio_effects: false, speed: false };

    pub fn color_only() -> AttributeSet {
        AttributeSet { color: true, ..AttributeSet::NONE }
    }

    pub fn any(&self) -> bool {
        *self != AttributeSet::NONE
    }
}

/// How the source's keyframes land on a clip of a different length.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum KeyTiming {
    /// Same clip-local times (keys past the end are simply never reached).
    #[default]
    Maintain,
    /// Scaled to the target's duration.
    Stretch,
}

fn stretch_param(p: &mut Param, k: f64) {
    if let Some(c) = p.curve.as_mut() {
        for key in &mut c.keys {
            key.time = key.time.mul_f64(k);
        }
    }
}

fn stretch_nodes(nodes: &mut [ColorCorrection], k: f64) {
    for n in nodes {
        for key in &mut n.keys {
            key.time = key.time.mul_f64(k);
        }
    }
}

/// `src`'s grade with fresh node ids (so two clips never share UI state),
/// keys scaled by `k`.
fn grade_copy(src: &ColorParams, k: f64) -> ColorParams {
    let mut g = src.clone();
    let mut ids = std::collections::HashMap::new();
    for n in &mut g.corrections {
        let old = n.id;
        *n = n.duplicate();
        ids.insert(old, n.id);
    }
    if let Some(graph) = &mut g.graph {
        graph.remap_correctors(&ids);
    }
    if k != 1.0 {
        g.params_mut().into_iter().for_each(|p| stretch_param(p, k));
        stretch_nodes(&mut g.corrections, k);
        if let Some(graph) = &mut g.graph {
            graph.for_each_param_mut(|p| stretch_param(p, k));
        }
    }
    g
}

/// The grade `src` has, ready to land on `dst` (keys per `timing`).
pub fn grade_for(src: &ColorParams, src_duration: Time, dst: &Clip, timing: KeyTiming) -> ColorParams {
    grade_copy(src, factor(src_duration, dst.duration, timing))
}

fn factor(from: Time, to: Time, timing: KeyTiming) -> f64 {
    match timing {
        KeyTiming::Maintain => 1.0,
        KeyTiming::Stretch if from.is_positive() => to.as_secs_f64() / from.as_secs_f64(),
        KeyTiming::Stretch => 1.0,
    }
}

/// `dst` with `sel`'s groups taken from `src`. Speed is left alone here
/// (it changes timing; see [`speed_commands`]).
pub fn apply(src: &Clip, dst: &Clip, sel: &AttributeSet, timing: KeyTiming) -> Clip {
    let k = factor(src.duration, dst.duration, timing);
    let mut out = dst.clone();
    let (sv, v) = (&src.video, &mut out.video);
    if sel.transform {
        v.position_x = sv.position_x.clone();
        v.position_y = sv.position_y.clone();
        v.scale_x = sv.scale_x.clone();
        v.scale_y = sv.scale_y.clone();
        v.rotation = sv.rotation.clone();
        v.anchor_x = sv.anchor_x.clone();
        v.anchor_y = sv.anchor_y.clone();
        v.flip_horizontal = sv.flip_horizontal;
        v.flip_vertical = sv.flip_vertical;
        v.scale_lock = sv.scale_lock;
        for p in [&mut v.position_x, &mut v.position_y, &mut v.scale_x, &mut v.scale_y, &mut v.rotation, &mut v.anchor_x, &mut v.anchor_y] {
            stretch_param(p, k);
        }
    }
    if sel.crop {
        v.crop_left = sv.crop_left.clone();
        v.crop_right = sv.crop_right.clone();
        v.crop_top = sv.crop_top.clone();
        v.crop_bottom = sv.crop_bottom.clone();
        v.crop_mode = sv.crop_mode;
        v.ken_burns_start = sv.ken_burns_start;
        v.ken_burns_end = sv.ken_burns_end;
        v.crop_aspect = sv.crop_aspect;
        v.crop_lock = sv.crop_lock;
        for p in [&mut v.crop_left, &mut v.crop_right, &mut v.crop_top, &mut v.crop_bottom] {
            stretch_param(p, k);
        }
    }
    if sel.compositing {
        v.opacity = sv.opacity.clone();
        v.blend_mode = sv.blend_mode;
        stretch_param(&mut v.opacity, k);
        v.masks = sv.masks.clone();
        v.track_matte = sv.track_matte;
        if k != 1.0 {
            for m in &mut v.masks {
                for p in m.params_mut() {
                    stretch_param(p, k);
                }
                for sk in &mut m.shape.shape_keys {
                    sk.time = sk.time.mul_f64(k);
                }
            }
        }
    }
    if sel.color {
        out.color = grade_copy(&src.color, k);
    }
    if sel.effects {
        out.effects = src.effects.iter().map(|e| e.duplicate()).collect();
        for e in &mut out.effects {
            e.params.iter_mut().for_each(|p| stretch_param(p, k));
        }
    }
    let (sa, a) = (&src.audio, &mut out.audio);
    if sel.volume {
        a.volume_db = sa.volume_db.clone();
        a.pan = sa.pan.clone();
        a.fade_in = sa.fade_in;
        a.fade_out = sa.fade_out;
        a.fade_shape = sa.fade_shape;
        a.muted = sa.muted;
        stretch_param(&mut a.volume_db, k);
        stretch_param(&mut a.pan, k);
    }
    if sel.audio_effects {
        let keep = (a.volume_db.clone(), a.pan.clone(), a.fade_in, a.fade_out, a.fade_shape, a.muted, a.role, a.audio_lead, a.audio_tail);
        *a = sa.clone();
        (a.volume_db, a.pan, a.fade_in, a.fade_out, a.fade_shape, a.muted, a.role, a.audio_lead, a.audio_tail) = keep;
        stretch_param(&mut a.lowpass_hz, k);
    }
    out
}

/// Commands that give `dst` the constant speed `src` plays at (keeping the
/// part of the source `dst` shows). Empty when nothing changes.
pub fn speed_commands(src: &Clip, dst: &Clip) -> Vec<Command> {
    let speed = |c: &Clip| match &c.retime {
        Some(r) => {
            let (lo, hi) = r.source_span();
            let t = r.timeline_duration().as_secs_f64();
            let s = if t > 0.0 { (hi - lo).as_secs_f64() / t } else { 1.0 };
            if r.segments.first().is_some_and(|s| s.speed < 0.0) {
                -s
            } else {
                s
            }
        }
        None => 1.0,
    };
    let (s, d) = (speed(src), speed(dst));
    if (s - d).abs() < 1e-6 && src.retime.is_none() == dst.retime.is_none() {
        return Vec::new();
    }
    let span = match &dst.retime {
        Some(r) => {
            let (lo, hi) = r.source_span();
            (hi - lo).max(Time(1))
        }
        None => dst.duration,
    };
    let mut out = Vec::new();
    if dst.retime.is_some() {
        // back to 100 % over the same source first
        out.push(Command::SetRetime { id: dst.id, retime: None });
    }
    if (s - 1.0).abs() > 1e-6 && s.abs() > 1e-6 {
        out.push(Command::SetRetime { id: dst.id, retime: Some(Retime::uniform(span, s)) });
    }
    out
}

/// One undo step pasting `sel` from `src` onto each of `targets`.
pub fn paste_commands(src: &Clip, targets: &[&Clip], sel: &AttributeSet, timing: KeyTiming) -> Option<Command> {
    let mut commands = Vec::new();
    for dst in targets.iter().filter(|d| d.id != src.id) {
        let new = apply(src, dst, sel, timing);
        if new != **dst {
            commands.push(Command::UpdateClip { clip: Box::new(new) });
        }
        if sel.speed {
            commands.extend(speed_commands(src, dst));
        }
    }
    (!commands.is_empty()).then_some(Command::Batch { label: "Paste Attributes", commands })
}

/// One undo step giving each of `targets` the grade `grade` (from a clip
/// `src_duration` long; keys per `timing`).
pub fn paste_grade_commands(grade: &ColorParams, src_duration: Time, targets: &[&Clip], timing: KeyTiming, label: &'static str) -> Option<Command> {
    let commands: Vec<Command> = targets
        .iter()
        .filter_map(|dst| {
            let mut c = (*dst).clone();
            c.color = grade_for(grade, src_duration, dst, timing);
            (c != **dst).then(|| Command::UpdateClip { clip: Box::new(c) })
        })
        .collect();
    (!commands.is_empty()).then_some(Command::Batch { label, commands })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keyframe::{Curve, Keyframe};

    fn clip(secs: i64) -> Clip {
        Clip::new("c", crate::clip::ClipKind::Gap, Time::from_secs(secs))
    }

    fn keyed(value: f64, at: Time) -> Param {
        let mut c = Curve::new();
        c.keys = vec![Keyframe::new(Time::ZERO, 0.0), Keyframe::new(at, value)];
        Param { value: 0.0, curve: Some(c) }
    }

    #[test]
    fn only_the_chosen_groups_are_pasted() {
        let mut src = clip(4);
        src.video.scale_x = Param::new(2.0);
        src.video.opacity = Param::new(0.5);
        src.audio.volume_db = Param::new(-6.0);
        src.color.saturation = Param::new(0.2);
        let dst = clip(4);
        let sel = AttributeSet { transform: true, ..AttributeSet::NONE };
        let out = apply(&src, &dst, &sel, KeyTiming::Maintain);
        assert_eq!(out.video.scale_x.value, 2.0);
        assert_eq!(out.video.opacity.value, dst.video.opacity.value);
        assert_eq!(out.audio.volume_db.value, dst.audio.volume_db.value);
        assert_eq!(out.color, dst.color);
        let all = apply(&src, &dst, &AttributeSet::default(), KeyTiming::Maintain);
        assert_eq!((all.video.opacity.value, all.audio.volume_db.value, all.color.saturation.value), (0.5, -6.0, 0.2));
        assert_eq!(all.id, dst.id);
    }

    #[test]
    fn stretch_scales_keys_to_the_target() {
        let mut src = clip(4);
        src.video.opacity = keyed(1.0, Time::from_secs(4));
        let mut n = ColorCorrection::named("Fade");
        n.set_key(Time::from_secs(2), Time(1));
        src.color.corrections.push(n);
        let dst = clip(8);
        let st = apply(&src, &dst, &AttributeSet::default(), KeyTiming::Stretch);
        assert_eq!(st.video.opacity.curve.as_ref().unwrap().keys[1].time, Time::from_secs(8));
        assert_eq!(st.color.corrections[0].keys[0].time, Time::from_secs(4));
        assert_ne!(st.color.corrections[0].id, src.color.corrections[0].id, "pasted nodes get their own ids");
        let kept = apply(&src, &dst, &AttributeSet::default(), KeyTiming::Maintain);
        assert_eq!(kept.video.opacity.curve.as_ref().unwrap().keys[1].time, Time::from_secs(4));
    }

    #[test]
    fn audio_effects_keep_the_targets_level_and_role() {
        let mut src = clip(4);
        src.audio.vocal_clarity = true;
        src.audio.volume_db = Param::new(-20.0);
        let mut dst = clip(4);
        dst.audio.volume_db = Param::new(3.0);
        let out = apply(&src, &dst, &AttributeSet { audio_effects: true, ..AttributeSet::NONE }, KeyTiming::Maintain);
        assert!(out.audio.vocal_clarity);
        assert_eq!(out.audio.volume_db.value, 3.0);
    }

    #[test]
    fn speed_is_pasted_as_a_constant_rate() {
        let mut src = clip(4);
        src.retime = Some(Retime::uniform(Time::from_secs(8), 2.0));
        src.duration = Time::from_secs(4);
        let dst = clip(6);
        let cmds = speed_commands(&src, &dst);
        match cmds.as_slice() {
            [Command::SetRetime { retime: Some(r), .. }] => assert_eq!(r.timeline_duration(), Time::from_secs(3)),
            other => panic!("{other:?}"),
        }
        assert!(speed_commands(&dst, &dst).is_empty());
        // a sped-up target goes back to 100 %
        assert!(matches!(speed_commands(&clip(2), &src).as_slice(), [Command::SetRetime { retime: None, .. }]));
    }

    #[test]
    fn paste_skips_the_source_and_unchanged_clips() {
        let mut src = clip(4);
        src.color.exposure = Param::new(1.0);
        let same = src.clone();
        let other = clip(4);
        assert!(paste_commands(&src, &[&src, &same], &AttributeSet::default(), KeyTiming::Maintain).is_none());
        match paste_commands(&src, &[&other], &AttributeSet::default(), KeyTiming::Maintain) {
            Some(Command::Batch { commands, .. }) => assert_eq!(commands.len(), 1),
            other => panic!("{other:?}"),
        }
        assert!(paste_grade_commands(&src.color, src.duration, &[&other], KeyTiming::Maintain, "Paste Grade").is_some());
    }
}
