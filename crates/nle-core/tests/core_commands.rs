//! Track controls, linked audio, clipboard, crossfades and their undo.

use nle_core::eval::{audio_spans, evaluate};
use nle_core::focus::Focus;
use nle_core::{AssetId, Clip, ClipId, ClipKind, Command, EditError, History, Project, Time, Timeline, TrackState, VideoFormat};

fn secs(s: i64) -> Time {
    Time::from_secs(s)
}

/// `length` seconds used from the middle of longer media.
fn media(asset: u64, length: i64) -> Clip {
    let mut c = Clip::new("media", ClipKind::Asset { asset: AssetId(asset) }, secs(length));
    c.source_in = secs(5);
    c.media_duration = Some(secs(length + 10));
    c
}

/// A (10 s) | B (10 s) | C (10 s)
fn abc() -> (Timeline, [ClipId; 3]) {
    let mut tl = Timeline::new("t", VideoFormat::default());
    let ids = [1, 2, 3].map(|asset| tl.append(media(asset, 10)).unwrap());
    (tl, ids)
}

/// Apply `cmd`, check undo and redo restore exact states, and return the result.
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

/// `abc` with the sound of B detached.
fn detached() -> (Timeline, [ClipId; 3], ClipId) {
    let (mut tl, ids) = abc();
    run(&mut tl, Command::DetachAudio { id: ids[1] });
    let audio = tl.placed_clips().iter().find(|p| p.lane < 0).expect("audio clip").clip.id;
    (tl, ids, audio)
}

/// Every linked pair as `((video start, source in, length), (audio ...))`.
type Span = (Time, Time, Time);

fn pairs(tl: &Timeline) -> Vec<(Span, Span)> {
    let placed = tl.placed_clips();
    let mut out = Vec::new();
    for v in placed.iter().filter(|p| p.clip.av.video && p.clip.link.is_some()) {
        let linked: Vec<_> = placed.iter().filter(|p| !p.clip.av.video && p.clip.link == v.clip.link).collect();
        assert_eq!(linked.len(), 1, "each picture has exactly one linked sound");
        let a = linked[0];
        out.push(((v.start, v.clip.source_in, v.clip.duration), (a.start, a.clip.source_in, a.clip.duration)));
    }
    out.sort();
    out
}

fn assert_in_sync(tl: &Timeline, count: usize) {
    let pairs = pairs(tl);
    assert_eq!(pairs.len(), count);
    for (video, audio) in pairs {
        assert_eq!(video, audio);
    }
}

fn lock(tl: &mut Timeline, lane: i32) {
    run(tl, Command::SetTrack { track: TrackState { locked: true, ..TrackState::new(lane) } });
}

// ----- saved projects

#[test]
fn projects_saved_before_these_fields_still_load() {
    let (mut tl, _) = abc();
    tl.tracks.clear();
    let json = serde_json::to_value(&tl).unwrap();
    let mut old = json.clone();
    for clip in old["spine"].as_array_mut().unwrap() {
        clip["color"].as_object_mut().unwrap().remove("brightness");
    }
    let loaded: Timeline = serde_json::from_value(old).unwrap();
    assert_eq!(loaded, tl);
    let text = json.to_string();
    for field in ["flip_horizontal", "scale_lock", "crop_lock", "crossfade_in", "\"link\"", "\"tracks\":[{\"lane"] {
        assert!(!text.contains(field), "{field} is only written when used");
    }
}

#[test]
fn new_fields_survive_save_and_load() {
    let (mut tl, _, _) = detached();
    let c = &mut tl.spine[0];
    c.video.flip_horizontal = true;
    c.video.scale_lock = false;
    c.video.crop_lock = true;
    c.color.brightness.value = 0.25;
    c.audio.crossfade_out = secs(1);
    lock(&mut tl, 2);
    let loaded: Timeline = serde_json::from_str(&serde_json::to_string(&tl).unwrap()).unwrap();
    assert_eq!(loaded, tl);
}

#[test]
fn loading_reserves_link_ids() {
    let (mut tl, _) = abc();
    let big = 9_000_000;
    tl.spine[0].link = Some(big);
    assert!(tl.max_clip_id() >= big);
}

#[test]
fn brightness_and_flip_reach_the_evaluated_frame() {
    let (mut tl, _) = abc();
    tl.spine[0].color.brightness.value = 0.25;
    tl.spine[0].video.flip_vertical = true;
    assert!(!tl.spine[0].color.is_identity());
    let frame = format!("{:?}", evaluate(&tl, secs(1)));
    assert!(frame.contains("brightness: 0.25"));
    assert!(frame.contains("flip: [false, true]"));
}

// ----- format

#[test]
fn format_changes_undo_even_while_stepped_in() {
    let (mut shown, ids) = abc();
    let original = shown.format;
    let comp = shown.make_compound(&ids[..2], "c").unwrap();
    let (mut history, mut focus) = (History::default(), Focus::default());
    focus.step_into(&mut shown, comp, Time::ZERO).unwrap();
    let format = VideoFormat { width: 1080, height: 1920, ..original };
    focus.execute(&mut shown, &mut history, Command::SetFormat { format }, None).unwrap();
    assert_eq!(shown.format, format);
    assert_eq!(focus.root(&shown).format, format);
    assert!(focus.undo(&mut shown, &mut history));
    assert_eq!(shown.format, original);
    assert_eq!(focus.root(&shown).format, original);
}

// ----- track controls

#[test]
fn eye_hides_a_lane_from_the_picture_only() {
    let (mut tl, _) = abc();
    tl.connect(media(9, 4), secs(1), 1).unwrap();
    let nodes = evaluate(&tl, secs(2)).nodes.len();
    run(&mut tl, Command::SetTrack { track: TrackState { hidden: true, ..TrackState::new(1) } });
    assert_eq!(evaluate(&tl, secs(2)).nodes.len(), nodes - 1);
    assert!(audio_spans(&tl).iter().any(|s| s.asset == AssetId(9)));
}

#[test]
fn mute_and_solo_choose_what_is_heard() {
    let (mut tl, _) = abc();
    tl.connect(media(8, 4), secs(1), -1).unwrap();
    tl.connect(media(9, 4), secs(1), -2).unwrap();
    let heard = |tl: &Timeline, asset| audio_spans(tl).iter().any(|s| s.asset == AssetId(asset));
    run(&mut tl, Command::SetTrack { track: TrackState { muted: true, ..TrackState::new(-1) } });
    assert!(!heard(&tl, 8) && heard(&tl, 9) && heard(&tl, 1));
    run(&mut tl, Command::SetTrack { track: TrackState { solo: true, ..TrackState::new(-2) } });
    assert!(!heard(&tl, 8) && heard(&tl, 9) && !heard(&tl, 1));
}

#[test]
fn a_locked_lane_refuses_edits_and_new_clips() {
    let (mut tl, _) = abc();
    let id = tl.connect(media(9, 4), secs(12), 1).unwrap();
    lock(&mut tl, 1);
    let before = tl.clone();
    let mut changed = Box::new(tl.clip(id).unwrap().clone());
    changed.name = "changed".into();
    for cmd in [
        Command::TrimEnd { id, delta: -secs(1) },
        Command::DeleteRipple { id },
        Command::MoveClip { id, at: secs(2), lane: 2 },
        Command::UpdateClip { clip: changed },
        Command::Connect { clip: media(7, 2), at: secs(1), lane: 1 },
    ] {
        let mut history = History::default();
        assert_eq!(history.execute(&mut tl, cmd.clone()), Err(EditError::Locked), "{cmd:?}");
        assert_eq!(tl, before, "{cmd:?}");
        assert!(!history.can_undo());
    }
}

#[test]
fn a_locked_lane_stays_put_when_the_storyline_ripples() {
    let (mut tl, ids) = abc();
    let id = tl.connect(media(9, 4), secs(22), 1).unwrap();
    lock(&mut tl, 1);
    run(&mut tl, Command::DeleteRipple { id: ids[0] });
    assert_eq!(tl.clip_start(id), Some(secs(22)));
}

#[test]
fn a_locked_lane_survives_deleting_its_parent() {
    let (mut tl, ids) = abc();
    let id = tl.connect(media(9, 4), secs(12), 1).unwrap();
    lock(&mut tl, 1);
    run(&mut tl, Command::DeleteRipple { id: ids[1] });
    assert_eq!(tl.clip_start(id), Some(secs(12)));
}

#[test]
fn a_locked_storyline_still_takes_connected_clips() {
    let (mut tl, ids) = abc();
    lock(&mut tl, 0);
    let before = tl.clone();
    let mut history = History::default();
    assert_eq!(history.execute(&mut tl, Command::DeleteRipple { id: ids[0] }), Err(EditError::Locked));
    assert_eq!(history.execute(&mut tl, Command::Blade { at: secs(5) }), Err(EditError::Locked));
    assert_eq!(tl, before);
    run(&mut tl, Command::Connect { clip: media(9, 3), at: secs(4), lane: 1 });
}

#[test]
fn unlocking_is_always_possible() {
    let (mut tl, _) = abc();
    lock(&mut tl, 0);
    run(&mut tl, Command::SetTrack { track: TrackState::new(0) });
    assert!(!tl.is_locked(0));
    assert!(tl.tracks.is_empty());
}

// ----- clipboard

#[test]
fn paste_inserts_fresh_copies_at_the_playhead() {
    let (mut tl, ids) = abc();
    let items = tl.copy_clips(&ids[..2]);
    run(&mut tl, Command::Paste { items, at: secs(20) });
    let order: Vec<_> = tl.spine.iter().map(|c| c.asset_id().unwrap().0).collect();
    assert_eq!(order, [1, 2, 1, 2, 3]);
    let mut all = tl.all_ids();
    let count = all.len();
    all.sort();
    all.dedup();
    assert_eq!(all.len(), count, "unique ids");
}

#[test]
fn pasting_into_a_clip_selects_only_the_copies() {
    // plain, with linked sound that is split along, and with a gap between the copies
    for case in 0..3 {
        let (mut tl, ids) = abc();
        if case == 1 {
            run(&mut tl, Command::DetachAudio { id: ids[1] });
        }
        let chosen = if case == 2 { vec![ids[0], ids[2]] } else { vec![ids[0]] };
        let items = tl.copy_clips(&chosen);
        let before: std::collections::HashSet<_> = tl.all_ids().into_iter().collect();
        let at = tl.clip_start(ids[1]).unwrap() + secs(1);
        run(&mut tl, Command::Paste { items: items.clone(), at });
        let mut got: Vec<_> = tl.pasted_ids(&before, items.len()).into_iter().map(|id| tl.clip(id).unwrap().asset_id().unwrap().0).collect();
        got.sort();
        assert_eq!(got, if case == 2 { vec![1, 3] } else { vec![1] }, "case {case}");
    }
}

#[test]
fn paste_keeps_the_spacing_of_connected_clips() {
    let (mut tl, _) = abc();
    let a = tl.connect(media(8, 2), secs(1), 1).unwrap();
    let b = tl.connect(media(9, 2), secs(6), 1).unwrap();
    let items = tl.copy_clips(&[a, b]);
    run(&mut tl, Command::Paste { items, at: secs(15) });
    for (asset, at) in [(8, 15), (9, 20)] {
        assert!(tl.placed_clips().iter().any(|p| p.clip.asset_id() == Some(AssetId(asset)) && p.start == secs(at)));
    }
}

#[test]
fn duplicate_follows_the_original() {
    let (mut tl, ids) = abc();
    run(&mut tl, Command::Duplicate { ids: vec![ids[1]] });
    let order: Vec<_> = tl.spine.iter().map(|c| c.asset_id().unwrap().0).collect();
    assert_eq!(order, [1, 2, 2, 3]);
}

#[test]
fn duplicating_a_pair_makes_a_separate_pair() {
    let (mut tl, ids, _) = detached();
    run(&mut tl, Command::Duplicate { ids: vec![ids[1]] });
    assert_in_sync(&tl, 2);
}

// ----- linked audio

#[test]
fn detaching_moves_the_sound_without_changing_it() {
    let (mut tl, ids) = abc();
    tl.spine[1].audio.fade_in = secs(2);
    tl.spine[1].audio.audio_lead = secs(1);
    let probe = |tl: &Timeline| -> Vec<(u64, f32)> {
        let mut out = Vec::new();
        for s in audio_spans(tl).iter().filter(|s| s.asset == AssetId(2)) {
            for ms in (9_000..20_000).step_by(250) {
                let t = Time::from_secs_f64(ms as f64 / 1000.0);
                if t >= s.start && t < s.end() {
                    assert_eq!(s.source_time(t), secs(5) + t - secs(10));
                    out.push((ms, s.gain_at(t)));
                }
            }
        }
        out
    };
    let before = probe(&tl);
    run(&mut tl, Command::DetachAudio { id: ids[1] });
    assert!(!tl.clip(ids[1]).unwrap().av.audio);
    assert_eq!(probe(&tl), before);
}

#[test]
fn trims_of_either_half_move_both() {
    for on_audio in [false, true] {
        for at_start in [false, true] {
            for delta in [secs(2), -secs(2)] {
                let (mut tl, ids, audio) = detached();
                let id = if on_audio { audio } else { ids[1] };
                let cmd = if at_start { Command::TrimStart { id, delta } } else { Command::TrimEnd { id, delta } };
                run(&mut tl, cmd);
                assert_in_sync(&tl, 1);
                assert_ne!(tl.clip(ids[1]).unwrap().duration, secs(10));
            }
        }
    }
}

#[test]
fn ripples_upstream_carry_the_pair() {
    let (mut tl, ids, _) = detached();
    run(&mut tl, Command::TrimEnd { id: ids[0], delta: -secs(3) });
    assert_in_sync(&tl, 1);
    assert_eq!(pairs(&tl)[0].0 .0, secs(7));
}

#[test]
fn splits_make_two_pairs() {
    for how in 0..4 {
        let (mut tl, ids, audio) = detached();
        let at = secs(14);
        run(
            &mut tl,
            match how {
                0 => Command::Blade { at },
                1 => Command::BladeAll { at },
                2 => Command::BladeClip { id: ids[1], at },
                _ => Command::BladeClip { id: audio, at },
            },
        );
        assert_in_sync(&tl, 2);
    }
}

#[test]
fn slip_move_speed_and_enable_follow() {
    let (mut tl, ids, audio) = detached();
    run(&mut tl, Command::Slip { id: audio, delta: secs(2) });
    assert_in_sync(&tl, 1);
    run(&mut tl, Command::MoveSpine { id: ids[1], to_index: 0 });
    assert_in_sync(&tl, 1);
    let audio = tl.placed_clips().iter().find(|p| p.lane < 0).unwrap().clip.id;
    run(&mut tl, Command::SetSpeed { id: audio, speed: 2.0 });
    assert_in_sync(&tl, 1);
    let a = tl.placed_clips().iter().find(|p| p.lane < 0).unwrap().clip.clone();
    assert_eq!(a.duration, secs(5));
    assert_eq!(a.retime, tl.clip(ids[1]).unwrap().retime);
    run(&mut tl, Command::ToggleEnabled { id: ids[1] });
    assert!(tl.placed_clips().iter().filter(|p| p.clip.link.is_some()).all(|p| !p.clip.enabled));
}

#[test]
fn a_connected_pair_moves_together() {
    let (mut tl, _) = abc();
    let v = tl.connect(media(9, 4), secs(2), 1).unwrap();
    run(&mut tl, Command::DetachAudio { id: v });
    run(&mut tl, Command::MoveClip { id: v, at: secs(13), lane: 2 });
    assert_in_sync(&tl, 1);
    assert_eq!(pairs(&tl)[0].0 .0, secs(13));
}

#[test]
fn deleting_either_half_deletes_both() {
    for (on_audio, gap) in [(false, false), (true, false), (false, true)] {
        let (mut tl, ids, audio) = detached();
        let id = if on_audio { audio } else { ids[1] };
        run(&mut tl, if gap { Command::DeleteWithGap { id } } else { Command::DeleteRipple { id } });
        assert!(audio_spans(&tl).iter().all(|s| s.asset != AssetId(2)));
        assert!(tl.placed_clips().iter().all(|p| p.clip.asset_id() != Some(AssetId(2))));
    }
}

#[test]
fn selecting_both_halves_edits_them_once() {
    let (mut tl, ids, audio) = detached();
    let commands = vec![Command::TrimEnd { id: ids[1], delta: -secs(1) }, Command::TrimEnd { id: audio, delta: -secs(1) }];
    run(&mut tl, Command::Batch { label: "Trim", commands });
    assert_in_sync(&tl, 1);
    assert_eq!(tl.clip(ids[1]).unwrap().duration, secs(9));
}

#[test]
fn two_edits_of_one_linked_clip_both_apply() {
    let (mut tl, ids, _) = detached();
    let trim = Command::TrimEnd { id: ids[1], delta: -secs(1) };
    run(&mut tl, Command::Batch { label: "Trim", commands: vec![trim.clone(), trim] });
    assert_eq!(tl.clip(ids[1]).unwrap().duration, secs(8));
    assert_in_sync(&tl, 1);
}

#[test]
fn unlinked_halves_edit_apart_and_can_relink() {
    let (mut tl, ids, audio) = detached();
    run(&mut tl, Command::Unlink { id: audio });
    run(&mut tl, Command::TrimEnd { id: audio, delta: -secs(3) });
    assert_eq!(tl.clip(ids[1]).unwrap().duration, secs(10));
    assert_eq!(tl.clip(audio).unwrap().duration, secs(7));
    run(&mut tl, Command::Link { a: ids[1], b: audio });
    assert!(tl.clip(audio).unwrap().link.is_some());
}

#[test]
fn a_locked_half_protects_its_partner() {
    let (mut tl, ids, _) = detached();
    lock(&mut tl, -1);
    let before = tl.clone();
    for cmd in [Command::TrimEnd { id: ids[1], delta: -secs(1) }, Command::Blade { at: secs(14) }, Command::DeleteRipple { id: ids[1] }] {
        assert_eq!(History::default().execute(&mut tl, cmd.clone()), Err(EditError::Locked), "{cmd:?}");
        assert_eq!(tl, before, "{cmd:?}");
    }
}

#[test]
fn a_pair_survives_save_and_load() {
    let (tl, _, _) = detached();
    let project = Project { timeline: tl, ..Default::default() };
    let loaded = Project::from_json(&project.to_json().unwrap()).unwrap();
    assert_in_sync(&loaded.timeline, 1);
}

// ----- audio crossfade

fn power_at(tl: &Timeline, t: Time) -> f32 {
    audio_spans(tl).iter().filter(|s| t >= s.start && t < s.end()).map(|s| s.gain_at(t).powi(2)).sum()
}

#[test]
fn crossfade_overlaps_into_spare_media_at_equal_power() {
    let (mut tl, _) = abc();
    tl.spine[0].audio.crossfade_out = secs(2);
    let spans = audio_spans(&tl);
    let (a, b) = (&spans[0], &spans[1]);
    assert_eq!((a.end(), b.start), (secs(11), secs(9)));
    assert_eq!(b.source_time(secs(10)), secs(5), "media stays in sync");
    assert_eq!(b.source_time(secs(9)), secs(4));
    for tenths in 91..110 {
        let p = power_at(&tl, Time::from_secs_f64(tenths as f64 / 10.0));
        assert!((p - 1.0).abs() < 0.01, "power {p} at {tenths}");
    }
}

#[test]
fn crossfade_set_on_the_second_clip_is_the_same() {
    let (mut tl, _) = abc();
    tl.spine[1].audio.crossfade_in = secs(2);
    let spans = audio_spans(&tl);
    assert_eq!((spans[0].end(), spans[1].start), (secs(11), secs(9)));
}

#[test]
fn crossfade_without_spare_media_fades_through_the_cut() {
    let (mut tl, _) = abc();
    for c in &mut tl.spine {
        c.source_in = Time::ZERO;
        c.media_duration = Some(c.duration);
    }
    tl.spine[0].audio.crossfade_out = secs(2);
    let spans = audio_spans(&tl);
    assert_eq!((spans[0].end(), spans[1].start), (secs(10), secs(10)));
    assert!(spans[0].gain_at(secs(10)) < 0.01 && spans[1].gain_at(secs(10)) < 0.01);
}

#[test]
fn crossfade_of_continuous_media_keeps_its_level() {
    let mut tl = Timeline::new("t", VideoFormat::default());
    tl.append(media(1, 20)).unwrap();
    tl.blade(secs(10)).unwrap();
    tl.spine[0].audio.crossfade_out = secs(2);
    let t = Time::from_secs_f64(10.3);
    let sum: f32 = audio_spans(&tl).iter().filter(|s| t >= s.start && t < s.end()).map(|s| s.gain_at(t)).sum();
    assert!((sum - 1.0).abs() < 0.01, "sum {sum}");
}

#[test]
fn crossfade_at_the_outer_edge_is_a_fade() {
    let (mut tl, _) = abc();
    tl.spine[0].audio.crossfade_in = secs(2);
    let first = &audio_spans(&tl)[0];
    assert_eq!(first.start, Time::ZERO);
    assert!(first.gain_at(Time::ZERO) < 0.01 && first.gain_at(secs(3)) > 0.99);
}

// ----- speed

#[test]
fn speed_covers_the_same_media_and_returns() {
    let (mut tl, ids) = abc();
    for (speed, length) in [(2.0, 5), (-1.0, 10), (0.5, 20), (1.0, 10)] {
        run(&mut tl, Command::SetSpeed { id: ids[0], speed });
        let c = tl.clip(ids[0]).unwrap();
        assert_eq!(c.duration, secs(length), "speed {speed}");
        let (lo, hi) = c.source_range();
        assert!((lo - secs(5)).abs() <= Time(1) && (hi - secs(15)).abs() <= Time(1), "speed {speed}: {lo:?}..{hi:?}");
    }
    assert!(tl.clip(ids[0]).unwrap().retime.is_none());
}

#[test]
fn speed_is_limited_to_a_tenth_and_ten_times() {
    let (mut tl, ids) = abc();
    run(&mut tl, Command::SetSpeed { id: ids[0], speed: 50.0 });
    assert_eq!(tl.clip(ids[0]).unwrap().duration, secs(1));
    assert!(History::default().execute(&mut tl, Command::SetSpeed { id: ids[0], speed: 0.0 }).is_err());
}

#[test]
fn dragging_speed_back_lands_on_the_same_media() {
    let (mut tl, ids) = abc();
    let before = tl.clone();
    let mut history = History::default();
    for step in (10..=80).chain((10..80).rev()) {
        history.execute_coalesced(&mut tl, Command::SetSpeed { id: ids[0], speed: step as f64 / 10.0 }, Some(1)).unwrap();
    }
    let (lo, hi) = tl.clip(ids[0]).unwrap().source_range();
    assert!((lo - secs(5)).abs() < Time::from_secs_f64(0.001) && (hi - secs(15)).abs() < Time::from_secs_f64(0.001), "{lo:?}..{hi:?}");
    assert!(history.undo(&mut tl));
    assert_eq!(tl, before, "one drag is one undo step");
}

// ----- project settings

#[test]
fn renaming_is_undoable() {
    let (mut tl, _) = abc();
    run(&mut tl, Command::Rename { name: "renamed".into() });
    assert_eq!(tl.name, "renamed");
}

#[test]
fn undo_and_redo_leave_the_snapping_switch_alone() {
    let (mut tl, ids) = abc();
    let mut history = History::default();
    history.execute(&mut tl, Command::ToggleEnabled { id: ids[0] }).unwrap();
    tl.snapping = false;
    assert!(history.undo(&mut tl));
    assert!(!tl.snapping);
    tl.snapping = true;
    assert!(history.redo(&mut tl));
    assert!(tl.snapping);
}
