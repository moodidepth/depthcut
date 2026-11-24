//! Multicam clips: several recordings of the same moment lined up as
//! angles, with one angle seen and one (possibly another) heard.
//!
//! A multicam clip is a compound clip whose `spine` always holds the active
//! video angle, so everything that understands compound clips (render,
//! export, step-in editing) keeps working; the audio comes from
//! [`Multicam::audio`]'s angle.

use crate::asset::MediaAsset;
use crate::clip::{Clip, ClipKind, CompoundClip};
use crate::time::Time;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Angle {
    pub name: String,
    /// Clips of this angle from the multicam's start (gaps where it has
    /// no media); every angle is padded to the same length.
    pub spine: Vec<Clip>,
}

impl Angle {
    pub fn duration(&self) -> Time {
        self.spine.iter().map(|c| c.duration).sum()
    }

    pub fn has_video(&self) -> bool {
        self.spine.iter().any(|c| c.has_video())
    }

    pub fn has_audio(&self) -> bool {
        self.spine.iter().any(|c| c.has_audio())
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Multicam {
    pub angles: Vec<Angle>,
    /// Angle shown.
    pub video: usize,
    /// Angle heard.
    pub audio: usize,
}

impl Multicam {
    pub fn duration(&self) -> Time {
        self.angles.iter().map(Angle::duration).max().unwrap_or(Time::ZERO)
    }

    /// Pad every angle with a trailing gap to the longest one.
    pub fn normalize(&mut self) {
        let len = self.duration();
        for a in &mut self.angles {
            let d = a.duration();
            if d < len {
                a.spine.push(Clip::gap(len - d));
            }
        }
        let last = self.angles.len().saturating_sub(1);
        self.video = self.video.min(last);
        self.audio = self.audio.min(last);
    }

    pub fn audio_spine(&self) -> Option<&[Clip]> {
        self.angles.get(self.audio).map(|a| a.spine.as_slice())
    }
}

/// One angle per recording, each starting at its `starts` time on the
/// multicam's clock (see [`crate::sync::starts_from_offsets`]).
pub fn build(name: &str, recordings: &[(&MediaAsset, Time)]) -> Option<Clip> {
    if recordings.is_empty() {
        return None;
    }
    let angles = recordings
        .iter()
        .map(|(a, start)| {
            let mut spine = Vec::new();
            if start.is_positive() {
                spine.push(Clip::gap(*start));
            }
            spine.push(Clip::from_asset(a));
            Angle { name: a.name.clone(), spine }
        })
        .collect::<Vec<_>>();
    let video = angles.iter().position(Angle::has_video).unwrap_or(0);
    // a separate sound recorder is usually the better audio
    let audio = angles.iter().position(|a| a.has_audio() && !a.has_video()).or_else(|| angles.iter().position(Angle::has_audio)).unwrap_or(0);
    let mut mc = Multicam { angles, video, audio };
    mc.normalize();
    let dur = mc.duration();
    if !dur.is_positive() {
        return None;
    }
    let mut comp = CompoundClip::new(mc.angles[mc.video].spine.clone());
    comp.multicam = Some(Box::new(mc));
    let mut clip = Clip::new(name, ClipKind::Compound(Box::new(comp)), dur);
    clip.media_duration = Some(dur);
    Some(clip)
}

/// The multicam data of a clip, if it is one.
pub fn of(c: &Clip) -> Option<&Multicam> {
    match &c.kind {
        ClipKind::Compound(comp) => comp.multicam.as_deref(),
        _ => None,
    }
}

/// Show and / or hear angle `video` / `audio` on this clip. False when the
/// clip is no multicam, the angle doesn't exist or nothing changes.
pub fn switch(c: &mut Clip, video: Option<usize>, audio: Option<usize>) -> bool {
    let ClipKind::Compound(comp) = &mut c.kind else { return false };
    let Some(mc) = comp.multicam.as_deref_mut() else { return false };
    let n = mc.angles.len();
    if video.is_some_and(|v| v >= n) || audio.is_some_and(|a| a >= n) {
        return false;
    }
    let mut changed = false;
    if let Some(a) = audio {
        changed |= mc.audio != a;
        mc.audio = a;
    }
    if let Some(v) = video {
        if mc.video != v {
            mc.video = v;
            comp.spine = mc.angles[v].spine.clone();
            changed = true;
        }
    }
    changed
}

/// After editing inside the clip: its spine is the shown angle's contents.
pub fn store_spine(comp: &mut CompoundClip) {
    let spine = comp.spine.clone();
    if let Some(mc) = comp.multicam.as_deref_mut() {
        if let Some(a) = mc.angles.get_mut(mc.video) {
            a.spine = spine;
        }
        mc.normalize();
        comp.spine = mc.angles[mc.video].spine.clone();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::asset::{AudioStreamInfo, VideoStreamInfo};
    use crate::command::Command;
    use crate::timeline::Timeline;
    use crate::VideoFormat;

    fn s(x: i64) -> Time {
        Time::from_secs(x)
    }

    fn sound() -> AudioStreamInfo {
        AudioStreamInfo { sample_rate: 48000, channels: 2, codec: "aac".into() }
    }

    fn cam(name: &str, len: i64) -> MediaAsset {
        let mut a = MediaAsset::new(name, format!("{name}.mov"), s(len));
        a.video = Some(VideoStreamInfo {
            width: 1920,
            height: 1080,
            frame_rate: crate::FrameRate::FPS_25,
            codec: "h264".into(),
            pix_fmt: "yuv420p".into(),
            bit_depth: 8,
            color_space: "bt709".into(),
            color_transfer: String::new(),
            color_primaries: String::new(),
            is_vfr: false,
            pts_table: Vec::new(),
        });
        a.audio = Some(sound());
        a
    }

    fn recorder(len: i64) -> MediaAsset {
        let mut a = MediaAsset::new("Zoom", "zoom.wav", s(len));
        a.audio = Some(sound());
        a.kind = crate::AssetKind::Audio;
        a
    }

    #[test]
    fn angles_line_up_and_share_one_length() {
        let (a, b, r) = (cam("A", 10), cam("B", 6), recorder(20));
        let clip = build("Interview", &[(&a, s(2)), (&b, s(5)), (&r, Time::ZERO)]).unwrap();
        let mc = of(&clip).unwrap();
        assert_eq!(clip.duration, s(20));
        assert!(mc.angles.iter().all(|x| x.duration() == s(20)));
        assert_eq!(mc.video, 0);
        assert_eq!(mc.audio, 2, "the recorder is heard");
        assert_eq!(mc.angles[1].spine[0].duration, s(5));
        let ClipKind::Compound(comp) = &clip.kind else { panic!() };
        assert_eq!(comp.spine, mc.angles[0].spine);
    }

    #[test]
    fn switching_changes_what_is_seen_and_heard() {
        let (a, b) = (cam("A", 10), cam("B", 10));
        let mut clip = build("M", &[(&a, Time::ZERO), (&b, Time::ZERO)]).unwrap();
        assert!(switch(&mut clip, Some(1), None));
        assert!(!switch(&mut clip, Some(1), None), "no change");
        assert!(!switch(&mut clip, Some(5), None), "no such angle");
        let mc = of(&clip).unwrap();
        assert_eq!((mc.video, mc.audio), (1, 0));
        let ClipKind::Compound(comp) = &clip.kind else { panic!() };
        assert_eq!(comp.spine, mc.angles[1].spine);
    }

    #[test]
    fn switch_angle_command_cuts_at_the_playhead() {
        let (a, b) = (cam("A", 10), cam("B", 10));
        let clip = build("M", &[(&a, Time::ZERO), (&b, Time::ZERO)]).unwrap();
        let id = clip.id;
        let mut tl = Timeline::new("T", VideoFormat::default());
        tl.append(clip).unwrap();
        Command::SwitchAngle { id, at: Some(s(4)), video: Some(1), audio: Some(1) }.apply(&mut tl).unwrap();
        assert_eq!(tl.spine.len(), 2);
        assert_eq!(of(&tl.spine[0]).map(|m| (m.video, m.audio)), Some((0, 0)));
        assert_eq!(of(&tl.spine[1]).map(|m| (m.video, m.audio)), Some((1, 1)));
        assert_eq!(tl.spine[1].source_in, s(4));
        // at the clip's own start: switch the whole clip, no cut
        let first = tl.spine[0].id;
        Command::SwitchAngle { id: first, at: Some(Time::ZERO), video: Some(1), audio: None }.apply(&mut tl).unwrap();
        assert_eq!(tl.spine.len(), 2);
        assert_eq!(of(&tl.spine[0]).map(|m| m.video), Some(1));
    }

    #[test]
    fn audio_comes_from_the_heard_angle() {
        let (a, r) = (cam("A", 10), recorder(10));
        let clip = build("M", &[(&a, Time::ZERO), (&r, Time::ZERO)]).unwrap();
        let mut tl = Timeline::new("T", VideoFormat::default());
        tl.append(clip).unwrap();
        let spans = crate::eval::audio_spans(&tl);
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].asset, r.id);
    }

    #[test]
    fn edits_inside_go_to_the_shown_angle() {
        let (a, b) = (cam("A", 10), cam("B", 10));
        let clip = build("M", &[(&a, Time::ZERO), (&b, Time::ZERO)]).unwrap();
        let id = clip.id;
        let mut tl = Timeline::new("T", VideoFormat::default());
        tl.append(clip).unwrap();
        Command::InContainer { container: id, cmd: Box::new(Command::Blade { at: s(3) }) }.apply(&mut tl).unwrap();
        let mc = of(&tl.spine[0]).unwrap();
        assert_eq!(mc.angles[0].spine.len(), 2);
        assert_eq!(mc.angles[1].spine.len(), 1);
    }
}
