//! Three-point editing: work out which source media goes where in the
//! timeline from In / Out marks on the source clip and on the timeline.
//!
//! The usual rules of Premiere / Resolve / Avid:
//!
//! * the duration comes from the timeline marks when both are set, else
//!   from the source marks (missing source marks = the start / end of the
//!   media);
//! * with only a timeline Out (or only a source Out next to a timeline
//!   duration) the edit is *backtimed*: it ends on that mark;
//! * with no timeline marks the edit lands at the playhead;
//! * Fit to Fill needs all four marks and changes the clip's speed so the
//!   source range fills the timeline range exactly.

use crate::asset::MediaAsset;
use crate::clip::Clip;
use crate::speed::{Retime, RetimeSegment};
use crate::time::Time;

/// In / Out marks of a viewer (either may be missing).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Marks {
    pub mark_in: Option<Time>,
    pub mark_out: Option<Time>,
}

impl Marks {
    pub const NONE: Marks = Marks { mark_in: None, mark_out: None };

    pub fn new(mark_in: Option<Time>, mark_out: Option<Time>) -> Self {
        Marks { mark_in, mark_out }
    }

    pub fn is_empty(&self) -> bool {
        self.mark_in.is_none() && self.mark_out.is_none()
    }

    /// Both marks, in order, with something between them.
    pub fn range(&self) -> Option<(Time, Time)> {
        match (self.mark_in, self.mark_out) {
            (Some(a), Some(b)) if b > a => Some((a, b)),
            _ => None,
        }
    }

    /// Set the In mark; an Out mark at or before it is dropped.
    pub fn set_in(&mut self, t: Time) {
        self.mark_in = Some(t);
        if self.mark_out.is_some_and(|o| o <= t) {
            self.mark_out = None;
        }
    }

    /// Set the Out mark; an In mark at or after it is dropped.
    pub fn set_out(&mut self, t: Time) {
        self.mark_out = Some(t);
        if self.mark_in.is_some_and(|i| i >= t) {
            self.mark_in = None;
        }
    }
}

/// A resolved three-point edit.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct EditPlan {
    /// First source frame used.
    pub source_in: Time,
    /// Source media consumed.
    pub source_len: Time,
    /// Where the edit starts on the timeline.
    pub record_in: Time,
    /// Length on the timeline.
    pub duration: Time,
    /// Playback speed (1.0 except for Fit to Fill).
    pub speed: f64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlanError {
    /// The marks leave nothing to edit.
    Empty,
    /// The timeline range asks for more media than the source has.
    NotEnoughMedia,
    /// A backtimed edit would start before the timeline does.
    BeforeStart,
    /// Fit to Fill without In and Out on both sides.
    FitNeedsFourMarks,
}

impl PlanError {
    pub fn message(self) -> &'static str {
        match self {
            PlanError::Empty => "The In and Out marks leave nothing to edit",
            PlanError::NotEnoughMedia => "The source clip is too short for the marked timeline range",
            PlanError::BeforeStart => "A backtimed edit would start before the beginning of the timeline",
            PlanError::FitNeedsFourMarks => "Fit to Fill needs In and Out marks in both the source and the timeline",
        }
    }
}

impl std::fmt::Display for PlanError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.message())
    }
}

/// Resolve an edit. `bounds` is the usable source media (the whole file,
/// or a subclip's range); marks outside it are clamped.
pub fn plan(source: Marks, bounds: (Time, Time), record: Marks, playhead: Time, fit: bool) -> Result<EditPlan, PlanError> {
    let (lo, hi) = bounds;
    if hi <= lo {
        return Err(PlanError::Empty);
    }
    let clamp = |t: Time| t.clamp(lo, hi);
    let s_in = source.mark_in.map(clamp);
    let s_out = source.mark_out.map(clamp);

    if fit {
        let (Some(a), Some(b), Some(ri), Some(ro)) = (s_in, s_out, record.mark_in, record.mark_out) else {
            return Err(PlanError::FitNeedsFourMarks);
        };
        if b <= a || ro <= ri {
            return Err(PlanError::Empty);
        }
        let len = b - a;
        let dur = ro - ri;
        let speed = len.as_secs_f64() / dur.as_secs_f64();
        return Ok(EditPlan { source_in: a, source_len: len, record_in: ri, duration: dur, speed });
    }

    if let (Some(ri), Some(ro)) = (record.mark_in, record.mark_out) {
        // the timeline decides the length
        if ro <= ri {
            return Err(PlanError::Empty);
        }
        let dur = ro - ri;
        let source_in = match (s_in, s_out) {
            (Some(a), _) => a,
            (None, Some(b)) => {
                if b - dur < lo {
                    return Err(PlanError::NotEnoughMedia);
                }
                b - dur
            }
            (None, None) => lo,
        };
        if source_in + dur > hi {
            return Err(PlanError::NotEnoughMedia);
        }
        return Ok(EditPlan { source_in, source_len: dur, record_in: ri, duration: dur, speed: 1.0 });
    }

    // the source decides the length
    let a = s_in.unwrap_or(lo);
    let b = s_out.unwrap_or(hi);
    if b <= a {
        return Err(PlanError::Empty);
    }
    let dur = b - a;
    let record_in = match (record.mark_in, record.mark_out) {
        (Some(ri), _) => ri,
        (None, Some(ro)) => {
            if ro < dur {
                return Err(PlanError::BeforeStart);
            }
            ro - dur
        }
        (None, None) => playhead,
    };
    Ok(EditPlan { source_in: a, source_len: dur, record_in, duration: dur, speed: 1.0 })
}

/// The clip a plan puts into the timeline.
pub fn plan_clip(asset: &MediaAsset, p: &EditPlan) -> Clip {
    let mut c = Clip::from_asset_range(asset, p.source_in, p.duration);
    if (p.speed - 1.0).abs() > 1e-9 {
        // one segment of exactly the timeline length, so the edit fills its range
        let blend = c.frame_quality();
        c.retime =
            Some(Retime { segments: vec![RetimeSegment::new(p.duration, p.speed)], start_offset: Time::ZERO, lead_speed: None, blend, velocity_curve: None, pins: Vec::new() });
    }
    c
}

/// Source time of the frame that a Replace edit lines up with the
/// playhead: with a source In mark the clip starts there; otherwise the
/// frame under the source playhead lands on the timeline playhead.
/// Returns the new clip's first source frame for a target clip starting at
/// `target_start` lasting `target_len`.
pub fn replace_source_in(source: Marks, source_pos: Time, bounds: (Time, Time), playhead: Time, target_start: Time, target_len: Time) -> Result<Time, PlanError> {
    let (lo, hi) = bounds;
    let start = match source.mark_in {
        Some(i) => i.clamp(lo, hi),
        None if playhead >= target_start && playhead < target_start + target_len => source_pos - (playhead - target_start),
        None => source.mark_in.unwrap_or(lo),
    };
    if start < lo || start + target_len > hi {
        return Err(PlanError::NotEnoughMedia);
    }
    Ok(start)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(x: f64) -> Time {
        Time::from_secs_f64(x)
    }
    fn m(a: Option<f64>, b: Option<f64>) -> Marks {
        Marks::new(a.map(s), b.map(s))
    }
    fn bounds(len: f64) -> (Time, Time) {
        (Time::ZERO, s(len))
    }

    #[test]
    fn source_marks_land_at_the_playhead() {
        let p = plan(m(Some(2.0), Some(5.0)), bounds(10.0), Marks::NONE, s(7.0), false).unwrap();
        assert_eq!((p.source_in, p.duration, p.record_in), (s(2.0), s(3.0), s(7.0)));
    }

    #[test]
    fn no_source_marks_use_the_whole_clip() {
        let p = plan(Marks::NONE, bounds(10.0), m(Some(1.0), None), s(0.0), false).unwrap();
        assert_eq!((p.source_in, p.duration, p.record_in), (Time::ZERO, s(10.0), s(1.0)));
    }

    #[test]
    fn timeline_range_sets_the_duration() {
        let p = plan(m(Some(3.0), None), bounds(10.0), m(Some(1.0), Some(3.0)), s(0.0), false).unwrap();
        assert_eq!((p.source_in, p.duration, p.record_in), (s(3.0), s(2.0), s(1.0)));
    }

    #[test]
    fn source_out_with_timeline_range_is_backtimed_in_the_source() {
        let p = plan(m(None, Some(6.0)), bounds(10.0), m(Some(1.0), Some(3.0)), s(0.0), false).unwrap();
        assert_eq!((p.source_in, p.duration), (s(4.0), s(2.0)));
    }

    #[test]
    fn timeline_out_alone_backtimes_the_edit() {
        let p = plan(m(Some(2.0), Some(5.0)), bounds(10.0), m(None, Some(8.0)), s(0.0), false).unwrap();
        assert_eq!((p.record_in, p.duration), (s(5.0), s(3.0)));
        assert_eq!(plan(m(Some(2.0), Some(5.0)), bounds(10.0), m(None, Some(1.0)), s(0.0), false), Err(PlanError::BeforeStart));
    }

    #[test]
    fn too_little_media_is_an_error() {
        assert_eq!(plan(m(Some(9.0), None), bounds(10.0), m(Some(0.0), Some(3.0)), s(0.0), false), Err(PlanError::NotEnoughMedia));
        assert_eq!(plan(m(None, Some(1.0)), bounds(10.0), m(Some(0.0), Some(3.0)), s(0.0), false), Err(PlanError::NotEnoughMedia));
    }

    #[test]
    fn subclip_bounds_limit_the_media() {
        let p = plan(Marks::NONE, (s(4.0), s(6.0)), Marks::NONE, s(1.0), false).unwrap();
        assert_eq!((p.source_in, p.duration), (s(4.0), s(2.0)));
        // marks outside the subclip are clamped into it
        let p = plan(m(Some(1.0), Some(9.0)), (s(4.0), s(6.0)), Marks::NONE, s(1.0), false).unwrap();
        assert_eq!((p.source_in, p.duration), (s(4.0), s(2.0)));
    }

    #[test]
    fn fit_to_fill_changes_speed() {
        let p = plan(m(Some(0.0), Some(4.0)), bounds(10.0), m(Some(1.0), Some(3.0)), s(0.0), true).unwrap();
        assert_eq!((p.source_in, p.source_len, p.duration, p.record_in), (Time::ZERO, s(4.0), s(2.0), s(1.0)));
        assert!((p.speed - 2.0).abs() < 1e-9);
        assert_eq!(plan(m(Some(0.0), None), bounds(10.0), m(Some(1.0), Some(3.0)), s(0.0), true), Err(PlanError::FitNeedsFourMarks));
    }

    #[test]
    fn fit_clip_is_exactly_the_marked_length_and_uses_the_source_range() {
        let a = MediaAsset::new("a", "a.mp4", s(10.0));
        let p = plan(m(Some(1.0), Some(4.0)), bounds(10.0), m(Some(0.0), Some(s(2.0).as_secs_f64())), s(0.0), true).unwrap();
        let c = plan_clip(&a, &p);
        assert_eq!(c.duration, s(2.0));
        let (lo, hi) = c.source_range();
        assert_eq!(lo, s(1.0));
        assert!((hi - s(4.0)).abs() <= Time(2), "{hi:?}");
    }

    #[test]
    fn marks_keep_their_order() {
        let mut k = Marks::default();
        k.set_out(s(2.0));
        k.set_in(s(3.0));
        assert_eq!(k, Marks::new(Some(s(3.0)), None));
        k.set_out(s(5.0));
        assert_eq!(k.range(), Some((s(3.0), s(5.0))));
    }

    #[test]
    fn replace_lines_up_the_source_playhead_with_the_timeline_playhead() {
        // target 10..14, playhead at 11, source parked on 6 -> clip starts at source 5
        let st = replace_source_in(Marks::NONE, s(6.0), bounds(20.0), s(11.0), s(10.0), s(4.0)).unwrap();
        assert_eq!(st, s(5.0));
        // an In mark wins
        let st = replace_source_in(m(Some(2.0), None), s(6.0), bounds(20.0), s(11.0), s(10.0), s(4.0)).unwrap();
        assert_eq!(st, s(2.0));
        assert_eq!(replace_source_in(m(Some(18.0), None), s(6.0), bounds(20.0), s(11.0), s(10.0), s(4.0)), Err(PlanError::NotEnoughMedia));
    }
}
