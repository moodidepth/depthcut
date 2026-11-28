//! Kill detector for gameplay recordings.
//!
//! Gunfire is broadband: on a spectrogram each shot is a vertical streak.
//! Games confirm a kill with a chime, a narrow tone that holds for a few
//! hundred milliseconds (Valorant's sits around 2–6 kHz depending on the
//! weapon skin). So a kill is a sustained tone in 1.5–10 kHz that follows
//! gunfire. The tone must peak 15 dB over its spectral floor and is timed
//! while it stays above 11 dB, so a chime with a loud attack and a quieter
//! ring counts at its full length. It must be pure: more than a handful of
//! other bins standing out at once mean music, voice or an ability (a chime
//! with a loud second note looks the same and is missed). It must last at
//! least 0.24 s, which rejects the short
//! periodic spike/bomb beeps, and must have at least two gunshots in the
//! 1.5 s before it, which rejects ability and UI sounds. A single shot (a
//! Sheriff/Vandal one-tap, an Operator flick) is enough when the chime is
//! clearer: 2.5 dB more prominent and 20 ms longer. A tone with no gunfire
//! before it is never a kill. The marker lands on the last shot before the
//! chime, i.e. the killing shot.
//!
//! Scored against Medal's kill events on 24 clips of one player (69 kills,
//! several weapon skins): it finds 38 (55 %), and 38 of its 60 marks are
//! real kills (63 %); the old single-threshold version found 27 at 50 %.
//! Misses are mostly chimes fainter than 15 dB, very short ones and kills
//! with no audible chime; false marks are pure tones after gunfire (other UI
//! sounds). Other players' skins and other games are untested, so prefer
//! Medal's own events when a clip has them. An earlier version marked every
//! loud transient, about 20 "kills" a minute.

use nle_core::Time;
use rustfft::num_complex::Complex32;
use rustfft::FftPlanner;

#[derive(Clone, Debug, PartialEq)]
pub struct KillPeak {
    pub time: Time,
    pub score: f32,
    pub is_multikill: bool,
    pub label: String,
}

/// Chime band.
const TONE_LO_HZ: f32 = 1_500.0;
const TONE_HI_HZ: f32 = 10_000.0;
/// Bins either side of a bin that form its local spectral floor (median).
const FLOOR_HALF: usize = 12;
/// A tone must peak this many dB above the floor ...
const TONE_DB: f32 = 15.0;
/// ... and its length is measured while it stays above this many dB ...
const TONE_TAIL_DB: f32 = 11.0;
/// ... for this many consecutive 10 ms hops (120 ms) to count at all ...
const HOLD_HOPS: usize = 12;
/// ... and the whole tone must last this long to be a kill chime.
const MIN_TONE_SECS: f64 = 0.24;
/// A shot: this fraction of all bins above 140 Hz jumps by 12 dB in one hop.
const SHOT_FRACTION: f32 = 0.25;
const SHOT_JUMP_DB: f32 = 12.0;
/// Gunshots needed in this window before the chime.
const SHOT_LOOKBACK_SECS: f64 = 1.5;
const MIN_SHOTS: usize = 2;
/// A lone shot counts when its chime beats the tone threshold by this much ...
const ONE_TAP_EXTRA_DB: f32 = 2.5;
/// ... and outlasts the minimum tone by this much.
const ONE_TAP_EXTRA_SECS: f64 = 0.02;
/// A chime is one pure note (plus its neighbour bins): more than this many
/// other chime-band bins standing out by `BUSY_DB` over the tone's first
/// `BUSY_HOPS` hops make it a dense sound (music, an ability, voice), not a chime.
const BUSY_MAX_BINS: usize = 5;
const BUSY_DB: f32 = 10.0;
const BUSY_NEAR_BINS: usize = 4;
const BUSY_HOPS: usize = 20;
/// The killing shot is the last shot at most this long before the chime.
const KILL_SHOT_SECS: f64 = 1.0;
/// Kills closer than this to the previous one form a multi-kill.
const STREAK_SECS: f64 = 6.0;

/// Find kills in mono or interleaved PCM. `sensitivity` in `0.2..2.0`
/// (default `1.0`; higher finds fainter chimes and more false hits).
pub fn analyze_kills_from_samples(samples: &[f32], channels: usize, rate: u32, sensitivity: f32) -> Vec<KillPeak> {
    let ch = channels.max(1);
    let mono: Vec<f32> = samples.chunks_exact(ch).map(|f| f.iter().sum::<f32>() / ch as f32).collect();
    if rate < 22_050 {
        return Vec::new();
    }
    let n_fft = if rate > 64_000 { 4096 } else { 2048 };
    let hop = (rate / 100) as usize;
    if mono.len() < n_fft + hop * (HOLD_HOPS + 2) {
        return Vec::new();
    }
    let sens = sensitivity.clamp(0.2, 2.0);
    let tone_db = TONE_DB / sens;
    let tail_db = TONE_TAIL_DB / sens;
    let min_tone = MIN_TONE_SECS / sens.sqrt() as f64;
    let hop_secs = hop as f64 / rate as f64;

    let bin_hz = rate as f32 / n_fft as f32;
    let half = n_fft / 2;
    let lo = ((TONE_LO_HZ / bin_hz).ceil() as usize).max(1);
    let hi = ((TONE_HI_HZ / bin_hz) as usize).min(half);
    let band = hi - lo;
    let k0 = ((140.0 / bin_hz).round() as usize).max(1);

    let mut planner = FftPlanner::<f32>::new();
    let fft = planner.plan_fft_forward(n_fft);
    let window: Vec<f32> = (0..n_fft).map(|i| 0.5 - 0.5 * (std::f32::consts::TAU * i as f32 / (n_fft - 1) as f32).cos()).collect();
    let frames = (mono.len() - n_fft) / hop + 1;

    let mut buf = vec![Complex32::new(0.0, 0.0); n_fft];
    let mut db = vec![0.0f32; half + 1];
    let mut prev_db: Option<Vec<f32>> = None;
    let mut prom = vec![0.0f32; band];
    let mut scratch = Vec::with_capacity(2 * FLOOR_HALF + 1);
    // Ring of the last HOLD_HOPS frames' prominence (widened by ±1 bin).
    let mut ring: Vec<Vec<f32>> = vec![vec![0.0f32; band]; HOLD_HOPS];
    let mut score = vec![0.0f32; frames];
    let mut score_bin = vec![0u16; frames];
    // per frame, which chime-band bins hold above BUSY_DB (one bit each)
    let words = band.div_ceil(64);
    let mut busy = vec![0u64; frames * words];
    let mut shot = vec![false; frames];

    for f in 0..frames {
        let s = &mono[f * hop..f * hop + n_fft];
        for (b, (x, w)) in buf.iter_mut().zip(s.iter().zip(&window)) {
            *b = Complex32::new(x * w, 0.0);
        }
        fft.process(&mut buf);
        for (d, c) in db.iter_mut().zip(&buf) {
            *d = 20.0 * (c.norm() + 1e-9).log10();
        }

        if let Some(prev) = &prev_db {
            let jumps = (k0..=half).filter(|&k| db[k] - prev[k] > SHOT_JUMP_DB).count();
            shot[f] = jumps as f32 > SHOT_FRACTION * (half + 1 - k0) as f32;
        }
        match &mut prev_db {
            Some(p) => p.copy_from_slice(&db),
            None => prev_db = Some(db.clone()),
        }

        // Prominence of each chime-band bin over the median of its neighbours.
        for (i, p) in prom.iter_mut().enumerate() {
            let k = lo + i;
            scratch.clear();
            // Mirror at the band edges so the floor is always 25 in-band bins.
            for o in -(FLOOR_HALF as isize)..=FLOOR_HALF as isize {
                let mut j = i as isize + o;
                if j < 0 {
                    j = -j - 1;
                } else if j >= band as isize {
                    j = 2 * band as isize - j - 1;
                }
                scratch.push(db[lo + j as usize]);
            }
            let mid = scratch.len() / 2;
            let (_, median, _) = scratch.select_nth_unstable_by(mid, f32::total_cmp);
            *p = db[k] - *median;
        }
        // Allow ±1 bin of pitch drift, then keep the frame in the ring.
        let slot = &mut ring[f % HOLD_HOPS];
        for i in 0..band {
            let a = if i > 0 { prom[i - 1] } else { f32::MIN };
            let b = prom.get(i + 1).copied().unwrap_or(f32::MIN);
            slot[i] = prom[i].max(a).max(b);
        }
        // Once HOLD_HOPS frames are in, a tone held across all of them scores
        // at the oldest frame.
        if f + 1 >= HOLD_HOPS {
            let at = f + 1 - HOLD_HOPS;
            let (mut best, mut best_bin) = (f32::MIN, 0);
            for i in 0..band {
                let held = ring.iter().map(|r| r[i]).fold(f32::MAX, f32::min);
                if held > best {
                    (best, best_bin) = (held, i);
                }
                if held > BUSY_DB {
                    busy[at * words + i / 64] |= 1 << (i % 64);
                }
            }
            score[at] = best;
            score_bin[at] = best_bin as u16;
        }
    }

    // Tones: runs of held prominence that peak above `tone_db` and are timed
    // while above `tail_db`; a kill needs a long, pure tone after gunfire.
    let lookback = (SHOT_LOOKBACK_SECS / hop_secs).round() as usize;
    let kill_shot = (KILL_SHOT_SECS / hop_secs).round() as usize;
    let mut kills: Vec<(f64, f32)> = Vec::new();
    let mut last_tone = f64::MIN;
    let mut i = 0;
    while i < frames {
        if score[i] <= tail_db {
            i += 1;
            continue;
        }
        let start = i;
        let (mut peak, mut peak_bin) = (0.0f32, 0usize);
        while i < frames && score[i] > tail_db {
            if score[i] > peak {
                (peak, peak_bin) = (score[i], score_bin[i] as usize);
            }
            i += 1;
        }
        if peak < tone_db {
            continue;
        }
        // other bins that stood out while the tone began
        let mut seen = vec![0u64; words];
        for f in start..i.min(start + BUSY_HOPS) {
            for (s, b) in seen.iter_mut().zip(&busy[f * words..(f + 1) * words]) {
                *s |= b;
            }
        }
        let others = (0..band).filter(|&b| b.abs_diff(peak_bin) > BUSY_NEAR_BINS && seen[b / 64] >> (b % 64) & 1 == 1).count();
        if others > BUSY_MAX_BINS {
            continue;
        }
        let secs = (i - start) as f64 * hop_secs + HOLD_HOPS as f64 * hop_secs;
        // count onsets: a shot sliding into the window can jump on two hops in a row
        let from = start.saturating_sub(lookback);
        let shots = (from..start).filter(|&j| shot[j] && (j == from || !shot[j - 1])).count();
        let tone_at = start as f64 * hop_secs;
        let clear = peak >= tone_db + ONE_TAP_EXTRA_DB && secs >= min_tone + ONE_TAP_EXTRA_SECS;
        let enough = shots >= MIN_SHOTS || (shots == 1 && clear);
        if secs < min_tone || !enough || tone_at - last_tone < 0.6 {
            continue;
        }
        last_tone = tone_at;
        let shot_at = (start.saturating_sub(kill_shot)..start).rev().find(|&j| shot[j]).map_or(tone_at, |j| j as f64 * hop_secs);
        kills.push((shot_at, peak));
    }

    let mut out = Vec::with_capacity(kills.len());
    let mut streak = 0u32;
    let mut prev = f64::MIN;
    for (secs, score) in kills {
        streak = if secs - prev <= STREAK_SECS { streak + 1 } else { 1 };
        prev = secs;
        let label = match streak {
            1 => "Kill".to_string(),
            2 => "Double Kill".to_string(),
            3 => "Triple Kill".to_string(),
            4 => "Quadra Kill".to_string(),
            n => format!("{n} Kills"),
        };
        out.push(KillPeak { time: Time::from_secs_f64(secs), score, is_multikill: streak >= 2, label });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const RATE: u32 = 48_000;

    /// Deterministic background hiss.
    fn noise_bed(secs: f64) -> Vec<f32> {
        let mut x = 0x9e37_79b9u32;
        (0..(RATE as f64 * secs) as usize)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 17;
                x ^= x << 5;
                (x as f32 / u32::MAX as f32 - 0.5) * 0.004
            })
            .collect()
    }

    /// A gunshot: 25 ms of loud broadband noise with a hard onset.
    fn shot(s: &mut [f32], at: f64) {
        let start = (at * RATE as f64) as usize;
        let mut x = 0x1234_5678u32 ^ start as u32;
        for i in 0..RATE as usize / 40 {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            let env = (-(i as f32) / (RATE as f32 * 0.008)).exp();
            s[start + i] += (x as f32 / u32::MAX as f32 - 0.5) * 1.2 * env;
        }
    }

    fn tone(s: &mut [f32], at: f64, hz: f32, secs: f64) {
        tone_amp(s, at, hz, secs, 0.15);
    }

    fn tone_amp(s: &mut [f32], at: f64, hz: f32, secs: f64, amp: f32) {
        let start = (at * RATE as f64) as usize;
        let n = (secs * RATE as f64) as usize;
        let fade = RATE as usize / 200; // 5 ms attack/release, as real chimes have
        for i in 0..n {
            let env = (i.min(n - i) as f32 / fade as f32).min(1.0);
            s[start + i] += (std::f32::consts::TAU * hz * i as f32 / RATE as f32).sin() * amp * env;
        }
    }

    #[test]
    fn a_chime_after_gunfire_is_a_kill_on_the_last_shot() {
        let mut s = noise_bed(9.0);
        for at in [1.6, 1.75, 1.9] {
            shot(&mut s, at);
        }
        tone(&mut s, 2.1, 5_320.0, 0.4); // kill chime
        // Spike beeps: short, periodic, no gunfire. Not kills.
        for k in 0..4 {
            tone(&mut s, 4.0 + k as f64, 7_968.0, 0.1);
        }
        // A long UI/ability tone without gunfire. Not a kill.
        tone(&mut s, 8.0, 3_000.0, 0.45);
        let peaks = analyze_kills_from_samples(&s, 1, RATE, 1.0);
        assert_eq!(peaks.len(), 1, "expected one kill, got {peaks:?}");
        assert!((peaks[0].time.as_secs_f64() - 1.9).abs() < 0.05, "kill should sit on the last shot: {peaks:?}");
        assert_eq!(peaks[0].label, "Kill");
    }

    #[test]
    fn gunfire_alone_is_not_a_kill_and_close_kills_are_a_double() {
        let mut s = noise_bed(10.0);
        for k in 0..12 {
            shot(&mut s, 0.5 + k as f64 * 0.12); // a spray with no chime
        }
        for (at, hz) in [(4.0, 5_320.0), (8.0, 2_085.0)] {
            shot(&mut s, at - 0.3);
            shot(&mut s, at - 0.15);
            tone(&mut s, at, hz, 0.4);
        }
        let peaks = analyze_kills_from_samples(&s, 1, RATE, 1.0);
        assert_eq!(peaks.len(), 2, "{peaks:?}");
        assert!((peaks[0].time.as_secs_f64() - 3.85).abs() < 0.05, "{peaks:?}");
        assert_eq!(peaks[1].label, "Double Kill");
        assert!(peaks[1].is_multikill);
    }

    #[test]
    fn a_one_tap_with_a_clear_chime_is_a_kill() {
        let mut s = noise_bed(12.0);
        shot(&mut s, 1.8); // a single Sheriff/Operator shot
        tone(&mut s, 2.0, 5_320.0, 0.4); // clear kill chime
        // A lone shot followed by a borderline tone (long enough after a
        // burst, see below) stays out: one shot needs the clearer chime.
        shot(&mut s, 4.8);
        tone(&mut s, 5.0, 3_000.0, 0.21);
        // Spike beeps and a long UI tone, no gunfire. Not kills.
        for k in 0..3 {
            tone(&mut s, 7.0 + k as f64, 7_968.0, 0.1);
        }
        tone(&mut s, 10.5, 3_000.0, 0.45);
        let peaks = analyze_kills_from_samples(&s, 1, RATE, 1.0);
        assert_eq!(peaks.len(), 1, "expected the one-tap only, got {peaks:?}");
        assert!((peaks[0].time.as_secs_f64() - 1.8).abs() < 0.05, "kill should sit on the shot: {peaks:?}");

        // the same tones with no shot at all find nothing
        let mut quiet = noise_bed(12.0);
        tone(&mut quiet, 2.0, 5_320.0, 0.4);
        tone(&mut quiet, 10.5, 3_000.0, 0.45);
        assert!(analyze_kills_from_samples(&quiet, 1, RATE, 1.0).is_empty());

        // that borderline tone does count after two shots
        let mut burst = noise_bed(4.0);
        shot(&mut burst, 1.65);
        shot(&mut burst, 1.8);
        tone(&mut burst, 2.0, 3_000.0, 0.21);
        assert_eq!(analyze_kills_from_samples(&burst, 1, RATE, 1.0).len(), 1);
    }

    #[test]
    fn a_chime_that_fades_but_holds_is_timed_by_its_tail() {
        // a short loud attack and a faint 0.5 s ring (an older skin's chime)
        let chime = |head: bool, tail: bool| {
            let mut s = noise_bed(4.0);
            shot(&mut s, 1.65);
            shot(&mut s, 1.8);
            if head {
                tone_amp(&mut s, 2.0, 5_320.0, 0.15, 0.15);
            }
            if tail {
                tone_amp(&mut s, 2.0, 5_320.0, 0.5, 0.00038);
            }
            analyze_kills_from_samples(&s, 1, RATE, 1.0)
        };
        assert_eq!(chime(true, true).len(), 1, "attack + ring is a kill");
        assert!(chime(true, false).is_empty(), "the attack alone is too short");
        assert!(chime(false, true).is_empty(), "the ring alone is too faint");
    }

    #[test]
    fn a_dense_tonal_sound_after_gunfire_is_not_a_kill() {
        let chord = |notes: &[f32]| {
            let mut s = noise_bed(4.0);
            shot(&mut s, 1.65);
            shot(&mut s, 1.8);
            for &hz in notes {
                tone(&mut s, 2.0, hz, 0.4);
            }
            analyze_kills_from_samples(&s, 1, RATE, 1.0)
        };
        // music or an ability: many notes at once
        let dense: Vec<f32> = (0..9).map(|k| 2_200.0 + k as f32 * 650.0).collect();
        assert!(chord(&dense).is_empty(), "{:?}", chord(&dense));
        // the same moment with one note is a kill
        assert_eq!(chord(&[5_320.0]).len(), 1);
    }

    /// `KILLS_RAW=clip.f32 cargo test -p nle-media kills_in_raw -- --ignored --nocapture`
    /// with `ffmpeg -i clip.mp4 -map 0:a:0 -ac 1 -ar 48000 -f f32le clip.f32`.
    #[test]
    #[ignore]
    fn kills_in_raw() {
        let path = std::env::var("KILLS_RAW").expect("KILLS_RAW");
        let bytes = std::fs::read(path).expect("read");
        let s: Vec<f32> = bytes.chunks_exact(4).map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]])).collect();
        let t = std::time::Instant::now();
        let peaks = analyze_kills_from_samples(&s, 1, RATE, 1.0);
        let list: Vec<String> = peaks.iter().map(|p| format!("{:.2}s {}", p.time.as_secs_f64(), p.label)).collect();
        println!("KILLS {} in {:?}: {}", peaks.len(), t.elapsed(), list.join(", "));
    }
}
