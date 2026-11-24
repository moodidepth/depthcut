//! Linked audio/video pairs (Detach Audio).
//!
//! A pair is a clip with picture and an audio-only clip sharing `Clip::link`.
//! The picture clip leads: a timing edit aimed at the audio is redirected to
//! it, and after every command the audio follows whatever happened to it
//! (trim, split, slip, move, retime, delete, enable). Unlink to edit apart.

use crate::clip::Clip;
use crate::command::Command;
use crate::ids::ClipId;
use crate::time::Time;
use crate::timeline::{EditResult, Timeline};

fn bare(c: &Clip) -> Clip {
    let mut c = c.clone();
    c.connected.clear();
    c
}

fn same_timing(a: &(Time, Clip), b: &(Time, Clip)) -> bool {
    a.0 == b.0 && a.1.source_in == b.1.source_in && a.1.duration == b.1.duration && a.1.retime == b.1.retime && a.1.enabled == b.1.enabled
}

pub(crate) struct Pair {
    link: u64,
    video: (Time, Clip),
    audio: (Time, Clip),
}

/// Every linked pair at the top level of `tl`.
pub(crate) fn pairs(tl: &Timeline) -> Vec<Pair> {
    if !tl.spine.iter().any(|p| p.link.is_some() || p.connected.iter().any(|c| c.link.is_some())) {
        return Vec::new();
    }
    let placed = tl.placed_clips();
    let mut out = Vec::new();
    for v in placed.iter().filter(|p| p.clip.av.video) {
        let Some(link) = v.clip.link else { continue };
        if let Some(a) = placed.iter().find(|p| p.clip.link == Some(link) && !p.clip.av.video) {
            out.push(Pair { link, video: (v.start, bare(v.clip)), audio: (a.start, bare(a.clip)) });
        }
    }
    out
}

/// The command to run when `cmd` aims a timing edit at a linked audio clip:
/// the same edit on its picture clip, plus the lane it was dragged to.
pub(crate) fn redirect(cmd: &Command, pairs: &[Pair]) -> Option<(Command, Option<(ClipId, i32)>)> {
    let target = cmd.target()?;
    let p = pairs.iter().find(|p| p.audio.1.id == target)?;
    let id = p.video.1.id;
    let (head, tail) = (p.audio.0 - p.video.0, (p.audio.0 + p.audio.1.duration) - (p.video.0 + p.video.1.duration));
    let mut relane = None;
    let redirected = match cmd {
        Command::TrimStart { delta, .. } => Command::TrimStart { id, delta: *delta },
        Command::TrimEnd { delta, .. } => Command::TrimEnd { id, delta: *delta },
        Command::RippleTrimStartTo { at, .. } => Command::RippleTrimStartTo { id, at: *at - head },
        Command::RippleTrimEndTo { at, .. } => Command::RippleTrimEndTo { id, at: *at - tail },
        Command::Slip { delta, .. } => Command::Slip { id, delta: *delta },
        Command::BladeClip { at, .. } => Command::BladeClip { id, at: *at },
        Command::DeleteRipple { .. } => Command::DeleteRipple { id },
        Command::DeleteWithGap { .. } => Command::DeleteWithGap { id },
        Command::ToggleEnabled { .. } => Command::ToggleEnabled { id },
        Command::SetRetime { retime, .. } => Command::SetRetime { id, retime: retime.clone() },
        Command::SetSpeed { speed, .. } => Command::SetSpeed { id, speed: *speed },
        Command::FreezeFrame { at, hold, .. } => Command::FreezeFrame { id, at: *at + head, hold: *hold },
        Command::MoveClip { at, lane, .. } | Command::PositionMove { at, lane, .. } => {
            if *lane != 0 && *lane != p.audio.1.lane {
                if *at == p.audio.0 {
                    return None; // only a change of lane
                }
                relane = Some((p.audio.1.id, *lane));
            }
            let (at, lane) = (*at - head, p.video.1.lane);
            if matches!(cmd, Command::MoveClip { .. }) {
                Command::MoveClip { id, at, lane }
            } else {
                Command::PositionMove { id, at, lane }
            }
        }
        _ => return None,
    };
    Some((redirected, relane))
}

/// Make the audio of each of `pairs` follow what `cmd` did to its picture.
pub(crate) fn follow(tl: &mut Timeline, cmd: &Command, pairs: &[Pair]) -> EditResult<()> {
    for p in pairs {
        follow_pair(tl, cmd, p)?;
    }
    Ok(())
}

fn follow_pair(tl: &mut Timeline, cmd: &Command, p: &Pair) -> EditResult<()> {
    let (v0_start, v0) = (p.video.0, &p.video.1);
    let (a0_start, a0) = (p.audio.0, &p.audio.1);

    let mut videos: Vec<(Time, Clip)> = Vec::new();
    let mut audios: Vec<(Time, Clip)> = Vec::new();
    for c in tl.placed_clips() {
        if c.clip.link == Some(p.link) {
            if c.clip.av.video { &mut videos } else { &mut audios }.push((c.start, bare(c.clip)));
        }
    }
    if videos.is_empty() {
        if tl.clip(v0.id).is_none() {
            // the picture was deleted
            for (_, a) in &audios {
                tl.remove_ripple(a.id)?;
            }
        }
        return Ok(()); // else unlinked, or moved into a container
    }
    if audios.is_empty() && tl.clip(a0.id).is_some() {
        return Ok(()); // unlinked, or moved into a container
    }
    if videos.len() == 1 && audios.len() == 1 && same_timing(&videos[0], &p.video) && same_timing(&audios[0], &p.audio) {
        return Ok(());
    }
    videos.sort_by_key(|v| v.0);
    let lane = audios.iter().find(|a| a.1.id == a0.id).map_or(a0.lane, |a| a.1.lane);
    for (_, a) in &audios {
        tl.remove_ripple(a.id)?;
    }

    let (head, tail) = (a0_start - v0_start, (a0_start + a0.duration) - (v0_start + v0.duration));
    let split = videos.len() > 1;
    let last = videos.len() - 1;
    let mut played = Time::ZERO;
    let mut made: Vec<(Time, Clip)> = Vec::new();
    for (i, (v_start, v)) in videos.iter().enumerate() {
        let link = if i == 0 { p.link } else { ClipId::new().0 };
        tl.clip_mut(v.id).expect("placed above").link = Some(link);

        let mut a = a0.clone();
        a.link = Some(link);
        if i > 0 {
            a.remint_ids();
        }
        a.lane = lane;
        a.enabled = v.enabled;
        // the pair's original extra audio belongs only at the outer edges
        let mut h = if i == 0 { head } else { Time::ZERO };
        let mut t = if i == last { tail } else { Time::ZERO };
        let local; // where this piece starts in the original audio
        if a0.retime.is_none() && v.retime.is_none() {
            h = h.max(-*v_start);
            let mut source_in = a0.source_in + (v.source_in - v0.source_in) + (h - head);
            if source_in.is_negative() {
                h -= source_in;
                source_in = Time::ZERO;
            }
            if let Some(md) = a0.media_duration {
                t = t.min(md - source_in - v.duration + h);
            }
            local = if matches!(cmd, Command::Slip { .. }) { Time::ZERO } else { source_in - a0.source_in };
            a.source_in = source_in;
        } else {
            // retimed: the audio takes the picture's timing exactly
            (h, t) = (Time::ZERO, Time::ZERO);
            local = if i > 0 {
                played
            } else if matches!(cmd, Command::TrimStart { .. } | Command::RippleTrimStartTo { .. }) {
                v0.duration - v.duration
            } else {
                Time::ZERO
            };
            a.source_in = v.source_in;
            a.retime = v.retime.clone();
        }
        played += v.duration;
        a.duration = v.duration + t - h;
        if !a.duration.is_positive() {
            continue;
        }
        if split {
            let end = a.duration;
            a.for_each_param_mut(|param| {
                param.keep_right(local);
                param.keep_left(end);
            });
            if i > 0 {
                a.audio.fade_in = Time::ZERO;
                a.audio.crossfade_in = Time::ZERO;
            }
            if i < last {
                a.audio.fade_out = Time::ZERO;
                a.audio.crossfade_out = Time::ZERO;
            }
        } else if !local.is_zero() {
            a.for_each_param_mut(|param| param.shift(-local));
        }
        a.anchor = (-h).max(Time::ZERO).min((a.duration - Time(1)).max(Time::ZERO));
        made.push((*v_start + h, a));
    }
    for (at, a) in made {
        tl.connect(a, at, lane)?;
    }
    Ok(())
}
