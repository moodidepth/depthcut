//! # nle-media
//!
//! Media I/O for Rust Cut Pro, tuned for an Intel Arc A750 + Ryzen 7 5800X:
//!
//! * [`hwaccel`] — hardware decode first (D3D11VA on Windows, QSV over
//!   VA-API `iHD` on Linux) for H.264 / HEVC 8+10-bit / AV1, with silent
//!   fallback to multi-threaded software decode.
//! * [`decode`] — decoders producing NV12/P010 frames for the GPU.
//! * [`queue`] / [`frame_server`] — bounded ring-buffer frame queues and the
//!   asynchronous "latest wins" scrubbing/playback decoder.
//! * [`threads`] — 12-worker rayon pool, 4 threads reserved for UI/audio.
//! * [`thumbnails`], [`waveform`], [`proxy`] — parallel background jobs.
//! * [`audio`] — 48 kHz stereo resampling, 3-band EQ, auto-ducking, the
//!   timeline mixer and low-latency cpal playback.
//! * [`analysis`] — EBU R128 / BS.1770 loudness and spectral-flux beat detection.
//! * [`jobs`] / [`paths`] — background jobs with progress, per-user data folders.
//! * `online` (feature) — key-less Openverse / Internet Archive audio search,
//!   downloads, the Whisper model manager and optional yt-dlp import.

pub mod analysis;
pub mod audio;
pub mod decode;
pub mod encoder_probe;
pub mod ffmpeg_cli;
pub mod frame;
pub mod frame_server;
pub mod hwaccel;
pub mod jobs;
pub mod matte;
#[cfg(feature = "online")]
pub mod online;
pub mod paths;
pub mod peaks;
pub mod probe;
pub mod proxy;
pub mod queue;
pub mod threads;
pub mod thumbnails;
pub mod transcribe;
pub mod waveform;

pub use analysis::{BeatAnalysis, BeatBand, KillPeak, MultiBandBeat};
pub use encoder_probe::{EncoderBackend, EncoderProbe};
pub use frame::{AudioBuffer, PixelFormat, VideoFrame};
pub use frame_server::{DecodedFrame, FrameServer};
pub use hwaccel::{HwBackend, Platform};
pub use peaks::PeakBuffer;
pub use queue::FrameQueue;
pub use waveform::Waveform;

use thiserror::Error;

#[derive(Debug, Error)]
pub enum MediaError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("FFmpeg is not installed (ffmpeg/ffprobe not found next to the editor, on PATH or in the winget/Scoop/Chocolatey folders; set RCP_FFMPEG to ffmpeg.exe)")]
    FfmpegMissing,
    #[error("decode error: {0}")]
    Decode(String),
    #[error("unsupported: {0}")]
    Unsupported(String),
    #[error("end of stream")]
    Eof,
}
