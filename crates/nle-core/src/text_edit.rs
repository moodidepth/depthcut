//! Text-based editing: the timeline's transcript (its caption words) read
//! as text, where deleting words ripple-deletes the speech they cover.
//!
//! Words are listed at the time they are heard: caption time plus the
//! track's sync offset. Cuts are frame-aligned ranges on the primary
//! storyline ([`Command::DeleteRange`]). Before cutting, the deleted words
//! leave their caption blocks and blocks that span a cut are split, so the
//! remaining captions keep following their speech (see
//! [`crate::caption_follow`]). Everything is one undo step.
//!
//! Source transcripts use the same word lists in source time: a text
//! selection becomes In / Out marks ([`selection_span`]).

use crate::captions::{CaptionSegment, CaptionTrack, CaptionWord};
use crate::command::Command;
use crate::time::{FrameRate, Time};

/// Single-token hesitation sounds ("um", "uh", …). Matched without
/// punctuation and case.
pub const FILLERS: &[&str] = &["um", "umm", "ummm", "uh", "uhh", "uhm", "uhmm", "er", "erm", "ah", "ahh", "eh", "hmm", "hm", "mm", "mmm", "mhm"];

/// A word as heard on the timeline.
#[derive(Clone, Debug, PartialEq)]
pub struct Word {
    pub start: Time,
    pub end: Time,
    pub text: String,
}

/// Words of `seg` (evenly timed when the block has no word timings, e.g. after an .srt import).
fn segment_words(seg: &CaptionSegment) -> Vec<CaptionWord> {
    if !seg.words.is_empty() {
        return seg.words.clone();
    }
    let mut s = seg.clone();
    s.retime_words_evenly();
    s.words
}

/// Every caption word in timeline order, at the time it is heard.
pub fn timeline_words(track: &CaptionTrack) -> Vec<Word> {
    let off = track.sync_offset();
    let mut words: Vec<Word> = track
        .segments
        .iter()
        .flat_map(segment_words)
        .filter(|w| !w.text.trim().is_empty())
        .map(|w| Word { start: w.start + off, end: w.end.max(w.start) + off, text: w.text.trim().to_string() })
        .collect();
    words.sort_by_key(|w| w.start);
    words
}

/// Source-time words of a clip transcript.
pub fn source_words(words: &[CaptionWord]) -> Vec<Word> {
    let mut out: Vec<Word> =
        words.iter().filter(|w| !w.text.trim().is_empty()).map(|w| Word { start: w.start, end: w.end.max(w.start), text: w.text.trim().to_string() }).collect();
    out.sort_by_key(|w| w.start);
    out
}

/// The word without surrounding punctuation, lower-cased.
pub fn bare(text: &str) -> String {
    text.trim_matches(|c: char| !c.is_alphanumeric()).to_lowercase()
}

pub fn is_filler(text: &str) -> bool {
    FILLERS.contains(&bare(text).as_str())
}

/// Indices of filler words.
pub fn filler_indices(words: &[Word]) -> Vec<usize> {
    words.iter().enumerate().filter(|(_, w)| is_filler(&w.text)).map(|(i, _)| i).collect()
}

/// Indices of words matching `query` (a phrase of one or more words,
/// compared without punctuation and case). Every word of each match is listed.
pub fn find_phrase(words: &[Word], query: &str) -> Vec<usize> {
    let q: Vec<String> = query.split_whitespace().map(bare).filter(|w| !w.is_empty()).collect();
    if q.is_empty() || q.len() > words.len() {
        return Vec::new();
    }
    let bares: Vec<String> = words.iter().map(|w| bare(&w.text)).collect();
    let mut hits = Vec::new();
    for i in 0..=bares.len() - q.len() {
        let last = q.len() - 1;
        // the last query word may be a prefix (search while typing)
        let ok = q.iter().enumerate().all(|(k, qw)| if k == last { bares[i + k].starts_with(qw.as_str()) } else { bares[i + k] == *qw });
        if ok {
            hits.extend(i..i + q.len());
        }
    }
    hits.dedup();
    hits
}

/// Snap a range to frames, dropping it if it becomes empty.
fn snapped(start: Time, end: Time, rate: FrameRate) -> Option<(Time, Time)> {
    let (s, e) = (start.max(Time::ZERO).snap_to_frame(rate), end.snap_to_frame(rate));
    (e > s).then_some((s, e))
}

/// Sort and merge overlapping or touching ranges.
pub fn merge_ranges(mut ranges: Vec<(Time, Time)>) -> Vec<(Time, Time)> {
    ranges.retain(|(s, e)| e > s);
    ranges.sort();
    let mut out: Vec<(Time, Time)> = Vec::with_capacity(ranges.len());
    for (s, e) in ranges {
        match out.last_mut() {
            Some(last) if s <= last.1 => last.1 = last.1.max(e),
            _ => out.push((s, e)),
        }
    }
    out
}

/// Timeline ranges that remove the selected words: each run of adjacent
/// selected words becomes one range from its first word's start to its last
/// word's end (the pause inside the run goes too), frame-aligned.
pub fn word_ranges(words: &[Word], selected: &[usize], rate: FrameRate) -> Vec<(Time, Time)> {
    let mut sel: Vec<usize> = selected.iter().copied().filter(|&i| i < words.len()).collect();
    sel.sort_unstable();
    sel.dedup();
    let mut ranges = Vec::new();
    let mut i = 0;
    while i < sel.len() {
        let mut j = i;
        while j + 1 < sel.len() && sel[j + 1] == sel[j] + 1 {
            j += 1;
        }
        let (a, b) = (&words[sel[i]], &words[sel[j]]);
        ranges.extend(snapped(a.start, b.end.max(a.end), rate));
        i = j + 1;
    }
    merge_ranges(ranges)
}

/// The span of a selection (first selected word's start to last's end), for
/// setting In / Out from text.
pub fn selection_span(words: &[Word], selected: &[usize]) -> Option<(Time, Time)> {
    let lo = selected.iter().filter(|&&i| i < words.len()).min()?;
    let hi = selected.iter().filter(|&&i| i < words.len()).max()?;
    Some((words[*lo].start, words[*hi].end.max(words[*lo].start)))
}

/// Silences between words longer than `min_gap`, shortened to `keep`
/// (half of it stays on each side so speech is not clipped).
pub fn pause_ranges(words: &[Word], min_gap: Time, keep: Time, rate: FrameRate) -> Vec<(Time, Time)> {
    let half = Time(keep.0.max(0) / 2);
    let mut ranges = Vec::new();
    let mut heard_until: Option<Time> = None;
    for w in words {
        if let Some(prev_end) = heard_until {
            if w.start - prev_end > min_gap && w.start - prev_end > keep {
                ranges.extend(snapped(prev_end + half, w.start - half, rate));
            }
        }
        heard_until = Some(heard_until.map_or(w.end, |t| t.max(w.end)));
    }
    merge_ranges(ranges)
}

/// Total length of `ranges`.
pub fn total(ranges: &[(Time, Time)]) -> Time {
    ranges.iter().fold(Time::ZERO, |acc, (s, e)| acc + (*e - *s))
}

/// Caption track with the words inside `ranges` (heard time) removed and
/// every block that spans a cut split there, so each block sits wholly on
/// one side of every cut.
pub fn captions_without(track: &CaptionTrack, ranges: &[(Time, Time)]) -> CaptionTrack {
    let off = track.sync_offset();
    let inside = |t: Time| ranges.iter().any(|(s, e)| t + off >= *s && t + off < *e);
    let between = |a: Time, b: Time| ranges.iter().any(|(s, e)| *s < b + off && *e > a + off);
    let mut out = track.clone();
    out.segments.clear();
    for seg in &track.segments {
        if seg.words.is_empty() {
            if !inside(Time((seg.start.0 + seg.end.0) / 2)) {
                out.segments.push(seg.clone());
            }
            continue;
        }
        let kept: Vec<&CaptionWord> = seg.words.iter().filter(|w| !inside(Time((w.start.0 + w.end.0) / 2))).collect();
        if kept.len() == seg.words.len() && !kept.windows(2).any(|p| between(mid(p[0]), mid(p[1]))) {
            out.segments.push(seg.clone());
            continue;
        }
        let mut groups: Vec<Vec<CaptionWord>> = Vec::new();
        for w in kept {
            match groups.last_mut() {
                Some(g) if !between(mid(g.last().unwrap()), mid(w)) => g.push(w.clone()),
                _ => groups.push(vec![w.clone()]),
            }
        }
        let n = groups.len();
        for (k, g) in groups.into_iter().enumerate() {
            let start = if k == 0 && !inside(seg.start) { seg.start.min(g[0].start) } else { g[0].start };
            let end = if k + 1 == n && !inside(seg.end - Time(1)) { seg.end.max(g[g.len() - 1].end) } else { g[g.len() - 1].end };
            let text = g.iter().map(|w| w.text.trim()).collect::<Vec<_>>().join(" ");
            out.segments.push(CaptionSegment { start, end: end.max(start), text, words: g });
        }
    }
    out.sort();
    out
}

fn mid(w: &CaptionWord) -> Time {
    Time((w.start.0 + w.end.0) / 2)
}

/// One undo step that ripple-deletes `ranges` (timeline time) from the
/// primary storyline and drops the words spoken there from the captions.
/// `None` when there is nothing to cut.
pub fn cut_command(track: &CaptionTrack, ranges: &[(Time, Time)], label: &'static str) -> Option<Command> {
    let ranges = merge_ranges(ranges.to_vec());
    if ranges.is_empty() {
        return None;
    }
    let mut commands = Vec::with_capacity(ranges.len() + 1);
    if !track.segments.is_empty() {
        commands.push(Command::SetCaptions { captions: Box::new(captions_without(track, &ranges)) });
    }
    // last range first, so the earlier ones keep their times
    commands.extend(ranges.iter().rev().map(|&(start, end)| Command::DeleteRange { start, end }));
    Some(Command::Batch { label, commands })
}

/// Index of the word heard at `t` (or the last one before it, within a pause).
pub fn word_at(words: &[Word], t: Time) -> Option<usize> {
    let i = words.partition_point(|w| w.start <= t);
    let i = i.checked_sub(1)?;
    (t < words[i].end || words.get(i + 1).is_some_and(|n| t < n.start)).then_some(i)
}

/// Paragraph breaks for display: before a word that follows a pause of at
/// least `gap`.
pub fn starts_paragraph(words: &[Word], i: usize, gap: Time) -> bool {
    i > 0 && words[i].start - words[i - 1].end >= gap
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::asset::MediaAsset;
    use crate::clip::Clip;
    use crate::timeline::Timeline;

    fn ms(v: i64) -> Time {
        Time::from_millis(v)
    }

    fn w(text: &str, a: i64, b: i64) -> CaptionWord {
        CaptionWord { start: ms(a), end: ms(b), text: text.into() }
    }

    fn rate() -> FrameRate {
        crate::timeline::VideoFormat::default().rate
    }

    /// One 10 s clip with "so um this is uh great" spoken in two caption blocks.
    fn spoken() -> Timeline {
        let mut tl = Timeline::new("T", Default::default());
        let a = MediaAsset::new("a", "a.mp4", Time::from_secs(10));
        tl.append(Clip::from_asset_range(&a, Time::ZERO, Time::from_secs(10))).unwrap();
        let words = [w("So", 1000, 1300), w("um,", 1400, 1900), w("this", 2000, 2300), w("is", 4000, 4200), w("uh", 4300, 4800), w("great.", 5000, 5600)];
        tl.captions.segments = vec![
            CaptionSegment { start: ms(1000), end: ms(2300), text: "So um, this".into(), words: words[..3].to_vec() },
            CaptionSegment { start: ms(4000), end: ms(5600), text: "is uh great.".into(), words: words[3..].to_vec() },
        ];
        tl
    }

    #[test]
    fn fillers_and_phrases_are_found() {
        let tl = spoken();
        let words = timeline_words(&tl.captions);
        assert_eq!(words.len(), 6);
        assert_eq!(filler_indices(&words), vec![1, 4]);
        assert_eq!(find_phrase(&words, "this IS"), vec![2, 3]);
        assert_eq!(find_phrase(&words, "gre"), vec![5], "prefix of the last word");
        assert!(find_phrase(&words, "  ").is_empty());
        assert_eq!(word_at(&words, ms(1500)), Some(1));
        assert_eq!(word_at(&words, ms(3000)), Some(2), "inside a pause: the word before");
        assert_eq!(word_at(&words, ms(500)), None);
        assert!(starts_paragraph(&words, 3, ms(1000)));
    }

    #[test]
    fn adjacent_words_cut_as_one_range() {
        let tl = spoken();
        let words = timeline_words(&tl.captions);
        let r = word_ranges(&words, &[1, 2, 4], rate());
        assert_eq!(r.len(), 2);
        // 30 fps default: 1400 ms → frame 42 (1400 ms), 2300 ms → frame 69
        assert_eq!(r[0], (ms(1400), ms(2300)));
        assert_eq!(selection_span(&words, &[4, 2]), Some((ms(2000), ms(4800))));
    }

    #[test]
    fn pauses_are_shortened_not_removed() {
        let tl = spoken();
        let words = timeline_words(&tl.captions);
        let r = pause_ranges(&words, ms(1000), ms(200), rate());
        assert_eq!(r.len(), 1, "only the 1.7 s pause between 'this' and 'is'");
        let (s, e) = r[0];
        assert!((s - ms(2400)).0.abs() <= Time::from_millis(17).0 && (e - ms(3900)).0.abs() <= Time::from_millis(17).0, "{s:?}..{e:?}");
    }

    #[test]
    fn deleting_fillers_ripples_and_keeps_captions_on_their_words() {
        let mut tl = spoken();
        let words = timeline_words(&tl.captions);
        let ranges = word_ranges(&words, &filler_indices(&words), rate());
        let removed = total(&ranges);
        let cmd = cut_command(&tl.captions, &ranges, "Delete Fillers").unwrap();
        cmd.apply(&mut tl).unwrap();
        assert_eq!(tl.extent(), Time::from_secs(10) - removed);
        let after = timeline_words(&tl.captions);
        let texts: Vec<&str> = after.iter().map(|w| w.text.as_str()).collect();
        assert_eq!(texts, ["So", "this", "is", "great."]);
        // "this" moved left by the first cut; "great." by both
        let cut1 = ranges[0].1 - ranges[0].0;
        assert_eq!(after[1].start, ms(2000) - cut1);
        assert_eq!(after[3].start, ms(5000) - removed);
        // the block that lost a word in the middle was split at the cut
        assert!(tl.captions.segments.iter().all(|s| !s.text.contains("um") && !s.text.contains("uh")));
        assert!(tl.captions.segments.iter().any(|s| s.text == "So"));
    }

    #[test]
    fn sync_offset_shifts_heard_time() {
        let mut tl = spoken();
        tl.captions.style.sync_offset_ms = 100;
        let words = timeline_words(&tl.captions);
        assert_eq!(words[0].start, ms(1100));
        // cutting the heard "um" removes its caption word too
        let ranges = word_ranges(&words, &[1], rate());
        let cut = captions_without(&tl.captions, &ranges);
        let texts: Vec<String> = timeline_words(&cut).into_iter().map(|w| w.text).collect();
        assert_eq!(texts, ["So", "this", "is", "uh", "great."]);
    }

    #[test]
    fn nothing_to_cut() {
        let tl = spoken();
        assert!(cut_command(&tl.captions, &[], "x").is_none());
        assert!(merge_ranges(vec![(ms(5), ms(5))]).is_empty());
        assert_eq!(merge_ranges(vec![(ms(3), ms(6)), (ms(0), ms(3)), (ms(10), ms(12))]), vec![(ms(0), ms(6)), (ms(10), ms(12))]);
    }
}
