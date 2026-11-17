//! A project: media library + timeline.

use crate::asset::MediaAsset;
use crate::ids::{AssetId, ClipId};
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
}

impl Default for Project {
    fn default() -> Self {
        Project::new("Untitled Project")
    }
}

impl Project {
    pub fn new(name: impl Into<String>) -> Self {
        let name = name.into();
        Project { version: PROJECT_FORMAT_VERSION, timeline: Timeline::new(name.clone(), Default::default()), name, assets: Vec::new() }
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
        let p: Project = serde_json::from_str(s)?;
        p.reserve_ids();
        Ok(p)
    }

    pub fn reserve_ids(&self) {
        ClipId::reserve_above(self.timeline.max_clip_id());
        if let Some(m) = self.assets.iter().map(|a| a.id.0).max() {
            AssetId::reserve_above(m);
        }
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
