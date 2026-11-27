//! Offline speech-to-text for auto-captions: whisper.cpp (via `whisper-rs`)
//! on the GPU through Vulkan (Intel Arc / NVIDIA / AMD) when built with the
//! `whisper-vulkan` feature, otherwise — or if the GPU run fails — on the CPU
//! with every worker thread. Input is 16 kHz mono `f32`
//! ([`crate::audio::to_mono_16k`]); output is word-level timestamps.
//!
//! Robustness rules (each one closes a whisper.cpp abort path):
//! - PCM is sanitised ([`prepare_pcm`]): NaN/Inf → 0, clamped to ±1, silent
//!   input skipped, clips under 1 s zero-padded.
//! - Long audio is split into 30 s windows with 0.5 s overlap
//!   ([`chunk_windows`]); each window gets `no_context` so a bad segment
//!   cannot poison the next, and overlapping words are de-duplicated at the
//!   overlap midpoint ([`merge_chunk_words`]).
//! - The abort callback is a plain `extern "C"` fn over an `AtomicBool`.
//!   `whisper-rs` 0.16's `set_abort_callback_safe` casts its user data to the
//!   wrong type, so whisper.cpp read heap garbage as "abort requested" and
//!   bailed out of the encoder (-6) or decoder (-9) at random.
//! - The model file header is checked before whisper.cpp loads it.
//!
//! The pre/post-processing is plain Rust and always compiled, so it is
//! unit-tested without a model.

use nle_core::{CaptionWord, Time};
use std::ops::Range;
use std::path::Path;

/// Whisper's expected sample rate.
pub const SAMPLE_RATE: u32 = 16_000;
/// Shortest buffer handed to Whisper (1 s); shorter clips are zero-padded.
pub const MIN_SAMPLES: usize = SAMPLE_RATE as usize;
/// Whisper's native window (30 s).
pub const CHUNK_SAMPLES: usize = 30 * SAMPLE_RATE as usize;
/// Overlap between consecutive windows (0.5 s).
pub const CHUNK_OVERLAP: usize = SAMPLE_RATE as usize / 2;
/// RMS below this is treated as digital silence.
pub const SILENCE_RMS: f32 = 1e-6;
/// whisper.cpp's `GGML_FILE_MAGIC` (0x67676d6c) as stored little-endian.
pub const GGML_MAGIC: [u8; 4] = *b"lmgg";

#[derive(Clone, Debug, PartialEq)]
pub struct Transcript {
    pub words: Vec<CaptionWord>,
    /// Detected (or forced) language code, e.g. "en".
    pub language: String,
    /// Where inference ran: "GPU: Vulkan (Intel(R) Arc(TM) A750 Graphics)",
    /// "CPU (16 threads)" or "CPU Fallback (16 threads)".
    pub backend: String,
    /// Why the GPU was not used when it was requested.
    pub fallback_reason: Option<String>,
}

impl Transcript {
    fn empty(backend: &str) -> Self {
        Transcript { words: Vec::new(), language: "?".into(), backend: backend.into(), fallback_reason: None }
    }
}

#[derive(Clone, Debug)]
pub struct TranscribeOptions {
    pub model: std::path::PathBuf,
    /// `None` = auto-detect.
    pub language: Option<String>,
    pub use_gpu: bool,
    pub threads: usize,
    /// Offset added to every timestamp (where the audio starts on the timeline).
    pub offset: Time,
}

impl TranscribeOptions {
    pub fn new(model: std::path::PathBuf) -> Self {
        TranscribeOptions { model, language: None, use_gpu: true, threads: cpu_threads(), offset: Time::ZERO }
    }
}

/// Every hardware thread, at least one (`n_threads = 0` makes whisper.cpp fail).
pub fn cpu_threads() -> usize {
    std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4).max(1)
}

/// Whether this build can transcribe at all.
pub const AVAILABLE: bool = cfg!(feature = "whisper");
/// Whether this build runs Whisper on the GPU (Vulkan).
pub const GPU_BUILD: bool = cfg!(feature = "whisper-vulkan");

// ---------------------------------------------------------------- input PCM

/// Root-mean-square level of `samples` (0 for an empty slice).
pub fn rms(samples: &[f32]) -> f32 {
    if samples.is_empty() {
        return 0.0;
    }
    let sum: f64 = samples.iter().map(|&s| f64::from(s) * f64::from(s)).sum();
    (sum / samples.len() as f64).sqrt() as f32
}

/// Make 16 kHz mono audio safe for whisper.cpp: NaN/Inf become 0, samples
/// are clamped to `[-1, 1]` and clips shorter than 1 s are right-padded with
/// silence (shorter mel inputs abort the encoder). `None` means empty or
/// silent input — nothing to transcribe.
pub fn prepare_pcm(samples_16k_mono: &[f32]) -> Option<Vec<f32>> {
    let mut pcm: Vec<f32> = samples_16k_mono.iter().map(|&s| if s.is_finite() { s.clamp(-1.0, 1.0) } else { 0.0 }).collect();
    if pcm.is_empty() || rms(&pcm) < SILENCE_RMS {
        return None;
    }
    if pcm.len() < MIN_SAMPLES {
        pcm.resize(MIN_SAMPLES, 0.0);
    }
    Some(pcm)
}

/// Sample ranges of the 30 s windows (0.5 s overlap) covering `len` samples.
pub fn chunk_windows(len: usize) -> Vec<Range<usize>> {
    let mut out = Vec::new();
    if len == 0 {
        return out;
    }
    let mut start = 0;
    loop {
        let end = (start + CHUNK_SAMPLES).min(len);
        out.push(start..end);
        if end == len {
            return out;
        }
        start = end - CHUNK_OVERLAP;
    }
}

/// Window `i`'s ownership range (absolute samples): words starting inside it
/// are kept from this window. Neighbours split their overlap at its midpoint,
/// and the padded tail past the real audio is never owned.
fn owned_range(windows: &[Range<usize>], i: usize) -> Range<usize> {
    let lo = if i == 0 { 0 } else { (windows[i - 1].end + windows[i].start) / 2 };
    let hi = windows.get(i + 1).map_or(windows[i].end, |next| (windows[i].end + next.start) / 2);
    lo..hi
}

/// Merge per-window raw segments (centiseconds relative to their window) into
/// one word list on the project timeline: each window's start is added back
/// as exact ticks, boundary duplicates from the overlap are dropped.
pub fn merge_chunk_words(windows: &[Range<usize>], chunks: &[Vec<(i64, i64, String)>], offset: Time) -> Vec<CaptionWord> {
    let mut out = Vec::new();
    for (i, (window, raw)) in windows.iter().zip(chunks).enumerate() {
        let own = owned_range(windows, i);
        let (lo, hi) = (Time::from_samples(own.start as i64, SAMPLE_RATE), Time::from_samples(own.end as i64, SAMPLE_RATE));
        let base = offset + Time::from_samples(window.start as i64, SAMPLE_RATE);
        out.extend(words_from_segments(raw, base).into_iter().filter(|w| {
            let rel = w.start - offset;
            rel >= lo && rel < hi
        }));
    }
    repair_words(&mut out);
    out
}

// ------------------------------------------------------ energy word snapping

/// Envelope hop: 10 ms at 16 kHz.
const ENV_HOP: usize = 160;
/// Boundary snap search radius (±150 ms) in envelope frames.
const SNAP_FRAMES: usize = 15;

/// Voice-band (≈ 250–3500 Hz) RMS envelope of 16 kHz mono audio in dBFS,
/// one value per 10 ms frame (20 ms window centred on the frame).
pub fn energy_envelope_db(pcm: &[f32]) -> Vec<f32> {
    let sr = SAMPLE_RATE as f32;
    let a_hp = (-std::f32::consts::TAU * 250.0 / sr).exp();
    let a_lp = 1.0 - (-std::f32::consts::TAU * 3_500.0 / sr).exp();
    let (mut hx, mut hy, mut lp) = (0.0f32, 0.0f32, 0.0f32);
    let band: Vec<f32> = pcm
        .iter()
        .map(|&x| {
            hy = a_hp * (hy + x - hx);
            hx = x;
            lp += a_lp * (hy - lp);
            lp
        })
        .collect();
    let frames = band.len().div_ceil(ENV_HOP);
    (0..frames)
        .map(|i| {
            let c = i * ENV_HOP + ENV_HOP / 2;
            let (lo, hi) = (c.saturating_sub(ENV_HOP), (c + ENV_HOP).min(band.len()));
            20.0 * rms(&band[lo..hi]).max(1e-6).log10()
        })
        .collect()
}

/// Absolute floor of the voice-activity threshold (voice-band dBFS): nothing
/// quieter counts as speech, however clean the recording.
pub const MIN_SPEECH_DB: f32 = -50.0;
/// Speech islands: gaps shorter than this (180 ms) are merged…
const ISLAND_MERGE_FRAMES: usize = 18;
/// …and each island is padded by 60 ms pre-roll / 80 ms post-roll.
const ISLAND_PRE_FRAMES: usize = 6;
const ISLAND_POST_FRAMES: usize = 8;
/// Furthest Whisper's start is pulled forward to the real onset (800 ms).
const MAX_ONSET_FRAMES: usize = 80;
/// Shortest word after snapping.
const MIN_WORD_MS: i64 = 50;

/// `(vad, snap)` thresholds for an envelope. `vad` is the lenient
/// voice-activity gate: 3× the noise-floor energy (20th percentile + 4.8 dB),
/// never below [`MIN_SPEECH_DB`]. `snap` is the boundary-snapping level, well
/// above the floor and scaled toward the loud parts (95th percentile). It is
/// never below `vad`.
fn speech_thresholds_db(env: &[f32]) -> (f32, f32) {
    let mut s: Vec<f32> = env.to_vec();
    s.sort_by(f32::total_cmp);
    let pct = |p: f32| s[((s.len() - 1) as f32 * p) as usize];
    let (floor, peak) = (pct(0.20), pct(0.95));
    let vad = (floor + 10.0 * 3.0f32.log10()).max(MIN_SPEECH_DB);
    (vad, (floor + (0.4 * (peak - floor)).max(6.0)).max(vad))
}

/// Active-speech islands of a 10 ms envelope, as frame ranges: frames at or
/// above `thr`, gaps under 180 ms merged, padded 60 ms before / 80 ms after.
pub fn speech_islands(env: &[f32], thr: f32) -> Vec<Range<usize>> {
    let mut out: Vec<Range<usize>> = Vec::new();
    let mut i = 0;
    while i < env.len() {
        if env[i] < thr {
            i += 1;
            continue;
        }
        let s = i;
        while i < env.len() && env[i] >= thr {
            i += 1;
        }
        match out.last_mut() {
            Some(r) if s - r.end < ISLAND_MERGE_FRAMES => r.end = i,
            _ => out.push(s..i),
        }
    }
    // merged gaps are ≥ 180 ms, so the padding (140 ms) never overlaps
    for r in &mut out {
        r.start = r.start.saturating_sub(ISLAND_PRE_FRAMES);
        r.end = (r.end + ISLAND_POST_FRAMES).min(env.len());
    }
    out
}

/// Speech islands of 16 kHz mono audio, relative to its first sample.
pub fn detect_speech(pcm16k: &[f32]) -> Vec<Range<Time>> {
    let env = energy_envelope_db(pcm16k);
    if env.is_empty() {
        return Vec::new();
    }
    let (vad, _) = speech_thresholds_db(&env);
    let t = |f: usize| Time::from_millis(f as i64 * 10);
    speech_islands(&env, vad).into_iter().map(|r| t(r.start)..t(r.end)).collect()
}

/// Make Whisper's words speech-only:
/// * **Silence gate** — a word whose span touches no speech island is a
///   hallucination over silent gameplay and is dropped.
/// * **Onset snap** — a start in silence moves forward (≤ 800 ms, staying
///   inside the word) to where voice energy rises. A start inside speech
///   walks back ≤ 120 ms to the real onset.
/// * **Offset snap** — an end in silence moves back to the last voiced frame.
///   An end inside speech extends ≤ 120 ms to where it stops.
///
/// Words come out strictly monotonic, non-overlapping and at least 50 ms
/// long. Snapping uses the loud-part threshold where the word reaches it,
/// otherwise the lenient VAD level, so quiet voice chat under loud game audio
/// still snaps.
pub fn snap_words_to_energy(words: &mut Vec<CaptionWord>, pcm16k: &[f32], offset: Time) {
    let env = energy_envelope_db(pcm16k);
    if env.len() < 4 || words.is_empty() {
        return;
    }
    let (vad, strict) = speech_thresholds_db(&env);
    let islands = speech_islands(&env, vad);
    let last = env.len() - 1;
    let frame = |t: Time| ((t - offset).as_secs_f64().max(0.0) * 100.0) as usize;
    let at = |f: usize| offset + Time::from_millis(f as i64 * 10);
    let originals: Vec<(Time, Time)> = words.iter().map(|w| (w.start, w.end)).collect();
    let mut kept: Vec<CaptionWord> = Vec::with_capacity(words.len());
    let mut prev_end = Time(-(i64::MAX / 4));
    for i in 0..words.len() {
        let (s0, e0) = originals[i];
        let next_start = originals.get(i + 1).map(|w| w.0).unwrap_or(Time::MAX);
        let sf = frame(s0).min(last);
        let ef = frame(e0 - Time(1)).min(last).max(sf);
        // --- silence gate: no speech island under the word → hallucination
        if !islands.iter().any(|r| r.start <= ef && sf < r.end) {
            continue;
        }
        let (lo, hi) = (sf.saturating_sub(SNAP_FRAMES), (ef + SNAP_FRAMES).min(last));
        let thr = if (lo..=hi).any(|j| env[j] >= strict) { strict } else { vad };
        let voiced = |j: usize| env[j.min(last)] >= thr;
        // --- start: onset of the speech the word begins in
        let start_f = if voiced(sf) {
            // already speaking: walk back (≤120 ms) to where the energy rose
            (sf.saturating_sub(SNAP_FRAMES)..sf).rev().find(|&j| !voiced(j)).map(|j| j + 1).unwrap_or(sf)
        } else {
            // Whisper started early: first voiced frame, ≤ 800 ms on, ≥ 40 ms before the word's end
            let lim = ef.saturating_sub(4).max(sf + SNAP_FRAMES).min(sf + MAX_ONSET_FRAMES).min(last);
            (sf + 1..=lim).find(|&j| voiced(j)).unwrap_or(sf)
        };
        // --- end: offset of the speech the word ends in
        let end_f = if voiced(ef) {
            (ef + 1..=(ef + SNAP_FRAMES).min(last)).find(|&j| !voiced(j)).unwrap_or(ef + 1)
        } else {
            (start_f..ef).rev().find(|&j| voiced(j)).map(|j| j + 1).unwrap_or(ef + 1)
        };
        let has_speech = (start_f..end_f.min(last + 1)).any(voiced);
        let (mut s, mut e) = if has_speech { (at(start_f), at(end_f)) } else { (s0, e0) };
        if has_speech {
            // never pull a boundary more than 120 ms *outside* Whisper's span
            let r = Time::from_millis(120);
            s = s.max(s0 - r);
            e = e.min(e0 + r);
        } else {
            // no energy at all: cap a stretched word to a plausible length
            let cap = Time::from_millis(250 + 90 * words[i].text.chars().count() as i64).min(Time::from_millis(1_200));
            e = e.min(s + cap);
        }
        let min = Time::from_millis(MIN_WORD_MS);
        s = s.max(prev_end);
        e = e.min(next_start.max(s + min));
        if e < s + min {
            e = s + min;
        }
        kept.push(CaptionWord { start: s, end: e, text: std::mem::take(&mut words[i].text) });
        prev_end = e;
    }
    *words = kept;
}

/// Words this close (centiseconds) are one back-to-back run for [`warp_to_speech`].
const WARP_RUN_GAP_CS: i64 = 30;
/// How far (centiseconds) a run may be pulled outside its stamped span.
const WARP_SLACK_CS: i64 = 50;

/// Speech islands of the whole recording (10 ms frames = centiseconds), for
/// [`warp_to_speech`].
pub fn speech_islands_cs(pcm16k: &[f32]) -> Vec<Range<i64>> {
    let env = energy_envelope_db(pcm16k);
    if env.is_empty() {
        return Vec::new();
    }
    let (vad, _) = speech_thresholds_db(&env);
    speech_islands(&env, vad).into_iter().map(|r| r.start as i64..r.end as i64).collect()
}

/// Re-time token-timestamp words (no DTW) onto the speech.
///
/// Without DTW, whisper.cpp lays a segment's words end to end across the
/// segment's whole span, pauses included, so the error grows through the
/// segment. Sentence starts and ends then sat in the silence and the
/// silence gate in [`snap_words_to_energy`] deleted them (measured on a
/// 77 s, 141-word clip: 109 words kept on Vulkan, and on the CPU with DTW
/// off, against 140 with DTW).
///
/// Only the word order and the run's rough span are trusted. Each run of
/// back-to-back words is aligned to the speech islands inside its span
/// (±0.5 s, never into a neighbouring run) by [`align_groups`]: ordered word
/// groups go to ordered islands so each island's length matches its words'
/// character weight, with island breaks preferred after `.?!` and `,`. In
/// an island, words are spread by character weight; the energy snap
/// refines the edges afterwards. A run with no speech under it is left
/// alone, and the gate still drops it as a hallucination.
///
/// `raw` is in centiseconds; `islands` likewise, on the same origin.
pub fn warp_to_speech(raw: &mut [(i64, i64, String)], islands: &[Range<i64>]) {
    let n = raw.len();
    let mut i = 0;
    while i < n {
        let mut j = i + 1;
        while j < n && raw[j].0 - raw[j - 1].1 <= WARP_RUN_GAP_CS {
            j += 1;
        }
        let (lo, hi) = (raw[i].0, raw[i..j].iter().map(|w| w.1).max().unwrap_or(raw[i].0));
        let prev_end = if i == 0 { i64::MIN } else { raw[i - 1].1 };
        let next_start = raw.get(j).map_or(i64::MAX, |w| w.0);
        let (a, b) = ((lo - WARP_SLACK_CS).max(prev_end), (hi + WARP_SLACK_CS).min(next_start));
        let speech: Vec<(i64, i64)> = islands.iter().map(|r| (r.start.max(a), r.end.min(b))).filter(|(s, e)| e > s).collect();
        if hi > lo && !speech.is_empty() {
            let weights: Vec<f64> = raw[i..j].iter().map(|w| word_weight(&w.2)).collect();
            let breaks: Vec<f64> = raw[i..j].iter().map(|w| break_bonus(&w.2)).collect();
            let lens: Vec<f64> = speech.iter().map(|(s, e)| (e - s) as f64).collect();
            let groups = align_groups(&weights, &breaks, &lens);
            for (k, g) in groups.iter().enumerate() {
                let (s, e) = speech[k];
                let total: f64 = weights[g.clone()].iter().sum();
                let mut acc = 0.0;
                for wi in g.clone() {
                    let at = |x: f64| s + ((e - s) as f64 * x / total).round() as i64;
                    let w = &mut raw[i + wi];
                    w.0 = at(acc);
                    acc += weights[wi];
                    w.1 = at(acc).max(w.0);
                }
            }
        }
        i = j;
    }
}

/// Relative spoken length of a word: its letters and digits, plus one.
fn word_weight(text: &str) -> f64 {
    1.0 + text.chars().filter(|c| c.is_alphanumeric()).count() as f64
}

/// How strongly a pause after this word is expected (in weight units).
fn break_bonus(text: &str) -> f64 {
    match text.trim_end_matches(['"', '\'', ')', ']']).chars().last() {
        Some('.' | '?' | '!' | '。' | '？' | '！') => 10.0,
        Some(',' | ';' | ':' | '、' | '，') => 4.0,
        _ => 0.0,
    }
}

/// Split words `0..n` into one contiguous, possibly empty group per island,
/// in order, minimising Σ 3·|want − got| / √max(want, got) minus the break
/// bonus of every group's last word that a pause follows. `want` is the
/// island's length over the run's overall speech time per weight unit,
/// `got` the group's weight.
pub fn align_groups(weights: &[f64], breaks: &[f64], lens: &[f64]) -> Vec<Range<usize>> {
    let (n, k) = (weights.len(), lens.len());
    if k == 0 {
        return Vec::new();
    }
    let total_w: f64 = weights.iter().sum();
    let rate = lens.iter().sum::<f64>() / total_w.max(1e-9);
    let mut prefix = vec![0.0; n + 1];
    for (i, w) in weights.iter().enumerate() {
        prefix[i + 1] = prefix[i] + w;
    }
    // dp[m][j]: best cost with words 0..j placed on islands 0..m
    let inf = f64::INFINITY;
    let mut dp = vec![vec![inf; n + 1]; k + 1];
    let mut from = vec![vec![0usize; n + 1]; k + 1];
    dp[0][0] = 0.0;
    for m in 0..k {
        let want = lens[m] / rate.max(1e-9);
        let last_island = m + 1 == k;
        for i in 0..=n {
            if dp[m][i] == inf {
                continue;
            }
            // the last island must take every remaining word
            let js: Box<dyn Iterator<Item = usize>> = if last_island { Box::new(std::iter::once(n)) } else { Box::new(i..=n) };
            for j in js {
                let got = prefix[j] - prefix[i];
                // speaking rate varies roughly in proportion to length, so the
                // mismatch is scaled by √size: a long island may be ±20 % off
                // without outweighing a sentence break
                let mut c = dp[m][i] + 3.0 * (want - got).abs() / want.max(got).max(1.0).sqrt();
                if j > i && !last_island {
                    c -= breaks[j - 1];
                }
                if c < dp[m + 1][j] {
                    dp[m + 1][j] = c;
                    from[m + 1][j] = i;
                }
            }
        }
    }
    let mut out = vec![0..0; k];
    let mut j = n;
    for m in (1..=k).rev() {
        let i = from[m][j];
        out[m - 1] = i..j;
        j = i;
    }
    out
}

/// DTW onsets land inside the word's first syllable: measured against the
/// energy onsets of TTS speech they trail by 150–280 ms (mean ≈ 220 ms) on
/// both base and large-v3-turbo, so starts are pulled back by this much
/// (centiseconds) before the valley refinement.
pub const DTW_LAG_CS: i64 = 20;
/// Valley search window around a (lag-corrected) DTW start: 120 ms back,
/// 100 ms on, in 10 ms envelope frames.
const VALLEY_BACK_FRAMES: usize = 12;
const VALLEY_FWD_FRAMES: usize = 10;
/// A valley must sit this far below the window's peak to count as a word
/// boundary.
const VALLEY_DEPTH_DB: f32 = 6.0;

/// DTW gives starts only: a word ends where the next one starts (≤ 1.5 s
/// away), the last one keeps its token end (≥ 300 ms). Silence after a word
/// is trimmed later by the energy snap. Times in centiseconds.
pub fn dtw_word_ends(raw: &mut [(i64, i64, String)]) {
    for i in 0..raw.len() {
        let s = raw[i].0;
        raw[i].1 = match raw.get(i + 1) {
            Some(next) if next.0 > s => next.0.min(s + 150),
            _ => raw[i].1.max(s + 30),
        };
    }
}

/// Refine DTW word timings against the voice envelope:
/// 1. **Stranded words** — DTW parks the last word of a phrase in the pause
///    after it (its start is silent and the next word claims the same
///    onset). Such a word is moved back into the second half of the previous
///    word's speech island.
/// 2. **Valley snap** — in a window of −120…+100 ms around each start, the
///    deepest envelope minimum (≥ 6 dB under the window's peak, window edges
///    included) marks the boundary; the start moves to where the energy
///    climbs 35 % of the way back to the peak.
/// 3. **No word spans a pause** — an end is cut where ≥ 120 ms of silence
///    begins.
///
/// Words stay ordered and non-overlapping, ≥ 50 ms long.
pub fn refine_onsets_to_valleys(words: &mut [CaptionWord], pcm16k: &[f32], offset: Time) {
    let env = energy_envelope_db(pcm16k);
    if env.len() < 4 || words.is_empty() {
        return;
    }
    let (vad, _) = speech_thresholds_db(&env);
    let last = env.len() - 1;
    let voiced = |j: usize| env[j.min(last)] >= vad;
    let frame = |t: Time| (((t - offset).as_secs_f64().max(0.0) * 100.0) as usize).min(last);
    let at = |f: usize| offset + Time::from_millis(f as i64 * 10);
    let min = Time::from_millis(MIN_WORD_MS);
    // first frame ≥ f where ≥ 120 ms of silence starts
    let island_end = |f: usize| (f..=last).find(|&j| (j..(j + 12).min(last + 1)).all(|k| !voiced(k))).unwrap_or(last + 1);
    let mut fixed = vec![false; words.len()];
    // 1. stranded phrase-final words
    for i in 1..words.len() {
        let sf = frame(words[i].start);
        if (sf..=(sf + 4).min(last)).any(voiced) {
            continue;
        }
        let Some(on) = (sf..=last).find(|&j| voiced(j)) else { continue };
        let claimed = words.get(i + 1).is_some_and(|n| frame(n.start) <= on + 15);
        let pf = frame(words[i - 1].start);
        let pe = island_end(pf);
        if claimed && pe > pf + 10 && pe <= sf {
            let mid = pf + (pe - pf) / 2;
            words[i].start = at(mid);
            words[i].end = at(pe);
            words[i - 1].end = words[i - 1].end.min(words[i].start);
            fixed[i] = true;
        }
    }
    // 2. valley snap
    for i in 0..words.len() {
        if fixed[i] {
            continue;
        }
        let sf = frame(words[i].start);
        let (lo, hi) = (sf.saturating_sub(VALLEY_BACK_FRAMES), (sf + VALLEY_FWD_FRAMES).min(last));
        if lo + 2 > hi {
            continue;
        }
        let peak = env[lo..=hi].iter().copied().fold(f32::MIN, f32::max);
        let is_min = |j: usize| (j == lo || env[j] <= env[j - 1]) && (j == hi || env[j] <= env[j + 1]);
        let valley = (lo..=hi).filter(|&j| is_min(j) && env[j] <= peak - VALLEY_DEPTH_DB).min_by(|&a, &b| env[a].total_cmp(&env[b]).then(a.abs_diff(sf).cmp(&b.abs_diff(sf))));
        let Some(v) = valley else { continue };
        let rise = env[v] + 0.35 * (peak - env[v]);
        let Some(on) = (v + 1..=hi).find(|&j| env[j] >= rise) else { continue };
        let floor = if i > 0 { words[i - 1].start + min } else { Time(i64::MIN / 4) };
        words[i].start = at(on).max(floor);
    }
    // 3. ends: never across a pause, never past the next start
    for i in 0..words.len() {
        let s = words[i].start;
        let mut e = words[i].end.min(at(island_end(frame(s))).max(s + min));
        if let Some(n) = words.get(i + 1) {
            e = e.min(n.start);
        }
        words[i].end = e.max(s + min);
    }
}

/// Trim and validate a language setting: `None` (auto-detect) for empty or
/// "auto", otherwise a lower-case ISO-639 code. Never `Some("")`.
pub fn normalize_language(lang: Option<&str>) -> Result<Option<String>, String> {
    let Some(code) = lang.map(|l| l.trim().to_ascii_lowercase()) else { return Ok(None) };
    if code.is_empty() || code == "auto" {
        return Ok(None);
    }
    if (2..=3).contains(&code.len()) && code.chars().all(|c| c.is_ascii_lowercase()) {
        Ok(Some(code))
    } else {
        Err(format!("invalid language code '{code}' (use a code like en, es, de, or leave empty for auto)"))
    }
}

// ------------------------------------------------------------------- model

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ModelFileError {
    #[error("model file is missing")]
    Missing,
    #[error("model file is truncated ({len} bytes, expected at least {min})")]
    Truncated { len: u64, min: u64 },
    #[error("model file is not a whisper.cpp GGML model (bad header)")]
    BadMagic,
    #[error("cannot read model file: {0}")]
    Io(String),
}

impl ModelFileError {
    /// Damaged (as opposed to missing or unreadable): delete and re-download.
    pub fn is_corrupt(&self) -> bool {
        matches!(self, ModelFileError::Truncated { .. } | ModelFileError::BadMagic)
    }
}

/// Check that `path` exists, is at least `min_bytes` long and starts with the
/// GGML magic. Loading a truncated model can abort inside whisper.cpp.
pub fn validate_model_file(path: &Path, min_bytes: u64) -> Result<(), ModelFileError> {
    use std::io::Read;
    let meta = match path.metadata() {
        Ok(m) if m.is_file() => m,
        Ok(_) => return Err(ModelFileError::Missing),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Err(ModelFileError::Missing),
        Err(e) => return Err(ModelFileError::Io(e.to_string())),
    };
    let min = min_bytes.max(GGML_MAGIC.len() as u64);
    if meta.len() < min {
        return Err(ModelFileError::Truncated { len: meta.len(), min });
    }
    let mut magic = [0u8; 4];
    std::fs::File::open(path).and_then(|mut f| f.read_exact(&mut magic)).map_err(|e| ModelFileError::Io(e.to_string()))?;
    if magic != GGML_MAGIC {
        return Err(ModelFileError::BadMagic);
    }
    Ok(())
}

// ------------------------------------------------------------- output words

/// Non-speech annotations Whisper emits ("[Music]", "(laughs)", "♪").
fn is_annotation(t: &str) -> bool {
    let t = t.trim();
    t.is_empty() || (t.starts_with('[') && t.ends_with(']')) || (t.starts_with('(') && t.ends_with(')')) || t.chars().all(|c| c == '♪' || c == '*' || c.is_whitespace())
}

/// Turn raw `(start_cs, end_cs, text)` word segments (centiseconds) into
/// clean caption words: annotations dropped, sub-word pieces without a
/// leading space glued to the previous word, timestamps made monotonic and
/// non-empty, punctuation-only pieces attached to the previous word.
pub fn words_from_segments(raw: &[(i64, i64, String)], offset: Time) -> Vec<CaptionWord> {
    let mut out: Vec<CaptionWord> = Vec::new();
    for (s, e, text) in raw {
        if is_annotation(text) {
            continue;
        }
        let start = offset + Time::from_millis((*s).max(0) * 10);
        let end = offset + Time::from_millis((*e).max(*s).max(0) * 10);
        let starts_word = text.starts_with(' ');
        let piece = text.trim();
        let punct_only = piece.chars().all(|c| c.is_ascii_punctuation());
        match out.last_mut() {
            Some(prev) if !starts_word || punct_only => {
                prev.text.push_str(piece);
                prev.end = prev.end.max(end);
            }
            _ => out.push(CaptionWord { start, end, text: piece.to_string() }),
        }
    }
    spread_collapsed_words(&mut out);
    repair_words(&mut out);
    out
}

/// Whisper often stamps several consecutive words with one identical
/// `t0 == t1` instant (or a single zero-length word). Pushing each such word
/// 80 ms past the previous one makes the captions drift later and later;
/// instead spread the run over the time it really has — up to the next
/// word's start (at most 600 ms per word, at least 80 ms) — with each word's
/// share weighted by its character count.
fn spread_collapsed_words(words: &mut [CaptionWord]) {
    let mut i = 0;
    while i < words.len() {
        let (s0, e0) = (words[i].start, words[i].end);
        let mut j = i + 1;
        while j < words.len() && words[j].start == s0 && words[j].end == e0 {
            j += 1;
        }
        let n = (j - i) as f64;
        if n > 1.0 || e0 <= s0 {
            let start = s0.as_secs_f64();
            let next = words.get(j).map(|w| w.start.as_secs_f64()).filter(|&t| t > start);
            let span = next.unwrap_or(start + 0.3 * n).min(start + 0.6 * n).max(e0.as_secs_f64()).max(start + 0.08 * n) - start;
            let weight = |w: &CaptionWord| w.text.chars().filter(|c| c.is_alphanumeric()).count().max(1) as f64;
            let total: f64 = words[i..j].iter().map(weight).sum();
            let mut acc = 0.0;
            for w in &mut words[i..j] {
                let k = weight(w);
                w.start = Time::from_secs_f64(start + span * acc / total);
                acc += k;
                w.end = Time::from_secs_f64(start + span * acc / total);
            }
        }
        i = j;
    }
}

/// Monotonic starts, at least 80 ms per word, no overlaps.
fn repair_words(words: &mut [CaptionWord]) {
    let min = Time::from_millis(80);
    for i in 0..words.len() {
        if i > 0 && words[i].start < words[i - 1].end {
            words[i].start = words[i - 1].end;
        }
        if words[i].end < words[i].start + min {
            words[i].end = words[i].start + min;
        }
    }
}

// ------------------------------------------------------------------ engine

#[cfg(feature = "whisper")]
mod engine {
    use super::*;
    use std::ffi::c_void;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use whisper_rs::{DtwMode, DtwModelPreset, DtwParameters, FullParams, SamplingStrategy, WhisperContext, WhisperContextParameters, WhisperSegment, WhisperTokenId};

    /// Minimum DTW word length, centiseconds.
    const MIN_DTW_WORD_CS: i64 = 5;

    /// DTW alignment-head preset for a ggml model file, from its name
    /// (`ggml-large-v3-turbo-q5_0.bin` → `LargeV3Turbo`). `None` = unknown
    /// model, word times then come from token timestamps only.
    pub(super) fn dtw_preset(model: &std::path::Path) -> Option<DtwModelPreset> {
        let name = model.file_name()?.to_string_lossy().to_ascii_lowercase();
        let en = name.contains(".en") || name.contains("-en");
        let has = |k: &str| name.contains(k);
        Some(if has("large-v3-turbo") || has("large-v3_turbo") {
            DtwModelPreset::LargeV3Turbo
        } else if has("large-v3") {
            DtwModelPreset::LargeV3
        } else if has("large-v2") {
            DtwModelPreset::LargeV2
        } else if has("large") {
            DtwModelPreset::LargeV1
        } else if has("medium") {
            if en {
                DtwModelPreset::MediumEn
            } else {
                DtwModelPreset::Medium
            }
        } else if has("small") {
            if en {
                DtwModelPreset::SmallEn
            } else {
                DtwModelPreset::Small
            }
        } else if has("base") {
            if en {
                DtwModelPreset::BaseEn
            } else {
                DtwModelPreset::Base
            }
        } else if has("tiny") {
            if en {
                DtwModelPreset::TinyEn
            } else {
                DtwModelPreset::Tiny
            }
        } else {
            return None;
        })
    }

    /// DTW (cross-attention aligned) time of a word segment's first text
    /// token, centiseconds; `None` when DTW gave nothing (`t_dtw < 0`).
    fn first_dtw(seg: &WhisperSegment, eot: WhisperTokenId) -> Option<i64> {
        (0..seg.n_tokens()).filter_map(|i| seg.get_token(i)).filter(|t| t.token_id() < eot).map(|t| t.token_data().t_dtw).find(|&d| d >= 0)
    }

    /// Word `[t0, t1]` from the segment timestamps, with the start replaced
    /// by the DTW time when there is one (t1 kept ≥ start + 50 ms).
    pub(super) fn word_span(t0: i64, t1: i64, dtw: Option<i64>) -> (i64, i64) {
        match dtw {
            Some(d) => {
                let s = (d - DTW_LAG_CS).max(0);
                (s, t1.max(s + MIN_DTW_WORD_CS))
            }
            None => (t0, t1.max(t0)),
        }
    }

    const CANCELLED: &str = "Cancelled";

    /// whisper.cpp abort callback. `user_data` is the run's `AtomicBool`.
    unsafe extern "C" fn abort_requested(user_data: *mut c_void) -> bool {
        // SAFETY: `run` passes `Arc::as_ptr(&cancel)` and keeps the Arc alive
        // until `state.full` returns; nothing else is behind the pointer.
        !user_data.is_null() && unsafe { (*(user_data as *const AtomicBool)).load(Ordering::Relaxed) }
    }

    #[cfg(feature = "whisper-vulkan")]
    extern "C" {
        fn ggml_backend_reg_by_name(name: *const std::ffi::c_char) -> *mut c_void;
        fn ggml_backend_reg_dev_count(reg: *mut c_void) -> usize;
    }

    /// Name of Vulkan device 0 (ggml lists discrete GPUs before integrated).
    ///
    /// ggml registers its Vulkan backend with the instance init inside a
    /// `try`; `list_devices()` re-runs that init without one, so with no
    /// Vulkan loader, a Vulkan 1.1 driver or no ICD it threw C++ through FFI
    /// and aborted. Ask the registry first; only a registered backend with
    /// devices is listed.
    fn vulkan_device() -> Option<String> {
        #[cfg(feature = "whisper-vulkan")]
        {
            // SAFETY: plain C calls into ggml's backend registry; the name is a
            // NUL-terminated literal and the returned handle is only passed back.
            let devices = unsafe {
                let reg = ggml_backend_reg_by_name(c"Vulkan".as_ptr());
                if reg.is_null() {
                    0
                } else {
                    ggml_backend_reg_dev_count(reg)
                }
            };
            if devices == 0 {
                return None;
            }
            return whisper_rs::vulkan::list_devices().into_iter().next().map(|d| d.name);
        }
        #[allow(unreachable_code)]
        None
    }

    struct Run {
        words: Vec<CaptionWord>,
        language: String,
    }

    #[allow(clippy::too_many_arguments)]
    fn run(
        audio: &[f32],
        opts: &TranscribeOptions,
        gpu: bool,
        dtw: bool,
        threads: usize,
        progress: &Arc<dyn Fn(f32) + Send + Sync>,
        cancel: &Arc<AtomicBool>,
    ) -> Result<Run, String> {
        let mut cp = WhisperContextParameters::default();
        cp.use_gpu(gpu);
        cp.gpu_device(0);
        let preset = if dtw { dtw_preset(&opts.model) } else { None };
        let dtw = preset.is_some();
        if let Some(model_preset) = preset {
            // DTW needs flash attention off (the default)
            cp.flash_attn(false);
            cp.dtw_parameters(DtwParameters { mode: DtwMode::ModelPreset { model_preset }, ..DtwParameters::default() });
        }
        let ctx = WhisperContext::new_with_params(&opts.model, cp).map_err(|e| format!("load model: {e}"))?;
        let eot = ctx.token_eot();
        // A fresh state per run: no decoder KV-cache survives from an earlier job.
        let mut state = ctx.create_state().map_err(|e| format!("whisper state: {e}"))?;

        let mut language = normalize_language(opts.language.as_deref())?;
        if let Some(code) = &language {
            if whisper_rs::get_lang_id(code).is_none() {
                return Err(format!("Whisper does not know the language '{code}'"));
            }
        }
        let threads = threads.clamp(1, i32::MAX as usize) as i32;
        let windows = chunk_windows(audio.len());
        let n = windows.len().max(1) as f32;
        // token-timestamp words are re-timed onto the speech (see warp_to_speech)
        let islands = if dtw { Vec::new() } else { speech_islands_cs(audio) };
        let mut chunks = Vec::with_capacity(windows.len());
        let mut detected = None;

        for (i, window) in windows.iter().enumerate() {
            if cancel.load(Ordering::Relaxed) {
                return Err(CANCELLED.into());
            }
            let mut pcm = audio[window.clone()].to_vec();
            if pcm.len() < MIN_SAMPLES {
                pcm.resize(MIN_SAMPLES, 0.0);
            }
            let mut params = FullParams::new(SamplingStrategy::Greedy { best_of: 1 });
            params.set_language(language.as_deref()); // None = auto-detect, never ""
            params.set_n_threads(threads);
            params.set_no_context(true);
            params.set_token_timestamps(true);
            params.set_max_len(1);
            params.set_split_on_word(true);
            params.set_print_progress(false);
            params.set_print_realtime(false);
            params.set_print_special(false);
            params.set_print_timestamps(false);
            params.set_suppress_blank(true);
            let p = progress.clone();
            let base = i as f32 / n;
            params.set_progress_callback_safe(move |pct: i32| p((base + pct as f32 / 100.0 / n).clamp(0.0, 1.0)));
            // SAFETY: `abort_requested` only reads the AtomicBool behind
            // `cancel`, which outlives this `state.full` call.
            unsafe {
                params.set_abort_callback(Some(abort_requested));
                params.set_abort_callback_user_data(Arc::as_ptr(cancel) as *mut c_void);
            }
            if let Err(e) = state.full(params, &pcm) {
                if cancel.load(Ordering::Relaxed) {
                    return Err(CANCELLED.into());
                }
                return Err(format!("transcription failed on chunk {}/{}: {e}", i + 1, windows.len()));
            }
            let mut raw = Vec::new();
            for s in 0..state.full_n_segments() {
                if let Some(seg) = state.get_segment(s) {
                    let text = seg.to_str_lossy().map(|t| t.into_owned()).unwrap_or_default();
                    let d = if dtw { first_dtw(&seg, eot) } else { None };
                    let (t0, t1) = word_span(seg.start_timestamp(), seg.end_timestamp(), d);
                    raw.push((t0, t1, text));
                }
            }
            if dtw {
                dtw_word_ends(&mut raw);
            } else {
                // window start in 10 ms frames (windows start on multiples of 8000 samples)
                let off = (window.start / ENV_HOP) as i64;
                let len = (window.len() / ENV_HOP) as i64;
                let local: Vec<Range<i64>> = islands.iter().filter(|r| r.end > off && r.start < off + len).map(|r| r.start - off..r.end - off).collect();
                warp_to_speech(&mut raw, &local);
            }
            // Lock the language detected on the first window with speech so
            // later windows cannot flip it.
            if language.is_none() && raw.iter().any(|(_, _, t)| !is_annotation(t)) {
                if let Some(code) = whisper_rs::get_lang_str(state.full_lang_id_from_state()) {
                    detected = Some(code.to_string());
                    language = detected.clone();
                }
            }
            chunks.push(raw);
            progress(((i + 1) as f32 / n).clamp(0.0, 1.0));
        }
        let language = detected.or(language).unwrap_or_else(|| "?".into());
        let mut words = merge_chunk_words(&windows, &chunks, opts.offset);
        if dtw {
            refine_onsets_to_valleys(&mut words, audio, opts.offset);
        }
        Ok(Run { words, language })
    }

    /// Whether to ask for DTW word timestamps on this backend: on unless
    /// `RCP_WHISPER_DTW=0`, and on the GPU also unless `RCP_WHISPER_DTW_GPU=0`.
    /// GPU DTW was off by default for a while because runs on the dev PC
    /// (Intel Arc) took the whole machine down; that turned out to be a CPU
    /// hardware fault, and `tests/whisper_backends.rs` shows Vulkan + DTW
    /// hearing the same words as the CPU, faster and with better timing.
    pub(super) fn dtw_enabled(gpu: bool, dtw_env: Option<&str>, dtw_gpu_env: Option<&str>) -> bool {
        dtw_env != Some("0") && (!gpu || dtw_gpu_env != Some("0"))
    }

    /// DTW word timestamps first when [`dtw_enabled`]; if the DTW run fails,
    /// the same backend again without DTW.
    fn run_dtw_or_plain(
        audio: &[f32],
        opts: &TranscribeOptions,
        gpu: bool,
        threads: usize,
        progress: &Arc<dyn Fn(f32) + Send + Sync>,
        cancel: &Arc<AtomicBool>,
    ) -> Result<Run, String> {
        let env = |k: &str| std::env::var(k).ok();
        let want = dtw_enabled(gpu, env("RCP_WHISPER_DTW").as_deref(), env("RCP_WHISPER_DTW_GPU").as_deref()) && dtw_preset(&opts.model).is_some();
        if want {
            match run(audio, opts, gpu, true, threads, progress, cancel) {
                Ok(r) => return Ok(r),
                Err(e) if e == CANCELLED => return Err(e),
                Err(e) => {
                    log::warn!("Whisper DTW timestamps failed ({e}); retrying without DTW");
                    progress(0.0);
                }
            }
        }
        run(audio, opts, gpu, false, threads, progress, cancel)
    }

    pub fn transcribe(
        audio: &[f32],
        opts: &TranscribeOptions,
        progress: Arc<dyn Fn(f32) + Send + Sync>,
        status: Arc<dyn Fn(String) + Send + Sync>,
        cancel: Arc<AtomicBool>,
    ) -> Result<Transcript, String> {
        let mut fallback_reason = None;
        if opts.use_gpu && GPU_BUILD {
            match vulkan_device() {
                Some(device) => {
                    let backend = format!("GPU: Vulkan ({device})");
                    status(format!("Transcribing on {backend}…"));
                    log::info!("Whisper: {backend}");
                    match run_dtw_or_plain(audio, opts, true, opts.threads, &progress, &cancel) {
                        Ok(r) => return Ok(Transcript { words: r.words, language: r.language, backend, fallback_reason: None }),
                        Err(e) if e == CANCELLED => return Err(e),
                        Err(e) => {
                            log::warn!("Whisper on the GPU failed ({e}); retrying on the CPU");
                            fallback_reason = Some(e);
                        }
                    }
                }
                None => fallback_reason = Some("no usable Vulkan 1.2 GPU (Vulkan runtime missing, driver too old, or no device)".into()),
            }
        }
        let threads = cpu_threads();
        let backend = if fallback_reason.is_some() { format!("CPU Fallback ({threads} threads)") } else { format!("CPU ({threads} threads)") };
        status(format!("Transcribing on {backend}…"));
        log::info!("Whisper: {backend}");
        progress(0.0);
        let r = run_dtw_or_plain(audio, opts, false, threads, &progress, &cancel)?;
        Ok(Transcript { words: r.words, language: r.language, backend, fallback_reason })
    }
}

/// Transcribe 16 kHz mono audio into timed words. `progress` gets 0..1,
/// `status` the active backend ("Transcribing on GPU: Vulkan (…)…");
/// setting `cancel` aborts. Empty or silent audio returns no words without
/// loading the model. Errors if this build has no Whisper support.
pub fn transcribe(
    audio_16k: &[f32],
    opts: &TranscribeOptions,
    progress: std::sync::Arc<dyn Fn(f32) + Send + Sync>,
    status: std::sync::Arc<dyn Fn(String) + Send + Sync>,
    cancel: std::sync::Arc<std::sync::atomic::AtomicBool>,
) -> Result<Transcript, String> {
    let Some(pcm) = prepare_pcm(audio_16k) else {
        progress(1.0);
        return Ok(Transcript::empty("skipped (silent audio)"));
    };
    normalize_language(opts.language.as_deref())?;
    match validate_model_file(&opts.model, 1 << 20) {
        Ok(()) => {}
        Err(ModelFileError::Missing) => return Err(format!("model not found: {} (download it in the Captions panel)", opts.model.display())),
        Err(e) => return Err(format!("{e}: {} (delete and re-download it in the Captions panel)", opts.model.display())),
    }
    #[cfg(feature = "whisper")]
    {
        let mut tr = engine::transcribe(&pcm, opts, progress, status, cancel)?;
        // Whisper token stamps hug its coarse segment edges (words start on
        // whole seconds and the last word runs into the silence after it):
        // align every word to the actual speech energy.
        snap_words_to_energy(&mut tr.words, &pcm, opts.offset);
        Ok(tr)
    }
    #[cfg(not(feature = "whisper"))]
    {
        let _ = (pcm, progress, status, cancel);
        Err("this build has no Whisper support (rebuild with --features whisper or whisper-vulkan)".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// DTW stays on for CPU runs but is opt-in on the GPU.
    #[cfg(feature = "whisper")]
    #[test]
    fn dtw_is_opt_in_on_gpu() {
        use engine::dtw_enabled;
        assert!(dtw_enabled(false, None, None));
        assert!(!dtw_enabled(false, Some("0"), None));
        assert!(dtw_enabled(true, None, None));
        assert!(!dtw_enabled(true, None, Some("0")));
        assert!(dtw_enabled(true, None, Some("1")));
        assert!(!dtw_enabled(true, Some("0"), Some("1")));
    }

    /// Whisper-style stamps glued to segment edges (start on a whole second,
    /// last word stretched into silence) snap to the real speech bursts.
    #[test]
    fn words_snap_to_speech_energy() {
        let sr = SAMPLE_RATE as f32;
        let bursts = [(1.30f32, 1.70f32), (1.80, 2.40)];
        let pcm: Vec<f32> = (0..(4.0 * sr) as usize)
            .map(|i| {
                let t = i as f32 / sr;
                let noise = 0.002 * ((i as f32 * 12.9898).sin() * 43_758.547).fract();
                let speech = if bursts.iter().any(|(a, b)| t >= *a && t < *b) { 0.3 * (std::f32::consts::TAU * 600.0 * t).sin() } else { 0.0 };
                noise + speech
            })
            .collect();
        let off = Time::from_secs(10);
        let w = |s: f64, e: f64| CaptionWord { start: off + Time::from_secs_f64(s), end: off + Time::from_secs_f64(e), text: "word".into() };
        let mut words = vec![w(1.0, 1.75), w(1.75, 3.0)];
        snap_words_to_energy(&mut words, &pcm, off);
        let rel = |t: Time| (t - off).as_secs_f64();
        let close = |a: f64, b: f64| (a - b).abs() <= 0.025;
        assert!(close(rel(words[0].start), 1.30) && close(rel(words[0].end), 1.70), "{:?}", (rel(words[0].start), rel(words[0].end)));
        assert!(close(rel(words[1].start), 1.80) && close(rel(words[1].end), 2.40), "{:?}", (rel(words[1].start), rel(words[1].end)));
        assert!(words[0].end <= words[1].start, "non-overlapping");

        // continuous speech: adjacent boundaries move ≤ 120 ms and stay ordered
        let mut words = vec![w(1.40, 1.55), w(1.55, 1.65)];
        snap_words_to_energy(&mut words, &pcm, off);
        assert!(words[0].start < words[0].end && words[0].end <= words[1].start && words[1].start < words[1].end);
        assert!((rel(words[0].end) - 1.55).abs() <= 0.12 + 1e-9);
    }

    /// Noise floor plus 600 Hz tone bursts `(start, end, amplitude)`.
    fn synth(secs: f32, bursts: &[(f32, f32, f32)]) -> Vec<f32> {
        let sr = SAMPLE_RATE as f32;
        (0..(secs * sr) as usize)
            .map(|i| {
                let t = i as f32 / sr;
                let noise = 0.002 * ((i as f32 * 12.9898).sin() * 43_758.547).fract();
                let amp: f32 = bursts.iter().filter(|(a, b, _)| t >= *a && t < *b).map(|b| b.2).sum();
                noise + amp * (std::f32::consts::TAU * 600.0 * t).sin()
            })
            .collect()
    }

    fn word(s: f64, e: f64, t: &str) -> CaptionWord {
        CaptionWord { start: Time::from_secs_f64(s), end: Time::from_secs_f64(e), text: t.into() }
    }

    /// Whisper hallucinates over silent gameplay and starts words up to
    /// 800 ms early: hallucinations are dropped, early starts snap forward.
    #[test]
    fn silence_gate_drops_hallucinations_and_snaps_early_onsets() {
        let pcm = synth(8.0, &[(3.0, 3.5, 0.3), (3.6, 4.0, 0.3)]);
        let mut words = vec![
            word(0.2, 0.9, "thanks"),    // pure silence → hallucination
            word(2.3, 3.45, "nice"),     // starts 700 ms early
            word(3.45, 4.6, "shot"),     // stretched 600 ms into silence
            word(6.0, 7.5, "subscribe"), // pure silence → hallucination
        ];
        snap_words_to_energy(&mut words, &pcm, Time::ZERO);
        let texts: Vec<&str> = words.iter().map(|w| w.text.as_str()).collect();
        assert_eq!(texts, ["nice", "shot"], "silent-section words dropped");
        let s = |t: Time| t.as_secs_f64();
        assert!((s(words[0].start) - 3.0).abs() <= 0.025, "onset snapped forward: {}", s(words[0].start));
        assert!((s(words[1].end) - 4.0).abs() <= 0.025, "offset snapped back: {}", s(words[1].end));
        assert!(words[0].end <= words[1].start);
        for w in &words {
            assert!(w.end - w.start >= Time::from_millis(MIN_WORD_MS));
        }
    }

    /// Loud gunfire lifts the loud-part threshold above quiet voice chat;
    /// the lenient VAD level still finds and snaps the voice.
    #[test]
    fn quiet_voice_under_loud_game_audio_still_snaps() {
        let mut bursts: Vec<(f32, f32, f32)> = (0..8).map(|k| (0.5 + k as f32 * 0.6, 0.6 + k as f32 * 0.6, 0.9)).collect();
        bursts.push((6.0, 6.5, 0.01)); // quiet voice, ~40 dB under the guns (below the loud-part threshold)
        let pcm = synth(8.0, &bursts);
        let mut words = vec![word(5.5, 6.4, "rotate")];
        snap_words_to_energy(&mut words, &pcm, Time::ZERO);
        assert_eq!(words.len(), 1, "quiet voice is not a hallucination");
        assert!((words[0].start.as_secs_f64() - 6.0).abs() <= 0.025, "{:?}", words[0]);
    }

    #[test]
    fn gapless_words_are_warped_onto_speech() {
        // whisper laid six words end to end over 0–6 s; speech is 0.5–2.5 s and 4–5.5 s
        let mut raw: Vec<(i64, i64, String)> = (0..6).map(|i| (i * 100, i * 100 + 100, format!("w{i}"))).collect();
        let islands = vec![50..250, 400..550];
        warp_to_speech(&mut raw, &islands);
        let inside = |t: i64| islands.iter().any(|r| r.start <= t && t <= r.end);
        for w in &raw {
            assert!(inside(w.0) && inside(w.1), "{w:?} not on speech");
        }
        assert!(raw.windows(2).all(|p| p[0].1 <= p[1].0), "order kept: {raw:?}");
        // a run with no speech under it is untouched (the gate handles it)
        let mut silent = vec![(1000, 1100, "x".to_string())];
        warp_to_speech(&mut silent, &islands);
        assert_eq!(silent[0].0, 1000);
    }

    #[test]
    fn sentences_land_on_their_own_pause_separated_islands() {
        // two sentences stamped end to end over 0–6 s; speech: 0–1.5 s, 3.5–5 s
        let words = ["The", "fox", "ran.", "Then", "it", "slept."];
        let mut raw: Vec<(i64, i64, String)> = words.iter().enumerate().map(|(i, w)| (i as i64 * 100, i as i64 * 100 + 100, w.to_string())).collect();
        warp_to_speech(&mut raw, &[0..150, 350..500]);
        assert!(raw[..3].iter().all(|w| w.1 <= 150), "{raw:?}");
        assert!(raw[3..].iter().all(|w| w.0 >= 350), "{raw:?}");
        // a lone noise island between them takes no words
        let g = align_groups(&[4.0, 4.0, 5.0, 5.0, 3.0, 7.0], &[0.0, 0.0, 8.0, 0.0, 0.0, 8.0], &[130.0, 20.0, 150.0]);
        assert_eq!(g, vec![0..3, 3..3, 3..6]);
    }

    #[test]
    fn speech_islands_merge_short_gaps_and_pad() {
        let pcm = synth(4.0, &[(1.0, 1.3, 0.3), (1.42, 1.7, 0.3), (2.5, 2.8, 0.3)]);
        let islands = detect_speech(&pcm);
        assert_eq!(islands.len(), 2, "120 ms gap merged, 800 ms gap kept: {islands:?}");
        let s = |t: Time| t.as_secs_f64();
        assert!((s(islands[0].start) - 0.94).abs() <= 0.025, "60 ms pre-roll: {islands:?}");
        assert!((s(islands[0].end) - 1.78).abs() <= 0.025, "80 ms post-roll: {islands:?}");
        assert!(detect_speech(&synth(2.0, &[])).is_empty(), "noise only → no speech");
    }

    #[test]
    fn cleans_word_segments() {
        let raw = vec![
            (0, 40, " Hello".to_string()),
            (40, 60, ",".to_string()),
            (60, 100, " wor".to_string()),
            (100, 120, "ld".to_string()),
            (120, 130, " [Music]".to_string()),
            (130, 130, " again".to_string()),
            (125, 200, " now".to_string()),
        ];
        let w = words_from_segments(&raw, Time::from_secs(10));
        let texts: Vec<&str> = w.iter().map(|w| w.text.as_str()).collect();
        assert_eq!(texts, ["Hello,", "world", "again", "now"]);
        assert_eq!(w[0].start, Time::from_secs(10));
        assert_eq!(w[0].end, Time::from_millis(10_600));
        assert_eq!(w[1].end, Time::from_millis(11_200));
        // zero-length word stretched, following word pushed after it
        assert!(w[2].end - w[2].start >= Time::from_millis(80));
        assert!(w[3].start >= w[2].end);
        assert!(is_annotation(" (laughs) ") && is_annotation("♪ ♪") && !is_annotation("hi"));
    }

    #[test]
    fn identical_timestamps_are_spread_by_character_weight() {
        // "I", "absolutely", "won" all stamped at 2.00 s; next word at 3.00 s
        let raw = vec![(200, 200, " I".to_string()), (200, 200, " absolutely".to_string()), (200, 200, " won".to_string()), (300, 340, " today".to_string())];
        let w = words_from_segments(&raw, Time::ZERO);
        let ms = |t: Time| (t.as_secs_f64() * 1000.0).round() as i64;
        assert_eq!(ms(w[0].start), 2000);
        assert_eq!(ms(w[2].end), 3000, "run fills the time up to the next word");
        assert_eq!(ms(w[3].start), 3000, "the next word is not pushed later");
        let len = |i: usize| ms(w[i].end) - ms(w[i].start);
        assert!(len(1) > 3 * len(2) && len(2) > len(0), "weighted by characters: {:?}", (len(0), len(1), len(2)));
        assert!(w.windows(2).all(|p| p[0].start < p[1].start && p[0].end <= p[1].start));
        // a run at the very end has no next word: plausible bounded length
        let raw = vec![(500, 500, " gg".to_string()), (500, 500, " wp".to_string())];
        let w = words_from_segments(&raw, Time::ZERO);
        assert!(w[1].end.as_secs_f64() - 5.0 <= 1.2 && w[0].end > w[0].start);
    }
}
