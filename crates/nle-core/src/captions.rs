//! Captions track: word-timed subtitle segments, CapCut-style styling and
//! SubRip (`.srt`) import / export.

use crate::clip::TitleParams;
use crate::text_anim::TextAnimation;
use crate::time::Time;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CaptionWord {
    pub start: Time,
    pub end: Time,
    pub text: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CaptionSegment {
    pub start: Time,
    pub end: Time,
    pub text: String,
    /// Word timings (absolute timeline time). May be empty (e.g. from SRT).
    #[serde(default)]
    pub words: Vec<CaptionWord>,
}

impl CaptionSegment {
    pub fn new(start: Time, end: Time, text: impl Into<String>) -> Self {
        CaptionSegment { start, end, text: text.into(), words: Vec::new() }
    }

    pub fn duration(&self) -> Time {
        self.end - self.start
    }

    /// Hard visibility gate for preview and export: a block shows only while
    /// one of its words is being spoken. That means
    /// `[first_word.start, last_word.end)`, also clipped to the block's own
    /// bounds. It never shows without words, so stale or hand-widened bounds
    /// can't bleed into silence.
    pub fn visible_at(&self, t: Time) -> bool {
        match (self.words.first(), self.words.last()) {
            (Some(first), Some(last)) => t >= self.start.max(first.start) && t < self.end.min(last.end),
            _ => false,
        }
    }

    /// Word timings relative to the segment start, if they match the text.
    pub fn relative_word_times(&self) -> Vec<[f32; 2]> {
        let n = self.text.split_whitespace().count();
        if self.words.len() != n {
            return Vec::new();
        }
        self.words.iter().map(|w| [(w.start - self.start).as_secs_f64() as f32, (w.end - self.start).as_secs_f64() as f32]).collect()
    }

    /// Shortest this block may be trimmed to: [`MIN_CAPTION_MS`], and at
    /// least [`MIN_WORD_MS`] per word.
    pub fn min_duration(&self) -> Time {
        Time::from_millis(MIN_CAPTION_MS.max(MIN_WORD_MS * self.words.len() as i64))
    }

    /// Move the block to `[start, end)` (end kept >= start + [`Self::min_duration`])
    /// and refit its words: [`WordFit::Scale`] maps every word proportionally,
    /// [`WordFit::Hold`] keeps inner words and stretches the first / last word
    /// to the new edges (falling back to scaling when a word would collapse).
    pub fn set_bounds(&mut self, start: Time, end: Time, fit: WordFit) {
        let end = end.max(start + self.min_duration());
        let (os, oe) = (self.start, self.end);
        let scale = |words: &mut [CaptionWord]| {
            let od = (oe - os).ticks().max(1) as f64;
            let nd = (end - start).ticks() as f64;
            let map = |t: Time| start + Time(((t - os).ticks() as f64 * nd / od).round() as i64);
            for w in words.iter_mut() {
                w.start = map(w.start);
                w.end = map(w.end);
            }
        };
        match fit {
            WordFit::Scale => scale(&mut self.words),
            WordFit::Hold => {
                let orig = self.words.clone();
                for w in &mut self.words {
                    w.start = w.start.max(start).min(end);
                    w.end = w.end.max(start).min(end);
                }
                if let Some(f) = self.words.first_mut() {
                    f.start = start;
                }
                if let Some(l) = self.words.last_mut() {
                    l.end = end;
                }
                let min_word = Time::from_millis(MIN_WORD_MS);
                if self.words.iter().any(|w| w.end - w.start < min_word) {
                    self.words = orig;
                    scale(&mut self.words);
                }
            }
        }
        self.start = start;
        self.end = end;
    }

    /// Move the block and all its words by `delta`.
    pub fn shift(&mut self, delta: Time) {
        self.start += delta;
        self.end += delta;
        for w in &mut self.words {
            w.start += delta;
            w.end += delta;
        }
    }

    /// Split at `t` into two blocks, partitioning the words (a word goes to
    /// the side holding its midpoint; each side keeps at least one). If `t`
    /// would leave a word shorter than [`MIN_WORD_MS`], the cut moves to the
    /// nearest word boundary. `None` when `t` is outside the block or it has
    /// fewer than two words.
    pub fn split_at(&self, t: Time) -> Option<(CaptionSegment, CaptionSegment)> {
        let n = self.words.len();
        if n < 2 || t <= self.start || t >= self.end {
            return None;
        }
        let k = self.words.iter().position(|w| w.start + (w.end - w.start).div_i64(2) >= t).unwrap_or(n).clamp(1, n - 1);
        let min_word = Time::from_millis(MIN_WORD_MS);
        let (last_left, first_right) = (&self.words[k - 1], &self.words[k]);
        // a cut that shaves a sliver off a word (or leaves one) snaps to the boundary
        let shaved = (last_left.end - t).max(t - first_right.start).max(Time::ZERO);
        let sliver = shaved.is_positive() && shaved < min_word + min_word;
        let cut = if sliver || t - last_left.start.max(self.start) < min_word || first_right.end.min(self.end) - t < min_word { first_right.start.max(last_left.end) } else { t };
        let side = |words: &[CaptionWord], start: Time, end: Time| {
            let words: Vec<CaptionWord> = words.iter().map(|w| CaptionWord { start: w.start.max(start).min(end), end: w.end.max(start).min(end), text: w.text.clone() }).collect();
            let text = words.iter().map(|w| w.text.as_str()).collect::<Vec<_>>().join(" ");
            CaptionSegment { start, end, text, words }
        };
        Some((side(&self.words[..k], self.start, cut), side(&self.words[k..], cut, self.end)))
    }

    /// Set word `j` to `[start, start + dur)`, clamped between its neighbours
    /// and the block bounds, never shorter than [`MIN_WORD_MS`].
    pub fn set_word_timing(&mut self, j: usize, start: Time, dur: Time) -> bool {
        let n = self.words.len();
        if j >= n {
            return false;
        }
        let min = Time::from_millis(MIN_WORD_MS);
        let lo = if j == 0 { self.start } else { self.words[j - 1].end.max(self.start) };
        let hi = if j + 1 == n { self.end } else { self.words[j + 1].start.min(self.end) };
        if hi - lo < min {
            return false;
        }
        let s = start.max(lo).min(hi - min);
        let e = (s + dur.max(min)).min(hi);
        let w = &mut self.words[j];
        let changed = (w.start, w.end) != (s, e);
        w.start = s;
        w.end = e;
        changed
    }

    /// Re-time the words evenly after the text was edited by hand.
    pub fn retime_words_evenly(&mut self) {
        let words: Vec<&str> = self.text.split_whitespace().collect();
        let n = words.len().max(1) as i64;
        let step = (self.end - self.start).div_i64(n);
        self.words = words
            .iter()
            .enumerate()
            .map(|(i, w)| CaptionWord {
                start: self.start + Time(step.0 * i as i64),
                end: if i as i64 + 1 == n { self.end } else { self.start + Time(step.0 * (i as i64 + 1)) },
                text: (*w).to_string(),
            })
            .collect();
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CaptionStyle {
    pub font_size: f32,
    pub color: [f32; 4],
    /// Colour of the word being spoken.
    pub highlight_color: [f32; 4],
    pub word_highlight: bool,
    pub outline: bool,
    pub outline_color: [f32; 4],
    pub outline_width: f32,
    pub pill: bool,
    pub pill_color: [f32; 4],
    /// Vertical position of the caption centre (0 top .. 1 bottom).
    pub position_y: f32,
    pub uppercase: bool,
    /// Global caption sync offset, ms (+ = captions later). Applied at
    /// evaluation time, so it shifts preview and export without re-transcribing.
    #[serde(default)]
    pub sync_offset_ms: i32,
    /// The pill sits behind the spoken word and glides between words,
    /// instead of backing the whole phrase.
    #[serde(default)]
    pub word_pill: bool,
    /// Font family ("Heavy" = Montserrat Black / Poppins ExtraBold / Segoe UI Black).
    #[serde(default = "default_caption_font")]
    pub font_family: String,
    /// Letter spacing, px at 1080p (negative = tighter).
    #[serde(default = "default_caption_tracking")]
    pub tracking: f32,
    /// Progressive colour pop: words not spoken yet render in `dim_color`.
    #[serde(default = "default_true")]
    pub dim_upcoming: bool,
    #[serde(default = "default_dim_color")]
    pub dim_color: [f32; 4],
}

fn default_true() -> bool {
    true
}
fn default_dim_color() -> [f32; 4] {
    DIM_UPCOMING
}

fn default_caption_font() -> String {
    HEAVY_FONT.into()
}
fn default_caption_tracking() -> f32 {
    -1.0
}

/// Font family keyword for the heaviest available sans (renderer resolves it).
pub const HEAVY_FONT: &str = "Heavy";
/// Range of the caption sync offset slider, ms.
pub const SYNC_OFFSET_MAX_MS: i32 = 300;

/// CapCut yellow.
pub const HIGHLIGHT_YELLOW: [f32; 4] = [1.0, 0.902, 0.0, 1.0];
/// Coral accent (`#FF5A36`).
pub const HIGHLIGHT_CORAL: [f32; 4] = [1.0, 0.353, 0.212, 1.0];
/// Muted white of upcoming, not yet spoken words (`rgba(255,255,255,0.42)`).
pub const DIM_UPCOMING: [f32; 4] = [1.0, 1.0, 1.0, 0.42];
/// CapCut green.
pub const HIGHLIGHT_GREEN: [f32; 4] = [0.0, 1.0, 0.533, 1.0];

impl Default for CaptionStyle {
    fn default() -> Self {
        CaptionStyle {
            font_size: 64.0,
            color: [1.0, 1.0, 1.0, 1.0],
            highlight_color: HIGHLIGHT_YELLOW,
            word_highlight: true,
            outline: true,
            outline_color: [0.0, 0.0, 0.0, 1.0],
            outline_width: 4.5,
            pill: false,
            pill_color: [0.0, 0.0, 0.0, 0.55],
            // above the TikTok / Reels / Shorts button and caption overlays
            position_y: 0.72,
            uppercase: true,
            sync_offset_ms: 0,
            word_pill: false,
            font_family: default_caption_font(),
            tracking: default_caption_tracking(),
            dim_upcoming: true,
            dim_color: DIM_UPCOMING,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CaptionTrack {
    pub enabled: bool,
    pub segments: Vec<CaptionSegment>,
    pub style: CaptionStyle,
}

impl Default for CaptionTrack {
    fn default() -> Self {
        CaptionTrack { enabled: true, segments: Vec::new(), style: CaptionStyle::default() }
    }
}

impl CaptionTrack {
    pub fn is_empty(&self) -> bool {
        self.segments.is_empty()
    }

    pub fn segment_at(&self, t: Time) -> Option<(usize, &CaptionSegment)> {
        self.segments.iter().enumerate().find(|(_, s)| s.visible_at(t))
    }

    /// Free room around block `i`: previous block's end (or 0) and next
    /// block's start (or unbounded).
    fn room(&self, i: usize) -> (Time, Time) {
        let lo = i.checked_sub(1).and_then(|j| self.segments.get(j)).map_or(Time::ZERO, |s| s.end);
        let hi = self.segments.get(i + 1).map_or(Time::MAX, |s| s.start);
        (lo, hi)
    }

    /// Drag one edge of block `i` to `to`, clamped so it never overlaps its
    /// neighbours or gets shorter than its minimum; words refit by `fit`.
    pub fn trim_segment(&mut self, i: usize, start_edge: bool, to: Time, fit: WordFit) -> bool {
        let (lo, hi) = self.room(i);
        let Some(seg) = self.segments.get_mut(i) else { return false };
        let min = seg.min_duration();
        let (s, e) = if start_edge { (to.min(seg.end - min).max(lo), seg.end) } else { (seg.start, to.max(seg.start + min).min(hi)) };
        if e - s < min || (s, e) == (seg.start, seg.end) {
            return false;
        }
        seg.set_bounds(s, e, fit);
        true
    }

    /// Slide block `i` (with its words) by `delta`, stopping at its neighbours
    /// and at zero. Returns the applied delta.
    pub fn slide_segment(&mut self, i: usize, delta: Time) -> Time {
        let (lo, hi) = self.room(i);
        let Some(seg) = self.segments.get_mut(i) else { return Time::ZERO };
        let d = delta.max(lo - seg.start).min(hi - seg.end);
        // never move against the requested direction (e.g. already overlapping)
        let d = if delta.is_negative() { d.min(Time::ZERO) } else { d.max(Time::ZERO) };
        if !d.is_zero() {
            seg.shift(d);
        }
        d
    }

    /// Split block `i` at `t` (see [`CaptionSegment::split_at`]).
    pub fn split_segment(&mut self, i: usize, t: Time) -> bool {
        let Some((a, b)) = self.segments.get(i).and_then(|s| s.split_at(t)) else { return false };
        self.segments[i] = a;
        self.segments.insert(i + 1, b);
        true
    }

    /// Index of the block whose bounds contain `t`.
    pub fn index_at(&self, t: Time) -> Option<usize> {
        self.segments.iter().position(|s| t >= s.start && t < s.end)
    }

    /// The sync offset as time (clamped to the slider range).
    pub fn sync_offset(&self) -> Time {
        Time::from_millis(self.style.sync_offset_ms.clamp(-SYNC_OFFSET_MAX_MS, SYNC_OFFSET_MAX_MS) as i64)
    }

    /// Caption time for timeline time `t`: the sync offset delays (+) or
    /// advances (-) every block and word highlight together.
    pub fn caption_time(&self, t: Time) -> Time {
        t - self.sync_offset()
    }

    pub fn sort(&mut self) {
        self.segments.sort_by_key(|s| s.start);
    }

    /// Title parameters that render segment `seg` in this track's style.
    pub fn title_params(&self, seg: &CaptionSegment) -> TitleParams {
        let s = &self.style;
        let text = if s.uppercase { seg.text.to_uppercase() } else { seg.text.clone() };
        TitleParams {
            text,
            font_family: s.font_family.clone(),
            tracking: s.tracking,
            word_pill: s.pill && s.word_pill && s.word_highlight,
            font_size: s.font_size,
            color: s.color,
            // soft drop shadow under the outline (CapCut look)
            shadow_enabled: true,
            shadow_color: [0.0, 0.0, 0.0, 0.55],
            shadow_offset: [0.0, 3.0],
            shadow_blur: 8.0,
            pill_enabled: s.pill,
            pill_color: s.pill_color,
            pill_padding: 18.0,
            pill_radius: 14.0,
            position: [0.5, s.position_y],
            animation: if s.word_highlight { TextAnimation::WordHighlight } else { TextAnimation::None },
            highlight_color: s.highlight_color,
            outline_enabled: s.outline,
            outline_color: s.outline_color,
            outline_width: s.outline_width.clamp(OUTLINE_MIN_PX, OUTLINE_MAX_PX),
            word_times: seg.relative_word_times(),
            dim_color: (s.word_highlight && s.dim_upcoming).then_some(s.dim_color),
            ..TitleParams::default()
        }
    }

    /// SubRip text of all segments.
    pub fn to_srt(&self) -> String {
        let mut out = String::new();
        for (i, s) in self.segments.iter().enumerate() {
            out.push_str(&format!("{}\n{} --> {}\n{}\n\n", i + 1, srt_time(s.start), srt_time(s.end), s.text.trim()));
        }
        out
    }

    /// CapCut-style phrases: 2–4 words per block (balanced, e.g. 5 → 3 + 2),
    /// a hard break after any pause longer than 250 ms or after punctuation.
    /// Word intervals are made strictly monotonic and non-overlapping. A block
    /// spans exactly its first word's start to its last word's end, and no
    /// block is empty.
    pub fn phrases(words: &[CaptionWord]) -> Vec<CaptionSegment> {
        const MAX_WORDS: usize = 4;
        const MAX_CHARS: usize = 24;
        let pause = Time::from_millis(PHRASE_PAUSE_MS);
        let words = sanitize_words(words);
        // runs between hard breaks
        let mut runs: Vec<Vec<CaptionWord>> = Vec::new();
        for w in words {
            let hard = match runs.last().and_then(|r| r.last()) {
                Some(prev) => w.start - prev.end > pause || prev.text.ends_with([',', '.', '?', '!', ';', ':', '…']),
                None => true,
            };
            match runs.last_mut() {
                Some(run) if !hard => run.push(w),
                _ => runs.push(vec![w]),
            }
        }
        let mut groups: Vec<Vec<CaptionWord>> = Vec::new();
        for run in runs {
            // fewest chunks that respect the word and character limits,
            // sized as evenly as possible
            let chars: usize = run.iter().map(|w| w.text.chars().count() + 1).sum();
            let mut k = run.len().div_ceil(MAX_WORDS).max(chars.div_ceil(MAX_CHARS + 1)).min(run.len()).max(1);
            // keep blocks at ≥ 2 words where the run allows it
            while k > 1 && run.len() / k < 2 && run.len().div_ceil(k - 1) <= MAX_WORDS {
                k -= 1;
            }
            let (base, extra) = (run.len() / k, run.len() % k);
            let mut it = run.into_iter();
            for c in 0..k {
                groups.push(it.by_ref().take(base + usize::from(c < extra)).collect());
            }
        }
        groups
            .into_iter()
            .filter_map(|g| {
                let (start, end) = (g.first()?.start, g.last()?.end);
                let text = g.iter().map(|w| w.text.as_str()).collect::<Vec<_>>().join(" ");
                Some(CaptionSegment { start, end, text, words: g })
            })
            .collect()
    }

    /// Group recognised words into readable caption segments: a new segment
    /// starts after a pause longer than `max_gap`, at sentence ends, or when
    /// the line would exceed `max_chars`.
    pub fn from_words(words: &[CaptionWord], max_chars: usize, max_gap: Time) -> Vec<CaptionSegment> {
        let mut segs: Vec<CaptionSegment> = Vec::new();
        let mut cur: Vec<CaptionWord> = Vec::new();
        let flush = |cur: &mut Vec<CaptionWord>, segs: &mut Vec<CaptionSegment>| {
            if cur.is_empty() {
                return;
            }
            let text = cur.iter().map(|w| w.text.trim()).collect::<Vec<_>>().join(" ");
            segs.push(CaptionSegment { start: cur[0].start, end: cur[cur.len() - 1].end, text, words: std::mem::take(cur) });
        };
        for w in words {
            let t = w.text.trim();
            if t.is_empty() {
                continue;
            }
            if let Some(last) = cur.last() {
                let len: usize = cur.iter().map(|c| c.text.trim().len() + 1).sum::<usize>() + t.len();
                let gap = w.start - last.end;
                let sentence_end = last.text.trim_end().ends_with(['.', '?', '!']);
                if gap > max_gap || len > max_chars || sentence_end {
                    flush(&mut cur, &mut segs);
                }
            }
            cur.push(CaptionWord { start: w.start, end: w.end.max(w.start), text: t.to_string() });
        }
        flush(&mut cur, &mut segs);
        segs
    }
}

/// How a caption block's words follow when its bounds are dragged.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum WordFit {
    /// Every word keeps its relative position and length.
    #[default]
    Scale,
    /// Inner words keep their times; the first / last word stretches.
    Hold,
}

/// Shortest caption block a trim can leave, ms.
pub const MIN_CAPTION_MS: i64 = 100;

/// Caption outline range (px at 1080p).
pub const OUTLINE_MIN_PX: f32 = 3.0;
pub const OUTLINE_MAX_PX: f32 = 5.0;
/// Any pause longer than this (ms) starts a new caption block.
pub const PHRASE_PAUSE_MS: i64 = 250;
/// Shortest caption word (ms).
pub const MIN_WORD_MS: i64 = 50;

/// Trimmed, non-empty words with strictly increasing, non-overlapping
/// intervals (`start < end <= next.start`).
pub fn sanitize_words(words: &[CaptionWord]) -> Vec<CaptionWord> {
    let mut out: Vec<CaptionWord> =
        words.iter().filter(|w| !w.text.trim().is_empty()).map(|w| CaptionWord { start: w.start, end: w.end.max(w.start), text: w.text.trim().to_string() }).collect();
    out.sort_by_key(|w| w.start);
    let min = Time::from_millis(MIN_WORD_MS);
    for i in 0..out.len() {
        if i > 0 && out[i].start < out[i - 1].end {
            out[i].start = out[i - 1].end;
        }
        if out[i].end < out[i].start + min {
            out[i].end = out[i].start + min;
        }
    }
    out
}

fn srt_time(t: Time) -> String {
    let ms = (t.as_secs_f64() * 1000.0).round().max(0.0) as u64;
    format!("{:02}:{:02}:{:02},{:03}", ms / 3_600_000, (ms / 60_000) % 60, (ms / 1000) % 60, ms % 1000)
}

fn parse_srt_time(s: &str) -> Option<Time> {
    let s = s.trim().replace('.', ",");
    let (hms, ms) = s.split_once(',').unwrap_or((s.as_str(), "0"));
    let mut it = hms.split(':');
    let h: u64 = it.next()?.trim().parse().ok()?;
    let m: u64 = it.next()?.trim().parse().ok()?;
    let sec: u64 = it.next()?.trim().parse().ok()?;
    let ms: u64 = ms.trim().parse().ok()?;
    Some(Time::from_millis((((h * 60 + m) * 60 + sec) * 1000 + ms) as i64))
}

#[derive(Debug, thiserror::Error, PartialEq)]
pub enum SrtError {
    #[error("line {0}: bad timing line")]
    BadTiming(usize),
}

/// Parse SubRip text. Tolerates BOMs, CRLF, missing indices and `.` decimals.
pub fn parse_srt(s: &str) -> Result<Vec<CaptionSegment>, SrtError> {
    let s = s.trim_start_matches('\u{feff}').replace("\r\n", "\n");
    let mut out = Vec::new();
    let lines: Vec<&str> = s.lines().collect();
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i].trim();
        if line.is_empty() {
            i += 1;
            continue;
        }
        // optional numeric index
        let timing_line = if line.contains("-->") { i } else { i + 1 };
        let Some(timing) = lines.get(timing_line) else { break };
        let (a, b) = timing.split_once("-->").ok_or(SrtError::BadTiming(timing_line + 1))?;
        // strip position hints after the end time ("00:00:01,000 X1:...")
        let b = b.split_whitespace().next().unwrap_or("");
        let start = parse_srt_time(a).ok_or(SrtError::BadTiming(timing_line + 1))?;
        let end = parse_srt_time(b).ok_or(SrtError::BadTiming(timing_line + 1))?;
        let mut text = Vec::new();
        i = timing_line + 1;
        while i < lines.len() && !lines[i].trim().is_empty() {
            text.push(lines[i].trim());
            i += 1;
        }
        out.push(CaptionSegment::new(start, end.max(start), text.join(" ")));
    }
    Ok(out)
}

/// Minimum captioned speech inside a clip before it is treated as the voice track.
const SPEECH_MIN: Time = Time::from_millis(300);

/// Audible `General`-role clips that carry captioned speech — they become
/// `Dialogue` (the ducking sidechain) after auto-captioning. Music / SFX
/// clips are never re-roled, so sung lyrics don't turn the music into a voice.
pub fn speech_clips(tl: &crate::timeline::Timeline, track: &CaptionTrack) -> Vec<crate::ids::ClipId> {
    use crate::clip::AudioRole;
    let spans: Vec<(Time, Time)> = track
        .segments
        .iter()
        .flat_map(|s| if s.words.is_empty() { vec![(s.start, s.end)] } else { s.words.iter().map(|w| (w.start, w.end)).collect() })
        .filter(|(a, b)| b > a)
        .collect();
    tl.placed_clips()
        .into_iter()
        .filter(|p| {
            let c = p.clip;
            c.enabled && c.av.audio && !c.audio.muted && c.audio.role == AudioRole::General && !c.is_gap()
        })
        .filter(|p| {
            let (s, e) = (p.start, p.end());
            let covered = spans.iter().map(|(a, b)| ((*b).min(e) - (*a).max(s)).max(Time::ZERO)).fold(Time::ZERO, |acc, d| acc + d);
            covered >= SPEECH_MIN
        })
        .map(|p| p.clip.id)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn w(s: f64, e: f64, t: &str) -> CaptionWord {
        CaptionWord { start: Time::from_secs_f64(s), end: Time::from_secs_f64(e), text: t.into() }
    }

    #[test]
    fn srt_round_trip() {
        let mut tr = CaptionTrack::default();
        tr.segments.push(CaptionSegment::new(Time::from_millis(1_500), Time::from_millis(3_250), "Hello there"));
        tr.segments.push(CaptionSegment::new(Time::from_millis(3_600_000 + 61_007), Time::from_millis(3_600_000 + 62_000), "Later"));
        let srt = tr.to_srt();
        assert!(srt.contains("00:00:01,500 --> 00:00:03,250"));
        assert!(srt.contains("01:01:01,007 --> 01:01:02,000"));
        let back = parse_srt(&srt).unwrap();
        assert_eq!(back, tr.segments);
    }

    #[test]
    fn srt_parser_is_tolerant() {
        let s = "\u{feff}1\r\n00:00:00.500 --> 00:00:01.000 X1:0\r\nline one\r\nline two\r\n\r\n00:00:02,000 --> 00:00:03,000\r\nno index\r\n";
        let segs = parse_srt(s).unwrap();
        assert_eq!(segs.len(), 2);
        assert_eq!(segs[0].text, "line one line two");
        assert_eq!(segs[0].start, Time::from_millis(500));
        assert_eq!(segs[1].text, "no index");
        assert!(parse_srt("1\nnot a timing\ntext\n").is_err());
    }

    #[test]
    fn words_group_into_segments() {
        let words = [w(0.0, 0.3, "Hello"), w(0.3, 0.6, "world."), w(0.7, 1.0, "This"), w(1.0, 1.2, "is"), w(3.0, 3.4, "later")];
        let segs = CaptionTrack::from_words(&words, 40, Time::from_millis(800));
        let texts: Vec<_> = segs.iter().map(|s| s.text.as_str()).collect();
        assert_eq!(texts, ["Hello world.", "This is", "later"]);
        assert_eq!(segs[1].start, Time::from_secs_f64(0.7));
        assert_eq!(segs[1].end, Time::from_secs_f64(1.2));
        assert_eq!(segs[1].relative_word_times().len(), 2);
        // character limit
        let many: Vec<_> = (0..20).map(|i| w(i as f64 * 0.2, i as f64 * 0.2 + 0.2, "word")).collect();
        assert!(CaptionTrack::from_words(&many, 20, Time::from_secs(1)).iter().all(|s| s.text.len() <= 20));
    }

    #[test]
    fn title_params_carry_word_times_and_style() {
        let mut tr = CaptionTrack::default();
        let words = [w(1.0, 1.5, "big"), w(1.5, 2.0, "news")];
        tr.segments = CaptionTrack::from_words(&words, 40, Time::from_secs(1));
        tr.style.uppercase = true;
        let p = tr.title_params(&tr.segments[0]);
        assert_eq!(p.text, "BIG NEWS");
        assert_eq!(p.word_times, vec![[0.0, 0.5], [0.5, 1.0]]);
        assert_eq!(p.animation, TextAnimation::WordHighlight);
        assert_eq!(tr.segment_at(Time::from_secs_f64(1.7)).map(|(i, _)| i), Some(0));
        assert!(tr.segment_at(Time::from_secs(3)).is_none());
    }

    #[test]
    fn retime_evenly_after_edit() {
        let mut s = CaptionSegment::new(Time::ZERO, Time::from_secs(3), "a b c");
        s.retime_words_evenly();
        assert_eq!(s.words.len(), 3);
        assert_eq!(s.words[1].start, Time::from_secs(1));
        assert_eq!(s.relative_word_times()[2], [2.0, 3.0]);
    }

    fn texts(segs: &[CaptionSegment]) -> Vec<&str> {
        segs.iter().map(|s| s.text.as_str()).collect()
    }

    #[test]
    fn phrases_group_two_to_four_words_and_break_on_pauses_and_punctuation() {
        let words = vec![
            w(0.00, 0.20, "one"),
            w(0.22, 0.40, "two"),
            w(0.42, 0.60, "three"),
            w(0.62, 0.80, "four"),
            w(0.82, 1.00, "five"),
            // 0.5 s pause → hard break
            w(1.50, 1.70, "after"),
            w(1.72, 1.90, "pause,"),
            // punctuation → hard break
            w(1.92, 2.10, "next"),
            w(2.12, 2.30, "bit"),
        ];
        let segs = CaptionTrack::phrases(&words);
        assert_eq!(texts(&segs), ["one two three", "four five", "after pause,", "next bit"]);
        for s in &segs {
            assert!((1..=4).contains(&s.words.len()));
        }
        // a long unbroken run is split into balanced 2–4 word blocks
        let run: Vec<CaptionWord> = (0..10).map(|i| w(i as f64 * 0.2, i as f64 * 0.2 + 0.18, "go")).collect();
        let sizes: Vec<usize> = CaptionTrack::phrases(&run).iter().map(|s| s.words.len()).collect();
        assert_eq!(sizes, [4, 3, 3]);
    }

    #[test]
    fn phrase_blocks_render_only_while_their_words_are_spoken() {
        let words = vec![w(1.0, 1.3, "hello"), w(1.35, 1.6, "there."), w(4.0, 4.4, "gg"), w(4.45, 4.9, "ez")];
        let tr = CaptionTrack { segments: CaptionTrack::phrases(&words), ..Default::default() };
        for s in &tr.segments {
            assert_eq!(s.start, s.words[0].start, "block starts on its first word");
            assert_eq!(s.end, s.words[s.words.len() - 1].end, "block ends on its last word");
        }
        assert!(tr.segment_at(Time::from_secs_f64(0.99)).is_none(), "nothing before first word");
        assert!(tr.segment_at(Time::from_secs_f64(1.0)).is_some());
        assert!(tr.segment_at(Time::from_secs_f64(1.59)).is_some());
        assert!(tr.segment_at(Time::from_secs_f64(1.6)).is_none(), "gone the moment the last word ends");
        assert!(tr.segment_at(Time::from_secs_f64(3.0)).is_none());
        assert!(tr.segment_at(Time::from_secs_f64(5.0)).is_none());
    }

    #[test]
    fn blocks_split_on_any_pause_over_250ms() {
        // 240 ms gap stays together; 260 ms gap splits and hides during the pause
        let words = vec![w(0.0, 0.2, "keep"), w(0.44, 0.6, "going"), w(0.86, 1.0, "then"), w(1.02, 1.2, "push")];
        let segs = CaptionTrack::phrases(&words);
        assert_eq!(texts(&segs), ["keep going", "then push"]);
        let tr = CaptionTrack { segments: segs, ..Default::default() };
        assert!(tr.segment_at(Time::from_secs_f64(0.7)).is_none(), "hidden during the 260 ms pause");
        assert!(tr.segment_at(Time::from_secs_f64(0.3)).is_some(), "short 240 ms gap stays on");
    }

    #[test]
    fn render_gate_hides_wordless_and_widened_blocks() {
        let mut tr = CaptionTrack::default();
        // stale / hand-widened bounds: block says 0–5 s, words say 2.0–2.5 s
        let mut seg = CaptionSegment::new(Time::ZERO, Time::from_secs(5), "late words");
        seg.words = vec![w(2.0, 2.2, "late"), w(2.25, 2.5, "words")];
        tr.segments.push(seg);
        // a block without word timings never renders
        tr.segments.push(CaptionSegment::new(Time::from_secs(6), Time::from_secs(8), "ghost"));
        assert!(tr.segment_at(Time::from_secs_f64(1.0)).is_none(), "no text during the leading silence");
        assert!(tr.segment_at(Time::from_secs_f64(2.1)).is_some());
        assert!(tr.segment_at(Time::from_secs_f64(3.0)).is_none(), "no text during the trailing silence");
        assert!(tr.segment_at(Time::from_secs_f64(7.0)).is_none(), "wordless block hidden");
        assert!(CaptionTrack::phrases(&[]).is_empty());
    }

    #[test]
    fn phrase_words_are_strictly_monotonic_and_non_overlapping() {
        // overlapping, out-of-order and zero-length stamps as Whisper can emit
        let words = vec![w(0.5, 0.9, "b"), w(0.0, 0.6, "a"), w(0.9, 0.9, "c"), w(0.95, 0.92, "d"), w(1.0, 1.2, "  ")];
        let segs = CaptionTrack::phrases(&words);
        let all: Vec<&CaptionWord> = segs.iter().flat_map(|s| &s.words).collect();
        assert_eq!(all.iter().map(|w| w.text.as_str()).collect::<Vec<_>>(), ["a", "b", "c", "d"]);
        for (i, w) in all.iter().enumerate() {
            assert!(w.start < w.end, "{w:?}");
            if i > 0 {
                assert!(w.start >= all[i - 1].end, "overlap at {i}");
            }
        }
        for pair in segs.windows(2) {
            assert!(pair[0].end <= pair[1].start, "blocks overlap");
        }
    }

    fn block(words: &[(f64, f64, &str)]) -> CaptionSegment {
        let words: Vec<CaptionWord> = words.iter().map(|&(s, e, t)| w(s, e, t)).collect();
        let text = words.iter().map(|w| w.text.as_str()).collect::<Vec<_>>().join(" ");
        CaptionSegment { start: words[0].start, end: words.last().unwrap().end, text, words }
    }

    fn ms(t: Time) -> i64 {
        (t.as_secs_f64() * 1000.0).round() as i64
    }

    fn assert_words_ok(s: &CaptionSegment) {
        for x in &s.words {
            assert!(x.start >= s.start && x.end <= s.end && x.start < x.end, "{s:?}");
        }
        for p in s.words.windows(2) {
            assert!(p[0].end <= p[1].start, "words overlap {s:?}");
        }
    }

    #[test]
    fn trim_scales_words_proportionally_or_holds_inner_words() {
        let b = block(&[(1.0, 1.4, "a"), (1.4, 1.6, "b"), (1.6, 2.0, "c")]);
        // stretch the right edge 2.0 -> 3.0: everything scales by 2x from 1.0
        let mut s = b.clone();
        s.set_bounds(s.start, Time::from_secs(3), WordFit::Scale);
        assert_eq!(s.words.iter().map(|x| (ms(x.start), ms(x.end))).collect::<Vec<_>>(), [(1000, 1800), (1800, 2200), (2200, 3000)]);
        assert_words_ok(&s);
        // hold: inner word untouched, last word extended
        let mut s = b.clone();
        s.set_bounds(s.start, Time::from_secs(3), WordFit::Hold);
        assert_eq!(s.words.iter().map(|x| (ms(x.start), ms(x.end))).collect::<Vec<_>>(), [(1000, 1400), (1400, 1600), (1600, 3000)]);
        // hold, left edge earlier: first word starts with the block
        let mut s = b.clone();
        s.set_bounds(Time::from_millis(800), s.end, WordFit::Hold);
        assert_eq!((ms(s.words[0].start), ms(s.words[0].end), ms(s.words[1].start)), (800, 1400, 1400));
        // hold that would crush a word falls back to scaling
        let mut s = b.clone();
        s.set_bounds(s.start, Time::from_millis(1450), WordFit::Hold);
        assert_words_ok(&s);
        assert!(s.words.iter().all(|x| x.end - x.start >= Time::from_millis(MIN_WORD_MS) - Time::from_millis(1)));
        // never shorter than the minimum
        let mut s = b.clone();
        s.set_bounds(s.start, s.start, WordFit::Scale);
        assert_eq!(s.duration(), s.min_duration());
    }

    #[test]
    fn track_trim_and_slide_stop_at_neighbours() {
        let mut tr = CaptionTrack { segments: vec![block(&[(1.0, 1.5, "a"), (1.5, 2.0, "b")]), block(&[(3.0, 3.5, "c"), (3.5, 4.0, "d")])], ..Default::default() };
        // right edge of block 0 can't pass block 1's start
        assert!(tr.trim_segment(0, false, Time::from_secs(5), WordFit::Scale));
        assert_eq!(tr.segments[0].end, Time::from_secs(3));
        // left edge of block 1 can't pass block 0's end, nor its own end - min
        assert!(!tr.trim_segment(1, true, Time::from_secs(2), WordFit::Scale));
        assert!(tr.trim_segment(1, true, Time::from_secs(9), WordFit::Scale));
        assert_eq!(tr.segments[1].start, Time::from_secs(4) - tr.segments[1].min_duration());
        assert_words_ok(&tr.segments[1]);
        // slide moves words along and stops at zero / neighbours
        let before = tr.segments[0].clone();
        let d = tr.slide_segment(0, -Time::from_secs(5));
        assert_eq!(d, -Time::from_secs(1));
        assert_eq!(tr.segments[0].start, Time::ZERO);
        assert_eq!(tr.segments[0].words[1].start - tr.segments[0].start, before.words[1].start - before.start);
        let d = tr.slide_segment(0, Time::from_secs(9));
        assert_eq!(tr.segments[0].end, tr.segments[1].start, "stops at the next block, slid {d:?}");
        // a frame nudge
        let f = crate::time::FrameRate::FPS_30.frame_duration();
        let mut tr2 = CaptionTrack { segments: vec![block(&[(1.0, 1.5, "a")])], ..Default::default() };
        assert_eq!(tr2.slide_segment(0, f), f);
        assert_eq!(tr2.segments[0].words[0].start, Time::from_secs(1) + f);
        assert_eq!(tr2.slide_segment(0, -f), -f);
        assert_eq!(tr2.segments[0].start, Time::from_secs(1));
    }

    #[test]
    fn split_partitions_words_at_the_playhead() {
        let mut tr = CaptionTrack { segments: vec![block(&[(1.0, 1.4, "one"), (1.4, 1.8, "two"), (1.8, 2.4, "three")])], ..Default::default() };
        // 1.65 s: inside "two" past its midpoint -> "one two" | "three"
        assert!(tr.split_segment(0, Time::from_millis(1650)));
        assert_eq!(texts(&tr.segments), ["one two", "three"]);
        let (a, b) = (&tr.segments[0], &tr.segments[1]);
        assert_eq!(a.end, b.start);
        assert_eq!(ms(a.end), 1650);
        assert_words_ok(a);
        assert_words_ok(b);
        assert_eq!((ms(a.words[1].end), ms(b.words[0].start)), (1650, 1800), "word clipped to the cut; next word keeps its time");
        // a cut that would leave a sliver snaps to the word boundary
        let mut tr = CaptionTrack { segments: vec![block(&[(1.0, 1.4, "one"), (1.4, 1.8, "two")])], ..Default::default() };
        assert!(tr.split_segment(0, Time::from_millis(1420)));
        assert_eq!(ms(tr.segments[0].end), 1400);
        assert_eq!(texts(&tr.segments), ["one", "two"]);
        // outside the block or a single word: no split
        assert!(!tr.split_segment(0, Time::from_secs(5)));
        assert!(!tr.split_segment(0, Time::from_millis(1200)));
        assert_eq!(tr.index_at(Time::from_millis(1500)), Some(1));
        assert_eq!(tr.index_at(Time::from_secs(3)), None);
    }

    #[test]
    fn word_timing_edits_stay_between_neighbours() {
        let mut s = block(&[(1.0, 1.4, "a"), (1.4, 1.6, "b"), (1.6, 2.0, "c")]);
        // move "b" earlier than "a" ends: clamped to 1.4
        assert!(!s.set_word_timing(1, Time::from_millis(1200), Time::from_millis(200)));
        // shorten "b" then push it right: its end stops at "c"
        assert!(s.set_word_timing(1, Time::from_millis(1450), Time::from_millis(100)));
        assert_eq!((ms(s.words[1].start), ms(s.words[1].end)), (1450, 1550));
        assert!(s.set_word_timing(1, Time::from_millis(1500), Time::from_secs(5)));
        assert_eq!((ms(s.words[1].start), ms(s.words[1].end)), (1500, 1600));
        // tiny durations are raised to the minimum
        assert!(s.set_word_timing(2, Time::from_millis(1600), Time::ZERO));
        assert_eq!(ms(s.words[2].end - s.words[2].start), MIN_WORD_MS);
        assert_words_ok(&s);
        assert!(!s.set_word_timing(9, Time::ZERO, Time::ZERO));
    }

    #[test]
    fn sync_offset_shifts_caption_time_and_is_clamped() {
        let mut tr = CaptionTrack { segments: CaptionTrack::phrases(&[w(1.0, 1.4, "hi"), w(1.4, 2.0, "there")]), ..Default::default() };
        assert_eq!(tr.caption_time(Time::from_millis(1100)), Time::from_millis(1100));
        tr.style.sync_offset_ms = 200;
        // captions now start 200 ms later
        assert!(tr.segment_at(tr.caption_time(Time::from_millis(1100))).is_none());
        assert!(tr.segment_at(tr.caption_time(Time::from_millis(1250))).is_some());
        assert!(tr.segment_at(tr.caption_time(Time::from_millis(2150))).is_some());
        tr.style.sync_offset_ms = -150;
        assert!(tr.segment_at(tr.caption_time(Time::from_millis(900))).is_some());
        tr.style.sync_offset_ms = 5_000;
        assert_eq!(tr.sync_offset(), Time::from_millis(SYNC_OFFSET_MAX_MS as i64));
        // old projects without the new fields still load
        let mut v = serde_json::to_value(CaptionStyle::default()).unwrap();
        for k in ["sync_offset_ms", "word_pill", "font_family", "tracking"] {
            v.as_object_mut().unwrap().remove(k);
        }
        let s: CaptionStyle = serde_json::from_value(v).unwrap();
        assert_eq!((s.sync_offset_ms, s.word_pill, s.font_family.as_str()), (0, false, HEAVY_FONT));
    }

    #[test]
    fn caption_style_is_white_outline_with_soft_shadow_and_yellow_highlight() {
        let mut tr = CaptionTrack::default();
        tr.style.outline_width = 9.0;
        tr.segments = CaptionTrack::phrases(&[w(0.0, 0.3, "nice"), w(0.3, 0.6, "shot")]);
        let p = tr.title_params(&tr.segments[0]);
        assert_eq!(p.color, [1.0, 1.0, 1.0, 1.0]);
        assert!(p.outline_enabled && p.outline_width == OUTLINE_MAX_PX && (4.0..=5.0).contains(&p.outline_width));
        // viral defaults: heavy font, tight tracking, ALL CAPS, safe-zone Y
        assert_eq!(p.font_family, HEAVY_FONT);
        assert!(p.tracking < 0.0);
        assert_eq!(p.text, "NICE SHOT");
        assert!((p.position[1] - 0.72).abs() < 1e-6);
        assert!((4.0..=5.0).contains(&CaptionStyle::default().outline_width));
        assert!(p.shadow_enabled && p.shadow_blur > 0.0 && p.shadow_color[3] < 1.0);
        assert_eq!(p.highlight_color, HIGHLIGHT_YELLOW);
    }

    #[test]
    fn captioned_general_clips_become_dialogue_candidates() {
        use crate::clip::{AudioRole, Clip};
        use crate::timeline::Timeline;
        let mut tl = Timeline::default();
        let mut game = Clip::adjustment(Time::from_secs(4));
        game.av.audio = true;
        game.audio.role = AudioRole::General;
        let mut song = Clip::adjustment(Time::from_secs(4));
        song.av.audio = true;
        song.audio.role = AudioRole::Music;
        let (g, m) = (game.id, song.id);
        tl.spine.push(game);
        tl.spine.push(song);
        let w = |a: i64, b: i64| CaptionWord { start: Time::from_millis(a), end: Time::from_millis(b), text: "hey".into() };
        // speech over both clips; only the General one is re-roled
        let track = CaptionTrack {
            segments: vec![CaptionSegment { start: Time::from_millis(500), end: Time::from_millis(6000), text: "hey hey".into(), words: vec![w(500, 1200), w(4500, 6000)] }],
            ..Default::default()
        };
        assert_eq!(speech_clips(&tl, &track), vec![g]);
        assert!(!speech_clips(&tl, &CaptionTrack::default()).contains(&m));
    }
}
