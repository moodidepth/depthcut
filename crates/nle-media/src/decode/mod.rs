//! Video decoders.
//!
//! * [`cli::CliDecoder`] — streams raw NV12/P010 frames from the `ffmpeg`
//!   executable using the QSV → VA-API/D3D11VA → software plan from
//!   [`crate::hwaccel`]. Works with any FFmpeg install, no dev libraries.
//! * `libav::LibavDecoder` (feature `ffmpeg`) — in-process decoding through
//!   `ffmpeg-next` with a QSV hardware device context.
//! * [`synthetic::SyntheticDecoder`] — procedural test pattern.

pub mod cli;
#[cfg(feature = "ffmpeg")]
pub mod libav;
pub mod synthetic;

use crate::frame::{PixelFormat, VideoFrame};
use crate::hwaccel::HwBackend;
use crate::probe;
use crate::MediaError;
use nle_core::{FrameRate, MediaAsset, Time};

pub trait VideoDecoder: Send {
    fn width(&self) -> u32;
    fn height(&self) -> u32;
    fn format(&self) -> PixelFormat;
    fn frame_rate(&self) -> FrameRate;
    fn backend(&self) -> HwBackend;
    /// Decode the frame displayed at source time `t`.
    fn frame_at(&mut self, t: Time) -> Result<VideoFrame, MediaError>;
    /// A small RGBA thumbnail at `t`.
    fn thumbnail_at(&mut self, t: Time, max_w: u32, max_h: u32) -> Result<(u32, u32, Vec<u8>), MediaError> {
        let f = self.frame_at(t)?;
        Ok(f.thumbnail_rgba(max_w, max_h))
    }
}

/// Options for opening a decoder.
#[derive(Clone, Copy, Debug, Default)]
pub struct DecodeOptions {
    /// Prefer the proxy file if one exists.
    pub use_proxy: bool,
    /// Downscale on decode to at most this height (preview quality).
    pub max_height: Option<u32>,
}

/// Open the best available decoder for `asset`.
pub fn open(asset: &MediaAsset, opts: DecodeOptions) -> Result<Box<dyn VideoDecoder>, MediaError> {
    if probe::is_synthetic(asset) {
        return Ok(Box::new(synthetic::SyntheticDecoder::new(asset, opts.max_height)));
    }
    if !asset.has_video() {
        return Err(MediaError::Unsupported(format!("{} has no video", asset.name)));
    }
    #[cfg(feature = "ffmpeg")]
    {
        match libav::LibavDecoder::open(asset, opts) {
            Ok(d) => return Ok(Box::new(d)),
            Err(e) => log::warn!("libav decoder failed for {}: {e} — falling back to CLI", asset.name),
        }
    }
    if !crate::ffmpeg_cli::available() {
        // Keep the UI alive with a placeholder pattern.
        return Ok(Box::new(synthetic::SyntheticDecoder::new(asset, opts.max_height)));
    }
    Ok(Box::new(cli::CliDecoder::open(asset, opts)?))
}

/// Output dimensions after optional downscale (even, aspect-preserving).
pub fn scaled_dims(w: u32, h: u32, max_h: Option<u32>) -> (u32, u32) {
    match max_h {
        Some(mh) if h > mh => {
            let nw = ((w as f64 * mh as f64 / h as f64) / 4.0).round() as u32 * 4;
            (nw.max(4), mh & !1)
        }
        _ => ((w + 3) & !3, (h + 1) & !1),
    }
}
