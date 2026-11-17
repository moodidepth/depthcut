//! Exact timeline time base.
//!
//! All times are stored as integer *flicks* (1/705,600,000 s). A flick divides
//! evenly into every common film/video frame rate (23.976, 24, 25, 29.97, 30,
//! 50, 59.94, 60, 120) and audio rate (44.1k, 48k, 96k), so edits never
//! accumulate rounding drift.

use serde::{Deserialize, Serialize};
use std::fmt;
use std::ops::{Add, AddAssign, Neg, Sub, SubAssign};

/// Ticks (flicks) per second.
pub const TICKS_PER_SECOND: i64 = 705_600_000;

/// A point or a duration on the timeline, in flicks.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Time(pub i64);

impl Time {
    pub const ZERO: Time = Time(0);
    /// A very large time used as an open upper bound.
    pub const MAX: Time = Time(i64::MAX / 4);

    pub const fn from_ticks(ticks: i64) -> Self {
        Time(ticks)
    }
    pub const fn ticks(self) -> i64 {
        self.0
    }
    pub const fn from_secs(secs: i64) -> Self {
        Time(secs * TICKS_PER_SECOND)
    }
    pub fn from_secs_f64(secs: f64) -> Self {
        Time((secs * TICKS_PER_SECOND as f64).round() as i64)
    }
    pub const fn from_millis(ms: i64) -> Self {
        Time(ms * (TICKS_PER_SECOND / 1000))
    }
    pub fn as_secs_f64(self) -> f64 {
        self.0 as f64 / TICKS_PER_SECOND as f64
    }
    /// Start time of frame `frames`: the exact rational `frames × den / num`
    /// seconds rounded to the nearest tick. For rates a flick divides evenly
    /// (24…480 fps and the NTSC family) this is exact; for others (e.g.
    /// 165 fps) the sub-tick error never accumulates with the frame count,
    /// unlike `frames × frame_ticks()`.
    pub fn from_frames(frames: i64, rate: FrameRate) -> Self {
        let n = rate.num.max(1) as i128;
        let v = frames as i128 * TICKS_PER_SECOND as i128 * rate.den.max(1) as i128;
        Time((2 * v + n).div_euclid(2 * n) as i64)
    }
    /// Index of the frame that contains this instant (inverse of
    /// [`Time::from_frames`]: the largest `n` with `from_frames(n) <= self`).
    pub fn frames_floor(self, rate: FrameRate) -> i64 {
        let d = TICKS_PER_SECOND as i128 * rate.den.max(1) as i128;
        let n = (self.0 as i128 * rate.num.max(1) as i128).div_euclid(d) as i64;
        if Time::from_frames(n + 1, rate) <= self {
            n + 1
        } else {
            n
        }
    }
    /// Nearest frame index.
    pub fn frames_round(self, rate: FrameRate) -> i64 {
        let d = TICKS_PER_SECOND as i128 * rate.den.max(1) as i128;
        (2 * self.0 as i128 * rate.num.max(1) as i128 + d).div_euclid(2 * d) as i64
    }
    /// Time of audio sample `samples` at `rate` Hz (exact for 44.1/48/96 kHz).
    pub fn from_samples(samples: i64, rate: u32) -> Self {
        let r = rate.max(1) as i128;
        Time((2 * samples as i128 * TICKS_PER_SECOND as i128 + r).div_euclid(2 * r) as i64)
    }
    /// Nearest audio sample index at `rate` Hz, in integer arithmetic.
    pub fn samples_round(self, rate: u32) -> i64 {
        let d = TICKS_PER_SECOND as i128;
        (2 * self.0 as i128 * rate.max(1) as i128 + d).div_euclid(2 * d) as i64
    }
    /// Quantise to the nearest frame boundary.
    pub fn snap_to_frame(self, rate: FrameRate) -> Time {
        Time::from_frames(self.frames_round(rate), rate)
    }
    pub fn is_zero(self) -> bool {
        self.0 == 0
    }
    pub fn is_positive(self) -> bool {
        self.0 > 0
    }
    pub fn is_negative(self) -> bool {
        self.0 < 0
    }
    pub fn abs(self) -> Time {
        Time(self.0.abs())
    }
    pub fn mul_f64(self, k: f64) -> Time {
        Time((self.0 as f64 * k).round() as i64)
    }
    pub fn div_i64(self, d: i64) -> Time {
        Time(self.0 / d)
    }

    /// Format as an FCPXML rational time string, e.g. `1001/30000s` or `5s`.
    pub fn to_rational_string(self) -> String {
        if self.0 == 0 {
            return "0s".to_string();
        }
        let g = gcd(self.0.unsigned_abs(), TICKS_PER_SECOND as u64) as i64;
        let n = self.0 / g;
        let d = TICKS_PER_SECOND / g;
        if d == 1 {
            format!("{n}s")
        } else {
            format!("{n}/{d}s")
        }
    }

    /// Parse an FCPXML rational time (`"1001/30000s"`, `"5s"`, `"2.5s"`).
    pub fn parse_rational(s: &str) -> Option<Time> {
        let s = s.trim();
        let s = s.strip_suffix('s').unwrap_or(s);
        if s.is_empty() {
            return None;
        }
        if let Some((n, d)) = s.split_once('/') {
            let n: i128 = n.trim().parse().ok()?;
            let d: i128 = d.trim().parse().ok()?;
            if d == 0 {
                return None;
            }
            let t = n * TICKS_PER_SECOND as i128;
            // round half away from zero
            let q = if (t >= 0) == (d > 0) {
                (t + d / 2) / d
            } else {
                (t - d / 2) / d
            };
            Some(Time(q as i64))
        } else if let Ok(i) = s.parse::<i64>() {
            Some(Time::from_secs(i))
        } else {
            s.parse::<f64>().ok().map(Time::from_secs_f64)
        }
    }
}

impl fmt::Display for Time {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:.3}s", self.as_secs_f64())
    }
}

impl Add for Time {
    type Output = Time;
    fn add(self, rhs: Time) -> Time {
        Time(self.0 + rhs.0)
    }
}
impl Sub for Time {
    type Output = Time;
    fn sub(self, rhs: Time) -> Time {
        Time(self.0 - rhs.0)
    }
}
impl Neg for Time {
    type Output = Time;
    fn neg(self) -> Time {
        Time(-self.0)
    }
}
impl AddAssign for Time {
    fn add_assign(&mut self, rhs: Time) {
        self.0 += rhs.0;
    }
}
impl SubAssign for Time {
    fn sub_assign(&mut self, rhs: Time) {
        self.0 -= rhs.0;
    }
}
impl std::iter::Sum for Time {
    fn sum<I: Iterator<Item = Time>>(iter: I) -> Time {
        iter.fold(Time::ZERO, |a, b| a + b)
    }
}

pub(crate) fn gcd(mut a: u64, mut b: u64) -> u64 {
    while b != 0 {
        let t = a % b;
        a = b;
        b = t;
    }
    a.max(1)
}

/// A rational frame rate (`num / den` frames per second).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct FrameRate {
    pub num: u32,
    pub den: u32,
}

impl Default for FrameRate {
    fn default() -> Self {
        FrameRate::FPS_30
    }
}

impl FrameRate {
    pub const FPS_23_976: FrameRate = FrameRate { num: 24000, den: 1001 };
    pub const FPS_24: FrameRate = FrameRate { num: 24, den: 1 };
    pub const FPS_25: FrameRate = FrameRate { num: 25, den: 1 };
    pub const FPS_29_97: FrameRate = FrameRate { num: 30000, den: 1001 };
    pub const FPS_30: FrameRate = FrameRate { num: 30, den: 1 };
    pub const FPS_50: FrameRate = FrameRate { num: 50, den: 1 };
    pub const FPS_59_94: FrameRate = FrameRate { num: 60000, den: 1001 };
    pub const FPS_60: FrameRate = FrameRate { num: 60, den: 1 };
    pub const FPS_120: FrameRate = FrameRate { num: 120, den: 1 };
    pub const FPS_240: FrameRate = FrameRate { num: 240, den: 1 };
    pub const FPS_480: FrameRate = FrameRate { num: 480, den: 1 };

    pub const PRESETS: [FrameRate; 11] = [
        Self::FPS_23_976,
        Self::FPS_24,
        Self::FPS_25,
        Self::FPS_29_97,
        Self::FPS_30,
        Self::FPS_50,
        Self::FPS_59_94,
        Self::FPS_60,
        Self::FPS_120,
        Self::FPS_240,
        Self::FPS_480,
    ];

    pub const fn new(num: u32, den: u32) -> Self {
        FrameRate { num, den }
    }
    pub fn fps(self) -> f64 {
        self.num as f64 / self.den.max(1) as f64
    }
    /// Integer timecode base (e.g. 30 for 29.97).
    pub fn nominal(self) -> i64 {
        (self.fps().round() as i64).max(1)
    }
    /// Duration of a single frame in ticks.
    pub fn frame_ticks(self) -> i64 {
        let n = self.num.max(1) as i128;
        let d = self.den.max(1) as i128;
        ((TICKS_PER_SECOND as i128 * d + n / 2) / n) as i64
    }
    pub fn frame_duration(self) -> Time {
        Time(self.frame_ticks())
    }
    /// Best-effort conversion from a floating-point rate.
    pub fn from_f64(fps: f64) -> FrameRate {
        for p in Self::PRESETS {
            if (p.fps() - fps).abs() < 0.005 {
                return p;
            }
        }
        FrameRate::new((fps * 1000.0).round() as u32, 1000)
    }
    pub fn label(self) -> String {
        if self.den == 1 {
            format!("{}p", self.num)
        } else {
            format!("{:.2}p", self.fps())
        }
    }
}

/// Format a time as non-drop-frame SMPTE timecode `HH:MM:SS:FF`.
pub fn format_timecode(t: Time, rate: FrameRate) -> String {
    let neg = t.0 < 0;
    let frames = t.abs().frames_floor(rate);
    let n = rate.nominal();
    let ff = frames % n;
    let total_s = frames / n;
    let ss = total_s % 60;
    let mm = (total_s / 60) % 60;
    let hh = total_s / 3600;
    format!("{}{:02}:{:02}:{:02}:{:02}", if neg { "-" } else { "" }, hh, mm, ss, ff)
}

/// Parse `HH:MM:SS:FF` (or fewer leading fields) into a time.
pub fn parse_timecode(s: &str, rate: FrameRate) -> Option<Time> {
    let parts: Vec<i64> = s
        .trim()
        .split([':', ';'])
        .map(|p| p.trim().parse::<i64>())
        .collect::<Result<_, _>>()
        .ok()?;
    if parts.is_empty() || parts.len() > 4 {
        return None;
    }
    let mut padded = [0i64; 4];
    padded[4 - parts.len()..].copy_from_slice(&parts);
    let [hh, mm, ss, ff] = padded;
    let n = rate.nominal();
    let frames = ((hh * 60 + mm) * 60 + ss) * n + ff;
    Some(Time::from_frames(frames, rate))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_ticks_are_exact_for_common_rates() {
        for r in FrameRate::PRESETS {
            let ticks = r.frame_ticks();
            assert_eq!(ticks as i128 * r.num as i128, TICKS_PER_SECOND as i128 * r.den as i128, "{r:?}");
        }
    }

    #[test]
    fn rational_round_trip() {
        let t = Time::from_frames(1, FrameRate::FPS_29_97);
        assert_eq!(t.to_rational_string(), "1001/30000s");
        assert_eq!(Time::parse_rational("1001/30000s"), Some(t));
        assert_eq!(Time::parse_rational("5s"), Some(Time::from_secs(5)));
        assert_eq!(Time::from_secs(5).to_rational_string(), "5s");
        assert_eq!(Time::parse_rational("2.5s"), Some(Time::from_millis(2500)));
    }

    #[test]
    fn timecode_formatting() {
        let r = FrameRate::FPS_30;
        let t = Time::from_frames(30 * 3661 + 12, r);
        assert_eq!(format_timecode(t, r), "01:01:01:12");
        assert_eq!(parse_timecode("01:01:01:12", r), Some(t));
    }
}
