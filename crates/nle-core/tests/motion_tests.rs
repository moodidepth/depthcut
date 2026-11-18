//! Phase 3 model behaviour: tracked masks / effects, follow, smart reframe,
//! stabilization in evaluation, scene-cut blading and FCPXML round trips.

use nle_core::color::{MaskShape, ShapeMask};
use nle_core::effects::{Effect, EffectKind};
use nle_core::eval::{evaluate, reframe_pan_for, RenderNode};
use nle_core::motion::{self, Follow, MotionModel, Stabilization, Track, TrackSample};
use nle_core::speed::{FrameBlendMode, Retime};
use nle_core::*;

fn secs(s: i64) -> Time {
    Time::from_secs(s)
}

fn asset(name: &str, dur: i64) -> MediaAsset {
    let mut a = MediaAsset::new(name, format!("/media/{name}.mp4"), secs(dur));
    a.video = Some(VideoStreamInfo {
        width: 1920,
        height: 1080,
        frame_rate: FrameRate::FPS_30,
        codec: "h264".into(),
        pix_fmt: "yuv420p".into(),
        bit_depth: 8,
        color_space: "bt709".into(),
        color_transfer: String::new(),
        color_primaries: String::new(),
        is_vfr: false,
        pts_table: Vec::new(),
    });
    a
}

/// A track whose subject moves right 0.1 (normalised) per source second.
fn moving_track() -> Track {
    let mut t = Track::new("Face", MotionModel::Translation, [0.4, 0.4, 0.2, 0.2], 0.0, 16.0 / 9.0);
    for i in 1..=40 {
        let s = i as f64 * 0.25;
        t.insert(TrackSample { t: s, h: motion::translation(0.1 * s as f32, 0.0) });
    }
    t
}

fn layer(comp: &Composition, i: usize) -> &Layer {
    match &comp.nodes[i] {
        RenderNode::Layer(l) => l,
        _ => panic!("not a layer"),
    }
}

#[test]
fn tracked_censor_mask_and_reframe_follow_the_subject() {
    let a = asset("Interview", 30);
    let mut tl = Timeline::default();
    // source 5 s .. 15 s placed at timeline 0
    let mut c = Clip::from_asset_range(&a, secs(5), secs(10));
    let tr = moving_track();
    let id = tr.id;
    c.tracks.push(tr);
    let mut blur = Effect::new(EffectKind::Pixelate);
    blur.mask = Some(ShapeMask { center: [0.5, 0.5], size: [0.2, 0.2], track: Some(id), ..ShapeMask::new(MaskShape::Rectangle) });
    c.effects.push(blur);
    let mut rf = Effect::new(EffectKind::GamingReframe916);
    rf.track = Some(id);
    c.effects.push(rf);
    tl.append(c).unwrap();

    // timeline 2 s → source 7 s → subject moved +0.7
    let comp = evaluate(&tl, secs(2));
    let l = layer(&comp, 0);
    let m = l.effects[0].mask.as_ref().expect("mask");
    assert!((m.center[0] - 1.2).abs() < 1e-4 && (m.center[1] - 0.5).abs() < 1e-4, "{:?}", m.center);
    let reframe = l.effects.iter().find(|e| e.kind == EffectKind::GamingReframe916).unwrap();
    // subject centre at 0.5 + 0.7 → clamps to the right edge
    assert!((reframe.get("Pan X") - 1.0).abs() < 1e-4);
    let comp = evaluate(&tl, Time::ZERO);
    let reframe = *layer(&comp, 0).effects.iter().find(|e| e.kind == EffectKind::GamingReframe916).unwrap();
    // source 5 s → x = 1.0, still right; check the formula on a middle value
    let pan = reframe_pan_for(0.6, 2, false, 16.0 / 9.0).unwrap();
    assert!((pan - 0.2 / (1.0 - (9.0 / 16.0) / (16.0 / 9.0))).abs() < 1e-5);
    assert!(reframe.get("Pan X") > 0.99);
    assert_eq!(reframe_pan_for(0.5, 1, false, 16.0 / 9.0), None, "layout without a window");
}

#[test]
fn titles_follow_a_track_on_another_clip() {
    let a = asset("Street", 30);
    let mut tl = Timeline::default();
    let mut c = Clip::from_asset_range(&a, Time::ZERO, secs(10));
    let tr = moving_track();
    let tid = tr.id;
    c.tracks.push(tr);
    let base = tl.append(c).unwrap();
    let mut title = Clip::title(TitleParams::default(), secs(4));
    title.video.position_x.value = -0.1;
    title.video.follow = Some(Follow { clip: base, track: tid, transform: false });
    tl.connect(title, secs(2), 1).unwrap();
    let comp = evaluate(&tl, secs(3));
    let l = comp
        .nodes
        .iter()
        .filter_map(|n| match n {
            RenderNode::Layer(l) if l.clip != base => Some(l),
            _ => None,
        })
        .next()
        .expect("title layer");
    // source 3 s → +0.3
    assert!((l.video.position[0] - 0.2).abs() < 1e-4, "{:?}", l.video.position);
}

#[test]
fn stabilization_reaches_the_layer_in_source_time() {
    let a = asset("Handheld", 30);
    let mut tl = Timeline::default();
    let mut c = Clip::from_asset_range(&a, secs(1), secs(5));
    let ts: Vec<f64> = (0..=180).map(|i| i as f64 / 30.0).collect();
    let deltas: Vec<_> = (0..180).map(|i| motion::translation(if i % 2 == 0 { 0.004 } else { -0.004 }, 0.0)).collect();
    let mut st = Stabilization { smoothing: 0.3, ..Default::default() };
    st.aspect = 16.0 / 9.0;
    st.set_path_from_deltas(&ts, &deltas);
    c.stabilization = Some(st.clone());
    tl.append(c.clone()).unwrap();
    let comp = evaluate(&tl, secs(2));
    assert_eq!(layer(&comp, 0).video.stabilize, st.correction(3.0), "uses source seconds (1 s in-point + 2 s)");
    c.stabilization.as_mut().unwrap().enabled = false;
    tl.spine[0] = c;
    assert_eq!(layer(&evaluate(&tl, secs(2)), 0).video.stabilize, None);
}

#[test]
fn scene_cuts_blade_in_one_undo_step() {
    let a = asset("Montage", 60);
    let mut p = Project::new("Cuts");
    p.add_asset(a.clone());
    p.timeline.append(Clip::from_asset_range(&a, Time::ZERO, secs(2))).unwrap();
    let id = p.timeline.append(Clip::from_asset_range(&a, secs(10), secs(20))).unwrap();
    // cuts reported a quarter frame into the first new frame (source secs);
    // one outside the used range is ignored
    let cuts = [12.0 + 0.25 / 30.0, 21.5 + 0.25 / 30.0, 45.0];
    let cmd = motion::scene_cut_command(&p.timeline, id, &cuts, 30.0).expect("command");
    let mut h = History::default();
    let before = p.timeline.clone();
    h.execute(&mut p.timeline, cmd).unwrap();
    let starts: Vec<f64> = (1..p.timeline.spine.len()).map(|i| p.timeline.spine_start(i).as_secs_f64()).collect();
    assert_eq!(p.timeline.spine.len(), 4);
    for (got, want) in starts.iter().zip([2.0, 4.0, 13.5]) {
        assert!((got - want).abs() < 1e-6, "{starts:?}");
    }
    assert!(h.undo(&mut p.timeline));
    assert_eq!(p.timeline, before, "one undo restores the clip");
    assert!(motion::scene_cut_command(&p.timeline, id, &[45.0], 30.0).is_none());
}

#[test]
fn fcpxml_round_trips_motion_data() {
    let mut p = Project::new("Motion");
    let a = asset("Shot", 60);
    p.add_asset(a.clone());
    let mut c = Clip::from_asset_range(&a, Time::ZERO, secs(4));
    c.retime = Some(Retime::uniform(secs(2), 0.5));
    c.duration = secs(4);
    c.retime.as_mut().unwrap().blend = FrameBlendMode::SpeedWarp;
    let tr = moving_track();
    let tid = tr.id;
    c.tracks.push(tr);
    let mut st = Stabilization { model: MotionModel::Similarity, tripod: true, ..Default::default() };
    st.set_path_from_deltas(&[0.0, 0.5, 1.0], &[motion::translation(0.01, 0.0), motion::translation(-0.01, 0.0)]);
    c.stabilization = Some(st);
    let mut e = Effect::new(EffectKind::GaussianBlur);
    e.mask = Some(ShapeMask { track: Some(tid), ..ShapeMask::new(MaskShape::Ellipse) });
    c.effects.push(e);
    let base = p.timeline.append(c.clone()).unwrap();
    let mut title = Clip::title(TitleParams::default(), secs(2));
    title.video.follow = Some(Follow { clip: base, track: tid, transform: true });
    p.timeline.connect(title, secs(1), 1).unwrap();

    let xml = fcpxml::export(&p);
    assert!(xml.contains("rcp.motion"));
    let q = fcpxml::import(&xml).expect("re-import");
    let r = &q.timeline.spine[0];
    assert_eq!(r.tracks, c.tracks);
    assert_eq!(r.stabilization, c.stabilization);
    assert_eq!(r.retime.as_ref().map(|r| r.blend), Some(FrameBlendMode::SpeedWarp));
    assert_eq!(r.effects[0].mask.as_ref().and_then(|m| m.track), Some(tid));
    let f = r.connected[0].video.follow.clone().expect("follow survives");
    // clip ids are re-minted on import: the follow must point at the new id
    assert_eq!(f.clip, r.id);
    assert!(f.transform && f.track == tid);
}
