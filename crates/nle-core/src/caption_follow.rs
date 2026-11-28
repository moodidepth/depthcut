//! Keeps captions on the speech they transcribe while the timeline is edited.
//!
//! Captions are stored in absolute timeline time, but they describe words
//! inside particular clips. Before an edit each caption is anchored to the
//! point of the source media under its midpoint; after the edit the caption is
//! moved to wherever that source point now sits. Captions whose speech was
//! removed from the timeline are dropped; captions that were not over any
//! media clip follow the storyline clip under them, or stay where they are.
//!
//! Analysis markers (kills, beats) are handled the same way: they mark a
//! moment in the gameplay or the music, so they move with it and vanish when
//! it is cut out. Markers placed by hand keep their timeline position.

use crate::clip::{AudioRole, Clip, ClipKind};
use crate::ids::{AssetId, ClipId};
use crate::time::Time;
use crate::timeline::{Marker, MarkerKind, PlacedClip, Timeline};

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

/// Snapshot of every caption's (and analysis marker's) anchor, taken before an edit.
#[derive(Clone, Debug)]
pub struct CaptionAnchors {
    captions: Vec<Anchor>,
    /// One per timeline marker; `None` for markers that stay put.
    markers: Vec<Option<Anchor>>,
}

/// Kill and beat markers come from analysing the media, so they mark a moment
/// in it and travel with it. Markers placed by hand stay where they are.
pub fn marker_follows_media(m: &Marker) -> bool {
    m.kind != MarkerKind::Standard
}

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

/// Anchor the moment `at` to the media under it. Among the media clips there,
/// the lowest `rank` wins, then the storyline before connected lanes.
fn anchor_at(placed: &[PlacedClip], at: Time, rank: impl Fn(&Clip) -> u8) -> Anchor {
    let under = |p: &&PlacedClip| p.start <= at && at < p.end();
    let media = placed.iter().filter(under).filter(|p| is_media(p.clip)).min_by_key(|p| (rank(p.clip), p.lane != 0, p.lane.abs()));
    if let Some(p) = media {
        return Anchor::Speech { clip: p.clip.id, asset: p.clip.asset_id(), source: p.clip.source_time(at - p.start), at };
    }
    match placed.iter().filter(under).find(|p| p.lane == 0) {
        Some(p) => Anchor::Local { clip: p.clip.id, local: at - p.start, at },
        None => Anchor::Free,
    }
}

/// Music the timeline plays: a clip given the Music role, or audio-only media.
fn is_music(c: &Clip) -> bool {
    audible(c) && (c.audio.role == AudioRole::Music || !c.has_video())
}

/// Anchor a beat to the music under it. Without music it stays put: a beat
/// grid must not slide or vanish when the gameplay under it is ramped or cut.
fn anchor_beat(placed: &[PlacedClip], at: Time) -> Anchor {
    let music: Vec<PlacedClip> = placed.iter().filter(|p| is_music(p.clip)).cloned().collect();
    match anchor_at(&music, at, |c| u8::from(c.audio.role != AudioRole::Music)) {
        a @ Anchor::Speech { .. } => a,
        _ => Anchor::Free,
    }
}

/// Audible clips of `role` first, then other audible media, then the rest.
fn role_rank(c: &Clip, role: AudioRole) -> u8 {
    2 * u8::from(!audible(c)) + u8::from(c.audio.role != role)
}

/// How far an anchored moment moved in the edit; `None` if its media is gone.
fn shift_for(placed: &[PlacedClip], anchor: &Anchor) -> Option<Time> {
    match anchor {
        Anchor::Speech { clip, asset, source, at } => {
            // the same clip first, then any other piece of the same media
            // (blades and duplicates), nearest to where it was
            let candidates = placed.iter().filter(|p| p.clip.id == *clip || (asset.is_some() && p.clip.asset_id() == *asset));
            let found = candidates.filter_map(|p| local_for_source(p.clip, *source).map(|l| (p.clip.id != *clip, p.start + l))).min_by_key(|(other, t)| (*other, (t.0 - at.0).abs()));
            match found {
                Some((_, t)) => Some(t - *at),
                None => {
                    let still_there = placed.iter().find(|p| p.clip.id == *clip);
                    // a reversed/ramped clip we cannot invert: leave it alone
                    let unmappable = still_there.is_some_and(|p| p.clip.retime.as_ref().is_some_and(|r| !r.is_monotonic()));
                    unmappable.then_some(Time::ZERO)
                }
            }
        }
        Anchor::Local { clip, local, at } => Some(placed.iter().find(|p| p.clip.id == *clip && *local < p.clip.duration).map_or(Time::ZERO, |p| p.start + *local - *at)),
        Anchor::Free => Some(Time::ZERO),
    }
}

/// Anchor every caption and analysis marker to the media under it. Cheap when there are none.
pub fn anchor_captions(tl: &Timeline) -> CaptionAnchors {
    let follow_markers = tl.markers.iter().any(marker_follows_media);
    if tl.captions.segments.is_empty() && !follow_markers {
        return CaptionAnchors { captions: Vec::new(), markers: Vec::new() };
    }
    let placed = tl.placed_clips();
    // captions: audible dialogue first, then other audible media, then any media
    let captions = tl
        .captions
        .segments
        .iter()
        .map(|seg| anchor_at(&placed, Time((seg.start.0 + seg.end.0) / 2), |c| role_rank(c, AudioRole::Dialogue)))
        .collect();
    let markers = if follow_markers {
        tl.markers
            .iter()
            .map(|m| {
                marker_follows_media(m).then(|| match m.kind {
                    // beats belong to the music; kills to the gameplay
                    MarkerKind::Beat | MarkerKind::SubBass | MarkerKind::Snare => anchor_beat(&placed, m.time),
                    _ => anchor_at(&placed, m.time, |c| u8::from(!audible(c))),
                })
            })
            .collect()
    } else {
        Vec::new()
    };
    CaptionAnchors { captions, markers }
}

/// Move captions and analysis markers after an edit so they stay on their media
/// (see module docs). `anchors` must come from [`anchor_captions`] on the same timeline.
pub fn follow_edit(tl: &mut Timeline, anchors: &CaptionAnchors) {
    let captions_ok = anchors.captions.len() == tl.captions.segments.len() && !anchors.captions.is_empty();
    let markers_ok = anchors.markers.len() == tl.markers.len() && !anchors.markers.is_empty();
    if !captions_ok && !markers_ok {
        return;
    }
    let placed = tl.placed_clips();
    let caption_shifts: Vec<Option<Time>> = if captions_ok { anchors.captions.iter().map(|a| shift_for(&placed, a)).collect() } else { Vec::new() };
    let marker_shifts: Vec<Option<Time>> =
        if markers_ok { anchors.markers.iter().map(|a| a.as_ref().map_or(Some(Time::ZERO), |a| shift_for(&placed, a))).collect() } else { Vec::new() };
    drop(placed);
    let unchanged = |v: &[Option<Time>]| v.iter().all(|s| s.is_some_and(|d| d.is_zero()));
    if captions_ok && !unchanged(&caption_shifts) {
        let mut i = 0;
        tl.captions.segments.retain_mut(|seg| {
            let shift = caption_shifts[i];
            i += 1;
            if let Some(d) = shift.filter(|d| !d.is_zero()) {
                seg.shift(d);
            }
            shift.is_some()
        });
        tl.captions.sort();
    }
    if markers_ok && !unchanged(&marker_shifts) {
        let mut i = 0;
        tl.markers.retain_mut(|m| {
            let shift = marker_shifts[i];
            i += 1;
            if let Some(d) = shift {
                m.time = (m.time + d).max(Time::ZERO);
            }
            shift.is_some()
        });
        tl.markers.sort_by_key(|m| m.time);
    }
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
    fn kill_markers_follow_the_gameplay_and_hand_markers_stay() {
        // one 54 s gameplay clip with kills at 9.5, 39.4 and 43.8 s
        let a = MediaAsset::new("game", "/m/game.mp4", secs(54));
        let mut tl = Timeline::default();
        tl.append(Clip::from_asset(&a)).unwrap();
        let ms = |ms: i64| Time::from_secs_f64(ms as f64 / 1000.0);
        tl.markers = vec![Marker::kill(ms(9500), "Kill"), Marker::new(secs(20), "note"), Marker::kill(ms(39400), "Kill"), Marker::kill(ms(43800), "Double Kill")];
        // keep 6-11 s and 36.5-46 s, as an editor would with blade + ripple delete
        for t in [6000, 11000, 36500, 46000] {
            Command::Blade { at: ms(t) }.apply(&mut tl).unwrap();
        }
        for i in [4, 2, 0] {
            let id = tl.spine[i].id;
            Command::DeleteRipple { id }.apply(&mut tl).unwrap();
        }
        let kills: Vec<i64> = tl.markers.iter().filter(|m| m.kind == MarkerKind::Kill).map(|m| (m.time.as_secs_f64() * 1000.0).round() as i64).collect();
        assert_eq!(kills, vec![3500, 7900, 12300], "kills sit on the same frames of gameplay");
        let note = tl.markers.iter().find(|m| m.kind == MarkerKind::Standard).expect("hand marker kept");
        assert_eq!(note.time, secs(20), "hand-placed markers keep their timeline position");
    }

    #[test]
    fn beats_stay_on_the_music_not_the_gameplay() {
        use crate::speed::Retime;
        let game = MediaAsset::new("game", "/m/game.mp4", secs(10));
        let mut tl = Timeline::default();
        let mut c = Clip::from_asset(&game);
        c.duration = secs(8);
        (c.av.video, c.av.audio) = (true, true);
        let id = tl.append(c).unwrap();
        tl.markers = vec![Marker::beat(secs(3)), Marker::kill(Time::from_millis(4500), "Kill")];
        // no music: a speed ramp on the gameplay must not drag the beat along
        let retime = Retime::velocity_preset("Valorant Flow Sync", secs(8)).unwrap();
        Command::SetRetime { id, retime: Some(retime) }.apply(&mut tl).unwrap();
        let beat = tl.markers.iter().find(|m| m.kind == MarkerKind::Beat).expect("beat kept");
        assert_eq!(beat.time, secs(3));
        // and cutting the gameplay to the beat keeps the beat
        Command::SyncClipsToBeats { ids: vec![id] }.apply(&mut tl).unwrap();
        assert_eq!(tl.spine[0].duration, secs(3));
        assert!(tl.markers.iter().any(|m| m.kind == MarkerKind::Beat && m.time == secs(3)));

        // with a song under it, the beat moves with the song
        let song = MediaAsset::new("song", "/m/song.wav", secs(30));
        let mut s = Clip::from_asset(&song);
        s.av.video = false;
        s.av.audio = true;
        s.audio.role = AudioRole::Music;
        s.duration = secs(3);
        let sid = tl.connect(s, Time::ZERO, -1).unwrap();
        tl.markers = vec![Marker::beat(secs(1))];
        Command::Slip { id: sid, delta: secs(-1) }.apply(&mut tl).ok();
        Command::Slip { id: sid, delta: secs(1) }.apply(&mut tl).unwrap();
        assert_eq!(tl.markers.len(), 1);
        assert_eq!(tl.markers[0].time, Time::ZERO, "slipping the song by 1 s moves its beat by 1 s");
    }

    #[test]
    fn kill_marker_in_a_deleted_section_is_removed() {
        let (mut tl, first, _) = two_clips();
        tl.markers = vec![Marker::kill(secs(4), "Kill"), Marker::kill(secs(14), "Kill")];
        Command::DeleteRipple { id: first }.apply(&mut tl).unwrap();
        assert_eq!(tl.markers.len(), 1);
        assert_eq!(tl.markers[0].time, secs(4));
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
