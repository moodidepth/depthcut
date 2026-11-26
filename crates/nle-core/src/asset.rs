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

/// YCbCr -> RGB matrix of a video stream.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum YuvMatrix {
    Bt709,
    Bt601,
    Bt2020,
}

impl VideoStreamInfo {
    /// The stream's YCbCr matrix: its `colorspace` tag, else its primaries,
    /// else the usual guess for untagged video (mpv / FFmpeg / Resolve):
    /// SD (narrower than 1280 and at most 576 lines) is BT.601, HD is BT.709,
    /// and high-bit-depth UHD is BT.2020.
    pub fn yuv_matrix(&self) -> YuvMatrix {
        match self.color_space.as_str() {
            "bt709" | "smpte240m" => return YuvMatrix::Bt709,
            "smpte170m" | "bt470bg" | "fcc" => return YuvMatrix::Bt601,
            "bt2020nc" | "bt2020c" => return YuvMatrix::Bt2020,
            _ => {}
        }
        match self.color_primaries.as_str() {
            "bt709" => return YuvMatrix::Bt709,
            "smpte170m" | "bt470bg" | "bt470m" => return YuvMatrix::Bt601,
            "bt2020" => return YuvMatrix::Bt2020,
            _ => {}
        }
        if self.width < 1280 && self.height <= 576 {
            YuvMatrix::Bt601
        } else if self.bit_depth > 8 && self.height > 1100 {
            YuvMatrix::Bt2020
        } else {
            YuvMatrix::Bt709
        }
    }

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
    /// Marked as rejected (never true together with `favorite`).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub rejected: bool,
    /// Bin the asset is filed in (`None` = top level).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bin: Option<u64>,
    /// Logging metadata (notes, scene, take, reel, camera).
    #[serde(default, skip_serializing_if = "crate::library::AssetMetadata::is_empty")]
    pub meta: crate::library::AssetMetadata,
    /// Integrated loudness (EBU R128 / BS.1770), measured after import.
    #[serde(default)]
    pub loudness_lufs: Option<f32>,
    /// Licence / credit for media downloaded from online libraries.
    #[serde(default)]
    pub attribution: Option<Attribution>,
    /// Timecode of the first frame (the file's `timecode` tag, or a WAV's
    /// BWF time reference). EDL source times are written relative to it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_timecode: Option<Time>,
    /// Clips on this asset still hold absolute source timecode (EDL import
    /// before the media was probed). The first probe subtracts
    /// `start_timecode` from them and clears this.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub source_tc_pending: bool,
    /// A subclip: the part of the file `(in, out)` this browser item stands
    /// for. The file, proxy and streams are the parent's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subclip: Option<(Time, Time)>,
    /// How the picture is scanned, as probed.
    #[serde(default, skip_serializing_if = "FieldOrder::is_progressive")]
    pub field_order: FieldOrder,
    /// Whether frames are deinterlaced on decode.
    #[serde(default, skip_serializing_if = "Deinterlace::is_auto")]
    pub deinterlace: Deinterlace,
    /// Words spoken in the clip (source time), from "Transcribe Clip"; used
    /// to find a line and mark In / Out from the text.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub transcript: Vec<crate::captions::CaptionWord>,
}

/// Scan type of a video stream.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum FieldOrder {
    #[default]
    Progressive,
    /// Interlaced, top field first.
    TopFirst,
    /// Interlaced, bottom field first.
    BottomFirst,
}

impl FieldOrder {
    pub fn is_progressive(&self) -> bool {
        *self == FieldOrder::Progressive
    }

    /// From ffprobe's `field_order` (`tt`, `bb`, `tb`, `bt`, `progressive`).
    pub fn from_ffprobe(s: &str) -> Self {
        match s {
            "tt" | "tb" => FieldOrder::TopFirst,
            "bb" | "bt" => FieldOrder::BottomFirst,
            _ => FieldOrder::Progressive,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            FieldOrder::Progressive => "Progressive",
            FieldOrder::TopFirst => "Interlaced (upper field first)",
            FieldOrder::BottomFirst => "Interlaced (lower field first)",
        }
    }
}

/// Per-clip deinterlacing choice.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Deinterlace {
    /// When the file is probed as interlaced.
    #[default]
    Auto,
    On,
    Off,
}

impl Deinterlace {
    pub const ALL: [Deinterlace; 3] = [Deinterlace::Auto, Deinterlace::On, Deinterlace::Off];

    pub fn is_auto(&self) -> bool {
        *self == Deinterlace::Auto
    }

    pub fn label(self) -> &'static str {
        match self {
            Deinterlace::Auto => "Auto",
            Deinterlace::On => "On",
            Deinterlace::Off => "Off",
        }
    }
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
            rejected: false,
            bin: None,
            meta: Default::default(),
            loudness_lufs: None,
            attribution: None,
            start_timecode: None,
            source_tc_pending: false,
            subclip: None,
            field_order: FieldOrder::Progressive,
            deinterlace: Deinterlace::Auto,
            transcript: Vec::new(),
        }
    }

    /// The field order to deinterlace with, when frames should be: the
    /// probed one, or upper-first when forced on for a progressive-tagged file.
    pub fn deinterlace_fields(&self) -> Option<FieldOrder> {
        if self.video.is_none() || self.kind == AssetKind::Image {
            return None;
        }
        match (self.deinterlace, self.field_order) {
            (Deinterlace::Off, _) | (Deinterlace::Auto, FieldOrder::Progressive) => None,
            (Deinterlace::On, FieldOrder::Progressive) => Some(FieldOrder::TopFirst),
            (_, f) => Some(f),
        }
    }

    /// Source media this item stands for: a subclip's range, else the file.
    pub fn bounds(&self) -> (Time, Time) {
        match self.subclip {
            Some((a, b)) if b > a => (a.max(Time::ZERO), b.min(self.duration).max(a)),
            _ => (Time::ZERO, self.duration),
        }
    }

    /// How far a clip of this item can be trimmed out: none for a still
    /// image, which shows the same picture for as long as the clip runs.
    pub fn clip_media_duration(&self) -> Option<Time> {
        (self.kind != AssetKind::Image).then_some(self.duration)
    }

    /// Length of the media this item stands for.
    pub fn span(&self) -> Time {
        let (a, b) = self.bounds();
        b - a
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
        self.name.to_lowercase().contains(&q)
            || self.tags.iter().any(|t| t.to_lowercase().contains(&q))
            || self.video.as_ref().is_some_and(|v| v.codec.to_lowercase().contains(&q))
            || self.meta.contains_lower(&q)
    }

    pub fn rating(&self) -> crate::library::Rating {
        if self.rejected {
            crate::library::Rating::Rejected
        } else if self.favorite {
            crate::library::Rating::Favorite
        } else {
            crate::library::Rating::None
        }
    }

    pub fn set_rating(&mut self, r: crate::library::Rating) {
        self.favorite = r == crate::library::Rating::Favorite;
        self.rejected = r == crate::library::Rating::Rejected;
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

#[cfg(test)]
mod tests {
    use super::*;

    fn info(w: u32, h: u32, depth: u8, space: &str, prim: &str) -> VideoStreamInfo {
        VideoStreamInfo {
            width: w,
            height: h,
            frame_rate: FrameRate::default(),
            codec: "h264".into(),
            pix_fmt: "yuv420p".into(),
            bit_depth: depth,
            color_space: space.into(),
            color_transfer: String::new(),
            color_primaries: prim.into(),
            is_vfr: false,
            pts_table: Vec::new(),
        }
    }

    #[test]
    fn yuv_matrix_prefers_tags_then_guesses_from_size() {
        assert_eq!(info(640, 480, 8, "bt709", "").yuv_matrix(), YuvMatrix::Bt709);
        assert_eq!(info(1920, 1080, 8, "smpte170m", "").yuv_matrix(), YuvMatrix::Bt601);
        assert_eq!(info(1920, 1080, 10, "bt2020nc", "").yuv_matrix(), YuvMatrix::Bt2020);
        assert_eq!(info(720, 576, 8, "unknown", "bt709").yuv_matrix(), YuvMatrix::Bt709);
        // untagged: SD -> 601, HD -> 709, 10-bit UHD -> 2020
        assert_eq!(info(640, 360, 8, "unknown", "").yuv_matrix(), YuvMatrix::Bt601);
        assert_eq!(info(1280, 720, 8, "unknown", "").yuv_matrix(), YuvMatrix::Bt709);
        assert_eq!(info(3840, 2160, 8, "unknown", "").yuv_matrix(), YuvMatrix::Bt709);
        assert_eq!(info(3840, 2160, 10, "unknown", "").yuv_matrix(), YuvMatrix::Bt2020);
    }
}
