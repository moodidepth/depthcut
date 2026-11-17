//! TEMPORARY probe tests written during the 2026-11-17 bug hunt. Delete after review.
use nle_core::speed::{FrameBlendMode, Retime, RetimeSegment};
use nle_core::*;

fn secs(s: i64) -> Time {
    Time::from_secs(s)
}

/// Extending the head of a retimed clip that already starts at source 0
/// should not reveal media before the source start.
#[test]
fn probe_retimed_trim_start_cannot_go_before_source() {
    let a = MediaAsset::new("a", "/m/a.mp4", secs(10));
    let mut c = Clip::from_asset(&a);
    c.duration = secs(4);
    c.retime = Some(Retime { segments: vec![RetimeSegment::new(secs(4), 0.5)], start_offset: Time::ZERO, lead_speed: None, blend: FrameBlendMode::FrameBlend, velocity_curve: None });
    let mut tl = Timeline::default();
    let id = tl.append(c).unwrap();
    let applied = tl.trim_start(id, -secs(2)).unwrap();
    let c = tl.clip(id).unwrap();
    let src0 = c.source_in.as_secs_f64() + c.retime.as_ref().unwrap().map_secs(0.0);
    println!("applied={:?} source at clip start = {src0}", applied);
    assert!(src0 >= -1e-6, "retimed head trim revealed source before 0: {src0}s");
}

/// Extending the tail of a retimed clip should stop at the end of the media.
#[test]
fn probe_retimed_trim_end_cannot_go_past_media() {
    let a = MediaAsset::new("a", "/m/a.mp4", secs(10));
    let mut c = Clip::from_asset(&a);
    c.duration = secs(4);
    c.retime = Some(Retime { segments: vec![RetimeSegment::new(secs(4), 2.0)], start_offset: Time::ZERO, lead_speed: None, blend: FrameBlendMode::FrameBlend, velocity_curve: None });
    let mut tl = Timeline::default();
    let id = tl.append(c).unwrap();
    tl.trim_end(id, secs(20)).unwrap();
    let c = tl.clip(id).unwrap();
    let r = c.retime.as_ref().unwrap();
    let src_end = c.source_in.as_secs_f64() + r.map_secs(c.duration.as_secs_f64());
    println!("duration={:?} source at clip end = {src_end}", c.duration);
    assert!(src_end <= 10.0 + 1e-3, "retimed tail trim ran past media end: {src_end}s of 10s");
}

/// Undoing back to the saved state should not leave the project "edited".
#[test]
fn probe_undo_to_clean_state() {
    let a = MediaAsset::new("a", "/m/a.mp4", secs(10));
    let mut tl = Timeline::default();
    tl.append(Clip::from_asset(&a)).unwrap();
    let mut h = History::new(10);
    let base = h.revision();
    h.execute(&mut tl, Command::Blade { at: secs(3) }).ok();
    h.undo(&mut tl);
    println!("base={base} after undo={}", h.revision());
}

/// Dragging the LEFT edge of a connected clip (B-roll/title/music) to the right
/// should move its start and keep its end where it was.
#[test]
fn probe_connected_trim_start_keeps_end() {
    let a = MediaAsset::new("a", "/m/a.mp4", secs(20));
    let mut tl = Timeline::default();
    tl.append(Clip::from_asset(&a)).unwrap();
    let mut b = Clip::from_asset(&a);
    b.duration = secs(5);
    let id = tl.connect(b, secs(2), 1).unwrap();
    let end_before = tl.clip_start(id).unwrap() + tl.clip(id).unwrap().duration;
    tl.trim_start(id, secs(1)).unwrap();
    let start = tl.clip_start(id).unwrap();
    let end_after = start + tl.clip(id).unwrap().duration;
    println!("start {:?} end {:?} -> {:?}", start, end_before, end_after);
    assert_eq!(end_after, end_before, "connected clip's end moved; start stayed at {start:?}");
}

/// Ripple-deleting a clip should keep later captions on the words they belong to.
#[test]
fn probe_captions_follow_ripple_delete() {
    let a = MediaAsset::new("a", "/m/a.mp4", secs(10));
    let mut tl = Timeline::default();
    let first = tl.append(Clip::from_asset(&a)).unwrap();
    tl.append(Clip::from_asset(&a)).unwrap();
    tl.captions.segments.push(nle_core::captions::CaptionSegment::new(secs(12), secs(13), "hello"));
    tl.remove_ripple(first).unwrap();
    let s = tl.captions.segments[0].start;
    println!("caption start after deleting first 10s clip: {s:?}");
    assert_eq!(s, secs(2), "caption stayed at old time");
}
