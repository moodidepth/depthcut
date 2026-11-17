//! Media assets referenced by clips.

use crate::ids::AssetId;
use crate::time::{FrameRate, Time};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct VideoStreamInfo {
    pub width: u32,
    pub height: u32,
    pub frame_rate: FrameRate,
    pub codec: String,
    pub pix_fmt: String,
    pub bit_depth: u8,
    pub color_space: String,
    #[serde(default)]
    pub color_transfer: String,
    #[serde(default)]
    pub color_primaries: String,
    /// True when the source file uses Variable Frame Rate (VFR), common in OBS/ShadowPlay.
    #[serde(default)]
    pub is_vfr: bool,
    /// PTS-to-CFR frame index table built on clip import for sample-accurate scrubbing.
    #[serde(default)]
    pub pts_table: Vec<Time>,
}

impl VideoStreamInfo {
    /// Nearest frame index for a source presentation timestamp.
    pub fn frame_index_for_pts(&self, t: Time) -> usize {
        if self.pts_table.is_empty() {
            let ticks = self.frame_rate.frame_ticks().max(1);
            return (t.0.max(0) / ticks) as usize;
        }
        match self.pts_table.binary_search(&t) {
            Ok(i) => i,
            Err(0) => 0,
            Err(i) if i >= self.pts_table.len() => self.pts_table.len() - 1,
            Err(i) => {
                let d0 = (t.0 - self.pts_table[i - 1].0).abs();
                let d1 = (self.pts_table[i].0 - t.0).abs();
                if d0 <= d1 {
                    i - 1
                } else {
                    i
                }
            }
        }
    }

    /// Lock a CFR timeline request to the exact indexed VFR PTS entry so audio stays in sync.
    pub fn cfr_locked_source_time(&self, requested: Time) -> Time {
        if !self.is_vfr || self.pts_table.is_empty() {
            return requested;
        }
        let idx = (requested.frames_round(self.frame_rate).max(0) as usize).min(self.pts_table.len() - 1);
        self.pts_table[idx]
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AudioStreamInfo {
    pub sample_rate: u32,
    pub channels: u16,
    pub codec: String,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum AssetKind {
    #[default]
    Video,
    Audio,
    Image,
    /// Procedurally generated media (test pattern / tone) used when no
    /// decoder is available.
    Synthetic,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MediaAsset {
    pub id: AssetId,
    pub name: String,
    pub path: PathBuf,
    #[serde(default)]
    pub proxy_path: Option<PathBuf>,
    pub kind: AssetKind,
    pub duration: Time,
    pub video: Option<VideoStreamInfo>,
    pub audio: Option<AudioStreamInfo>,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub favorite: bool,
    /// Integrated loudness (EBU R128 / BS.1770), measured after import.
    #[serde(default)]
    pub loudness_lufs: Option<f32>,
    /// Licence / credit for media downloaded from online libraries.
    #[serde(default)]
    pub attribution: Option<Attribution>,
}

/// Credit line for Creative Commons / public-domain media.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Attribution {
    pub title: String,
    pub creator: String,
    /// e.g. "CC0 1.0", "CC BY 4.0", "Public Domain".
    pub license: String,
    #[serde(default)]
    pub license_url: String,
    /// Landing page of the original work.
    #[serde(default)]
    pub source_url: String,
    /// Provider (Freesound, Jamendo, Wikimedia Commons, Internet Archive...).
    #[serde(default)]
    pub provider: String,
}

impl Attribution {
    /// Whether the licence requires a credit (anything but CC0 / public domain).
    pub fn requires_credit(&self) -> bool {
        let l = self.license.to_ascii_lowercase();
        !(l.contains("cc0") || l.contains("pdm") || l.contains("public domain") || l == "pd")
    }

    /// One-line TASL credit: Title, Author, Source, License.
    pub fn credit_line(&self) -> String {
        let mut s = format!("\"{}\"", self.title.trim());
        if !self.creator.trim().is_empty() {
            s.push_str(&format!(" by {}", self.creator.trim()));
        }
        if !self.provider.is_empty() {
            s.push_str(&format!(" ({})", self.provider));
        }
        s.push_str(&format!(", licensed under {}", self.license));
        if !self.license_url.is_empty() {
            s.push_str(&format!(" <{}>", self.license_url));
        }
        if !self.source_url.is_empty() {
            s.push_str(&format!(" - {}", self.source_url));
        }
        s
    }
}

impl MediaAsset {
    pub fn new(name: impl Into<String>, path: impl Into<PathBuf>, duration: Time) -> Self {
        MediaAsset {
            id: AssetId::new(),
            name: name.into(),
            path: path.into(),
            proxy_path: None,
            kind: AssetKind::Video,
            duration,
            video: None,
            audio: None,
            tags: Vec::new(),
            favorite: false,
            loudness_lufs: None,
            attribution: None,
        }
    }

    pub fn has_video(&self) -> bool {
        self.video.is_some()
    }
    pub fn has_audio(&self) -> bool {
        self.audio.is_some()
    }

    pub fn frame_rate(&self) -> FrameRate {
        self.video.as_ref().map(|v| v.frame_rate).unwrap_or_default()
    }

    /// Case-insensitive match against name, tags and codec.
    pub fn matches(&self, query: &str) -> bool {
        let q = query.trim().to_lowercase();
        if q.is_empty() {
            return true;
        }
        self.name.to_lowercase().contains(&q) || self.tags.iter().any(|t| t.to_lowercase().contains(&q)) || self.video.as_ref().is_some_and(|v| v.codec.to_lowercase().contains(&q))
    }

    /// Human-readable one-line metadata summary.
    pub fn summary(&self) -> String {
        let mut parts = Vec::new();
        if let Some(v) = &self.video {
            parts.push(format!("{}×{}", v.width, v.height));
            parts.push(format!("{:.3} fps", v.frame_rate.fps()));
            parts.push(v.codec.to_uppercase());
            parts.push(format!("{}-bit {}", v.bit_depth, v.color_space));
        }
        if let Some(a) = &self.audio {
            parts.push(format!("{} Hz {}ch {}", a.sample_rate, a.channels, a.codec));
        }
        parts.join(" · ")
    }
}
