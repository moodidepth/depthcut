//! Keeps captions on the speech they transcribe while the timeline is edited.
//!
//! Captions are stored in absolute timeline time, but they describe words
//! inside particular clips. Before an edit each caption is anchored to the
//! point of the source media under its midpoint; after the edit the caption is
//! moved to wherever that source point now sits. Captions whose speech was
//! removed from the timeline are dropped; captions that were not over any
//! media clip follow the storyline clip under them, or stay where they are.

use crate::clip::{AudioRole, Clip, ClipKind};
use crate::ids::{AssetId, ClipId};
use crate::time::Time;
use crate::timeline::{PlacedClip, Timeline};

/// Where one caption's speech lives before an edit.
#[derive(Clone, Debug)]
enum Anchor {
    /// A point in the media of an asset/compound clip (normally the speech).
    Speech { clip: ClipId, asset: Option<AssetId>, source: Time, at: Time },
    /// Over a title/generator only: follow that storyline clip if it moves.
    Local { clip: ClipId, local: Time, at: Time },
    /// Not over any clip: leave it alone.
    Free,
}

/// Snapshot of every caption's anchor, taken before an edit.
#[derive(Clone, Debug)]
pub struct CaptionAnchors(Vec<Anchor>);

fn audible(c: &Clip) -> bool {
    c.enabled && c.has_audio() && !c.audio.muted && !c.is_gap()
}

/// A clip that shows real media (so a caption can be pinned to its source time).
fn is_media(c: &Clip) -> bool {
    c.enabled && matches!(c.kind, ClipKind::Asset { .. } | ClipKind::Compound(_))
}

/// Clip-local timeline offset at which `c` shows source time `source`.
fn local_for_source(c: &Clip, source: Time) -> Option<Time> {
    let local = match &c.retime {
        Some(r) => r.inverse(source - c.source_in)?,
        None => source - c.source_in,
    };
    // the end itself is outside the clip; allow one tick of rounding slack
    (local >= Time::ZERO - Time(1) && local < c.duration).then_some(local.max(Time::ZERO))
}

/// Anchor every caption to the media under it. Cheap when there are no captions.
pub fn anchor_captions(tl: &Timeline) -> CaptionAnchors {
    if tl.captions.segments.is_empty() {
        return CaptionAnchors(Vec::new());
    }
    let placed = tl.placed_clips();
    let anchors = tl
        .captions
        .segments
        .iter()
        .map(|seg| {
            let at = Time((seg.start.0 + seg.end.0) / 2);
            let under = |p: &&PlacedClip| p.start <= at && at < p.end();
            // audible dialogue first, then other audible media, then any media;
            // within each, the storyline before connected lanes
            let speech = placed.iter().filter(under).filter(|p| is_media(p.clip)).min_by_key(|p| {
                (!audible(p.clip), p.clip.audio.role != AudioRole::Dialogue, p.lane != 0, p.lane.abs())
            });
            if let Some(p) = speech {
                return Anchor::Speech {
                    clip: p.clip.id,
                    asset: p.clip.asset_id(),
                    source: p.clip.source_time(at - p.start),
                    at,
                };
            }
            match placed.iter().filter(under).find(|p| p.lane == 0) {
                Some(p) => Anchor::Local { clip: p.clip.id, local: at - p.start, at },
                None => Anchor::Free,
            }
        })
        .collect();
    CaptionAnchors(anchors)
}

/// Move captions after an edit so they stay on their speech (see module docs).
/// `anchors` must come from [`anchor_captions`] on the same caption track.
pub fn follow_edit(tl: &mut Timeline, anchors: &CaptionAnchors) {
    if anchors.0.len() != tl.captions.segments.len() || anchors.0.is_empty() {
        return;
    }
    let placed = tl.placed_clips();
    let mut keep = vec![true; anchors.0.len()];
    let mut shifts = vec![Time::ZERO; anchors.0.len()];
    for (i, anchor) in anchors.0.iter().enumerate() {
        match anchor {
            Anchor::Speech { clip, asset, source, at } => {
                // the same clip first, then any other piece of the same media
                // (blades and duplicates), nearest to where the caption was
                let candidates = placed.iter().filter(|p| p.clip.id == *clip || (asset.is_some() && p.clip.asset_id() == *asset));
                let found = candidates
                    .filter_map(|p| local_for_source(p.clip, *source).map(|l| (p.clip.id != *clip, p.start + l)))
                    .min_by_key(|(other, t)| (*other, (t.0 - at.0).abs()));
                match found {
                    Some((_, t)) => shifts[i] = t - *at,
                    None => {
                        let still_there = placed.iter().find(|p| p.clip.id == *clip);
                        // a reversed/ramped clip we cannot invert: leave the caption alone
                        let unmappable = still_there.is_some_and(|p| p.clip.retime.as_ref().is_some_and(|r| !r.is_monotonic()));
                        keep[i] = unmappable;
                    }
                }
            }
            Anchor::Local { clip, local, at } => {
                if let Some(p) = placed.iter().find(|p| p.clip.id == *clip && *local < p.clip.duration) {
                    shifts[i] = p.start + *local - *at;
                }
            }
            Anchor::Free => {}
        }
    }
    drop(placed);
    if keep.iter().all(|k| *k) && shifts.iter().all(|s| s.is_zero()) {
        return;
    }
    let mut i = 0;
    tl.captions.segments.retain_mut(|seg| {
        let (k, d) = (keep[i], shifts[i]);
        i += 1;
        if k && !d.is_zero() {
            seg.shift(d);
        }
        k
    });
    tl.captions.sort();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::asset::MediaAsset;
    use crate::captions::CaptionSegment;
    use crate::command::Command;

    fn secs(s: i64) -> Time {
        Time::from_secs(s)
    }

    fn two_clips() -> (Timeline, ClipId, ClipId) {
        let a = MediaAsset::new("a", "/m/a.mp4", secs(10));
        let b = MediaAsset::new("b", "/m/b.mp4", secs(10));
        let mut tl = Timeline::default();
        let first = tl.append(Clip::from_asset(&a)).unwrap();
        let second = tl.append(Clip::from_asset(&b)).unwrap();
        (tl, first, second)
    }

    #[test]
    fn ripple_delete_moves_later_captions_and_drops_deleted_speech() {
        let (mut tl, first, _) = two_clips();
        tl.captions.segments.push(CaptionSegment::new(secs(3), secs(4), "in first"));
        tl.captions.segments.push(CaptionSegment::new(secs(12), secs(13), "in second"));
        Command::DeleteRipple { id: first }.apply(&mut tl).unwrap();
        assert_eq!(tl.captions.segments.len(), 1, "caption over deleted speech is removed");
        assert_eq!(tl.captions.segments[0].start, secs(2));
        assert_eq!(tl.captions.segments[0].text, "in second");
    }

    #[test]
    fn trim_start_of_earlier_clip_pulls_captions_left() {
        let (mut tl, first, _) = two_clips();
        tl.captions.segments.push(CaptionSegment::new(secs(15), secs(16), "x"));
        Command::TrimStart { id: first, delta: secs(3) }.apply(&mut tl).unwrap();
        assert_eq!(tl.captions.segments[0].start, secs(12));
    }

    #[test]
    fn caption_stays_on_its_words_when_head_is_trimmed() {
        let (mut tl, first, _) = two_clips();
        // speech at 6–7 s inside the first clip; trimming 2 s off its head moves it to 4 s
        tl.captions.segments.push(CaptionSegment::new(secs(6), secs(7), "x"));
        Command::TrimStart { id: first, delta: secs(2) }.apply(&mut tl).unwrap();
        assert_eq!(tl.captions.segments[0].start, secs(4));
    }

    #[test]
    fn blade_does_not_move_captions() {
        let (mut tl, _, _) = two_clips();
        tl.captions.segments.push(CaptionSegment::new(secs(4), secs(6), "x"));
        Command::Blade { at: secs(5) }.apply(&mut tl).unwrap();
        assert_eq!(tl.captions.segments.len(), 1);
        assert_eq!(tl.captions.segments[0].start, secs(4));
    }

    #[test]
    fn reordering_clips_carries_captions_along() {
        let (mut tl, first, _) = two_clips();
        tl.captions.segments.push(CaptionSegment::new(secs(2), secs(3), "first"));
        tl.captions.segments.push(CaptionSegment::new(secs(12), secs(13), "second"));
        Command::MoveSpine { id: first, to_index: 1 }.apply(&mut tl).unwrap();
        let texts: Vec<_> = tl.captions.segments.iter().map(|s| (s.text.as_str(), s.start)).collect();
        assert_eq!(texts, vec![("second", secs(2)), ("first", secs(12))]);
    }

    #[test]
    fn caption_edits_are_not_re_anchored() {
        let (mut tl, _, _) = two_clips();
        let mut track = tl.captions.clone();
        track.segments.push(CaptionSegment::new(secs(1), secs(2), "x"));
        Command::SetCaptions { captions: Box::new(track) }.apply(&mut tl).unwrap();
        assert_eq!(tl.captions.segments[0].start, secs(1));
    }
}
