//! Fusion-style animated Text+ presets.
//!
//! An animation preset turns clip-local time into a [`TitleAnim`] state
//! (scale, offset, opacity, visible characters, tracking, glitch amount,
//! highlighted word). The state is pure data, evaluated on the CPU and
//! applied by the GPU title shader, so it is deterministic and testable.

use crate::clip::TitleParams;
use crate::time::Time;
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum TextAnimation {
    #[default]
    None,
    /// Elastic scale-in with overshoot.
    PopBounce,
    /// Characters appear one by one.
    Typewriter,
    /// Slides up into place (and back down at the end), fading.
    SlideUpLowerThird,
    /// Letter spacing slowly opens up across the clip.
    TrackingExpand,
    /// RGB split + horizontal slice jitter bursts.
    Glitch,
    /// CapCut-style: the spoken word is coloured and pops.
    WordHighlight,
}

impl TextAnimation {
    pub const ALL: [TextAnimation; 7] = [
        TextAnimation::None,
        TextAnimation::PopBounce,
        TextAnimation::Typewriter,
        TextAnimation::SlideUpLowerThird,
        TextAnimation::TrackingExpand,
        TextAnimation::Glitch,
        TextAnimation::WordHighlight,
    ];
    pub fn label(self) -> &'static str {
        match self {
            TextAnimation::None => "None",
            TextAnimation::PopBounce => "Pop / Elastic Bounce",
            TextAnimation::Typewriter => "Typewriter",
            TextAnimation::SlideUpLowerThird => "Slide-Up Lower Third",
            TextAnimation::TrackingExpand => "Tracking Expand",
            TextAnimation::Glitch => "Glitch",
            TextAnimation::WordHighlight => "CapCut Word-Highlight",
        }
    }
}

/// Animation state of a title at one instant.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TitleAnim {
    /// Uniform scale about the text centre.
    pub scale: f32,
    /// Offset in pixels at 1080p.
    pub offset: [f32; 2],
    pub opacity: f32,
    /// Number of characters shown (typewriter), `None` = all.
    pub visible_chars: Option<usize>,
    /// Extra letter spacing in pixels at 1080p.
    pub tracking_add: f32,
    /// 0..1 glitch intensity.
    pub glitch: f32,
    /// Changes every frame while glitching (drives the jitter pattern).
    pub glitch_seed: f32,
    /// Index of the highlighted word (word-highlight / captions).
    pub highlight_word: Option<usize>,
    /// Scale of the highlighted word (pop).
    pub word_scale: f32,
    /// Vertical offset of the highlighted word, pixels at 1080p (negative = up).
    pub word_lift: f32,
    /// Word the sliding highlight pill glides away from.
    pub pill_from: Option<usize>,
    /// 0..1 progress of the pill's glide from `pill_from` to `highlight_word`.
    pub pill_glide: f32,
    /// Word-highlight: words from this index on are not spoken yet (dimmed).
    pub dim_from: Option<usize>,
}

impl Default for TitleAnim {
    fn default() -> Self {
        TitleAnim {
            scale: 1.0,
            offset: [0.0, 0.0],
            opacity: 1.0,
            visible_chars: None,
            tracking_add: 0.0,
            glitch: 0.0,
            glitch_seed: 0.0,
            highlight_word: None,
            word_scale: 1.0,
            word_lift: 0.0,
            pill_from: None,
            pill_glide: 1.0,
            dim_from: None,
        }
    }
}

impl TitleAnim {
    /// Quantised key for caching rasterised text (things that change glyph layout).
    pub fn layout_key(&self) -> (Option<usize>, i32) {
        (self.visible_chars, (self.tracking_add * 4.0).round() as i32)
    }
}

/// Elastic ease-out: 0 → 1 with a decaying overshoot.
pub fn elastic_out(u: f32) -> f32 {
    if u <= 0.0 {
        return 0.0;
    }
    if u >= 1.0 {
        return 1.0;
    }
    let c4 = std::f32::consts::TAU / 3.0;
    2f32.powf(-10.0 * u) * ((u * 10.0 - 0.75) * c4).sin() + 1.0
}

fn ease_out_cubic(u: f32) -> f32 {
    let v = 1.0 - u.clamp(0.0, 1.0);
    1.0 - v * v * v
}

fn smoothstep(u: f32) -> f32 {
    let u = u.clamp(0.0, 1.0);
    u * u * (3.0 - 2.0 * u)
}

/// Spring-like overshoot over `u` in 0..1: ease out from `from` to `peak`
/// during the first `split` of the time, then settle smoothly on `settle`.
/// Holds `settle` for `u >= 1`.
pub fn overshoot(u: f32, from: f32, peak: f32, settle: f32, split: f32) -> f32 {
    if u <= 0.0 {
        return from;
    }
    if u >= 1.0 {
        return settle;
    }
    if u < split {
        from + (peak - from) * ease_out_cubic(u / split)
    } else {
        peak + (settle - peak) * smoothstep((u - split) / (1.0 - split))
    }
}

/// Phrase entrance scale: 0.84 -> 1.03 -> 1.0 over [`PHRASE_IN_SECS`].
pub fn phrase_entrance_scale(t: f32) -> f32 {
    overshoot(t / PHRASE_IN_SECS, 0.84, 1.03, 1.0, 0.7)
}

/// Active word pop: crisp ease-out 1.0 -> [`WORD_POP`] over [`WORD_POP_SECS`], then held.
pub fn word_pop_scale(since: f32) -> f32 {
    1.0 + (WORD_POP - 1.0) * ease_out_cubic(since / WORD_POP_SECS)
}

/// Exponential glide of the highlight pill, 0 -> ~1 (time constant [`PILL_GLIDE_TAU`]).
pub fn pill_glide(since: f32) -> f32 {
    if since <= 0.0 {
        0.0
    } else {
        1.0 - (-since / PILL_GLIDE_TAU).exp()
    }
}

fn hash(x: f32) -> f32 {
    let s = (x * 12.9898).sin() * 43_758.547;
    s - s.floor()
}

/// The words of a title (whitespace separated).
pub fn words(text: &str) -> Vec<&str> {
    text.split_whitespace().collect()
}

/// Per-word `[start, end]` times in seconds relative to the clip start:
/// explicit timings when present (captions), else an even split.
pub fn word_timings(p: &TitleParams, duration: f32) -> Vec<[f32; 2]> {
    let n = words(&p.text).len();
    if !p.word_times.is_empty() && p.word_times.len() == n {
        return p.word_times.clone();
    }
    let step = if n > 0 { duration / n as f32 } else { duration };
    (0..n).map(|i| [i as f32 * step, (i + 1) as f32 * step]).collect()
}

/// Evaluate `p`'s animation at clip-local `local` for a clip of `duration`.
pub fn title_anim(p: &TitleParams, local: Time, duration: Time) -> TitleAnim {
    let t = local.as_secs_f64() as f32;
    let dur = duration.as_secs_f64().max(1e-3) as f32;
    let a_in = p.anim_duration.max(0.05);
    let u = (t / a_in).clamp(0.0, 1.0);
    // time left in the clip, for out-animations
    let left = (dur - t).max(0.0);
    let u_out = (left / a_in).clamp(0.0, 1.0);
    let mut s = TitleAnim::default();
    match p.animation {
        TextAnimation::None => {}
        TextAnimation::PopBounce => {
            s.scale = elastic_out(u).max(0.0);
            s.opacity = (u * 4.0).min(1.0);
        }
        TextAnimation::Typewriter => {
            let n = p.text.chars().count() + p.subtitle.chars().count();
            s.visible_chars = Some(((n as f32) * u).floor() as usize).filter(|&v| v < n);
        }
        TextAnimation::SlideUpLowerThird => {
            let k = ease_out_cubic(u) * ease_out_cubic(u_out);
            s.offset = [0.0, (1.0 - k) * 70.0];
            s.opacity = k;
        }
        TextAnimation::TrackingExpand => {
            s.tracking_add = 40.0 * (t / dur).clamp(0.0, 1.0);
            s.opacity = ease_out_cubic(u);
        }
        TextAnimation::Glitch => {
            // strong burst while entering, then short random hits ~3 times a second
            let frame = (t * 24.0).floor();
            let burst = 1.0 - u;
            let hit = if hash(frame) > 0.86 { 0.35 + 0.5 * hash(frame + 3.1) } else { 0.0 };
            s.glitch = burst.max(hit).clamp(0.0, 1.0);
            s.glitch_seed = frame;
            s.opacity = if u < 1.0 && hash(frame + 7.7) > 0.7 { 0.4 } else { 1.0 };
        }
        TextAnimation::WordHighlight => {
            let times = word_timings(p, dur);
            // progressive colour pop: words not started yet are dimmed, the
            // word being spoken (start <= t < end) pops in the highlight
            // colour, spoken words settle on the text colour
            s.dim_from = times.iter().position(|w| t < w[0]);
            let active = times.iter().position(|w| t >= w[0] && t < w[1]);
            match active {
                Some(i) => {
                    s.highlight_word = Some(i);
                    // CapCut spring pop with a small lift, then held
                    let since = (t - times[i][0]).max(0.0);
                    s.word_scale = word_pop_scale(since);
                    s.word_lift = WORD_LIFT_PX * ease_out_cubic(since / WORD_POP_SECS);
                    s.pill_from = i.checked_sub(1);
                    s.pill_glide = if i == 0 { 1.0 } else { pill_glide(since) };
                }
                _ => s.highlight_word = None,
            }
            // phrase entrance: punch in on speech onset
            let first = times.first().map_or(0.0, |w| w[0]).max(0.0);
            let since_in = t - first;
            if since_in < PHRASE_IN_SECS {
                s.scale = phrase_entrance_scale(since_in);
                s.opacity = (since_in / PHRASE_IN_SECS).clamp(0.0, 1.0);
            }
        }
    }
    s
}

/// Held scale of the active caption word after its spring pop.
pub const WORD_POP: f32 = 1.10;
/// Largest scale of the active word (the pop eases out without overshoot).
pub const WORD_POP_PEAK: f32 = WORD_POP;
/// Duration of the word pop spring, seconds.
pub const WORD_POP_SECS: f32 = 0.075;
/// Lift of the active word, pixels at 1080p (negative = up).
pub const WORD_LIFT_PX: f32 = -3.0;
/// Phrase entrance pop duration, seconds.
pub const PHRASE_IN_SECS: f32 = 0.065;
/// Time constant of the sliding highlight pill's exponential glide, seconds.
pub const PILL_GLIDE_TAU: f32 = 0.03;

#[cfg(test)]
mod tests {
    use super::*;

    fn params(anim: TextAnimation, text: &str) -> TitleParams {
        TitleParams { text: text.into(), animation: anim, anim_duration: 0.5, ..Default::default() }
    }

    #[test]
    fn elastic_starts_at_zero_overshoots_and_settles() {
        assert_eq!(elastic_out(0.0), 0.0);
        assert_eq!(elastic_out(1.0), 1.0);
        let peak = (1..100).map(|i| elastic_out(i as f32 / 100.0)).fold(0.0, f32::max);
        assert!(peak > 1.0, "overshoot {peak}");
    }

    #[test]
    fn pop_bounce_scales_in() {
        let p = params(TextAnimation::PopBounce, "Hi");
        let d = Time::from_secs(3);
        assert!(title_anim(&p, Time::ZERO, d).scale < 0.01);
        assert!((title_anim(&p, Time::from_secs(1), d).scale - 1.0).abs() < 1e-3);
    }

    #[test]
    fn typewriter_reveals_characters() {
        let p = params(TextAnimation::Typewriter, "abcdefghij");
        let d = Time::from_secs(2);
        assert_eq!(title_anim(&p, Time::ZERO, d).visible_chars, Some(0));
        assert_eq!(title_anim(&p, Time::from_millis(250), d).visible_chars, Some(5));
        assert_eq!(title_anim(&p, Time::from_secs(1), d).visible_chars, None);
    }

    #[test]
    fn slide_up_enters_and_exits() {
        let p = params(TextAnimation::SlideUpLowerThird, "Name");
        let d = Time::from_secs(4);
        let a0 = title_anim(&p, Time::ZERO, d);
        assert!(a0.offset[1] > 60.0 && a0.opacity < 0.01);
        let mid = title_anim(&p, Time::from_secs(2), d);
        assert!(mid.offset[1].abs() < 1e-3 && (mid.opacity - 1.0).abs() < 1e-3);
        assert!(title_anim(&p, Time::from_millis(3990), d).opacity < 0.1);
    }

    #[test]
    fn word_highlight_follows_explicit_timings() {
        let mut p = params(TextAnimation::WordHighlight, "one two three");
        p.word_times = vec![[0.0, 0.3], [0.3, 1.0], [1.0, 1.4]];
        let d = Time::from_millis(1500);
        assert_eq!(title_anim(&p, Time::from_millis(100), d).highlight_word, Some(0));
        let a = title_anim(&p, Time::from_millis(310), d);
        assert_eq!(a.highlight_word, Some(1));
        assert!(a.word_scale > 1.0 && a.word_scale < WORD_POP, "pop is easing in {}", a.word_scale);
        assert!(a.word_lift < 0.0 && a.word_lift > WORD_LIFT_PX, "lift easing in {}", a.word_lift);
        // crisp ease-out pop to 1.10 by 75 ms, no overshoot, then held
        let peak = (300..375).map(|ms| title_anim(&p, Time::from_millis(ms), d).word_scale).fold(0.0, f32::max);
        assert!(peak <= WORD_POP + 1e-4, "peak {peak}");
        for ms in 376..900 {
            let a = title_anim(&p, Time::from_millis(ms), d);
            assert!((a.word_scale - WORD_POP).abs() < 1e-4, "held pop {} at {ms} ms", a.word_scale);
            assert!((a.word_lift - WORD_LIFT_PX).abs() < 1e-4);
        }
        assert_eq!(title_anim(&p, Time::from_millis(1200), d).highlight_word, Some(2));
        assert_eq!(title_anim(&p, Time::from_millis(1450), d).highlight_word, None);
        // three tones: word 0 spoken, word 1 active, word 2 still dimmed
        let a = title_anim(&p, Time::from_millis(500), d);
        assert_eq!((a.highlight_word, a.dim_from), (Some(1), Some(2)));
        assert_eq!(title_anim(&p, Time::from_millis(100), d).dim_from, Some(1));
        assert_eq!(title_anim(&p, Time::from_millis(1450), d).dim_from, None);
        // in a gap between words nothing lingers: no active word, the next is dimmed
        p.word_times = vec![[0.0, 0.3], [0.5, 1.0], [1.0, 1.4]];
        let a = title_anim(&p, Time::from_millis(400), d);
        assert_eq!((a.highlight_word, a.dim_from), (None, Some(1)));
        // without timings the clip is split evenly
        p.word_times.clear();
        assert_eq!(title_anim(&p, Time::from_millis(1100), d).highlight_word, Some(2));
    }

    #[test]
    fn spring_curves_hit_their_targets() {
        // phrase entrance 0.84 -> ~1.03 -> 1.0 in 65 ms
        assert!((phrase_entrance_scale(0.0) - 0.84).abs() < 1e-6);
        let peak = (0..=650).map(|i| phrase_entrance_scale(i as f32 / 10_000.0)).fold(0.0, f32::max);
        assert!((peak - 1.03).abs() < 2e-3, "entrance peak {peak}");
        assert_eq!(phrase_entrance_scale(PHRASE_IN_SECS), 1.0);
        assert_eq!(phrase_entrance_scale(1.0), 1.0);
        // word pop 1.0 -> 1.10 ease-out in 75 ms, monotonic (no overshoot)
        assert_eq!(word_pop_scale(0.0), 1.0);
        let peak = (0..=750).map(|i| word_pop_scale(i as f32 / 10_000.0)).fold(0.0, f32::max);
        assert!((peak - WORD_POP).abs() < 1e-4, "pop peak {peak}");
        assert_eq!(word_pop_scale(WORD_POP_SECS), WORD_POP);
        assert!((1..=75).all(|ms| word_pop_scale(ms as f32 / 1000.0) >= word_pop_scale((ms - 1) as f32 / 1000.0)));
        // no jumps
        let mut prev = 1.0;
        for ms in 1..=75 {
            let v = word_pop_scale(ms as f32 / 1000.0);
            assert!((v - prev).abs() < 0.05, "jump at {ms} ms: {prev} -> {v}");
            prev = v;
        }
        // pill glide: exponential, monotonic, ~done after 5 time constants
        assert_eq!(pill_glide(0.0), 0.0);
        assert!(pill_glide(PILL_GLIDE_TAU) > 0.6 && pill_glide(PILL_GLIDE_TAU) < 0.65);
        assert!(pill_glide(5.0 * PILL_GLIDE_TAU) > 0.99);
        assert!((1..100).all(|i| pill_glide(i as f32 * 0.002) > pill_glide((i - 1) as f32 * 0.002)));
    }

    #[test]
    fn phrase_pops_in_on_first_word_and_pill_glides() {
        let mut p = params(TextAnimation::WordHighlight, "one two");
        p.word_times = vec![[0.1, 0.4], [0.4, 0.9]];
        let d = Time::from_secs(1);
        let a = title_anim(&p, Time::from_millis(100), d);
        assert!((a.scale - 0.84).abs() < 1e-3 && a.opacity < 1e-3, "entrance starts small and clear");
        let a = title_anim(&p, Time::from_millis(130), d);
        assert!(a.scale > 0.84 && a.opacity > 0.3 && a.opacity < 1.0);
        let a = title_anim(&p, Time::from_millis(200), d);
        assert_eq!((a.scale, a.opacity), (1.0, 1.0));
        // first word: the pill sits on it; second word: glides from word 0
        assert_eq!(a.pill_from, None);
        assert_eq!(a.pill_glide, 1.0);
        let a = title_anim(&p, Time::from_millis(410), d);
        assert_eq!((a.highlight_word, a.pill_from), (Some(1), Some(0)));
        assert!(a.pill_glide > 0.0 && a.pill_glide < 1.0);
    }

    #[test]
    fn glitch_is_strong_on_entry() {
        let p = params(TextAnimation::Glitch, "ERROR");
        let a = title_anim(&p, Time::ZERO, Time::from_secs(2));
        assert!(a.glitch > 0.9);
    }
}
