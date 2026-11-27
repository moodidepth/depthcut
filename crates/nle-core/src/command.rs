//! Undoable edit commands and the undo/redo stack.
//!
//! Every edit goes through a [`Command`]. The [`History`] keeps a
//! `Vec<AppliedCommand>` for undo and another for redo; each entry stores the
//! timeline state before *and* after the command, so undo/redo restore the
//! exact same state (including freshly generated clip ids) every time.

use crate::captions::CaptionTrack;
use crate::clip::{AudioRole, Clip, Transition};
use crate::edit_ops::CopiedClip;
use crate::ids::ClipId;
use crate::speed::Retime;
use crate::time::Time;
use crate::timeline::{ClipLoc, EditError, EditResult, Marker, Timeline, VideoFormat};
use crate::tracks::TrackState;

#[derive(Clone, Debug, PartialEq)]
pub enum Command {
    Append {
        clip: Clip,
    },
    Insert {
        at: Time,
        clip: Clip,
    },
    Overwrite {
        at: Time,
        clip: Clip,
    },
    Connect {
        clip: Clip,
        at: Time,
        lane: i32,
    },
    DeleteRipple {
        id: ClipId,
    },
    DeleteWithGap {
        id: ClipId,
    },
    DeleteRange {
        start: Time,
        end: Time,
    },
    /// Track targeting: overwrite onto a lane (0 = storyline).
    OverwriteLane {
        at: Time,
        clip: Clip,
        lane: i32,
    },
    /// Track targeting: ripple everything and place the clip on a lane.
    InsertLane {
        at: Time,
        clip: Clip,
        lane: i32,
    },
    /// Clear `[start, end)` on `lanes` (all when empty), nothing moves.
    LiftRange {
        start: Time,
        end: Time,
        lanes: Vec<i32>,
    },
    /// Clear `[start, end)` on `lanes` (all when empty) and close the gap.
    ExtractRange {
        start: Time,
        end: Time,
        lanes: Vec<i32>,
    },
    Blade {
        at: Time,
    },
    BladeClip {
        id: ClipId,
        at: Time,
    },
    BladeAll {
        at: Time,
    },
    TrimStart {
        id: ClipId,
        delta: Time,
    },
    TrimEnd {
        id: ClipId,
        delta: Time,
    },
    RippleTrimStartTo {
        id: ClipId,
        at: Time,
    },
    RippleTrimEndTo {
        id: ClipId,
        at: Time,
    },
    SyncClipsToBeats {
        ids: Vec<ClipId>,
    },
    Slip {
        id: ClipId,
        delta: Time,
    },
    Slide {
        id: ClipId,
        delta: Time,
    },
    /// Roll the edit point after spine clip `left` (length-neutral).
    Roll {
        left: ClipId,
        delta: Time,
    },
    /// J/L split edit: extend a clip's audio before / after its video.
    SetAudioSplit {
        id: ClipId,
        lead: Time,
        tail: Time,
    },
    MoveClip {
        id: ClipId,
        at: Time,
        lane: i32,
    },
    MoveSpine {
        id: ClipId,
        to_index: usize,
    },
    LiftToLane {
        id: ClipId,
        lane: i32,
    },
    MakeCompound {
        ids: Vec<ClipId>,
        name: String,
    },
    BreakApart {
        id: ClipId,
    },
    SetRetime {
        id: ClipId,
        retime: Option<Retime>,
    },
    FreezeFrame {
        id: ClipId,
        at: Time,
        hold: Time,
    },
    SetTransition {
        id: ClipId,
        at_end: bool,
        transition: Option<Transition>,
    },
    /// Replace a clip's parameters (inspector edits, keyframes, titles...).
    /// Structural fields (id, kind's identity, connected clips, duration)
    /// are preserved from the timeline.
    UpdateClip {
        clip: Box<Clip>,
    },
    /// Replace a clip wholesale *including* its timing (duration, source in,
    /// retime) — montage presets. Lane, offset and connected clips are kept.
    ReplaceClip {
        label: &'static str,
        clip: Box<Clip>,
    },
    ToggleEnabled {
        id: ClipId,
    },
    /// Give a clip `role`, and every clip inside it when it is a compound or
    /// multicam (their sound routes by their own roles).
    SetRole {
        id: ClipId,
        role: AudioRole,
    },
    /// Replace all timeline markers.
    SetMarkers {
        markers: Vec<Marker>,
    },
    /// Replace the captions track.
    SetCaptions {
        captions: Box<CaptionTrack>,
    },
    /// Replace the mixer's bus settings.
    SetMixer {
        mixer: Box<crate::audio_fx::MixerSettings>,
    },
    /// Insert a gap at `at` (splits the storyline clip under it).
    InsertGap {
        at: Time,
        duration: Time,
    },
    /// Several commands applied as one atomic undo step (all or nothing).
    Batch {
        label: &'static str,
        commands: Vec<Command>,
    },
    /// Apply `cmd` inside compound clip / secondary storyline `container`
    /// (step-in editing; build nested paths with [`Command::in_path`]).
    InContainer {
        container: ClipId,
        cmd: Box<Command>,
    },
    /// Group connected clips into a secondary storyline.
    MakeStoryline {
        ids: Vec<ClipId>,
    },
    /// Position tool: move without rippling (gap behind, overwrite at target).
    PositionMove {
        id: ClipId,
        at: Time,
        lane: i32,
    },
    /// Edit spine clips `ids` while the clips connected to them stay at
    /// their timeline positions (hold ` to override connections).
    HoldConnections {
        ids: Vec<ClipId>,
        cmd: Box<Command>,
    },
    /// Move a connected clip's connection point (Ctrl+Alt+Click).
    SetConnectionPoint {
        id: ClipId,
        at: Time,
    },
    AddAuditionTake {
        id: ClipId,
        take: Box<Clip>,
    },
    /// Make the take `step` places along the active audition pick.
    CycleAudition {
        id: ClipId,
        step: i32,
    },
    FinalizeAudition {
        id: ClipId,
    },
    /// The project's frame size and rate.
    SetFormat {
        format: VideoFormat,
    },
    /// The timeline's (and so the project's) name.
    Rename {
        name: String,
    },
    /// Eye / mute / solo / lock of one lane.
    SetTrack {
        track: TrackState,
    },
    Paste {
        items: Vec<CopiedClip>,
        at: Time,
    },
    Duplicate {
        ids: Vec<ClipId>,
    },
    DetachAudio {
        id: ClipId,
    },
    Unlink {
        id: ClipId,
    },
    Link {
        a: ClipId,
        b: ClipId,
    },
    /// Constant speed over the media the clip uses (negative = reverse).
    SetSpeed {
        id: ClipId,
        speed: f64,
    },
    /// Show / hear another angle of multicam clip `id`, cutting it at `at`
    /// first (no cut at its own start).
    SwitchAngle {
        id: ClipId,
        at: Option<Time>,
        video: Option<usize>,
        audio: Option<usize>,
    },
}

impl Command {
    pub fn label(&self) -> &'static str {
        match self {
            Command::Append { .. } => "Append",
            Command::Insert { .. } => "Insert",
            Command::Overwrite { .. } => "Overwrite",
            Command::Connect { .. } => "Connect",
            Command::DeleteRipple { .. } => "Delete",
            Command::DeleteWithGap { .. } => "Replace with Gap",
            Command::DeleteRange { .. } => "Delete Range",
            Command::OverwriteLane { .. } => "Overwrite",
            Command::InsertLane { .. } => "Insert",
            Command::LiftRange { .. } => "Lift",
            Command::ExtractRange { .. } => "Extract",
            Command::Blade { .. } | Command::BladeClip { .. } => "Blade",
            Command::BladeAll { .. } => "Blade All",
            Command::TrimStart { .. } | Command::TrimEnd { .. } => "Trim",
            Command::RippleTrimStartTo { .. } => "Ripple Trim Start",
            Command::RippleTrimEndTo { .. } => "Ripple Trim End",
            Command::SyncClipsToBeats { .. } => "Sync Clips to Beats",
            Command::Slip { .. } => "Slip",
            Command::Slide { .. } => "Slide",
            Command::Roll { .. } => "Roll Edit",
            Command::SetAudioSplit { .. } => "Split Edit (J/L Cut)",
            Command::MoveClip { .. } | Command::MoveSpine { .. } => "Move",
            Command::LiftToLane { .. } => "Lift from Storyline",
            Command::MakeCompound { .. } => "New Compound Clip",
            Command::BreakApart { .. } => "Break Apart Clip Items",
            Command::SetRetime { .. } => "Retime",
            Command::FreezeFrame { .. } => "Freeze Frame",
            Command::SetTransition { .. } => "Transition",
            Command::UpdateClip { .. } => "Change Parameters",
            Command::ReplaceClip { label, .. } => label,
            Command::ToggleEnabled { .. } => "Enable/Disable",
            Command::SetRole { .. } => "Set Role",
            Command::SetMarkers { .. } => "Markers",
            Command::SetCaptions { .. } => "Captions",
            Command::SetMixer { .. } => "Mixer",
            Command::InsertGap { .. } => "Insert Gap",
            Command::Batch { label, .. } => label,
            Command::InContainer { cmd, .. } | Command::HoldConnections { cmd, .. } => cmd.label(),
            Command::MakeStoryline { .. } => "Create Storyline",
            Command::PositionMove { .. } => "Position",
            Command::SetConnectionPoint { .. } => "Change Connection Point",
            Command::AddAuditionTake { .. } => "Add to Audition",
            Command::CycleAudition { .. } => "Audition Pick",
            Command::FinalizeAudition { .. } => "Finalize Audition",
            Command::SetFormat { .. } => "Project Format",
            Command::Rename { .. } => "Rename Project",
            Command::SetTrack { .. } => "Track Controls",
            Command::Paste { .. } => "Paste",
            Command::Duplicate { .. } => "Duplicate",
            Command::DetachAudio { .. } => "Detach Audio",
            Command::Unlink { .. } => "Unlink",
            Command::Link { .. } => "Link",
            Command::SetSpeed { .. } => "Speed",
            Command::SwitchAngle { at: Some(_), .. } => "Cut and Switch Angle",
            Command::SwitchAngle { .. } => "Switch Angle",
        }
    }

    /// Whether this always applies to the root timeline, even while stepped in.
    pub(crate) fn is_project_wide(&self) -> bool {
        matches!(self, Command::SetFormat { .. } | Command::Rename { .. })
    }

    /// The clip a single-clip timing edit is aimed at.
    pub(crate) fn target(&self) -> Option<ClipId> {
        match self {
            Command::TrimStart { id, .. }
            | Command::TrimEnd { id, .. }
            | Command::RippleTrimStartTo { id, .. }
            | Command::RippleTrimEndTo { id, .. }
            | Command::Slip { id, .. }
            | Command::BladeClip { id, .. }
            | Command::DeleteRipple { id }
            | Command::DeleteWithGap { id }
            | Command::ToggleEnabled { id }
            | Command::SetRetime { id, .. }
            | Command::SetSpeed { id, .. }
            | Command::FreezeFrame { id, .. }
            | Command::MoveClip { id, .. }
            | Command::PositionMove { id, .. } => Some(*id),
            _ => None,
        }
    }

    /// Wrap `cmd` so it applies inside the nested containers of `path`
    /// (outermost first: the step-in breadcrumb). Each level resizes its
    /// container to its new contents.
    pub fn in_path(path: &[ClipId], cmd: Command) -> Command {
        path.iter().rev().fold(cmd, |cmd, &container| Command::InContainer { container, cmd: Box::new(cmd) })
    }

    /// Whether this command can move clips (and so the speech under captions).
    fn moves_media(&self) -> bool {
        // a Batch re-anchors around each of its sub-commands instead
        !matches!(
            self,
            Command::SetCaptions { .. }
                | Command::SetMarkers { .. }
                | Command::SetRole { .. }
                | Command::SetMixer { .. }
                | Command::SetFormat { .. }
                | Command::Rename { .. }
                | Command::SetTrack { .. }
                | Command::Batch { .. }
        )
    }

    /// Apply to `tl`. On error the timeline may be partially modified; the
    /// [`History`] restores the snapshot in that case.
    ///
    /// Captions follow the speech they transcribe through every edit
    /// (see [`crate::caption_follow`]).
    pub fn apply(&self, tl: &mut Timeline) -> EditResult<()> {
        if !self.moves_media() {
            return self.apply_inner(tl);
        }
        let edit = |tl: &mut Timeline| crate::tracks::with_locks(tl, |tl| self.apply_linked(tl));
        if tl.captions.segments.is_empty() {
            return edit(tl);
        }
        let anchors = crate::caption_follow::anchor_captions(tl);
        edit(tl)?;
        crate::caption_follow::follow_edit(tl, &anchors);
        Ok(())
    }

    /// Apply, keeping linked audio/video pairs together.
    fn apply_linked(&self, tl: &mut Timeline) -> EditResult<()> {
        if matches!(self, Command::Batch { .. }) {
            return self.apply_inner(tl); // each of its commands follows for itself
        }
        let pairs = crate::links::pairs(tl);
        if pairs.is_empty() {
            return self.apply_inner(tl);
        }
        let redirected = crate::links::redirect(self, &pairs);
        let cmd = match &redirected {
            Some((cmd, relane)) => {
                if let Some((id, lane)) = *relane {
                    tl.clip_mut(id).ok_or(EditError::NotFound(id))?.lane = lane;
                }
                cmd
            }
            None => self,
        };
        cmd.apply_inner(tl)?;
        crate::links::follow(tl, cmd, &pairs)
    }

    fn apply_inner(&self, tl: &mut Timeline) -> EditResult<()> {
        match self {
            Command::Append { clip } => tl.append(clip.clone()).map(drop),
            Command::Insert { at, clip } => tl.insert(*at, clip.clone()).map(drop),
            Command::Overwrite { at, clip } => tl.overwrite(*at, clip.clone()).map(drop),
            Command::Connect { clip, at, lane } => tl.connect(clip.clone(), *at, *lane).map(drop),
            Command::DeleteRipple { id } => tl.remove_ripple(*id).map(drop),
            Command::DeleteWithGap { id } => tl.remove_with_gap(*id).map(drop),
            Command::DeleteRange { start, end } => tl.range_delete(*start, *end).map(drop),
            Command::OverwriteLane { at, clip, lane } => tl.overwrite_on_lane(*at, clip.clone(), *lane).map(drop),
            Command::InsertLane { at, clip, lane } => tl.insert_on_lane(*at, clip.clone(), *lane).map(drop),
            Command::LiftRange { start, end, lanes } => tl.lift_range(*start, *end, lanes).map(drop),
            Command::ExtractRange { start, end, lanes } => tl.extract_range(*start, *end, lanes).map(drop),
            Command::Blade { at } => tl.blade(*at).map(drop),
            Command::BladeClip { id, at } => tl.blade_clip(*id, *at).map(drop),
            Command::BladeAll { at } => {
                if tl.blade_all(*at) == 0 {
                    Err(EditError::Nothing)
                } else {
                    Ok(())
                }
            }
            Command::TrimStart { id, delta } => tl.trim_start(*id, *delta).map(drop),
            Command::TrimEnd { id, delta } => tl.trim_end(*id, *delta).map(drop),
            Command::RippleTrimStartTo { id, at } => tl.ripple_trim_start_to(*id, *at).map(drop),
            Command::RippleTrimEndTo { id, at } => tl.ripple_trim_end_to(*id, *at).map(drop),
            Command::SyncClipsToBeats { ids } => {
                if tl.sync_clips_to_beats(ids) == 0 {
                    Err(EditError::Nothing)
                } else {
                    Ok(())
                }
            }
            Command::Slip { id, delta } => tl.slip(*id, *delta).map(drop),
            Command::Slide { id, delta } => tl.slide(*id, *delta).map(drop),
            Command::Roll { left, delta } => tl.roll(*left, *delta).map(drop),
            Command::SetAudioSplit { id, lead, tail } => tl.set_audio_split(*id, *lead, *tail),
            Command::MoveClip { id, at, lane } => tl.move_clip(*id, *at, *lane).map(drop),
            Command::MoveSpine { id, to_index } => tl.move_spine(*id, *to_index).map(drop),
            Command::LiftToLane { id, lane } => tl.lift_to_lane(*id, *lane).map(drop),
            Command::MakeCompound { ids, name } => tl.make_compound(ids, name).map(drop),
            Command::BreakApart { id } => tl.break_apart(*id).map(drop),
            Command::SetRetime { id, retime } => {
                let c = tl.clip_mut(*id).ok_or(EditError::NotFound(*id))?;
                if let Some(r) = retime {
                    c.duration = r.timeline_duration();
                } else if let Some(old) = &c.retime {
                    // drop retiming: keep the same source span at 100 %
                    let (lo, hi) = old.source_span();
                    c.source_in += lo;
                    c.duration = (hi - lo).max(Time(1));
                }
                c.retime = retime.clone();
                Ok(())
            }
            Command::FreezeFrame { id, at, hold } => {
                let c = tl.clip_mut(*id).ok_or(EditError::NotFound(*id))?;
                let mut r = c.retime.clone().unwrap_or_else(|| Retime::uniform(c.duration, 1.0));
                r.freeze_frame(*at, *hold);
                c.duration = r.timeline_duration();
                c.retime = Some(r);
                Ok(())
            }
            Command::SetTransition { id, at_end, transition } => {
                // a transition on a connected clip needs a storyline to live in
                if transition.is_some() && matches!(tl.locate(*id), Some(ClipLoc::Connected { .. })) && !tl.clip(*id).is_some_and(Clip::is_storyline) {
                    tl.make_storyline(&[*id])?;
                }
                let c = tl.clip_mut(*id).ok_or(EditError::NotFound(*id))?;
                if *at_end {
                    c.transition_out = transition.clone();
                } else {
                    c.transition_in = transition.clone();
                }
                Ok(())
            }
            Command::UpdateClip { clip } => {
                let c = tl.clip_mut(clip.id).ok_or(EditError::NotFound(clip.id))?;
                let connected = std::mem::take(&mut c.connected);
                let (duration, lane, offset, link) = (c.duration, c.lane, c.offset, c.link);
                let kind_is_compound = matches!(c.kind, crate::clip::ClipKind::Compound(_));
                let old_kind = c.kind.clone();
                *c = (**clip).clone();
                c.connected = connected;
                c.duration = duration;
                c.lane = lane;
                c.offset = offset;
                c.link = link;
                if kind_is_compound {
                    c.kind = old_kind;
                }
                Ok(())
            }
            Command::ReplaceClip { clip, .. } => {
                if clip.duration <= Time::ZERO {
                    return Err(EditError::Nothing);
                }
                let c = tl.clip_mut(clip.id).ok_or(EditError::NotFound(clip.id))?;
                let connected = std::mem::take(&mut c.connected);
                let (lane, offset, link) = (c.lane, c.offset, c.link);
                let old_kind = matches!(c.kind, crate::clip::ClipKind::Compound(_)).then(|| c.kind.clone());
                *c = (**clip).clone();
                c.connected = connected;
                c.lane = lane;
                c.offset = offset;
                c.link = link;
                if let Some(k) = old_kind {
                    c.kind = k;
                }
                Ok(())
            }
            Command::ToggleEnabled { id } => {
                let c = tl.clip_mut(*id).ok_or(EditError::NotFound(*id))?;
                c.enabled = !c.enabled;
                Ok(())
            }
            Command::SetRole { id, role } => {
                let c = tl.clip_mut(*id).ok_or(EditError::NotFound(*id))?;
                c.audio.role = *role;
                if let crate::clip::ClipKind::Compound(comp) = &mut c.kind {
                    comp.set_roles(*role);
                }
                Ok(())
            }
            Command::SetMarkers { markers } => {
                let mut m = markers.clone();
                m.sort_by_key(|m| m.time);
                tl.markers = m;
                Ok(())
            }
            Command::SetCaptions { captions } => {
                let mut c = (**captions).clone();
                c.sort();
                tl.captions = c;
                Ok(())
            }
            Command::SetMixer { mixer } => {
                tl.mixer = (**mixer).clone();
                Ok(())
            }
            Command::InsertGap { at, duration } => {
                if !duration.is_positive() {
                    return Err(EditError::Nothing);
                }
                tl.insert(*at, Clip::gap(*duration)).map(drop)
            }
            Command::Batch { commands, .. } => {
                if commands.is_empty() {
                    return Err(EditError::Nothing);
                }
                // History snapshots `tl` first, so a failure here rolls back the whole batch
                // Both halves of a linked pair are often selected together: the
                // first one's edit already covers the second.
                let placed = tl.placed_clips();
                let links: std::collections::HashMap<ClipId, u64> = placed.iter().filter_map(|p| Some((p.clip.id, p.clip.link?))).collect();
                let mut done = Vec::new();
                for c in commands {
                    if let Some((id, link)) = c.target().and_then(|id| Some((id, *links.get(&id)?))) {
                        let kind = std::mem::discriminant(c);
                        if done.iter().any(|&(k, l, other)| k == kind && l == link && other != id) {
                            continue;
                        }
                        done.push((kind, link, id));
                    }
                    c.apply(tl)?;
                }
                Ok(())
            }
            // lanes inside a container have no track controls of their own
            Command::InContainer { cmd, .. } if matches!(**cmd, Command::SetTrack { .. }) => Err(EditError::Nothing),
            Command::InContainer { container, cmd } => tl.edit_inside(*container, |inner| cmd.apply_linked(inner)),
            Command::MakeStoryline { ids } => tl.make_storyline(ids).map(drop),
            Command::PositionMove { id, at, lane } => tl.position_move(*id, *at, *lane).map(drop),
            Command::HoldConnections { ids, cmd } => {
                let held = tl.detach_connected(ids);
                cmd.apply_inner(tl)?;
                tl.reattach(held)
            }
            Command::SetConnectionPoint { id, at } => tl.set_connection_point(*id, *at).map(drop),
            Command::AddAuditionTake { id, take } => tl.add_audition_take(*id, (**take).clone()),
            Command::CycleAudition { id, step } => tl.cycle_audition(*id, *step),
            Command::FinalizeAudition { id } => tl.finalize_audition(*id),
            Command::SetFormat { format } => {
                if format.width == 0 || format.height == 0 {
                    return Err(EditError::OutOfRange);
                }
                tl.format = *format;
                tl.format_chosen = true;
                Ok(())
            }
            Command::Rename { name } => {
                tl.name.clone_from(name);
                Ok(())
            }
            Command::SetTrack { track } => {
                tl.set_track(*track);
                Ok(())
            }
            Command::Paste { items, at } => tl.paste_clips(items, *at).map(drop),
            Command::Duplicate { ids } => tl.duplicate_clips(ids).map(drop),
            Command::DetachAudio { id } => tl.detach_audio(*id).map(drop),
            Command::Unlink { id } => tl.unlink(*id),
            Command::Link { a, b } => tl.link_clips(*a, *b),
            Command::SetSpeed { id, speed } => tl.set_speed(*id, *speed),
            Command::SwitchAngle { id, at, video, audio } => {
                let target = match at {
                    Some(t) => match tl.blade_clip(*id, *t) {
                        Ok((_, right)) => right,
                        Err(EditError::AtBoundary) => *id,
                        Err(e) => return Err(e),
                    },
                    None => *id,
                };
                let c = tl.clip_mut(target).ok_or(EditError::NotFound(target))?;
                // the same angle again changes nothing (the cut is rolled back too)
                if crate::multicam::switch(c, *video, *audio) {
                    Ok(())
                } else {
                    Err(EditError::Nothing)
                }
            }
        }
    }
}

/// A command together with the states it moved between.
#[derive(Clone, Debug)]
pub struct AppliedCommand {
    pub command: Command,
    pub before: Timeline,
    pub after: Timeline,
    coalesce_key: Option<u64>,
    /// Identifies the `after` state (see [`History::state_id`]).
    state: u64,
}

#[derive(Clone, Debug)]
pub struct History {
    undo: Vec<AppliedCommand>,
    redo: Vec<AppliedCommand>,
    limit: usize,
    /// Monotonic counter bumped on every change (cache invalidation).
    revision: u64,
    /// Source of fresh state ids.
    next_state: u64,
    /// State id of the timeline below the oldest undo entry.
    floor_state: u64,
}

impl Default for History {
    fn default() -> Self {
        History::new(500)
    }
}

impl History {
    pub fn new(limit: usize) -> Self {
        History { undo: Vec::new(), redo: Vec::new(), limit: limit.max(1), revision: 0, next_state: 1, floor_state: 0 }
    }

    fn fresh_state(&mut self) -> u64 {
        self.next_state += 1;
        self.next_state
    }

    /// Identifies the current timeline state: undoing back to a state gives
    /// the same id again, so comparing against the id stored at save time
    /// tells whether the project differs from what is on disk.
    pub fn state_id(&self) -> u64 {
        self.undo.last().map(|e| e.state).unwrap_or(self.floor_state)
    }

    pub fn revision(&self) -> u64 {
        self.revision
    }
    pub fn can_undo(&self) -> bool {
        !self.undo.is_empty()
    }
    pub fn can_redo(&self) -> bool {
        !self.redo.is_empty()
    }
    pub fn undo_label(&self) -> Option<&'static str> {
        self.undo.last().map(|c| c.command.label())
    }
    pub fn redo_label(&self) -> Option<&'static str> {
        self.redo.last().map(|c| c.command.label())
    }
    pub fn undo_len(&self) -> usize {
        self.undo.len()
    }
    pub fn redo_len(&self) -> usize {
        self.redo.len()
    }

    /// Apply `cmd` to `tl` and record it. On failure `tl` is left untouched.
    pub fn execute(&mut self, tl: &mut Timeline, cmd: Command) -> EditResult<()> {
        self.execute_coalesced(tl, cmd, None)
    }

    /// Like [`execute`](Self::execute), but consecutive commands sharing the
    /// same `key` (e.g. one slider drag) merge into a single undo step.
    pub fn execute_coalesced(&mut self, tl: &mut Timeline, cmd: Command, key: Option<u64>) -> EditResult<()> {
        let before = tl.clone();
        if let Err(e) = cmd.apply(tl) {
            *tl = before;
            return Err(e);
        }
        self.revision += 1;
        self.redo.clear();
        if let (Some(k), Some(top)) = (key, self.undo.last_mut()) {
            if top.coalesce_key == Some(k) {
                top.after = tl.clone();
                top.command = cmd;
                self.next_state += 1;
                top.state = self.next_state;
                return Ok(());
            }
        }
        let state = self.fresh_state();
        self.undo.push(AppliedCommand { command: cmd, before, after: tl.clone(), coalesce_key: key, state });
        if self.undo.len() > self.limit {
            let excess = self.undo.len() - self.limit;
            self.floor_state = self.undo[excess - 1].state;
            self.undo.drain(..excess);
        }
        Ok(())
    }

    /// End any in-progress coalescing so the next edit is a new undo step.
    pub fn seal(&mut self) {
        if let Some(top) = self.undo.last_mut() {
            top.coalesce_key = None;
        }
    }

    pub fn undo(&mut self, tl: &mut Timeline) -> bool {
        match self.undo.pop() {
            Some(mut entry) => {
                let snapping = tl.snapping; // a switch, not an edit
                *tl = entry.before.clone();
                tl.snapping = snapping;
                entry.coalesce_key = None;
                self.redo.push(entry);
                self.revision += 1;
                true
            }
            None => false,
        }
    }

    pub fn redo(&mut self, tl: &mut Timeline) -> bool {
        match self.redo.pop() {
            Some(entry) => {
                let snapping = tl.snapping;
                *tl = entry.after.clone();
                tl.snapping = snapping;
                self.undo.push(entry);
                self.revision += 1;
                true
            }
            None => false,
        }
    }

    pub fn clear(&mut self) {
        self.undo.clear();
        self.redo.clear();
        self.revision += 1;
        self.floor_state = self.fresh_state();
    }
}

/// Convenience: is `id` on the primary storyline?
pub fn is_spine(tl: &Timeline, id: ClipId) -> bool {
    matches!(tl.locate(id), Some(ClipLoc::Spine(_)))
}
