//! Step-in editing (FCP "Open in Timeline"): a compound clip or secondary
//! storyline opened in the timeline.
//!
//! While stepped in, the *shown* timeline holds the innermost container's
//! contents and the project's real timeline is kept here. Every edit is
//! wrapped with [`Command::in_path`] and applied to the real timeline, so it
//! is one ordinary undo step that also resizes the enclosing clips; the
//! shown timeline is then rebuilt from the result.

use crate::timeline::EditResult;
use crate::{ClipId, ClipKind, Command, EditError, History, Time, Timeline};

/// One opened container, outermost first.
#[derive(Clone, Debug)]
pub struct FocusLevel {
    pub id: ClipId,
    pub name: String,
    /// Playhead in the enclosing timeline, restored on the way out.
    pub outer_playhead: Time,
}

#[derive(Default)]
pub struct Focus {
    levels: Vec<FocusLevel>,
    /// The real timeline while `levels` is non-empty.
    root: Option<Timeline>,
    /// Set when levels were dropped because their container vanished (an
    /// undo removed it): the playhead to return to.
    restored: Option<Time>,
}

impl Focus {
    pub fn levels(&self) -> &[FocusLevel] {
        &self.levels
    }

    pub fn is_empty(&self) -> bool {
        self.levels.is_empty()
    }

    /// The project's real timeline (`shown` itself when not stepped in).
    pub fn root<'a>(&'a self, shown: &'a Timeline) -> &'a Timeline {
        self.root.as_ref().unwrap_or(shown)
    }

    /// Forget all levels without restoring anything (the shown timeline was
    /// replaced wholesale, e.g. by loading a project).
    pub fn reset(&mut self) {
        self.levels.clear();
        self.root = None;
        self.restored = None;
    }

    /// The playhead to restore after levels were dropped by an edit or undo.
    pub fn take_restored_playhead(&mut self) -> Option<Time> {
        self.restored.take()
    }

    /// Open compound clip / storyline `id` of the shown timeline. Returns the
    /// playhead inside it: the same frame when the playhead was over the clip,
    /// otherwise its first visible frame.
    pub fn step_into(&mut self, shown: &mut Timeline, id: ClipId, playhead: Time) -> EditResult<Time> {
        let c = shown.clip(id).ok_or(EditError::NotFound(id))?;
        let ClipKind::Compound(comp) = &c.kind else {
            return Err(EditError::NotCompound);
        };
        let inner = match (shown.clip_start(id), &c.retime) {
            (Some(s), None) if playhead >= s && playhead < s + c.duration => playhead - s + c.source_in,
            _ => c.source_in,
        }
        .min(comp.duration());
        let level = FocusLevel { id, name: c.name.clone(), outer_playhead: playhead };
        if self.root.is_none() {
            self.root = Some(shown.clone());
        }
        self.levels.push(level);
        self.refresh(shown);
        Ok(inner)
    }

    /// Step back out `levels` containers. Returns the playhead to restore, or
    /// `None` when not stepped in.
    pub fn step_out(&mut self, shown: &mut Timeline, levels: usize) -> Option<Time> {
        if self.levels.is_empty() {
            return None;
        }
        let mut playhead = None;
        for _ in 0..levels.min(self.levels.len()) {
            playhead = self.levels.pop().map(|f| f.outer_playhead);
        }
        self.refresh(shown);
        playhead
    }

    /// Execute `cmd` in the innermost open container (or on `shown` when not
    /// stepped in), coalescing like [`History::execute_coalesced`].
    pub fn execute(&mut self, shown: &mut Timeline, history: &mut History, cmd: Command, key: Option<u64>) -> EditResult<()> {
        let path: Vec<ClipId> = self.levels.iter().map(|f| f.id).collect();
        let Some(root) = self.root.as_mut() else {
            return history.execute_coalesced(shown, cmd, key);
        };
        root.snapping = shown.snapping;
        let cmd = if cmd.is_project_wide() { cmd } else { Command::in_path(&path, cmd) };
        let r = history.execute_coalesced(root, cmd, key);
        self.refresh(shown);
        r
    }

    pub fn undo(&mut self, shown: &mut Timeline, history: &mut History) -> bool {
        let done = history.undo(self.root.as_mut().unwrap_or(shown));
        self.refresh(shown);
        done
    }

    pub fn redo(&mut self, shown: &mut Timeline, history: &mut History) -> bool {
        let done = history.redo(self.root.as_mut().unwrap_or(shown));
        self.refresh(shown);
        done
    }

    /// Rebuild the shown timeline from the real one. Levels whose container
    /// no longer exists are dropped; with no levels left the real timeline
    /// goes back into `shown`.
    fn refresh(&mut self, shown: &mut Timeline) {
        let Some(root) = self.root.as_mut() else { return };
        // snapping is toggled on the shown timeline; keep it project-wide
        root.snapping = shown.snapping;
        let root = &*root;
        let mut view: Option<Timeline> = None;
        let mut keep = 0;
        for f in &mut self.levels {
            let cur = view.as_ref().unwrap_or(root);
            let Some(c) = cur.clip(f.id) else { break };
            let ClipKind::Compound(comp) = &c.kind else { break };
            let mut next = Timeline::nested(cur.format, comp.spine.clone());
            next.name = c.name.clone();
            next.snapping = root.snapping;
            // a renamed container shows up in the breadcrumb
            f.name = c.name.clone();
            view = Some(next);
            keep += 1;
        }
        if keep < self.levels.len() {
            self.restored = Some(self.levels[keep].outer_playhead);
            self.levels.truncate(keep);
        }
        match view {
            Some(v) => *shown = v,
            None => {
                if let Some(root) = self.root.take() {
                    *shown = root;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Clip, ClipKind, VideoFormat};

    fn secs(s: i64) -> Time {
        Time::from_secs(s)
    }

    fn solid(name: &str, s: i64) -> Clip {
        Clip::new(name, ClipKind::Adjustment, secs(s))
    }

    /// A(2s) | Compound[ B(3s) C(1s) ] | D(2s)
    fn timeline() -> (Timeline, ClipId) {
        let mut tl = Timeline::new("Project", VideoFormat::default());
        for (name, len) in [("A", 2), ("B", 3), ("C", 1), ("D", 2)] {
            tl.append(solid(name, len)).unwrap();
        }
        let ids: Vec<ClipId> = tl.spine[1..3].iter().map(|c| c.id).collect();
        let comp = tl.make_compound(&ids, "Comp").unwrap();
        (tl, comp)
    }

    #[test]
    fn edits_inside_a_container_resize_it_and_undo_across_step_out() {
        let (mut shown, comp) = timeline();
        let mut history = History::new(50);
        let mut focus = Focus::default();
        // playhead 3s is 1s into the compound
        let inner = focus.step_into(&mut shown, comp, secs(3)).unwrap();
        assert_eq!(inner, secs(1));
        assert_eq!(shown.name, "Comp");
        assert_eq!(shown.spine.len(), 2);
        assert_eq!(focus.root(&shown).spine.len(), 3);

        // delete C inside: the compound shrinks, D ripples left on the real timeline
        let c = shown.spine[1].id;
        focus.execute(&mut shown, &mut history, Command::DeleteRipple { id: c }, None).unwrap();
        assert_eq!(shown.spine.len(), 1);
        let root = focus.root(&shown);
        assert_eq!(root.spine[1].duration, secs(3));
        assert_eq!(root.extent(), secs(7));

        assert_eq!(focus.step_out(&mut shown, 1), Some(secs(3)));
        assert!(focus.is_empty());
        assert_eq!(shown.name, "Project");
        assert_eq!(shown.extent(), secs(7));
        // the inner edit is a normal undo step on the real timeline
        assert!(focus.undo(&mut shown, &mut history));
        assert_eq!(shown.extent(), secs(8));
        assert!(matches!(&shown.spine[1].kind, ClipKind::Compound(c) if c.spine.len() == 2));
    }

    #[test]
    fn nested_levels_and_undo_that_removes_the_container() {
        let (mut shown, comp) = timeline();
        let mut history = History::new(50);
        let mut focus = Focus::default();
        // make a compound *inside* the compound, through the focus
        focus.step_into(&mut shown, comp, Time::ZERO).unwrap();
        let ids: Vec<ClipId> = shown.spine.iter().map(|c| c.id).collect();
        let before = history.undo_len();
        focus.execute(&mut shown, &mut history, Command::MakeCompound { ids, name: "Inner".into() }, None).unwrap();
        assert_eq!(history.undo_len(), before + 1);
        let inner = shown.spine[0].id;
        focus.step_into(&mut shown, inner, Time::ZERO).unwrap();
        assert_eq!(focus.levels().len(), 2);
        assert_eq!(shown.name, "Inner");
        assert_eq!(shown.spine.len(), 2);

        // undo the inner compound creation while standing inside it: that
        // level vanishes and the view falls back to the outer compound
        assert!(focus.undo(&mut shown, &mut history));
        assert_eq!(focus.levels().len(), 1);
        assert_eq!(focus.take_restored_playhead(), Some(Time::ZERO));
        assert_eq!(shown.name, "Comp");
        assert_eq!(shown.spine.len(), 2);
        // redo does not re-enter it; the edit itself is back
        assert!(focus.redo(&mut shown, &mut history));
        assert_eq!(shown.spine.len(), 1);
        assert_eq!(focus.step_out(&mut shown, usize::MAX), Some(Time::ZERO));
        assert_eq!(shown.extent(), secs(8));
    }

    #[test]
    fn stepping_into_a_non_compound_is_refused_and_changes_nothing() {
        let (mut shown, _) = timeline();
        let a = shown.spine[0].id;
        let mut focus = Focus::default();
        assert_eq!(focus.step_into(&mut shown, a, Time::ZERO), Err(EditError::NotCompound));
        assert!(focus.is_empty());
        assert_eq!(focus.step_out(&mut shown, 1), None);
        assert_eq!(shown.spine.len(), 3);
    }
}
