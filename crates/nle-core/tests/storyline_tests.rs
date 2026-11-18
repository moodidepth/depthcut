//! Magnetic-timeline features: secondary storylines, step-in editing of
//! compound clips, the Position tool, held connections, connection points
//! and auditions. Each goes through `Command`s, undoes, and survives both
//! `.rcp` JSON and FCPXML.

use nle_core::clip::*;
use nle_core::command::{Command, History};
use nle_core::eval::{evaluate, LayerSource, RenderNode};
use nle_core::fcpxml;
use nle_core::*;

fn secs(s: i64) -> Time {
    Time::from_secs(s)
}

fn ms(v: i64) -> Time {
    Time::from_secs_f64(v as f64 / 1000.0)
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

/// A project with one 60 s asset and clips cut from it.
fn project() -> (Project, MediaAsset) {
    let mut p = Project::new("Storylines");
    let a = asset("Shot", 60);
    p.add_asset(a.clone());
    (p, a)
}

fn cut(a: &MediaAsset, name: &str, src: i64, dur_ms: i64) -> Clip {
    let mut c = Clip::from_asset_range(a, secs(src), ms(dur_ms));
    c.name = name.into();
    c
}

/// Ids of the layers drawn at `t`, bottom to top (transitions list both sides).
fn layers_at(tl: &Timeline, t: Time) -> Vec<(ClipId, bool)> {
    let mut out = Vec::new();
    for n in evaluate(tl, t).nodes {
        match n {
            RenderNode::Layer(l) => out.push((l.clip, false)),
            RenderNode::Transition { from, to, .. } => {
                for l in [from, to].into_iter().flatten() {
                    out.push((l.clip, true));
                }
            }
        }
    }
    out
}

fn json_round_trip(tl: &Timeline) -> Timeline {
    serde_json::from_str(&serde_json::to_string(tl).unwrap()).unwrap()
}

#[test]
fn storyline_groups_connected_clips_and_ripples_on_its_own() {
    let (mut p, a) = project();
    let tl = &mut p.timeline;
    let base = cut(&a, "Base", 0, 10_000);
    let base_id = base.id;
    tl.append(base).unwrap();
    let (x, y) = (cut(&a, "X", 20, 2_000), cut(&a, "Y", 30, 2_000));
    let (xid, yid) = (x.id, y.id);
    tl.connect(x, secs(1), 1).unwrap();
    tl.connect(y, secs(4), 1).unwrap();

    let mut h = History::default();
    h.execute(tl, Command::MakeStoryline { ids: vec![yid, xid] }).unwrap();
    let sl = tl.spine[0].connected[0].clone();
    assert!(sl.is_storyline());
    assert_eq!((sl.lane, tl.clip_start(sl.id)), (1, Some(secs(1))));
    assert_eq!(sl.duration, secs(5), "X, a 1 s gap, Y");
    // nothing moved on screen
    assert!(layers_at(tl, ms(1_500)).contains(&(xid, false)));
    assert!(layers_at(tl, ms(4_500)).contains(&(yid, false)));
    // the gap inside the storyline is transparent: only the base clip draws
    assert_eq!(layers_at(tl, ms(3_500)), vec![(base_id, false)]);

    // ripple delete X inside the storyline: Y slides left, the base does not
    let gap_id = match &sl.kind {
        ClipKind::Compound(c) => c.spine[1].id,
        _ => unreachable!(),
    };
    h.execute(tl, Command::InContainer { container: sl.id, cmd: Box::new(Command::DeleteRipple { id: xid }) }).unwrap();
    h.execute(tl, Command::InContainer { container: sl.id, cmd: Box::new(Command::DeleteRipple { id: gap_id }) }).unwrap();
    let sl2 = &tl.spine[0].connected[0];
    assert_eq!(sl2.duration, secs(2), "storyline shrinks to its contents");
    assert!(layers_at(tl, ms(1_500)).contains(&(yid, false)), "Y rippled to the storyline start");
    assert_eq!(tl.spine[0].duration, secs(10));
    assert_eq!(tl.clip_start(sl2.id), Some(secs(1)));

    // serialises both ways
    assert_eq!(json_round_trip(tl), *tl);
    let xml = fcpxml::export(&p);
    assert!(xml.contains("<spine lane=\"1\""), "secondary storyline exported as a laned spine");
    let q = fcpxml::import(&xml).unwrap();
    let back = &q.timeline.spine[0].connected[0];
    assert!(back.is_storyline());
    assert_eq!((back.duration, back.offset, back.lane), (secs(2), secs(1), 1));

    // undo restores the loose connected clips
    assert!(h.undo(&mut p.timeline) && h.undo(&mut p.timeline) && h.undo(&mut p.timeline));
    let tl = &p.timeline;
    assert_eq!(tl.spine[0].connected.len(), 2);
    assert!(tl.spine[0].connected.iter().all(|c| !c.is_storyline()));
}

#[test]
fn storyline_rejects_spine_clips_and_overlaps() {
    let (mut p, a) = project();
    let tl = &mut p.timeline;
    let base = cut(&a, "Base", 0, 10_000);
    let base_id = base.id;
    tl.append(base).unwrap();
    let (x, y) = (cut(&a, "X", 20, 3_000), cut(&a, "Y", 30, 2_000));
    let (xid, yid) = (x.id, y.id);
    tl.connect(x, secs(1), 1).unwrap();
    tl.connect(y, secs(2), 2).unwrap();
    let mut h = History::default();
    assert_eq!(h.execute(tl, Command::MakeStoryline { ids: vec![base_id] }), Err(EditError::NotConnected));
    assert_eq!(h.execute(tl, Command::MakeStoryline { ids: vec![xid, yid] }), Err(EditError::Overlap));
    assert_eq!(tl.spine[0].connected.len(), 2, "failed commands leave the timeline alone");
}

#[test]
fn transition_on_a_connected_clip_makes_a_storyline_that_dissolves_over_the_base() {
    let (mut p, a) = project();
    let tl = &mut p.timeline;
    let base = cut(&a, "Base", 0, 10_000);
    let base_id = base.id;
    tl.append(base).unwrap();
    let x = cut(&a, "X", 20, 4_000);
    let xid = x.id;
    tl.connect(x, secs(2), 1).unwrap();
    let tr = Transition::new(TransitionKind::default(), secs(1));
    let mut h = History::default();
    h.execute(tl, Command::SetTransition { id: xid, at_end: false, transition: Some(tr) }).unwrap();
    let sl = &tl.spine[0].connected[0];
    assert!(sl.is_storyline(), "the connected clip was wrapped in a storyline");
    assert_eq!(tl.clip(xid).and_then(|c| c.transition_in.as_ref()).map(|t| t.duration), Some(secs(1)));
    // during the fade the storyline mixes with the base, not with black
    let comp = evaluate(tl, ms(2_500));
    assert!(matches!(&comp.nodes[0], RenderNode::Layer(l) if l.clip == base_id));
    match &comp.nodes[1] {
        RenderNode::Transition { from: Some(f), to: Some(t), transition } => {
            assert!(matches!(f.source, LayerSource::Solid(c) if c[3] == 0.0), "open side is transparent");
            assert_eq!((t.clip, t.lane), (xid, 1));
            assert!(transition.progress > 0.3 && transition.progress < 0.7);
        }
        other => panic!("expected a storyline transition, got {other:?}"),
    }
}

#[test]
fn step_into_compound_edits_resize_the_parent_and_nest() {
    let (mut p, a) = project();
    let tl = &mut p.timeline;
    let (c1, c2, after) = (cut(&a, "C1", 0, 2_000), cut(&a, "C2", 5, 2_000), cut(&a, "After", 10, 1_000));
    let (c1id, c2id, after_id) = (c1.id, c2.id, after.id);
    tl.append(c1).unwrap();
    tl.append(c2).unwrap();
    tl.append(after).unwrap();
    let mut h = History::default();
    h.execute(tl, Command::MakeCompound { ids: vec![c1id, c2id], name: "Outer".into() }).unwrap();
    let outer = tl.spine[0].id;

    // insert inside the compound: the parent clip grows and the spine ripples
    let extra = cut(&a, "Extra", 20, 1_500);
    h.execute(tl, Command::InContainer { container: outer, cmd: Box::new(Command::Insert { at: secs(2), clip: extra }) }).unwrap();
    assert_eq!(tl.spine[0].duration, ms(5_500));
    assert_eq!(tl.clip_start(after_id), Some(ms(5_500)));

    // a compound inside the compound, edited through the breadcrumb path
    h.execute(tl, Command::InContainer { container: outer, cmd: Box::new(Command::MakeCompound { ids: vec![c1id], name: "Inner".into() }) }).unwrap();
    let inner = match &tl.spine[0].kind {
        ClipKind::Compound(c) => c.spine[0].id,
        _ => unreachable!(),
    };
    h.execute(tl, Command::in_path(&[outer, inner], Command::TrimEnd { id: c1id, delta: ms(-500) })).unwrap();
    assert_eq!(tl.clip(inner).unwrap().duration, ms(1_500), "inner compound follows its contents");
    assert_eq!(tl.spine[0].duration, secs(5), "and so does the outer one");
    assert_eq!(tl.clip_start(after_id), Some(secs(5)));
    assert_eq!(h.undo_label(), Some("Trim"));

    // a trimmed compound keeps its window instead of growing
    h.execute(tl, Command::TrimEnd { id: outer, delta: secs(-1) }).unwrap();
    h.execute(tl, Command::in_path(&[outer], Command::Append { clip: cut(&a, "Tail", 40, 2_000) })).unwrap();
    assert_eq!(tl.spine[0].duration, secs(4));
    assert_eq!(tl.spine[0].media_duration, Some(secs(7)));
    assert_eq!(json_round_trip(tl), *tl);

    assert_eq!(h.execute(tl, Command::InContainer { container: after_id, cmd: Box::new(Command::Blade { at: ms(100) }) }), Err(EditError::NotCompound));
}

#[test]
fn position_tool_moves_without_ripple_and_fills_with_gaps() {
    let (mut p, a) = project();
    let tl = &mut p.timeline;
    let (ca, cb, cc) = (cut(&a, "A", 0, 2_000), cut(&a, "B", 5, 2_000), cut(&a, "C", 10, 2_000));
    let (aid, bid, cid) = (ca.id, cb.id, cc.id);
    tl.append(ca).unwrap();
    tl.append(cb).unwrap();
    tl.append(cc).unwrap();
    let title = Clip::title(TitleParams::default(), secs(1));
    let tid = title.id;
    tl.connect(title, ms(4_500), 1).unwrap(); // on C
    let mut h = History::default();

    // A to 5 s: a gap where A was, B stays at 2 s, A overwrites the second half of C
    h.execute(tl, Command::PositionMove { id: aid, at: secs(5), lane: 0 }).unwrap();
    let names: Vec<&str> = tl.spine.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(names, ["Gap", "B", "C", "A"]);
    assert_eq!(tl.clip_start(bid), Some(secs(2)));
    assert_eq!(tl.clip_start(cid), Some(secs(4)));
    assert_eq!(tl.clip(cid).unwrap().duration, secs(1));
    assert_eq!(tl.clip_start(aid), Some(secs(5)));
    assert_eq!(tl.clip_start(tid), Some(ms(4_500)), "connected clips hold their position");
    assert_eq!(tl.duration(), secs(7));

    // past the end: a gap fills the space, and trailing gaps never linger
    h.execute(tl, Command::PositionMove { id: aid, at: secs(9), lane: 0 }).unwrap();
    assert_eq!(tl.clip_start(aid), Some(secs(9)));
    assert_eq!(tl.duration(), secs(11));
    h.execute(tl, Command::PositionMove { id: aid, at: secs(0), lane: 0 }).unwrap();
    assert_eq!(tl.spine.last().map(|c| c.id), Some(cid), "no empty gap left at the end");
    assert_eq!(tl.clip_start(bid), Some(secs(2)));
    assert!(tl.spine.windows(2).all(|w| !(w[0].is_gap() && w[1].is_gap())), "gaps merged");
    assert_eq!(json_round_trip(tl), *tl);
    assert_eq!(h.undo_label(), Some("Position"));
}

#[test]
fn held_connections_stay_put_when_their_parent_moves() {
    let (mut p, a) = project();
    let tl = &mut p.timeline;
    let (ca, cb) = (cut(&a, "A", 0, 2_000), cut(&a, "B", 5, 3_000));
    let (aid, bid) = (ca.id, cb.id);
    tl.append(ca).unwrap();
    tl.append(cb).unwrap();
    let x = cut(&a, "X", 20, 1_000);
    let xid = x.id;
    tl.connect(x, ms(500), 1).unwrap(); // on A
    let mut h = History::default();

    // normal move: X travels with A
    h.execute(tl, Command::MoveSpine { id: aid, to_index: 1 }).unwrap();
    assert_eq!(tl.clip_start(xid), Some(ms(3_500)));
    h.undo(tl);
    // with ` held: X stays at 0.5 s and now hangs off B
    h.execute(tl, Command::HoldConnections { ids: vec![aid], cmd: Box::new(Command::MoveSpine { id: aid, to_index: 1 }) }).unwrap();
    assert_eq!(tl.clip_start(xid), Some(ms(500)));
    assert_eq!(tl.locate(xid), Some(ClipLoc::Connected { parent: 0, index: 0 }));
    assert_eq!(tl.spine[0].id, bid);
    assert_eq!(h.undo_label(), Some("Move"));
    // deleting with ` held keeps the connected clip too
    h.undo(tl);
    h.execute(tl, Command::HoldConnections { ids: vec![aid], cmd: Box::new(Command::DeleteRipple { id: aid }) }).unwrap();
    assert_eq!(tl.clip_start(xid), Some(ms(500)));
}

#[test]
fn connection_point_reassigns_the_parent_and_round_trips() {
    let (mut p, a) = project();
    let (ca, cb) = (cut(&a, "A", 0, 2_000), cut(&a, "B", 5, 2_000));
    let aid = ca.id;
    p.timeline.append(ca).unwrap();
    p.timeline.append(cb).unwrap();
    let x = cut(&a, "X", 20, 2_000);
    let xid = x.id;
    p.timeline.connect(x, ms(1_500), 1).unwrap(); // spans 1.5–3.5 s, attached to A
    let mut h = History::default();

    h.execute(&mut p.timeline, Command::SetConnectionPoint { id: xid, at: secs(3) }).unwrap();
    let tl = &p.timeline;
    let x = tl.clip(xid).unwrap();
    assert_eq!((x.anchor, x.offset), (ms(1_500), ms(-500)));
    assert_eq!(tl.locate(xid), Some(ClipLoc::Connected { parent: 1, index: 0 }));
    assert_eq!(tl.clip_start(xid), Some(ms(1_500)), "the clip itself does not move");

    // serialises (FCPXML keeps it as metadata)
    assert_eq!(json_round_trip(tl), *tl);
    let q = fcpxml::import(&fcpxml::export(&p)).unwrap();
    let back = q.timeline.spine[1].connected[0].clone();
    assert_eq!((back.anchor, back.offset), (ms(1_500), ms(-500)));

    // now X follows B: shortening A pulls it left
    h.execute(&mut p.timeline, Command::TrimEnd { id: aid, delta: secs(-1) }).unwrap();
    assert_eq!(p.timeline.clip_start(xid), Some(ms(500)));

    // blading B before the connection point keeps X on the right half
    let tl = &mut p.timeline;
    let (_, right) = tl.blade(ms(1_500)).unwrap();
    assert_eq!(tl.locate(xid).map(|l| matches!(l, ClipLoc::Connected { parent, .. } if tl.spine[parent].id == right)), Some(true));
    assert_eq!(tl.clip_start(xid), Some(ms(500)));

    // spine clips have no connection point to move
    assert_eq!(h.execute(&mut p.timeline, Command::SetConnectionPoint { id: aid, at: secs(1) }), Err(EditError::NotConnected));
}

#[test]
fn auditions_cycle_takes_ripple_and_round_trip() {
    let (mut p, a) = project();
    let (slot, take2, after) = (cut(&a, "Take 1", 0, 2_000), cut(&a, "Take 2", 10, 3_000), cut(&a, "After", 30, 1_000));
    let (slot_id, take2_id, after_id) = (slot.id, take2.id, after.id);
    p.timeline.append(slot).unwrap();
    p.timeline.append(after).unwrap();
    let x = Clip::title(TitleParams::default(), secs(1));
    let xid = x.id;
    p.timeline.connect(x, ms(500), 1).unwrap();
    let mut h = History::default();
    h.execute(&mut p.timeline, Command::AddAuditionTake { id: slot_id, take: Box::new(take2) }).unwrap();
    h.execute(&mut p.timeline, Command::AddAuditionTake { id: slot_id, take: Box::new(cut(&a, "Take 3", 20, 1_000)) }).unwrap();

    h.execute(&mut p.timeline, Command::CycleAudition { id: slot_id, step: 1 }).unwrap();
    let tl = &p.timeline;
    let pick = tl.clip(slot_id).unwrap();
    assert_eq!((pick.name.as_str(), pick.duration, pick.source_in), ("Take 2", secs(3), secs(10)));
    assert_eq!(pick.audition.iter().map(|t| t.name.as_str()).collect::<Vec<_>>(), ["Take 3", "Take 1"]);
    assert_eq!(pick.audition[0].id, take2_id, "ids belong to positions: the pick always keeps the slot id");
    assert_eq!(tl.clip_start(after_id), Some(secs(3)), "a longer take ripples");
    assert_eq!(tl.locate(xid), Some(ClipLoc::Connected { parent: 0, index: 0 }), "connected clips stay with the slot");
    assert!(tl.max_clip_id() >= take2_id.0);

    assert_eq!(json_round_trip(tl), *tl);
    let xml = fcpxml::export(&p);
    assert!(xml.contains("<audition"));
    let q = fcpxml::import(&xml).unwrap();
    let back = &q.timeline.spine[0];
    assert_eq!(back.name, "Take 2");
    assert_eq!(back.audition.iter().map(|t| (t.name.as_str(), t.duration)).collect::<Vec<_>>(), [("Take 3", secs(1)), ("Take 1", secs(2))]);
    assert_eq!(back.connected.len(), 1);
    assert_eq!(q.timeline.spine[1].name, "After");

    // backwards wraps to the original pick
    h.execute(&mut p.timeline, Command::CycleAudition { id: slot_id, step: -1 }).unwrap();
    assert_eq!(p.timeline.clip(slot_id).unwrap().name, "Take 1");
    h.execute(&mut p.timeline, Command::FinalizeAudition { id: slot_id }).unwrap();
    assert!(p.timeline.clip(slot_id).unwrap().audition.is_empty());
    assert_eq!(h.execute(&mut p.timeline, Command::CycleAudition { id: slot_id, step: 1 }), Err(EditError::Nothing));
    assert!(h.undo(&mut p.timeline));
    assert_eq!(p.timeline.clip(slot_id).unwrap().audition.len(), 2);
}

#[test]
fn old_project_json_without_new_fields_still_loads() {
    let (mut p, a) = project();
    p.timeline.append(cut(&a, "A", 0, 1_000)).unwrap();
    let json = serde_json::to_string(&p.timeline).unwrap();
    assert!(!json.contains("\"anchor\"") && !json.contains("\"audition\"") && !json.contains("\"storyline\""), "defaults are not written");
    let back: Timeline = serde_json::from_str(&json).unwrap();
    assert_eq!(back, p.timeline);
}
