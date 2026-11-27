//! Downsampled mono min/max audio peaks for timeline waveforms.
//!
//! One bucket covers [`BUCKET_SAMPLES`] audio frames. Peaks are quantised to
//! `i8` (≈1.4 MB per hour of 48 kHz audio), carry a mip pyramid so any zoom
//! level is drawn in O(1) per pixel column, and are cached on disk in
//! `%LOCALAPPDATA%/Depthcut/waveforms/`, keyed by source path + file size +
//! mtime, so re-importing a VOD shows its waveform instantly.

use crate::frame::AudioBuffer;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

/// Audio frames per level-0 bucket.
pub const BUCKET_SAMPLES: usize = 256;
/// Reduction factor between mip levels.
const MIP_FACTOR: usize = 8;
/// A query never folds more than this many buckets (except at the top level).
const MAX_FOLD: usize = 16;
const MAGIC: &[u8; 4] = b"RCPK";
const VERSION: u8 = 1;

/// Mono min/max peak pyramid. An empty buffer is valid (video-only clips).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PeakBuffer {
    pub sample_rate: u32,
    /// `levels[0]` is the full-resolution `(min, max)` list; each further
    /// level merges [`MIP_FACTOR`] buckets of the previous one.
    levels: Vec<Vec<(i8, i8)>>,
}

fn quantise(v: f32) -> i8 {
    (v.clamp(-1.0, 1.0) * 127.0).round() as i8
}

impl PeakBuffer {
    /// No audio: draws nothing, never errors.
    pub fn empty() -> Self {
        PeakBuffer::default()
    }

    /// Reduce interleaved audio to mono min/max buckets.
    pub fn from_audio(buf: &AudioBuffer) -> Self {
        let ch = buf.channels.max(1) as usize;
        let base: Vec<(i8, i8)> = buf
            .samples
            .chunks(BUCKET_SAMPLES * ch)
            .map(|c| {
                let (mut lo, mut hi) = (f32::MAX, f32::MIN);
                for f in c.chunks(ch) {
                    let m = f.iter().sum::<f32>() / f.len() as f32;
                    lo = lo.min(m);
                    hi = hi.max(m);
                }
                (quantise(lo), quantise(hi))
            })
            .collect();
        Self::from_base(buf.sample_rate, base)
    }

    fn from_base(sample_rate: u32, base: Vec<(i8, i8)>) -> Self {
        if base.is_empty() {
            return PeakBuffer { sample_rate, levels: Vec::new() };
        }
        let mut levels = vec![base];
        while levels.last().is_some_and(|l| l.len() > MAX_FOLD) {
            let next: Vec<(i8, i8)> = levels[levels.len() - 1].chunks(MIP_FACTOR).map(|c| c.iter().fold((i8::MAX, i8::MIN), |(lo, hi), (a, b)| (lo.min(*a), hi.max(*b)))).collect();
            levels.push(next);
        }
        PeakBuffer { sample_rate, levels }
    }

    pub fn is_empty(&self) -> bool {
        self.levels.is_empty()
    }

    /// Number of full-resolution buckets.
    pub fn len(&self) -> usize {
        self.levels.first().map_or(0, Vec::len)
    }

    /// Full-resolution `(min, max)` buckets.
    pub fn buckets(&self) -> &[(i8, i8)] {
        self.levels.first().map_or(&[], Vec::as_slice)
    }

    pub fn buckets_per_second(&self) -> f64 {
        self.sample_rate.max(1) as f64 / BUCKET_SAMPLES as f64
    }

    /// `(min, max)` in -1..1 over source seconds `[t0, t1)`. `(0, 0)` when
    /// the range lies outside the audio or the buffer is empty.
    pub fn range(&self, t0: f64, t1: f64) -> (f32, f32) {
        let n = self.len();
        if n == 0 || !t0.is_finite() || !t1.is_finite() {
            return (0.0, 0.0);
        }
        let bps = self.buckets_per_second();
        let (t0, t1) = if t0 <= t1 { (t0, t1) } else { (t1, t0) };
        let mut a = (t0 * bps).floor().max(0.0) as usize;
        if a >= n {
            return (0.0, 0.0);
        }
        let mut b = ((t1 * bps).ceil().max(0.0) as usize).clamp(a + 1, n);
        let mut level = 0;
        while b - a > MAX_FOLD && level + 1 < self.levels.len() {
            level += 1;
            a /= MIP_FACTOR;
            b = b.div_ceil(MIP_FACTOR);
        }
        let l = &self.levels[level];
        let b = b.min(l.len());
        let a = a.min(b.saturating_sub(1));
        let (lo, hi) = l[a..b].iter().fold((i8::MAX, i8::MIN), |(lo, hi), (x, y)| (lo.min(*x), hi.max(*y)));
        (lo as f32 / 127.0, hi as f32 / 127.0)
    }

    /// Source times (seconds) of loud onsets — gunshots, kicks, impacts —
    /// usable as snap points. A bucket is a transient when it is loud in
    /// absolute terms and several times louder than the preceding ~200 ms.
    pub fn transients(&self) -> Vec<f64> {
        const WINDOW: usize = 40;
        const MIN_LEVEL: f32 = 0.22;
        const RATIO: f32 = 3.0;
        let bps = self.buckets_per_second();
        let min_gap = (0.12 * bps).ceil() as usize;
        let amp = |&(lo, hi): &(i8, i8)| (lo as f32).abs().max((hi as f32).abs()) / 127.0;
        let base = self.buckets();
        let mut out = Vec::new();
        let mut sum = 0.0f32;
        let mut last: Option<usize> = None;
        for (i, bucket) in base.iter().enumerate() {
            let v = amp(bucket);
            let count = i.min(WINDOW);
            let avg = if count == 0 { 0.0 } else { sum / count as f32 };
            if v >= MIN_LEVEL && v >= RATIO * (avg + 0.02) && last.is_none_or(|l| i - l >= min_gap) {
                out.push(i as f64 / bps);
                last = Some(i);
            }
            sum += v;
            if i >= WINDOW {
                sum -= amp(&base[i - WINDOW]);
            }
        }
        out
    }

    /// Serialise the full-resolution level (the pyramid is rebuilt on load).
    pub fn to_bytes(&self) -> Vec<u8> {
        let base = self.buckets();
        let mut out = Vec::with_capacity(21 + base.len() * 2);
        out.extend_from_slice(MAGIC);
        out.push(VERSION);
        out.extend_from_slice(&self.sample_rate.to_le_bytes());
        out.extend_from_slice(&(BUCKET_SAMPLES as u32).to_le_bytes());
        out.extend_from_slice(&(base.len() as u64).to_le_bytes());
        for (lo, hi) in base {
            out.push(*lo as u8);
            out.push(*hi as u8);
        }
        out
    }

    /// Parse [`PeakBuffer::to_bytes`] output; `None` for truncated, corrupt
    /// or incompatible data (the caller then recomputes).
    pub fn from_bytes(bytes: &[u8]) -> Option<Self> {
        let head = bytes.get(..21)?;
        if &head[..4] != MAGIC || head[4] != VERSION {
            return None;
        }
        let sample_rate = u32::from_le_bytes(head[5..9].try_into().ok()?);
        let bucket = u32::from_le_bytes(head[9..13].try_into().ok()?);
        let count = usize::try_from(u64::from_le_bytes(head[13..21].try_into().ok()?)).ok()?;
        let body = &bytes[21..];
        if bucket as usize != BUCKET_SAMPLES || sample_rate == 0 || body.len() != count.checked_mul(2)? {
            return None;
        }
        let base = body.as_chunks::<2>().0.iter().map(|p| (p[0] as i8, p[1] as i8)).collect();
        Some(Self::from_base(sample_rate, base))
    }
}

/// `%LOCALAPPDATA%/Depthcut/waveforms/` (or the platform equivalent).
pub fn cache_dir() -> PathBuf {
    crate::paths::local_data_dir().join("waveforms")
}

fn fnv1a(bytes: &[u8], mut h: u64) -> u64 {
    for b in bytes {
        h ^= *b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// Cache file name for `source`: stable hash of path + size + mtime, so an
/// edited or replaced file never reuses stale peaks. `None` when the file
/// cannot be inspected (synthetic/demo assets, offline media).
pub fn cache_file_name(source: &Path) -> Option<String> {
    let meta = std::fs::metadata(source).ok()?;
    if !meta.is_file() {
        return None;
    }
    let mtime = meta.modified().ok()?.duration_since(std::time::UNIX_EPOCH).ok()?.as_nanos();
    let mut h = fnv1a(source.to_string_lossy().as_bytes(), 0xcbf2_9ce4_8422_2325);
    h = fnv1a(&meta.len().to_le_bytes(), h);
    h = fnv1a(&mtime.to_le_bytes(), h);
    Some(format!("{h:016x}.peaks"))
}

/// Cached peaks for `source`, if present and valid.
pub fn load_cached(source: &Path) -> Option<PeakBuffer> {
    load_cached_in(&cache_dir(), source)
}

pub fn load_cached_in(dir: &Path, source: &Path) -> Option<PeakBuffer> {
    let mut bytes = Vec::new();
    std::fs::File::open(dir.join(cache_file_name(source)?)).ok()?.read_to_end(&mut bytes).ok()?;
    PeakBuffer::from_bytes(&bytes)
}

/// Write `peaks` to the cache (temp file + rename, so a crash never leaves a
/// half-written entry). Uncacheable sources are skipped silently.
pub fn store_cached(source: &Path, peaks: &PeakBuffer) -> io::Result<()> {
    store_cached_in(&cache_dir(), source, peaks)
}

pub fn store_cached_in(dir: &Path, source: &Path, peaks: &PeakBuffer) -> io::Result<()> {
    let Some(name) = cache_file_name(source) else { return Ok(()) };
    std::fs::create_dir_all(dir)?;
    let tmp = dir.join(format!("{name}.{}.tmp", std::process::id()));
    let res = std::fs::File::create(&tmp).and_then(|mut f| f.write_all(&peaks.to_bytes())).and_then(|_| std::fs::rename(&tmp, dir.join(name)));
    if res.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    res
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone(secs: f64, rate: u32, channels: u16, amp: f32) -> AudioBuffer {
        let n = (secs * rate as f64) as usize;
        let mut s = Vec::with_capacity(n * channels as usize);
        for i in 0..n {
            let v = (i as f32 * 0.3).sin() * amp;
            for _ in 0..channels {
                s.push(v);
            }
        }
        AudioBuffer::new(rate, channels, s)
    }

    #[test]
    fn empty_audio_gives_empty_peaks() {
        let p = PeakBuffer::from_audio(&AudioBuffer::new(48_000, 2, Vec::new()));
        assert!(p.is_empty());
        assert_eq!(p.range(0.0, 10.0), (0.0, 0.0));
        assert!(p.transients().is_empty());
        assert_eq!(PeakBuffer::from_bytes(&p.to_bytes()), Some(p));
        assert!(PeakBuffer::empty().is_empty());
    }

    #[test]
    fn one_bucket_per_256_frames_with_partial_tail() {
        let p = PeakBuffer::from_audio(&tone(1.0, 48_000, 2, 0.5));
        assert_eq!(p.len(), 48_000usize.div_ceil(BUCKET_SAMPLES));
        let (lo, hi) = p.range(0.0, 1.0);
        assert!((hi - 0.5).abs() < 0.02 && (lo + 0.5).abs() < 0.02, "{lo} {hi}");
    }

    #[test]
    fn mip_query_matches_brute_force() {
        let mut buf = tone(20.0, 48_000, 1, 0.3);
        buf.samples[48_000 * 7 + 11] = 0.9;
        buf.samples[48_000 * 13 + 5] = -0.8;
        let p = PeakBuffer::from_audio(&buf);
        let bps = p.buckets_per_second();
        for (t0, t1) in [(0.0, 20.0), (6.5, 7.5), (12.99, 13.01), (7.0, 7.004), (19.9, 30.0), (3.0, 3.0)] {
            let a = (t0 * bps).floor() as usize;
            let b = ((t1 * bps).ceil() as usize).clamp(a + 1, p.len());
            let (lo, hi) = p.buckets()[a..b].iter().fold((i8::MAX, i8::MIN), |(l, h), (x, y)| (l.min(*x), h.max(*y)));
            let (qlo, qhi) = p.range(t0, t1);
            // mip levels may widen the window slightly, never narrow it
            assert!(qlo <= lo as f32 / 127.0 + 1e-6 && qhi >= hi as f32 / 127.0 - 1e-6, "{t0}..{t1}");
        }
        assert!(p.range(6.5, 7.5).1 > 0.85);
        assert!(p.range(12.5, 13.5).0 < -0.75);
        assert_eq!(p.range(25.0, 26.0), (0.0, 0.0));
    }

    #[test]
    fn transients_find_gunshots_in_quiet_audio() {
        let rate = 48_000u32;
        let mut buf = tone(6.0, rate, 1, 0.02);
        for shot in [1.0f64, 3.5, 5.0] {
            let at = (shot * rate as f64) as usize;
            for i in 0..2_000 {
                buf.samples[at + i] = 0.9 * (1.0 - i as f32 / 2_000.0);
            }
        }
        let t = PeakBuffer::from_audio(&buf).transients();
        assert_eq!(t.len(), 3, "{t:?}");
        for (found, want) in t.iter().zip([1.0, 3.5, 5.0]) {
            assert!((found - want).abs() < 0.02, "{found} vs {want}");
        }
        // steady loud audio has no onsets after the first
        assert!(PeakBuffer::from_audio(&tone(3.0, rate, 1, 0.8)).transients().len() <= 1);
    }

    #[test]
    fn bytes_round_trip_and_reject_corruption() {
        let p = PeakBuffer::from_audio(&tone(2.0, 44_100, 2, 0.7));
        let bytes = p.to_bytes();
        assert_eq!(PeakBuffer::from_bytes(&bytes).as_ref(), Some(&p));
        assert!(PeakBuffer::from_bytes(&bytes[..bytes.len() - 1]).is_none());
        assert!(PeakBuffer::from_bytes(b"nope").is_none());
        let mut bad = bytes.clone();
        bad[4] = 99;
        assert!(PeakBuffer::from_bytes(&bad).is_none());
    }

    #[test]
    fn disk_cache_is_keyed_by_path_size_and_mtime() {
        let dir = std::env::temp_dir().join(format!("rcp-peaks-test-{}", std::process::id()));
        let cache = dir.join("cache");
        std::fs::create_dir_all(&dir).unwrap();
        let src = dir.join("clip.wav");
        std::fs::write(&src, b"first").unwrap();
        let p = PeakBuffer::from_audio(&tone(1.0, 48_000, 2, 0.4));
        assert!(load_cached_in(&cache, &src).is_none());
        store_cached_in(&cache, &src, &p).unwrap();
        assert_eq!(load_cached_in(&cache, &src), Some(p.clone()));
        let key = cache_file_name(&src).unwrap();
        // changing the file invalidates the entry
        std::fs::write(&src, b"second, longer").unwrap();
        assert_ne!(cache_file_name(&src).unwrap(), key);
        assert!(load_cached_in(&cache, &src).is_none());
        // missing sources are skipped without error
        let missing = dir.join("synthetic://demo");
        assert!(cache_file_name(&missing).is_none());
        store_cached_in(&cache, &missing, &p).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }
}
