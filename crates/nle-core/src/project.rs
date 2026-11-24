//! A project: media library, the open timeline and any other timelines.
//!
//! `timeline` is always the timeline being edited, so the rest of the editor
//! never has to look anything up. The project's other timelines wait in
//! `timelines`; [`Project::switch_timeline`] swaps one in. Indices used by the
//! timeline functions count all of them in display order, with the open one
//! at `active_timeline`.

use crate::asset::MediaAsset;
use crate::ids::{AssetId, ClipId};
use crate::library::Library;
use crate::timeline::Timeline;
use serde::{Deserialize, Serialize};
use std::path::Path;

pub const PROJECT_FORMAT_VERSION: u32 = 1;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Project {
    pub version: u32,
    pub name: String,
    pub assets: Vec<MediaAsset>,
    pub timeline: Timeline,
    /// The project's other timelines (not including `timeline`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub timelines: Vec<Timeline>,
    /// Position of `timeline` among all timelines, in display order.
    #[serde(default)]
    pub active_timeline: usize,
    /// Bins and smart collections.
    #[serde(default, skip_serializing_if = "Library::is_empty")]
    pub library: Library,
}

impl Default for Project {
    fn default() -> Self {
        Project::new("Untitled Project")
    }
}

impl Project {
    pub fn new(name: impl Into<String>) -> Self {
        let name = name.into();
        Project {
            version: PROJECT_FORMAT_VERSION,
            timeline: Timeline::new(name.clone(), Default::default()),
            name,
            assets: Vec::new(),
            timelines: Vec::new(),
            active_timeline: 0,
            library: Library::default(),
        }
    }

    pub fn asset(&self, id: AssetId) -> Option<&MediaAsset> {
        self.assets.iter().find(|a| a.id == id)
    }

    pub fn asset_mut(&mut self, id: AssetId) -> Option<&mut MediaAsset> {
        self.assets.iter_mut().find(|a| a.id == id)
    }

    pub fn add_asset(&mut self, asset: MediaAsset) -> AssetId {
        let id = asset.id;
        self.assets.push(asset);
        id
    }

    pub fn to_json(&self) -> serde_json::Result<String> {
        serde_json::to_string_pretty(self)
    }

    /// Deserialize and make sure freshly generated ids never collide with
    /// ids stored in the file.
    pub fn from_json(s: &str) -> serde_json::Result<Project> {
        let mut p: Project = serde_json::from_str(s)?;
        p.active_timeline = p.active_timeline.min(p.timelines.len());
        p.reserve_ids();
        Ok(p)
    }

    pub fn reserve_ids(&self) {
        let max = self.timelines.iter().map(Timeline::max_clip_id).fold(self.timeline.max_clip_id(), u64::max);
        ClipId::reserve_above(max);
        if let Some(m) = self.assets.iter().map(|a| a.id.0).max() {
            AssetId::reserve_above(m);
        }
    }

    // ----- several timelines -------------------------------------------------

    /// How many timelines the project has (always at least one).
    pub fn timeline_count(&self) -> usize {
        self.timelines.len() + 1
    }

    /// Timeline `i` in display order.
    pub fn timeline_at(&self, i: usize) -> Option<&Timeline> {
        use std::cmp::Ordering::*;
        match i.cmp(&self.active_timeline) {
            Equal => Some(&self.timeline),
            Less => self.timelines.get(i),
            Greater => self.timelines.get(i - 1),
        }
    }

    fn timeline_at_mut(&mut self, i: usize) -> Option<&mut Timeline> {
        use std::cmp::Ordering::*;
        match i.cmp(&self.active_timeline) {
            Equal => Some(&mut self.timeline),
            Less => self.timelines.get_mut(i),
            Greater => self.timelines.get_mut(i - 1),
        }
    }

    /// Every timeline in display order.
    pub fn all_timelines(&self) -> impl Iterator<Item = &Timeline> {
        (0..self.timeline_count()).filter_map(|i| self.timeline_at(i))
    }

    /// Open timeline `i` for editing. Returns false when it is already open
    /// or does not exist.
    pub fn switch_timeline(&mut self, i: usize) -> bool {
        if i == self.active_timeline || i >= self.timeline_count() {
            return false;
        }
        let stored = if i < self.active_timeline { i } else { i - 1 };
        let incoming = self.timelines.remove(stored);
        let outgoing = std::mem::replace(&mut self.timeline, incoming);
        // the outgoing timeline keeps its display position
        self.timelines.insert(self.active_timeline.min(self.timelines.len()), outgoing);
        self.active_timeline = i;
        true
    }

    /// A name no timeline uses yet: `base`, `base 2`, `base 3`...
    pub fn unique_timeline_name(&self, base: &str) -> String {
        let taken = |n: &str| self.all_timelines().any(|t| t.name == n);
        if !taken(base) {
            return base.to_string();
        }
        (2..).map(|k| format!("{base} {k}")).find(|n| !taken(n)).unwrap_or_default()
    }

    /// Add an empty timeline with the open timeline's format at the end.
    /// Returns its index; it is not opened.
    pub fn add_timeline(&mut self, name: &str) -> usize {
        let name = self.unique_timeline_name(name);
        let format = self.timeline.format;
        self.timelines.push(Timeline::new(name, format));
        self.timeline_count() - 1
    }

    /// Copy timeline `i` (clips get new ids) right after it. Returns the
    /// copy's index; it is not opened.
    pub fn duplicate_timeline(&mut self, i: usize) -> Option<usize> {
        let mut copy = self.timeline_at(i)?.clone();
        copy.name = self.unique_timeline_name(&format!("{} copy", copy.name));
        copy.spine.iter_mut().for_each(crate::clip::Clip::remint_ids);
        let at = i + 1;
        // insert at display position `at`; the open timeline keeps its place
        let stored = if at <= self.active_timeline { at } else { at - 1 };
        self.timelines.insert(stored, copy);
        if at <= self.active_timeline {
            self.active_timeline += 1;
        }
        Some(at)
    }

    /// Rename timeline `i`. Names must be unique and not blank.
    pub fn rename_timeline(&mut self, i: usize, name: &str) -> bool {
        let name = name.trim();
        if name.is_empty() || self.all_timelines().enumerate().any(|(j, t)| j != i && t.name == name) {
            return false;
        }
        match self.timeline_at_mut(i) {
            Some(t) => {
                t.name = name.to_string();
                true
            }
            None => false,
        }
    }

    /// Delete a timeline that is not open (so the last one can never go).
    pub fn remove_timeline(&mut self, i: usize) -> Option<Timeline> {
        if i == self.active_timeline || i >= self.timeline_count() {
            return None;
        }
        let stored = if i < self.active_timeline { i } else { i - 1 };
        if i < self.active_timeline {
            self.active_timeline -= 1;
        }
        Some(self.timelines.remove(stored))
    }

    /// Assets placed in any of the project's timelines (nested ones included).
    pub fn used_assets(&self) -> std::collections::HashSet<AssetId> {
        let mut used = std::collections::HashSet::new();
        for t in self.all_timelines() {
            for c in &t.spine {
                c.collect_assets(&mut used);
            }
        }
        used
    }

    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        let json = self.to_json().map_err(std::io::Error::other)?;
        crate::autosave::write_atomic(path, json.as_bytes())
    }

    pub fn load(path: &Path) -> std::io::Result<Project> {
        let s = std::fs::read_to_string(path)?;
        Project::from_json(&s).map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clip::Clip;
    use crate::time::Time;

    fn names(p: &Project) -> Vec<String> {
        p.all_timelines().map(|t| t.name.clone()).collect()
    }

    fn project_with_clip() -> (Project, AssetId) {
        let mut p = Project::new("Main");
        let a = MediaAsset::new("a", "a.mp4", Time::from_secs(10));
        let id = p.add_asset(a.clone());
        p.timeline.append(Clip::from_asset(&a)).unwrap();
        (p, id)
    }

    #[test]
    fn switching_keeps_order_and_content() {
        let (mut p, asset) = project_with_clip();
        assert_eq!(p.add_timeline("Shorts"), 1);
        assert_eq!(p.add_timeline("Shorts"), 2, "names are made unique");
        assert_eq!(names(&p), ["Main", "Shorts", "Shorts 2"]);
        assert!(p.switch_timeline(2));
        assert_eq!(p.timeline.name, "Shorts 2");
        assert_eq!(names(&p), ["Main", "Shorts", "Shorts 2"], "switching never reorders");
        assert!(p.switch_timeline(0));
        assert_eq!(p.timeline.spine.len(), 1, "the clip came back with its timeline");
        assert!(!p.switch_timeline(0) && !p.switch_timeline(9));
        assert!(p.used_assets().contains(&asset));
    }

    #[test]
    fn duplicate_rename_remove() {
        let (mut p, _) = project_with_clip();
        p.add_timeline("B");
        p.switch_timeline(1);
        let copy = p.duplicate_timeline(0).unwrap();
        assert_eq!(copy, 1);
        assert_eq!(names(&p), ["Main", "Main copy", "B"]);
        assert_eq!(p.active_timeline, 2, "the open timeline moved down one place");
        assert_eq!(p.timeline.name, "B");
        let orig = p.timeline_at(0).unwrap().spine[0].id;
        let dup = p.timeline_at(1).unwrap().spine[0].id;
        assert_ne!(orig, dup, "copied clips get new ids");

        assert!(!p.rename_timeline(1, "Main"), "duplicate name");
        assert!(!p.rename_timeline(1, "   "));
        assert!(p.rename_timeline(1, "Cut 2"));
        assert!(p.remove_timeline(2).is_none(), "the open timeline cannot be deleted");
        assert_eq!(p.remove_timeline(0).unwrap().name, "Main");
        assert_eq!(p.active_timeline, 1);
        assert_eq!(names(&p), ["Cut 2", "B"]);
    }

    #[test]
    fn round_trip_and_old_files() {
        let (mut p, _) = project_with_clip();
        p.add_timeline("Alt");
        p.switch_timeline(1);
        p.library.add_bin("Footage", None);
        p.assets[0].set_rating(crate::library::Rating::Rejected);
        p.assets[0].meta.scene = "12".into();
        let back = Project::from_json(&p.to_json().unwrap()).unwrap();
        assert_eq!(back, p);

        // a file written before timelines / bins existed
        let mut old: serde_json::Value = serde_json::from_str(&Project::new("Old").to_json().unwrap()).unwrap();
        let o = old.as_object_mut().unwrap();
        o.remove("active_timeline");
        let p = Project::from_json(&old.to_string()).unwrap();
        assert_eq!(p.timeline_count(), 1);
        assert!(p.library.is_empty());

        // an out-of-range index in a hand-edited file is clamped
        let mut bad: serde_json::Value = serde_json::from_str(&Project::new("X").to_json().unwrap()).unwrap();
        bad["active_timeline"] = 7.into();
        assert_eq!(Project::from_json(&bad.to_string()).unwrap().active_timeline, 0);
    }
}
