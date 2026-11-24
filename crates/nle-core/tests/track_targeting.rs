//! Track targeting: overwrite / insert onto a lane, lift and extract over a
//! range on targeted lanes, and their undo.

use nle_core::{AssetId, Clip, ClipKind, Command, History, Time, Timeline, TrackState, VideoFormat};

fn secs(s: i64) -> Time {
    Time::from_secs(s)
}

fn media(asset: u64, length: i64) -> Clip {
    let mut c = Clip::new("media", ClipKind::Asset { asset: AssetId(asset) }, secs(length));
    c.source_in = secs(5);
    c.media_duration = Some(secs(length + 30));
    c
}

/// A | B | C (10 s each) with a 6 s clip on lane 1 at 2 s and a 4 s clip
/// on lane 1 at 20 s.
fn setup() -> Timeline {
    let mut tl = Timeline::new("t", VideoFormat::default());
    for a in 1..=3 {
        tl.append(media(a, 10)).unwrap();
    }
    tl.connect(media(4, 6), secs(2), 1).unwrap();
    tl.connect(media(5, 4), secs(20), 1).unwrap();
    tl
}

/// (start, end) of the clips on `lane`, in order.
fn lane(tl: &Timeline, l: i32) -> Vec<(i64, i64)> {
    let mut v: Vec<(i64, i64)> =
        tl.placed_clips().iter().filter(|p| p.lane == l && !p.clip.is_gap()).map(|p| (p.start.as_secs_f64().round() as i64, p.end().as_secs_f64().round() as i64)).collect();
    v.sort();
    v
}

fn run(tl: &mut Timeline, cmd: Command) {
    let mut history = History::default();
    let before = tl.clone();
    history.execute(tl, cmd).unwrap();
    let after = tl.clone();
    assert!(history.undo(tl));
    assert_eq!(*tl, before, "undo");
    assert!(history.redo(tl));
    assert_eq!(*tl, after, "redo");
}

#[test]
fn overwrite_onto_a_connected_lane_replaces_only_that_lane() {
    let mut tl = setup();
    run(&mut tl, Command::OverwriteLane { at: secs(5), clip: media(6, 4), lane: 1 });
    assert_eq!(lane(&tl, 1), vec![(2, 5), (5, 9), (20, 24)]);
    assert_eq!(lane(&tl, 0), vec![(0, 10), (10, 20), (20, 30)]);
    assert_eq!(tl.duration(), secs(30));
}

#[test]
fn insert_onto_a_lane_ripples_everything_and_leaves_a_storyline_gap() {
    let mut tl = setup();
    run(&mut tl, Command::InsertLane { at: secs(4), clip: media(6, 3), lane: 1 });
    // the lane-1 clip at 2..8 is cut at 4; its tail moves with the ripple
    assert_eq!(lane(&tl, 1), vec![(2, 4), (4, 7), (7, 11), (23, 27)]);
    assert_eq!(lane(&tl, 0), vec![(0, 4), (7, 13), (13, 23), (23, 33)]);
    assert_eq!(tl.duration(), secs(33));
}

#[test]
fn lift_on_one_lane_cuts_the_range_out_and_moves_nothing() {
    let mut tl = setup();
    run(&mut tl, Command::LiftRange { start: secs(3), end: secs(6), lanes: vec![1] });
    assert_eq!(lane(&tl, 1), vec![(2, 3), (6, 8), (20, 24)]);
    assert_eq!(lane(&tl, 0), vec![(0, 10), (10, 20), (20, 30)]);
}

#[test]
fn lift_on_every_lane_leaves_a_gap_in_the_storyline() {
    let mut tl = setup();
    run(&mut tl, Command::LiftRange { start: secs(12), end: secs(15), lanes: vec![] });
    assert_eq!(lane(&tl, 0), vec![(0, 10), (10, 12), (15, 20), (20, 30)]);
    assert_eq!(lane(&tl, 1), vec![(2, 8), (20, 24)]);
    assert_eq!(tl.duration(), secs(30));
}

#[test]
fn extract_on_the_storyline_keeps_other_lanes_in_sync() {
    let mut tl = setup();
    run(&mut tl, Command::ExtractRange { start: secs(12), end: secs(15), lanes: vec![0] });
    assert_eq!(lane(&tl, 0), vec![(0, 10), (10, 12), (12, 17), (17, 27)]);
    // the later lane-1 clip moved up with the storyline; the earlier one stayed
    assert_eq!(lane(&tl, 1), vec![(2, 8), (17, 21)]);
    assert_eq!(tl.duration(), secs(27));
}

#[test]
fn extract_on_targeted_lanes_cuts_clips_crossing_the_range() {
    let mut tl = setup();
    run(&mut tl, Command::ExtractRange { start: secs(18), end: secs(22), lanes: vec![0, 1] });
    // lane-1 clip 20..24 loses 20..22, its 22..24 tail closes up to 18..20
    assert_eq!(lane(&tl, 1), vec![(2, 8), (18, 20)]);
    assert_eq!(lane(&tl, 0), vec![(0, 10), (10, 18), (18, 26)]);
}

#[test]
fn target_lane_picks_the_targeted_lane_nearest_the_storyline() {
    let mut tl = setup();
    assert_eq!(tl.target_lane(true), None);
    for l in [3, 2, -1, -2] {
        run(&mut tl, Command::SetTrack { track: TrackState { target: true, ..TrackState::new(l) } });
    }
    assert_eq!(tl.target_lane(true), Some(2));
    assert_eq!(tl.target_lane(false), Some(-1));
    assert_eq!(tl.targeted_lanes().len(), 4);
}

#[test]
fn lift_everything_leaves_a_locked_lane_alone() {
    let mut tl = setup();
    run(&mut tl, Command::SetTrack { track: TrackState { locked: true, ..TrackState::new(1) } });
    run(&mut tl, Command::LiftRange { start: secs(1), end: secs(25), lanes: vec![] });
    assert_eq!(lane(&tl, 1), vec![(2, 8), (20, 24)]);
    assert_eq!(lane(&tl, 0), vec![(0, 1), (25, 30)]);
}

#[test]
fn an_empty_range_is_an_error_and_changes_nothing() {
    let mut tl = setup();
    let before = tl.clone();
    let mut h = History::default();
    assert!(h.execute(&mut tl, Command::LiftRange { start: secs(40), end: secs(45), lanes: vec![1] }).is_err());
    assert_eq!(tl, before);
}
