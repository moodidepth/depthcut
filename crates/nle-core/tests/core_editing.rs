//! Core editing: what is stored on a clip is what preview and export get.

use nle_core::eval::{audio_spans, evaluate};
use nle_core::{AssetId, Clip, ClipKind, Retime, Time, Timeline, VideoFormat};

fn secs(s: i64) -> Time {
    Time::from_secs(s)
}

fn media(asset: u64, length: i64) -> Clip {
    let mut c = Clip::new("media", ClipKind::Asset { asset: AssetId(asset) }, secs(length));
    c.media_duration = Some(secs(length));
    c
}

fn compound_of_one(length: i64) -> Timeline {
    let mut tl = Timeline::new("t", VideoFormat::default());
    let id = tl.append(media(1, length)).unwrap();
    tl.make_compound(&[id], "compound").unwrap();
    tl
}

#[test]
fn compound_clip_fades_reach_the_mixer() {
    let mut tl = compound_of_one(10);
    tl.spine[0].audio.fade_in = secs(2);
    tl.spine[0].audio.fade_out = secs(2);
    let span = &audio_spans(&tl)[0];
    assert!(span.gain_at(Time::ZERO) < 0.01, "compound fade-in");
    assert!(span.gain_at(secs(5)) > 0.99);
    assert!(span.gain_at(secs(10)) < 0.01, "compound fade-out");
}

#[test]
fn compound_clip_volume_keyframes_reach_the_mixer() {
    let mut tl = compound_of_one(10);
    tl.spine[0].audio.volume_db.add_keyframe(Time::ZERO);
    tl.spine[0].audio.volume_db.add_keyframe(secs(10));
    tl.spine[0].audio.volume_db.set(secs(10), -24.0);
    let span = &audio_spans(&tl)[0];
    assert!(span.gain_at(secs(9)) < 0.2, "compound volume automation");
}

#[test]
fn compound_clip_pan_reaches_the_mixer() {
    let mut tl = compound_of_one(10);
    tl.spine[0].audio.pan.value = -1.0;
    assert_eq!(audio_spans(&tl)[0].pan_at(secs(5)), -1.0);
}

#[test]
fn retimed_compound_clip_is_audible() {
    let mut tl = compound_of_one(10);
    let r = Retime::uniform(secs(10), 2.0);
    tl.spine[0].duration = r.timeline_duration();
    tl.spine[0].retime = Some(r);
    let spans = audio_spans(&tl);
    assert_eq!(spans.len(), 1, "a retimed compound still has sound");
    assert!((spans[0].source_time(secs(2)).as_secs_f64() - 4.0).abs() < 1e-3);
}

#[test]
fn opposite_crops_never_remove_more_than_the_whole_frame() {
    let mut tl = Timeline::new("t", VideoFormat::default());
    let id = tl.append(media(1, 10)).unwrap();
    let v = &mut tl.clip_mut(id).unwrap().video;
    v.crop_left.value = 0.8;
    v.crop_right.value = 0.7;
    let [l, r, _, _] = evaluate(&tl, secs(1)).layers()[0].video.crop;
    assert!(l + r < 1.0, "crop inverted: left {l} + right {r}");
}

#[test]
fn start_transition_works_after_another_clip() {
    let mut tl = Timeline::new("t", VideoFormat::default());
    tl.append(media(1, 10)).unwrap();
    let b = tl.append(media(2, 10)).unwrap();
    tl.clip_mut(b).unwrap().transition_in = Some(nle_core::Transition::new(nle_core::TransitionKind::CrossDissolve, secs(2)));
    let comp = evaluate(&tl, secs(10));
    assert!(matches!(comp.nodes[0], nle_core::RenderNode::Transition { .. }), "a start transition at a cut was ignored");
}

#[test]
fn splitting_a_compound_gives_each_half_its_own_nested_ids() {
    let mut tl = compound_of_one(10);
    let (left, right) = tl.spine[0].split(secs(4)).unwrap();
    tl.spine = vec![left, right];
    let mut ids = Vec::new();
    tl.spine.iter().for_each(|c| c.collect_ids(&mut ids));
    let n = ids.len();
    ids.sort();
    ids.dedup();
    assert_eq!(ids.len(), n, "duplicate nested ids");
}

#[test]
fn reversed_compound_plays_each_inner_clip_only_in_its_turn() {
    let mut tl = Timeline::new("t", VideoFormat::default());
    let a = tl.append(media(1, 4)).unwrap();
    let b = tl.append(media(2, 4)).unwrap();
    tl.make_compound(&[a, b], "compound").unwrap();
    tl.spine[0].retime = Some(Retime::uniform(secs(8), -1.0));
    let heard = |t: i64| -> Vec<u64> { audio_spans(&tl).iter().filter(|s| s.start <= secs(t) && secs(t) < s.end() && s.gain_at(secs(t)) > 0.5).map(|s| s.asset.0).collect() };
    assert_eq!((heard(2), heard(6)), (vec![2], vec![1]));
}

#[test]
fn pans_of_a_clip_and_its_compound_multiply() {
    let mut tl = compound_of_one(10);
    tl.spine[0].audio.pan.value = -1.0;
    let ClipKind::Compound(comp) = &mut tl.spine[0].kind else { unreachable!() };
    comp.spine[0].audio.pan.value = 1.0;
    assert_eq!(audio_spans(&tl)[0].balance_at(secs(1)), (0.0, 0.0));
}
