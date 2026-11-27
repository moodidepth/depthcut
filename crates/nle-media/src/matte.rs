//! Per-frame AI matte cache (person segmentation / depth maps).
//!
//! Mattes are generated offline (nle-ai, local ONNX models) and stored one
//! file per source frame under
//! `%LOCALAPPDATA%/Depthcut/mattes/<source hash>/<kind>/<frame>.rcpm`, so
//! preview and export read exactly the same data. A file is a small header
//! (`RCPM`, width, height as little-endian u32) followed by `width × height`
//! 8-bit values (matte opacity, or relative nearness for depth: 255 = near).

use nle_core::{MatteKind, MediaAsset};
use std::path::{Path, PathBuf};

const MAGIC: &[u8; 4] = b"RCPM";

/// One 8-bit single-channel matte.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Matte {
    pub width: u32,
    pub height: u32,
    pub data: Vec<u8>,
}

impl Matte {
    pub fn new(width: u32, height: u32, data: Vec<u8>) -> Option<Self> {
        (width > 0 && height > 0 && data.len() == (width * height) as usize).then_some(Matte { width, height, data })
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(12 + self.data.len());
        out.extend_from_slice(MAGIC);
        out.extend_from_slice(&self.width.to_le_bytes());
        out.extend_from_slice(&self.height.to_le_bytes());
        out.extend_from_slice(&self.data);
        out
    }

    pub fn decode(bytes: &[u8]) -> Option<Self> {
        if bytes.len() < 12 || &bytes[..4] != MAGIC {
            return None;
        }
        let w = u32::from_le_bytes(bytes[4..8].try_into().ok()?);
        let h = u32::from_le_bytes(bytes[8..12].try_into().ok()?);
        let len = (w as usize).checked_mul(h as usize)?;
        Matte::new(w, h, bytes.get(12..12 + len)?.to_vec())
    }

    /// Bilinear value at normalised `(u, v)` (0..1).
    pub fn sample(&self, u: f32, v: f32) -> f32 {
        let (w, h) = (self.width as f32, self.height as f32);
        let x = (u * w - 0.5).clamp(0.0, w - 1.0);
        let y = (v * h - 0.5).clamp(0.0, h - 1.0);
        let (x0, y0) = (x.floor() as u32, y.floor() as u32);
        let (x1, y1) = ((x0 + 1).min(self.width - 1), (y0 + 1).min(self.height - 1));
        let (fx, fy) = (x - x0 as f32, y - y0 as f32);
        let g = |x: u32, y: u32| self.data[(y * self.width + x) as usize] as f32 / 255.0;
        let top = g(x0, y0) + (g(x1, y0) - g(x0, y0)) * fx;
        let bot = g(x0, y1) + (g(x1, y1) - g(x0, y1)) * fx;
        top + (bot - top) * fy
    }
}

/// `%LOCALAPPDATA%/Depthcut/mattes`.
pub fn cache_root() -> PathBuf {
    crate::paths::local_data_dir().join("mattes")
}

/// Folder holding `asset`'s mattes of `kind` under `root` (`None` for
/// media that cannot be fingerprinted: synthetic or offline assets).
pub fn dir_in(root: &Path, asset: &MediaAsset, kind: MatteKind) -> Option<PathBuf> {
    let name = crate::peaks::cache_file_name(&asset.path)?;
    let hash = name.split('.').next()?.to_string();
    Some(root.join(hash).join(kind.slug()))
}

pub fn frame_path_in(root: &Path, asset: &MediaAsset, kind: MatteKind, frame: i64) -> Option<PathBuf> {
    Some(dir_in(root, asset, kind)?.join(format!("{frame:08}.rcpm")))
}

/// The cached matte of source frame `frame`, if generated.
pub fn load(asset: &MediaAsset, kind: MatteKind, frame: i64) -> Option<Matte> {
    load_in(&cache_root(), asset, kind, frame)
}

pub fn load_in(root: &Path, asset: &MediaAsset, kind: MatteKind, frame: i64) -> Option<Matte> {
    Matte::decode(&std::fs::read(frame_path_in(root, asset, kind, frame)?).ok()?)
}

pub fn store(asset: &MediaAsset, kind: MatteKind, frame: i64, m: &Matte) -> std::io::Result<()> {
    store_in(&cache_root(), asset, kind, frame, m)
}

pub fn store_in(root: &Path, asset: &MediaAsset, kind: MatteKind, frame: i64, m: &Matte) -> std::io::Result<()> {
    let path = frame_path_in(root, asset, kind, frame).ok_or_else(|| std::io::Error::other("media file cannot be fingerprinted"))?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    // write-then-rename: a reader never sees a half-written file
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, m.encode())?;
    std::fs::rename(&tmp, &path)
}

/// Number of cached frames of `kind` for `asset` (Inspector status line).
pub fn cached_frames(asset: &MediaAsset, kind: MatteKind) -> usize {
    dir_in(&cache_root(), asset, kind)
        .and_then(|d| std::fs::read_dir(d).ok())
        .map_or(0, |r| r.filter_map(Result::ok).filter(|e| e.path().extension().is_some_and(|x| x == "rcpm")).count())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_round_trip_and_rejects_garbage() {
        let m = Matte::new(3, 2, vec![0, 64, 128, 192, 255, 7]).unwrap();
        assert_eq!(Matte::decode(&m.encode()), Some(m.clone()));
        assert_eq!(Matte::decode(b"RCPM\x03\0\0\0\x02\0\0\0\x01"), None, "truncated");
        assert_eq!(Matte::decode(b"XXXX\x01\0\0\0\x01\0\0\0\x01"), None, "bad magic");
        assert!(Matte::new(2, 2, vec![0; 3]).is_none());
        assert!((m.sample(0.0, 0.0) - 0.0).abs() < 1e-6);
        assert!((m.sample(1.0, 1.0) - 7.0 / 255.0).abs() < 1e-6);
    }

    #[test]
    fn store_and_load_by_source_file() {
        let root = std::env::temp_dir().join(format!("rcp-matte-test-{}", std::process::id()));
        let src = root.join("clip.mp4");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(&src, b"not really a video").unwrap();
        let a = MediaAsset::new("clip", src.clone(), nle_core::Time::from_secs(1));
        let m = Matte::new(2, 1, vec![10, 250]).unwrap();
        store_in(&root, &a, MatteKind::Person, 42, &m).unwrap();
        assert_eq!(load_in(&root, &a, MatteKind::Person, 42), Some(m));
        assert_eq!(load_in(&root, &a, MatteKind::Depth, 42), None, "kinds are separate");
        assert_eq!(load_in(&root, &a, MatteKind::Person, 43), None);
        let offline = MediaAsset::new("gone", root.join("missing.mp4"), nle_core::Time::from_secs(1));
        assert!(store_in(&root, &offline, MatteKind::Person, 0, &Matte::new(1, 1, vec![0]).unwrap()).is_err());
        let _ = std::fs::remove_dir_all(&root);
    }
}
