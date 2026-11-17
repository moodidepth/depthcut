//! Process-unique identifiers for clips and media assets.

use serde::{Deserialize, Serialize};
use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT_CLIP: AtomicU64 = AtomicU64::new(1);
static NEXT_ASSET: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ClipId(pub u64);

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct AssetId(pub u64);

impl ClipId {
    /// Allocate a fresh, never-before-used clip id.
    #[allow(clippy::new_without_default)]
    pub fn new() -> Self {
        ClipId(NEXT_CLIP.fetch_add(1, Ordering::Relaxed))
    }
    /// Make sure future ids are larger than `id` (after loading a project).
    pub fn reserve_above(id: u64) {
        NEXT_CLIP.fetch_max(id + 1, Ordering::Relaxed);
    }
}

impl AssetId {
    #[allow(clippy::new_without_default)]
    pub fn new() -> Self {
        AssetId(NEXT_ASSET.fetch_add(1, Ordering::Relaxed))
    }
    pub fn reserve_above(id: u64) {
        NEXT_ASSET.fetch_max(id + 1, Ordering::Relaxed);
    }
}

impl fmt::Display for ClipId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "clip#{}", self.0)
    }
}

impl fmt::Display for AssetId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "asset#{}", self.0)
    }
}
