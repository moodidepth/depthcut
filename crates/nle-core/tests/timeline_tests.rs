//! End-to-end tests for the nle-core data model.

use nle_core::autosave::Autosaver;
use nle_core::clip::*;
use nle_core::command::{Command, History};
use nle_core::eval::{audio_spans, evaluate, LayerSource, RenderNode};
use nle_core::fcpxml;
use nle_core::keyframe::{cubic_bezier_ease, Curve, Handle, Interpolation, Keyframe, Param};
use nle_core::speed::{FrameBlendMode, Retime, RetimeSegment};
use nle_core::*;
use std::time::{Duration, Instant};

fn secs(s: i64) -> Time {
    Time::from_secs(s)
}

fn asset(name: &str, dur: i64) -> MediaAsset {
    let mut a = MediaAsset::new(name, format!("/media/{name}.mp4"), secs(dur));
    a.video = Some(VideoStreamInfo {
        width: 3840,
        height: 2160,
        frame_rate: FrameRate::FPS_29_97,
        codec: "hevc".into(),
        pix_fmt: "yuv420p10le".into(),
        bit_depth: 10,
        color_space: "bt709".into(),
        color_transfer: String::new(),
        color_primaries: String::new(),
        is_vfr: false,
        pts_table: Vec::new(),
    });
    a.audio = Some(AudioStreamInfo { sample_rate: 48_000, channels: 2, codec: "aac".into() });
    a
}

fn clip(name: &str, dur: i64) -> Clip {
    Clip::from_asset(&asset(name, dur))
}

fn names(tl: &Timeline) -> Vec<String> {
    tl.spine.iter().map(|c| c.name.clone()).collect()
}

fn durations(tl: &Timeline) -> Vec<i64> {
    tl.spine.iter().map(|c| c.duration.ticks() / TICKS_PER_SECOND).collect()
}

/// Spine invariant: no clip has a non-positive duration.
fn assert_magnetic(tl: &Timeline) {
    for c in &tl.spine {
        assert!(c.duration.is_positive(), "zero-length clip {}", c.name);
        assert_eq!(c.lane, 0);
    }
    let starts = tl.spine_starts();
    for w in starts.windows(2).zip(&tl.spine) {
        assert_eq!(w.0[1] - w.0[0], w.1.duration, "storyline must be gapless");
    }
}

// ---------------------------------------------------------------------------
// Magnetic timeline
// ---------------------------------------------------------------------------

#[test]
fn ripple_insert_at_boundary_shifts_downstream() {
    let mut tl = Timeline::default();
    tl.append(clip("A", 5)).unwrap();
    tl.append(clip("B", 3)).unwrap();
    let b_id = tl.spine[1].id;
    assert_eq!(tl.clip_start(b_id), Some(secs(5)));

    tl.insert(secs(5), clip("X", 2)).unwrap();
    assert_eq!(names(&tl), ["A", "X", "B"]);
    assert_eq!(tl.clip_start(b_id), Some(secs(7)), "B rippled right by 2s");
    assert_eq!(tl.duration(), secs(10));
    assert_magnetic(&tl);
}

#[test]
fn ripple_insert_mid_clip_splits_it() {
    let mut tl = Timeline::default();
    tl.append(clip("A", 10)).unwrap();
    tl.insert(secs(4), clip("X", 2)).unwrap();
    assert_eq!(names(&tl), ["A", "X", "A"]);
    assert_eq!(durations(&tl), [4, 2, 6]);
    // the right half continues where the left half stopped in the source
    assert_eq!(tl.spine[2].source_in, secs(4));
    assert_eq!(tl.duration(), secs(12));
    assert_magnetic(&tl);
}

#[test]
fn insert_past_end_appends_without_gap() {
    let mut tl = Timeline::default();
    tl.append(clip("A", 2)).unwrap();
    tl.insert(secs(100), clip("B", 1)).unwrap();
    assert_eq!(names(&tl), ["A", "B"]);
    assert_eq!(tl.duration(), secs(3));
}

#[test]
fn ripple_delete_closes_hole() {
    let mut tl = Timeline::default();
    tl.append(clip("A", 2)).unwrap();
    let b = tl.append(clip("B", 3)).unwrap();
    let c = tl.append(clip("C", 4)).unwrap();
    tl.remove_ripple(b).unwrap();
    assert_eq!(names(&tl), ["A", "C"]);
    assert_eq!(tl.clip_start(c), Some(secs(2)));
    assert_magnetic(&tl);
}

#[test]
fn shift_delete_replaces_with_gap_and_keeps_sync() {
    let mut tl = Timeline::default();
    tl.append(clip("A", 2)).unwrap();
    let b = tl.append(clip("B", 3)).unwrap();
    let c = tl.append(clip("C", 4)).unwrap();
    // a title connected to B must survive on the gap
    tl.connect(Clip::title(TitleParams::default(), secs(1)), secs(3), 1).unwrap();
    let gap = tl.remove_with_gap(b).unwrap();
    assert_eq!(names(&tl), ["A", "Gap", "C"]);
    assert!(tl.clip(gap).unwrap().is_gap());
    assert_eq!(tl.clip_start(c), Some(secs(5)), "downstream did not move");
    assert_eq!(tl.spine[1].connected.len(), 1, "connected clip stays attached to gap");
    assert_magnetic(&tl);
}

#[test]
fn blade_splits_media_keyframes_and_connected_clips() {
    let mut tl = Timeline::default();
    let mut a = clip("A", 10);
    a.source_in = secs(1);
    a.video.opacity.add_keyframe(secs(0));
    a.video.opacity.curve.as_mut().unwrap().set_value_at(secs(10), 0.0);
    a.transition_out = Some(Transition::new(TransitionKind::CrossDissolve, secs(1)));
    tl.append(a).unwrap();
    let early = tl.connect(clip("early", 1), secs(2), 1).unwrap();
    let late = tl.connect(clip("late", 1), secs(7), -1).unwrap();

    let (l, r) = tl.blade(secs(6)).unwrap();
    assert_eq!(durations(&tl), [6, 4]);
    let (lc, rc) = (tl.clip(l).unwrap(), tl.clip(r).unwrap());
    assert_eq!(lc.source_in, secs(1));
    assert_eq!(rc.source_in, secs(7));
    assert!(lc.transition_out.is_none());
    assert!(rc.transition_out.is_some(), "outgoing transition stays at the far end");

    // keyframed opacity is continuous across the cut
    let at_cut = 1.0 - 6.0 / 10.0;
    assert!((lc.video.opacity.at(secs(6)) - at_cut).abs() < 1e-9);
    assert!((rc.video.opacity.at(Time::ZERO) - at_cut).abs() < 1e-9);
    assert!((rc.video.opacity.at(secs(4)) - 0.0).abs() < 1e-9);

    // connected clips follow their position
    assert!(lc.connected.iter().any(|c| c.id == early));
    let late_c = rc.connected.iter().find(|c| c.id == late).expect("late moved to right half");
    assert_eq!(late_c.offset, secs(1));
    assert_eq!(tl.clip_start(late), Some(secs(7)), "absolute position unchanged");

    assert_eq!(tl.blade(secs(6)), Err(EditError::AtBoundary));
    assert_magnetic(&tl);
}

#[test]
fn connected_clips_move_with_parent() {
    let mut tl = Timeline::default();
    tl.append(clip("A", 4)).unwrap();
    let b = tl.append(clip("B", 4)).unwrap();
    let broll = tl.connect(clip("broll", 2), secs(5), 1).unwrap();
    assert_eq!(tl.clip_start(broll), Some(secs(5)));
    // insert before the parent → B-roll ripples with it
    tl.insert(Time::ZERO, clip("intro", 3)).unwrap();
    assert_eq!(tl.clip_start(b), Some(secs(7)));
    assert_eq!(tl.clip_start(broll), Some(secs(8)));
    // deleting the parent deletes the connected clip too
    tl.remove_ripple(b).unwrap();
    assert!(tl.clip(broll).is_none());
}

#[test]
fn connect_beyond_end_appends_gap() {
    let mut tl = Timeline::default();
    tl.append(clip("A", 2)).unwrap();
    let t = tl.connect(clip("music", 5), secs(4), -1).unwrap();
    assert_eq!(names(&tl), ["A", "Gap"]);
    assert_eq!(tl.clip_start(t), Some(secs(4)));
    assert!(tl.duration() >= secs(9));
}

#[test]
fn trims_ripple_and_respect_media_bounds() {
    let mut tl = Timeline::default();
    let a = tl.append(clip("A", 10)).unwrap();
    let b = tl.append(clip("B", 10)).unwrap();
    // shorten A by 3s → B moves left
    assert_eq!(tl.trim_end(a, secs(-3)).unwrap(), secs(-3));
    assert_eq!(tl.clip_start(b), Some(secs(7)));
    // cannot extend past the end of the media
    assert_eq!(tl.trim_end(a, secs(10)).unwrap(), secs(3));
    // trim head: remove 2s of A
    assert_eq!(tl.trim_start(a, secs(2)).unwrap(), secs(2));
    assert_eq!(tl.clip(a).unwrap().source_in, secs(2));
    assert_eq!(tl.clip_start(b), Some(secs(8)));
    // cannot reveal media before the source start
    assert_eq!(tl.trim_start(a, secs(-5)).unwrap(), secs(-2));
    assert_eq!(tl.clip(a).unwrap().source_in, Time::ZERO);
    // slip within bounds
    tl.trim_end(a, secs(-4)).unwrap();
    assert_eq!(tl.slip(a, secs(100)).unwrap(), secs(4));
    assert_eq!(tl.clip(a).unwrap().source_in, secs(4));
    assert_magnetic(&tl);
}

#[test]
fn range_delete_and_overwrite() {
    let mut tl = Timeline::default();
    tl.append(clip("A", 10)).unwrap();
    tl.range_delete(secs(2), secs(5)).unwrap();
    assert_eq!(durations(&tl), [2, 5]);
    tl.overwrite(secs(1), clip("X", 2)).unwrap();
    assert_eq!(names(&tl), ["A", "X", "A"]);
    assert_eq!(tl.duration(), secs(7));
    assert_magnetic(&tl);
}

#[test]
fn move_clip_reorders_and_lifts() {
    let mut tl = Timeline::default();
    let a = tl.append(clip("A", 2)).unwrap();
    tl.append(clip("B", 2)).unwrap();
    tl.append(clip("C", 2)).unwrap();
    tl.move_clip(a, secs(6), 0).unwrap();
    assert_eq!(names(&tl), ["B", "C", "A"]);
    // lift C into lane 1: a gap keeps everything in sync
    let c = tl.spine[1].id;
    tl.move_clip(c, secs(2), 1).unwrap();
    assert_eq!(names(&tl), ["B", "Gap", "A"]);
    assert_eq!(tl.clip_start(c), Some(secs(2)));
}

#[test]
fn snapping_finds_nearest_edit_point() {
    let mut tl = Timeline::default();
    tl.append(clip("A", 2)).unwrap();
    tl.append(clip("B", 3)).unwrap();
    let (t, snapped) = tl.snap(Time::from_millis(2_050), Time::from_millis(100), &[], None);
    assert!(snapped);
    assert_eq!(t, secs(2));
    tl.snapping = false;
    let (t, snapped) = tl.snap(Time::from_millis(2_050), Time::from_millis(100), &[], None);
    assert!(!snapped);
    assert_eq!(t, Time::from_millis(2_050));
}

// ---------------------------------------------------------------------------
// Compound clips
// ---------------------------------------------------------------------------

#[test]
fn compound_clip_nesting_and_break_apart() {
    let mut tl = Timeline::default();
    let a = tl.append(clip("A", 2)).unwrap();
    let b = tl.append(clip("B", 3)).unwrap();
    let c = tl.append(clip("C", 4)).unwrap();
    let d = tl.append(clip("D", 1)).unwrap();
    let title = tl.connect(Clip::title(TitleParams::default(), secs(1)), secs(3), 1).unwrap();

    assert_eq!(tl.make_compound(&[a, c], "bad"), Err(EditError::NotContiguous));

    let comp = tl.make_compound(&[b, c], "Compound Clip").unwrap();
    assert_eq!(names(&tl), ["A", "Compound Clip", "D"]);
    assert_eq!(tl.clip(comp).unwrap().duration, secs(7));
    assert_eq!(tl.clip_start(d), Some(secs(9)));
    // nested clips (and the title connected to B) are still findable
    assert!(tl.clip(b).is_some());
    assert!(tl.clip(title).is_some());

    // the compound renders its inner storyline
    let comp_at = evaluate(&tl, secs(6));
    match &comp_at.nodes[0] {
        RenderNode::Layer(l) => match &l.source {
            LayerSource::Compound(inner) => {
                assert!(inner.media_requests().iter().any(|(_, t)| *t == secs(1)), "C at 1s into its media");
            }
            other => panic!("expected compound, got {other:?}"),
        },
        other => panic!("unexpected {other:?}"),
    }

    // compound of a compound
    let outer = tl.make_compound(&[a, comp], "Outer").unwrap();
    assert_eq!(tl.spine.len(), 2);
    let ids = tl.break_apart(outer).unwrap();
    assert_eq!(ids, vec![a, comp]);
    let ids = tl.break_apart(comp).unwrap();
    assert_eq!(ids, vec![b, c]);
    assert_eq!(names(&tl), ["A", "B", "C", "D"]);
    assert_eq!(tl.clip_start(title), Some(secs(3)));
    assert_eq!(tl.break_apart(a), Err(EditError::NotCompound));
    assert_magnetic(&tl);
}

#[test]
fn break_apart_trimmed_compound_extracts_used_range() {
    let mut tl = Timeline::default();
    let a = tl.append(clip("A", 4)).unwrap();
    let b = tl.append(clip("B", 4)).unwrap();
    let comp = tl.make_compound(&[a, b], "CC").unwrap();
    tl.trim_start(comp, secs(1)).unwrap();
    tl.trim_end(comp, secs(-2)).unwrap();
    assert_eq!(tl.duration(), secs(5));
    tl.break_apart(comp).unwrap();
    assert_eq!(durations(&tl), [3, 2]);
    assert_eq!(tl.spine[0].source_in, secs(1));
    assert_eq!(tl.duration(), secs(5));
}

// ---------------------------------------------------------------------------
// Keyframes
// ---------------------------------------------------------------------------

#[test]
fn bezier_keyframe_interpolation() {
    // CSS reference values for ease-in-out (0.42, 0, 0.58, 1)
    let y = cubic_bezier_ease(0.42, 0.0, 0.58, 1.0, 0.25);
    assert!((y - 0.129_161).abs() < 1e-3, "{y}");

    let mut c = Curve::new();
    c.insert(Keyframe::new(Time::ZERO, 0.0).with_interp(Interpolation::EaseInOut));
    c.insert(Keyframe::new(secs(4), 100.0));
    assert!((c.eval(secs(2)) - 50.0).abs() < 1e-6, "symmetric ease is 50% at midpoint");
    assert!(c.eval(secs(1)) < 25.0);
    assert!(c.eval(secs(3)) > 75.0);

    // free Bézier with an overshooting handle
    let mut k0 = Keyframe::new(Time::ZERO, 0.0).with_interp(Interpolation::Bezier);
    k0.out_handle = Handle { dt: 0.3, dv: 150.0 };
    let mut k1 = Keyframe::new(secs(1), 100.0);
    k1.in_handle = Handle { dt: 0.3, dv: 0.0 };
    let mut b = Curve::new();
    b.insert(k0);
    b.insert(k1);
    let peak = (0..=100).map(|i| b.eval(Time::from_millis(i * 10))).fold(f64::MIN, f64::max);
    assert!(peak > 100.0, "handle should overshoot, got {peak}");
    assert_eq!(b.eval(Time::ZERO), 0.0);
    assert!((b.eval(secs(1)) - 100.0).abs() < 1e-9);

    // hold
    let mut h = Curve::new();
    h.insert(Keyframe::new(Time::ZERO, 1.0).with_interp(Interpolation::Hold));
    h.insert(Keyframe::new(secs(1), 2.0));
    assert_eq!(h.eval(Time::from_millis(999)), 1.0);
    assert_eq!(h.eval(secs(1)), 2.0);

    // Param falls back to the static value
    let mut p = Param::new(0.5);
    assert_eq!(p.at(secs(3)), 0.5);
    p.add_keyframe(Time::ZERO);
    p.add_keyframe(secs(2));
    p.set(secs(2), 1.5);
    assert!((p.at(secs(1)) - 1.0).abs() < 1e-9);
}

// ---------------------------------------------------------------------------
// Retiming
// ---------------------------------------------------------------------------

#[test]
fn speed_presets_map_time() {
    for (speed, expected_dur) in [(0.25, 8.0), (0.5, 4.0), (2.0, 1.0), (4.0, 0.5)] {
        let r = Retime::uniform(secs(2), speed);
        assert!((r.timeline_duration().as_secs_f64() - expected_dur).abs() < 1e-9);
        let half = r.timeline_duration().div_i64(2);
        assert!((r.map(half).as_secs_f64() - 1.0).abs() < 1e-6, "midpoint maps to source midpoint");
    }
}

#[test]
fn reverse_and_freeze_frame() {
    let rev = Retime::uniform(secs(4), -1.0);
    assert_eq!(rev.map(Time::ZERO), secs(4));
    assert_eq!(rev.map(secs(4)), Time::ZERO);
    assert_eq!(rev.map(secs(1)), secs(3));

    let mut r = Retime::uniform(secs(4), 1.0);
    r.freeze_frame(secs(2), secs(1));
    assert_eq!(r.timeline_duration(), secs(5));
    assert_eq!(r.map(Time::from_millis(2_500)), secs(2), "held frame");
    assert_eq!(r.map(secs(3)), secs(2));
    assert_eq!(r.map(secs(4)), secs(3));
    assert_eq!(r.map(secs(5)), secs(4));
}

#[test]
fn speed_ramp_is_smooth_continuous_and_monotonic() {
    // 100% for 2s, then ramp over 1s down to 25% for 4s
    let mut seg2 = RetimeSegment::new(secs(4), 0.25);
    seg2.ramp = secs(1);
    let r = Retime { segments: vec![RetimeSegment::new(secs(2), 1.0), seg2], start_offset: Time::ZERO, lead_speed: None, blend: FrameBlendMode::OpticalFlow, velocity_curve: None };
    // smoothstep ramp integral: 1s * (1 + 0.25)/2 = 0.625, then 3s * 0.25
    let end = r.map_secs(6.0);
    assert!((end - (2.0 + 0.625 + 0.75)).abs() < 1e-9, "{end}");
    assert!((r.speed_at(secs(1)) - 1.0).abs() < 1e-12);
    assert!((r.speed_at(Time::from_millis(2_500)) - 0.625).abs() < 1e-9, "smoothstep midpoint");
    assert!((r.speed_at(secs(5)) - 0.25).abs() < 1e-12);

    let mut prev = -1.0;
    for i in 0..=600 {
        let v = r.map_secs(i as f64 / 100.0);
        assert!(v >= prev, "monotonic");
        assert!(v - prev.max(0.0) < 0.011, "continuous");
        prev = v;
    }
    // inverse mapping
    let t = r.inverse(Time::from_secs_f64(2.3)).unwrap();
    assert!((r.map(t).as_secs_f64() - 2.3).abs() < 1e-6);

    // splitting mid-ramp preserves the mapping exactly
    let (left, right) = r.split_at(Time::from_millis(2_400));
    for ms in [0, 500, 2_000, 2_399] {
        assert!((left.map(Time::from_millis(ms)) - r.map(Time::from_millis(ms))).abs() <= Time(2));
    }
    for ms in [0, 300, 1_000, 3_600] {
        let expect = r.map(Time::from_millis(2_400 + ms));
        assert!((right.map(Time::from_millis(ms)) - expect).abs() <= Time(2), "ms={ms}");
    }
}

#[test]
fn retimed_clip_on_timeline() {
    let mut tl = Timeline::default();
    let mut h = History::default();
    let a = clip("A", 4);
    let a_id = a.id;
    h.execute(&mut tl, Command::Append { clip: a }).unwrap();
    h.execute(&mut tl, Command::SetRetime { id: a_id, retime: Some(Retime::uniform(secs(4), 0.5)) }).unwrap();
    assert_eq!(tl.duration(), secs(8), "50% doubles the length");
    let c = tl.clip(a_id).unwrap();
    assert_eq!(c.source_time(secs(6)), secs(3));
    // blade a retimed clip: both halves agree on source time
    let (l, r) = tl.blade(secs(3)).unwrap();
    assert_eq!(tl.clip(l).unwrap().source_time(secs(3)), tl.clip(r).unwrap().source_time(Time::ZERO));
    // freeze frame
    h.execute(&mut tl, Command::FreezeFrame { id: r, at: secs(1), hold: secs(2) }).unwrap();
    assert_eq!(tl.duration(), secs(10));
    // remove retime → back to 100%
    h.execute(&mut tl, Command::SetRetime { id: l, retime: None }).unwrap();
    assert_eq!(tl.clip(l).unwrap().duration, Time::from_millis(1_500));
}

// ---------------------------------------------------------------------------
// Undo / redo
// ---------------------------------------------------------------------------

#[test]
fn undo_redo_state_integrity() {
    let mut tl = Timeline::default();
    let mut h = History::default();
    let mut states = vec![tl.clone()];

    let a = clip("A", 10);
    let a_id = a.id;
    h.execute(&mut tl, Command::Append { clip: a }).unwrap();
    states.push(tl.clone());
    h.execute(&mut tl, Command::Insert { at: secs(4), clip: clip("B", 2) }).unwrap();
    states.push(tl.clone());
    h.execute(&mut tl, Command::Blade { at: secs(9) }).unwrap();
    states.push(tl.clone());
    h.execute(&mut tl, Command::Connect { clip: clip("broll", 1), at: secs(1), lane: 1 }).unwrap();
    states.push(tl.clone());
    h.execute(&mut tl, Command::DeleteWithGap { id: a_id }).unwrap();
    states.push(tl.clone());
    let ids: Vec<ClipId> = tl.spine[1..3].iter().map(|c| c.id).collect();
    h.execute(&mut tl, Command::MakeCompound { ids, name: "CC".into() }).unwrap();
    states.push(tl.clone());

    // a failing command leaves state and history untouched
    let before = tl.clone();
    assert!(h.execute(&mut tl, Command::Blade { at: secs(1000) }).is_err());
    assert_eq!(tl, before);
    assert_eq!(h.undo_len(), states.len() - 1);

    // undo all the way back, checking every intermediate state
    for expected in states.iter().rev().skip(1) {
        assert!(h.undo(&mut tl));
        assert_eq!(&tl, expected);
    }
    assert!(!h.undo(&mut tl));
    // redo all the way forward — identical states, including generated ids
    for expected in states.iter().skip(1) {
        assert!(h.redo(&mut tl));
        assert_eq!(&tl, expected);
    }
    assert!(!h.redo(&mut tl));

    // a new edit after undo clears the redo stack
    h.undo(&mut tl);
    h.execute(&mut tl, Command::Append { clip: clip("Z", 1) }).unwrap();
    assert!(!h.can_redo());
}

#[test]
fn coalesced_parameter_edits_are_one_undo_step() {
    let mut tl = Timeline::default();
    let mut h = History::default();
    let a = clip("A", 5);
    let id = a.id;
    h.execute(&mut tl, Command::Append { clip: a }).unwrap();
    let original = tl.clone();
    for i in 1..=10 {
        let mut c = tl.clip(id).unwrap().clone();
        c.video.opacity.value = 1.0 - i as f64 * 0.05;
        h.execute_coalesced(&mut tl, Command::UpdateClip { clip: Box::new(c) }, Some(42)).unwrap();
    }
    assert!((tl.clip(id).unwrap().video.opacity.value - 0.5).abs() < 1e-9);
    assert_eq!(h.undo_len(), 2);
    h.undo(&mut tl);
    assert_eq!(tl, original);
}

// ---------------------------------------------------------------------------
// Evaluation
// ---------------------------------------------------------------------------

#[test]
fn evaluation_layers_and_transitions() {
    let mut tl = Timeline::default();
    let mut a = clip("A", 4);
    a.transition_out = Some(Transition::new(TransitionKind::CrossDissolve, secs(2)));
    tl.append(a).unwrap();
    tl.append(clip("B", 4)).unwrap();
    tl.connect(Clip::title(TitleParams::default(), secs(2)), secs(5), 1).unwrap();
    let mut under = clip("under", 8);
    under.av.audio = false;
    tl.connect(under, Time::ZERO, -1).unwrap();

    // in the dissolve window [3s, 5s)
    let comp = evaluate(&tl, secs(4));
    let tr = comp.nodes.iter().find_map(|n| match n {
        RenderNode::Transition { from, to, transition } => Some((from, to, transition)),
        _ => None,
    });
    let (from, to, t) = tr.expect("transition active");
    assert!((t.progress - 0.5).abs() < 1e-6);
    assert!(from.is_some() && to.is_some());
    assert!(matches!(comp.nodes[0], RenderNode::Layer(ref l) if l.lane == -1), "lower lane composites first");

    // after the window: B plus the title on top
    let comp = evaluate(&tl, Time::from_millis(5_500));
    assert_eq!(comp.nodes.len(), 3);
    assert!(matches!(&comp.nodes[2], RenderNode::Layer(l) if matches!(l.source, LayerSource::Title(..))));

    // audio spans: A, B (the lane -1 clip has audio disabled)
    let spans = audio_spans(&tl);
    assert_eq!(spans.len(), 2);
    assert_eq!(spans[1].source_time(secs(5)), secs(1));
}

// ---------------------------------------------------------------------------
// FCPXML
// ---------------------------------------------------------------------------

fn sample_project() -> Project {
    let mut p = Project::new("Round Trip Event");
    p.timeline.name = "My Cut".into();
    p.timeline.format = VideoFormat::uhd(FrameRate::FPS_29_97);
    let mut a1 = asset("Interview A", 120);
    a1.tags = vec!["interview".into(), "a-cam".into()];
    let mut a2 = asset("B Roll", 30);
    a2.path = "C:/Footage/B Roll #2.mov".into();
    let mut music = MediaAsset::new("Score", "/audio/score.wav", secs(200));
    music.kind = AssetKind::Audio;
    music.audio = Some(AudioStreamInfo { sample_rate: 48_000, channels: 2, codec: "pcm_s24le".into() });
    let (id1, id2) = (a1.id, a2.id);
    p.add_asset(a1.clone());
    p.add_asset(a2.clone());
    p.add_asset(music.clone());

    let tl = &mut p.timeline;
    let mut c1 = Clip::from_asset_range(&a1, secs(10), secs(8));
    c1.video.position_x.value = 0.1;
    c1.video.scale_x.value = 1.5;
    c1.video.rotation.value = 12.0;
    c1.video.opacity.add_keyframe(Time::ZERO);
    c1.video.opacity.curve.as_mut().unwrap().insert(Keyframe::new(secs(2), 0.25).with_interp(Interpolation::EaseOut));
    c1.video.blend_mode = BlendMode::Screen;
    c1.audio.volume_db.value = -6.0;
    c1.audio.fade_in = secs(1);
    c1.color.exposure.value = 0.5;
    c1.color.midtones = ColorWheel { r: 0.1, g: 0.0, b: -0.1, master: 0.05 };
    c1.color.lut = Some(LutRef { path: "/luts/film.cube".into(), intensity: 0.8 });
    c1.transition_out = Some(Transition::new(TransitionKind::WhipPan, secs(1)));
    tl.append(c1).unwrap();
    let mut c2 = Clip::from_asset_range(&a2, secs(2), secs(4));
    c2.retime = Some(Retime::uniform(secs(4), 0.5));
    c2.duration = secs(8);
    tl.append(c2).unwrap();
    tl.append(Clip::gap(secs(2))).unwrap();
    let c3 = Clip::from_asset_range(&a1, secs(50), secs(5));
    let c4 = Clip::from_asset_range(&a2, Time::ZERO, secs(3));
    let (i3, i4) = (c3.id, c4.id);
    tl.append(c3).unwrap();
    tl.append(c4).unwrap();
    tl.make_compound(&[i3, i4], "Nested").unwrap();

    tl.connect(Clip::title(TitleParams::lower_third("Jane Doe", "Director"), secs(4)), secs(1), 1).unwrap();
    let mut m = Clip::from_asset(&music);
    m.duration = secs(20);
    m.audio.ducking.enabled = true;
    tl.connect(m, Time::ZERO, -1).unwrap();
    let _ = (id1, id2);
    p
}

/// Compare the structurally meaningful parts of two clip trees.
fn assert_clips_equivalent(a: &[Clip], b: &[Clip], pa: &Project, pb: &Project) {
    assert_eq!(a.len(), b.len(), "clip count");
    for (x, y) in a.iter().zip(b) {
        assert_eq!(x.name, y.name);
        assert_eq!(x.duration, y.duration, "{} duration", x.name);
        assert_eq!(x.lane, y.lane, "{} lane", x.name);
        assert_eq!(x.offset, y.offset, "{} offset", x.name);
        match (&x.kind, &y.kind) {
            (ClipKind::Asset { asset: ia }, ClipKind::Asset { asset: ib }) => {
                assert_eq!(x.source_in, y.source_in);
                assert_eq!(pa.asset(*ia).unwrap().path, pb.asset(*ib).unwrap().path);
                assert_eq!(x.media_duration, y.media_duration);
            }
            (ClipKind::Gap, ClipKind::Gap) | (ClipKind::Adjustment, ClipKind::Adjustment) => {}
            (ClipKind::Title(ta), ClipKind::Title(tb)) => {
                assert_eq!(ta.text, tb.text);
                assert_eq!(ta.subtitle, tb.subtitle);
                assert_eq!(ta.style, tb.style);
                assert_eq!(ta.pill_enabled, tb.pill_enabled);
                assert_eq!(ta.font_size, tb.font_size);
            }
            (ClipKind::Compound(ca), ClipKind::Compound(cb)) => assert_clips_equivalent(&ca.spine, &cb.spine, pa, pb),
            (ka, kb) => panic!("kind mismatch {ka:?} vs {kb:?}"),
        }
        let close = |p: f64, q: f64| (p - q).abs() < 1e-5;
        assert!(close(x.video.position_x.value, y.video.position_x.value), "{} pos {} vs {}", x.name, x.video.position_x.value, y.video.position_x.value);
        assert!(close(x.video.scale_x.value, y.video.scale_x.value));
        assert!(close(x.video.rotation.value, y.video.rotation.value));
        if x.av.video && !x.is_gap() {
            assert_eq!(x.video.blend_mode, y.video.blend_mode);
            assert_eq!(x.video.opacity.curve, y.video.opacity.curve);
            assert!(close(x.color.exposure.value, y.color.exposure.value));
            assert_eq!(x.color.midtones, y.color.midtones);
            assert_eq!(x.color.lut, y.color.lut);
        }
        if x.has_audio() {
            assert!(close(x.audio.volume_db.value, y.audio.volume_db.value));
            assert_eq!(x.audio.fade_in, y.audio.fade_in);
        }
        assert_eq!(x.retime.as_ref().map(|r| r.timeline_duration()), y.retime.as_ref().map(|r| r.timeline_duration()));
        if let (Some(ra), Some(rb)) = (&x.retime, &y.retime) {
            assert!((ra.speed_at(Time::ZERO) - rb.speed_at(Time::ZERO)).abs() < 1e-9);
            assert_eq!(ra.map(secs(3)), rb.map(secs(3)));
        }
        assert_eq!(x.transition_out.as_ref().map(|t| (t.kind, t.duration)), y.transition_out.as_ref().map(|t| (t.kind, t.duration)));
        assert_clips_equivalent(&x.connected, &y.connected, pa, pb);
    }
}

#[test]
fn fcpxml_round_trip() {
    let p = sample_project();
    let xml = fcpxml::export(&p);
    assert!(xml.contains("<fcpxml version=\"1.10\">"));
    assert!(xml.contains("frameDuration=\"1001/30000s\""));
    assert!(xml.contains("<ref-clip"));
    assert!(xml.contains("lane=\"-1\""));
    assert!(xml.contains("file:///C:/Footage/B%20Roll%20%232.mov"));

    let q = fcpxml::import(&xml).expect("parse exported fcpxml");
    assert_eq!(q.name, p.name);
    assert_eq!(q.timeline.name, p.timeline.name);
    assert_eq!(q.timeline.format, p.timeline.format);
    assert_eq!(q.assets.len(), p.assets.len());
    for (a, b) in p.assets.iter().zip(&q.assets) {
        assert_eq!(a.name, b.name);
        assert_eq!(a.path, b.path);
        assert_eq!(a.duration, b.duration);
        assert_eq!(a.kind, b.kind);
        assert_eq!(a.tags, b.tags);
        assert_eq!(
            a.video.as_ref().map(|v| (v.width, v.height, v.frame_rate, v.codec.clone(), v.bit_depth)),
            b.video.as_ref().map(|v| (v.width, v.height, v.frame_rate, v.codec.clone(), v.bit_depth))
        );
    }
    assert_eq!(q.timeline.duration(), p.timeline.duration());
    assert_clips_equivalent(&p.timeline.spine, &q.timeline.spine, &p, &q);

    // exporting the re-imported project yields identical XML (stable round trip)
    assert_eq!(fcpxml::export(&q), xml);
}

#[test]
fn blend_modes_keep_shader_indices_and_round_trip_fcpxml() {
    // the first five indices predate the new modes and are baked into old projects
    let old = [BlendMode::Normal, BlendMode::Multiply, BlendMode::Screen, BlendMode::Overlay, BlendMode::Add];
    for (i, b) in old.into_iter().enumerate() {
        assert_eq!(b.shader_index(), i as u32);
    }
    assert_eq!(BlendMode::ALL.len(), 14);
    for (i, b) in BlendMode::ALL.into_iter().enumerate() {
        assert_eq!(b.shader_index(), i as u32, "{b:?} index matches composite.wgsl");
        assert_eq!(BlendMode::from_name(b.label()), Some(b));
        let json = serde_json::to_string(&b).unwrap();
        assert_eq!(serde_json::from_str::<BlendMode>(&json).unwrap(), b);
    }
    assert_eq!(BlendMode::from_name("softlight"), Some(BlendMode::SoftLight));
    assert_eq!(BlendMode::from_name("color-dodge"), Some(BlendMode::ColorDodge));
    assert_eq!(BlendMode::from_name("Vivid Light"), None);

    let mut p = Project::new("Blend");
    let a = asset("Shot", 60);
    p.add_asset(a.clone());
    for b in BlendMode::ALL {
        let mut c = Clip::from_asset_range(&a, Time::ZERO, secs(1));
        c.video.blend_mode = b;
        p.timeline.append(c).unwrap();
    }
    let q = fcpxml::import(&fcpxml::export(&p)).expect("re-import");
    let modes: Vec<BlendMode> = q.timeline.spine.iter().map(|c| c.video.blend_mode).collect();
    assert_eq!(modes, BlendMode::ALL.to_vec());
}

#[test]
fn fcpxml_import_rejects_garbage() {
    assert!(fcpxml::import("<notfcp/>").is_err());
    assert!(fcpxml::import("<fcpxml><resources/></fcpxml>").is_err());
    assert!(fcpxml::import("<fcpxml").is_err());
}

// ---------------------------------------------------------------------------
// Autosave / project files
// ---------------------------------------------------------------------------

#[test]
fn autosave_is_atomic_periodic_and_recoverable() {
    let dir = std::env::temp_dir().join(format!("rcp-autosave-test-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let mut saver = Autosaver::new(&dir, "Test Project").with_interval(Duration::from_secs(30));
    let mut p = sample_project();
    let t0 = Instant::now();
    assert!(saver.tick_at(&p, t0).unwrap(), "first tick saves");
    assert!(!saver.tick_at(&p, t0 + Duration::from_secs(5)).unwrap(), "interval not elapsed");
    p.timeline.append(clip("late", 1)).unwrap();
    assert!(!saver.tick_at(&p, t0 + Duration::from_secs(29)).unwrap());
    assert!(saver.tick_at(&p, t0 + Duration::from_secs(31)).unwrap(), "changed + interval elapsed");
    assert!(!saver.tick_at(&p, t0 + Duration::from_secs(62)).unwrap(), "unchanged content is not rewritten");
    assert!(!dir.join("Test_Project.autosave.json.tmp").exists(), "no temp file left behind");

    let (rec, _) = saver.recover().expect("recover");
    assert_eq!(rec, p);
    // a corrupt primary falls back to the previous snapshot
    std::fs::write(saver.path(), b"{ truncated").unwrap();
    let (prev, _) = saver.recover().expect("fallback to .prev");
    assert_eq!(prev.timeline.spine.len(), p.timeline.spine.len() - 1);

    // full project save/load
    let file = dir.join("project.rcp.json");
    p.save(&file).unwrap();
    assert_eq!(Project::load(&file).unwrap(), p);
    // ids generated after loading never collide
    let fresh = ClipId::new();
    assert!(!p.timeline.all_ids().contains(&fresh));
    saver.discard();
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn ripple_trim_q_w_and_sync_clips_to_beats() {
    let mut tl = Timeline::new("Montage", VideoFormat::default());
    let c1 = tl.append(clip("frag1", 4)).unwrap();
    let c2 = tl.append(clip("frag2", 4)).unwrap();
    assert_eq!(tl.duration(), secs(8));

    // Q at 1s trims start of frag1 to 1s and ripples frag2 earlier by 1s.
    tl.ripple_trim_start_to(c1, secs(1)).unwrap();
    assert_eq!(tl.clip(c1).unwrap().duration, secs(3));
    assert_eq!(tl.clip_start(c2), Some(secs(3)));

    // W at 2s trims end of frag1 to 2s and ripples frag2 to 2s.
    tl.ripple_trim_end_to(c1, secs(2)).unwrap();
    assert_eq!(tl.clip(c1).unwrap().duration, secs(2));
    assert_eq!(tl.clip_start(c2), Some(secs(2)));

    // Add a sub-bass kick marker at 2.5s and snare marker at 5.5s, then sync!
    tl.markers.push(Marker::sub_bass(Time::from_millis(2500)));
    tl.markers.push(Marker::snare(Time::from_millis(5500)));
    let synced = tl.sync_clips_to_beats(&[]);
    assert!(synced >= 1);
    assert_eq!(tl.clip(c1).unwrap().duration, Time::from_millis(2500));
}

// ----- regressions from the 2026-11-17 bug hunt ------------------------------

fn slow_or_fast_clip(asset: &MediaAsset, dur: i64, speed: f64) -> Clip {
    let mut c = Clip::from_asset(asset);
    c.duration = Time::from_secs(dur);
    c.retime = Some(Retime {
        segments: vec![RetimeSegment::new(Time::from_secs(dur), speed)],
        start_offset: Time::ZERO,
        lead_speed: None,
        blend: FrameBlendMode::FrameBlend,
        velocity_curve: None,
    });
    c
}

#[test]
fn retimed_head_trim_cannot_reveal_media_before_source_start() {
    let a = MediaAsset::new("a", "/m/a.mp4", Time::from_secs(10));
    let mut tl = Timeline::default();
    let id = tl.append(slow_or_fast_clip(&a, 4, 0.5)).unwrap();
    tl.trim_start(id, -Time::from_secs(2)).unwrap();
    let c = tl.clip(id).unwrap();
    let src0 = c.source_in.as_secs_f64() + c.retime.as_ref().unwrap().map_secs(0.0);
    assert!(src0 >= -1e-6, "head trim revealed source before 0: {src0}s");
}

#[test]
fn retimed_tail_trim_stops_at_media_end() {
    let a = MediaAsset::new("a", "/m/a.mp4", Time::from_secs(10));
    let mut tl = Timeline::default();
    let id = tl.append(slow_or_fast_clip(&a, 4, 2.0)).unwrap();
    tl.trim_end(id, Time::from_secs(20)).unwrap();
    let c = tl.clip(id).unwrap();
    let src_end = c.source_in.as_secs_f64() + c.retime.as_ref().unwrap().map_secs(c.duration.as_secs_f64());
    assert!(src_end <= 10.0 + 1e-3, "tail trim ran past media end: {src_end}s of 10s");
    assert!(src_end > 9.9, "tail trim should reach the media end, got {src_end}s");
}

#[test]
fn undoing_back_to_saved_state_is_clean() {
    let a = MediaAsset::new("a", "/m/a.mp4", Time::from_secs(10));
    let mut tl = Timeline::default();
    tl.append(Clip::from_asset(&a)).unwrap();
    let mut h = History::new(10);
    let saved = h.state_id();
    h.execute(&mut tl, Command::Blade { at: Time::from_secs(3) }).unwrap();
    assert_ne!(h.state_id(), saved);
    h.undo(&mut tl);
    assert_eq!(h.state_id(), saved, "undo back to the saved state");
    h.redo(&mut tl);
    assert_ne!(h.state_id(), saved);
    // a different edit from the saved state is not "clean"
    h.undo(&mut tl);
    h.execute(&mut tl, Command::Blade { at: Time::from_secs(5) }).unwrap();
    assert_ne!(h.state_id(), saved);
}

#[test]
fn connected_clip_left_edge_trim_keeps_its_end() {
    let a = MediaAsset::new("a", "/m/a.mp4", Time::from_secs(20));
    let mut tl = Timeline::default();
    tl.append(Clip::from_asset(&a)).unwrap();
    let mut b = Clip::from_asset(&a);
    b.duration = Time::from_secs(5);
    let id = tl.connect(b, Time::from_secs(2), 1).unwrap();
    let end_before = tl.clip_start(id).unwrap() + tl.clip(id).unwrap().duration;
    tl.trim_start(id, Time::from_secs(1)).unwrap();
    let start = tl.clip_start(id).unwrap();
    assert_eq!(start, Time::from_secs(3));
    assert_eq!(start + tl.clip(id).unwrap().duration, end_before);
}

#[test]
fn captions_follow_ripple_delete() {
    let a = MediaAsset::new("a", "/m/a.mp4", Time::from_secs(10));
    let mut tl = Timeline::default();
    let first = tl.append(Clip::from_asset(&a)).unwrap();
    tl.append(Clip::from_asset(&a)).unwrap();
    tl.captions.segments.push(CaptionSegment::new(Time::from_secs(12), Time::from_secs(13), "hello"));
    let mut h = History::new(10);
    h.execute(&mut tl, Command::DeleteRipple { id: first }).unwrap();
    assert_eq!(tl.captions.segments[0].start, Time::from_secs(2));
    h.undo(&mut tl);
    assert_eq!(tl.captions.segments[0].start, Time::from_secs(12), "undo restores caption timing");
}
