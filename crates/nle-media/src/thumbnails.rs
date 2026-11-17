//! Parallel filmstrip thumbnail generation for the browser and timeline.

use crate::decode::{self, DecodeOptions};
use crate::MediaError;
use nle_core::{MediaAsset, Time};
use rayon::prelude::*;

#[derive(Clone, Debug)]
pub struct Thumbnail {
    pub time: Time,
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

#[derive(Clone, Debug, Default)]
pub struct Filmstrip {
    pub interval: Time,
    pub frames: Vec<Thumbnail>,
}

impl Filmstrip {
    /// Thumbnail nearest to source time `t`.
    pub fn at(&self, t: Time) -> Option<&Thumbnail> {
        if self.frames.is_empty() {
            return None;
        }
        let i = if self.interval.is_positive() { (t.ticks() / self.interval.ticks()).clamp(0, self.frames.len() as i64 - 1) as usize } else { 0 };
        self.frames.get(i)
    }
}

/// Generate `count` evenly spaced thumbnails. Work is split into chunks so
/// each rayon worker reuses one decoder for several thumbnails.
pub fn generate(asset: &MediaAsset, count: usize, max_w: u32, max_h: u32) -> Result<Filmstrip, MediaError> {
    if !asset.has_video() || count == 0 {
        return Ok(Filmstrip::default());
    }
    let interval = Time(asset.duration.ticks() / count as i64).max(Time(1));
    let times: Vec<Time> = (0..count).map(|i| Time(interval.ticks() * i as i64 + interval.ticks() / 2)).collect();
    let workers = rayon::current_num_threads().max(1);
    let per = times.len().div_ceil(workers).max(1);
    let opts = DecodeOptions { use_proxy: true, max_height: Some(max_h.max(90) * 2) };
    let chunks: Vec<Vec<Thumbnail>> = times
        .par_chunks(per)
        .map(|ts| {
            let mut dec = match decode::open(asset, opts) {
                Ok(d) => d,
                Err(e) => {
                    log::warn!("thumbnail decoder for {}: {e}", asset.name);
                    return Vec::new();
                }
            };
            ts.iter().filter_map(|t| dec.thumbnail_at(*t, max_w, max_h).ok().map(|(w, h, rgba)| Thumbnail { time: *t, width: w, height: h, rgba })).collect()
        })
        .collect();
    Ok(Filmstrip { interval, frames: chunks.into_iter().flatten().collect() })
}
