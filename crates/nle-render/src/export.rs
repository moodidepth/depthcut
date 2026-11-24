//! Hardware-accelerated export.
//!
//! Frames are rendered on the GPU, packed to NV12 by a compute shader, read
//! back once and streamed to FFmpeg, which encodes on the Arc A750's media
//! engines: **AV1 (`av1_qsv`)**, **HEVC (`hevc_qsv`)**, **H.264
//! (`h264_qsv` / `h264_vaapi`)**, the NVIDIA NVENC / AMD AMF equivalents, or
//! `libx264` / `libx265` in software (see `nle_media::encoder_probe`).
//! Audio is mixed offline (same mixer as playback) and muxed as AAC or PCM.
//!
//! Mastering formats (ProRes, DNxHR, PNG / TIFF / EXR sequences) read the
//! frame back as 16-bit RGBA and let FFmpeg's software encoders do the
//! 4:2:2 / 4:4:4 conversion. HEVC and AV1 can also be exported 10-bit (P010).

use crate::gpu::GpuContext;
use crate::renderer::{FrameSource, RenderOptions, Renderer};
use nle_core::{eval, AssetId, FrameRate, MediaAsset, Project, Time};
use nle_media::audio::TimelineMixer;
use nle_media::encoder_probe::EncoderProbe;
use nle_media::hwaccel::{self, HwCaps, Platform};
use nle_media::{ffmpeg_cli, AudioBuffer, FrameServer, VideoFrame};
use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Instant;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ExportError {
    #[error("FFmpeg not available")]
    NoFfmpeg,
    #[error("I/O: {0}")]
    Io(#[from] std::io::Error),
    #[error("encoder failed: {0}")]
    Encoder(String),
    #[error("export cancelled")]
    Cancelled,
    /// A clip's picture or sound could not be read.
    #[error("media unreadable: {0}")]
    Media(String),
}

/// Compressed video format of an [`ExportPreset`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ExportCodec {
    Av1,
    Hevc,
    H264,
    ProRes,
    Dnxhr,
    ImageSequence,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ExportPreset {
    Av1Qsv,
    HevcQsv,
    H264Qsv,
    Av1Nvenc,
    HevcNvenc,
    H264Nvenc,
    Av1Amf,
    HevcAmf,
    H264Amf,
    H264Vaapi,
    H264Software,
    HevcSoftware,
    ProRes422,
    ProRes422Hq,
    ProRes4444,
    DnxhrHq,
    DnxhrHqx,
    Dnxhr444,
    PngSequence,
    TiffSequence,
    ExrSequence,
}

impl ExportPreset {
    /// Mastering / intermediate formats (software encoders in every full FFmpeg build).
    pub const MASTERING: [ExportPreset; 9] = [
        ExportPreset::ProRes422,
        ExportPreset::ProRes422Hq,
        ExportPreset::ProRes4444,
        ExportPreset::DnxhrHq,
        ExportPreset::DnxhrHqx,
        ExportPreset::Dnxhr444,
        ExportPreset::PngSequence,
        ExportPreset::TiffSequence,
        ExportPreset::ExrSequence,
    ];

    /// Fastest first: Intel QSV → NVIDIA NVENC → AMD AMF → VA-API → CPU.
    pub const ALL: [ExportPreset; 12] = [
        ExportPreset::Av1Qsv,
        ExportPreset::HevcQsv,
        ExportPreset::H264Qsv,
        ExportPreset::Av1Nvenc,
        ExportPreset::HevcNvenc,
        ExportPreset::H264Nvenc,
        ExportPreset::Av1Amf,
        ExportPreset::HevcAmf,
        ExportPreset::H264Amf,
        ExportPreset::H264Vaapi,
        ExportPreset::H264Software,
        ExportPreset::HevcSoftware,
    ];

    pub fn label(self) -> &'static str {
        match self {
            ExportPreset::Av1Qsv => "AV1 — Intel Quick Sync (av1_qsv)",
            ExportPreset::HevcQsv => "HEVC — Intel Quick Sync (hevc_qsv)",
            ExportPreset::H264Qsv => "H.264 — Intel Quick Sync (h264_qsv)",
            ExportPreset::Av1Nvenc => "AV1 — NVIDIA NVENC (av1_nvenc)",
            ExportPreset::HevcNvenc => "HEVC — NVIDIA NVENC (hevc_nvenc)",
            ExportPreset::H264Nvenc => "H.264 — NVIDIA NVENC (h264_nvenc)",
            ExportPreset::Av1Amf => "AV1 — AMD AMF (av1_amf)",
            ExportPreset::HevcAmf => "HEVC — AMD AMF (hevc_amf)",
            ExportPreset::H264Amf => "H.264 — AMD AMF (h264_amf)",
            ExportPreset::H264Vaapi => "H.264 — VA-API (h264_vaapi)",
            ExportPreset::H264Software => "H.264 — Software (libx264)",
            ExportPreset::HevcSoftware => "HEVC / H.265 — Software (libx265, slow)",
            ExportPreset::ProRes422 => "Apple ProRes 422 (10-bit)",
            ExportPreset::ProRes422Hq => "Apple ProRes 422 HQ (10-bit)",
            ExportPreset::ProRes4444 => "Apple ProRes 4444 (10-bit, alpha)",
            ExportPreset::DnxhrHq => "Avid DNxHR HQ (8-bit 4:2:2)",
            ExportPreset::DnxhrHqx => "Avid DNxHR HQX (10-bit 4:2:2)",
            ExportPreset::Dnxhr444 => "Avid DNxHR 444 (10-bit 4:4:4)",
            ExportPreset::PngSequence => "PNG sequence (16-bit, alpha)",
            ExportPreset::TiffSequence => "TIFF sequence (16-bit, alpha)",
            ExportPreset::ExrSequence => "OpenEXR sequence (half float, linear)",
        }
    }
    pub fn encoder(self) -> &'static str {
        match self {
            ExportPreset::Av1Qsv => "av1_qsv",
            ExportPreset::HevcQsv => "hevc_qsv",
            ExportPreset::H264Qsv => "h264_qsv",
            ExportPreset::Av1Nvenc => "av1_nvenc",
            ExportPreset::HevcNvenc => "hevc_nvenc",
            ExportPreset::H264Nvenc => "h264_nvenc",
            ExportPreset::Av1Amf => "av1_amf",
            ExportPreset::HevcAmf => "hevc_amf",
            ExportPreset::H264Amf => "h264_amf",
            ExportPreset::H264Vaapi => "h264_vaapi",
            ExportPreset::H264Software => "libx264",
            ExportPreset::HevcSoftware => "libx265",
            ExportPreset::ProRes422 | ExportPreset::ProRes422Hq | ExportPreset::ProRes4444 => "prores_ks",
            ExportPreset::DnxhrHq | ExportPreset::DnxhrHqx | ExportPreset::Dnxhr444 => "dnxhd",
            ExportPreset::PngSequence => "png",
            ExportPreset::TiffSequence => "tiff",
            ExportPreset::ExrSequence => "exr",
        }
    }
    /// Preset for an FFmpeg encoder name.
    pub fn from_encoder(name: &str) -> Option<ExportPreset> {
        ExportPreset::ALL.into_iter().find(|p| p.encoder() == name)
    }
    pub fn codec(self) -> ExportCodec {
        match self {
            ExportPreset::Av1Qsv | ExportPreset::Av1Nvenc | ExportPreset::Av1Amf => ExportCodec::Av1,
            ExportPreset::HevcQsv | ExportPreset::HevcNvenc | ExportPreset::HevcAmf | ExportPreset::HevcSoftware => ExportCodec::Hevc,
            ExportPreset::ProRes422 | ExportPreset::ProRes422Hq | ExportPreset::ProRes4444 => ExportCodec::ProRes,
            ExportPreset::DnxhrHq | ExportPreset::DnxhrHqx | ExportPreset::Dnxhr444 => ExportCodec::Dnxhr,
            ExportPreset::PngSequence | ExportPreset::TiffSequence | ExportPreset::ExrSequence => ExportCodec::ImageSequence,
            _ => ExportCodec::H264,
        }
    }
    /// Intermediate / mastering format (no rate control, PCM audio).
    pub fn is_mastering(self) -> bool {
        matches!(self.codec(), ExportCodec::ProRes | ExportCodec::Dnxhr | ExportCodec::ImageSequence)
    }
    pub fn is_image_sequence(self) -> bool {
        self.codec() == ExportCodec::ImageSequence
    }
    /// Can carry an alpha channel.
    pub fn supports_alpha(self) -> bool {
        matches!(self, ExportPreset::ProRes4444 | ExportPreset::PngSequence | ExportPreset::TiffSequence | ExportPreset::ExrSequence)
    }
    /// Has a 10-bit (Main10 / P010) option.
    pub fn supports_ten_bit(self) -> bool {
        matches!(self.codec(), ExportCodec::Hevc | ExportCodec::Av1)
    }
    /// File extension of the output (per frame for sequences).
    pub fn extension(self) -> &'static str {
        match self {
            ExportPreset::PngSequence => "png",
            ExportPreset::TiffSequence => "tif",
            ExportPreset::ExrSequence => "exr",
            p if p.is_mastering() => "mov",
            _ => "mp4",
        }
    }
    pub fn is_qsv(self) -> bool {
        matches!(self, ExportPreset::Av1Qsv | ExportPreset::HevcQsv | ExportPreset::H264Qsv)
    }
    pub fn is_nvenc(self) -> bool {
        matches!(self, ExportPreset::Av1Nvenc | ExportPreset::HevcNvenc | ExportPreset::H264Nvenc)
    }
    pub fn is_amf(self) -> bool {
        matches!(self, ExportPreset::Av1Amf | ExportPreset::HevcAmf | ExportPreset::H264Amf)
    }
    /// Whether this preset encodes on a GPU.
    pub fn is_hardware(self) -> bool {
        self.is_qsv() || self.is_nvenc() || self.is_amf() || self == ExportPreset::H264Vaapi
    }
    /// Sensible default quality (ICQ / CQ / QP / CRF).
    pub fn default_quality(self) -> u8 {
        match self.codec() {
            ExportCodec::Av1 => 28,
            ExportCodec::Hevc => 24,
            ExportCodec::H264 => 22,
            ExportCodec::ProRes | ExportCodec::Dnxhr | ExportCodec::ImageSequence => 0,
        }
    }
    pub fn supported_on(self, platform: Platform) -> bool {
        !(self == ExportPreset::H264Vaapi && platform != Platform::Linux)
    }
}

/// Presets the installed FFmpeg was *built* with (best first). Prefer
/// [`working_presets`], which only returns encoders that passed a real
/// test encode on this machine.
pub fn available_presets(platform: Platform, caps: &HwCaps) -> Vec<ExportPreset> {
    ExportPreset::ALL.into_iter().filter(|p| p.supported_on(platform) && (caps.encoders.is_empty() || caps.has_encoder(p.encoder()))).collect()
}

/// Mastering presets whose (software) encoder this FFmpeg build has.
pub fn mastering_presets(caps: &HwCaps) -> Vec<ExportPreset> {
    ExportPreset::MASTERING.into_iter().filter(|p| caps.encoders.is_empty() || caps.has_encoder(p.encoder())).collect()
}

/// `shot.png` → `shot_%06d.png` (FFmpeg image2 pattern) for sequences.
pub fn sequence_pattern(path: &Path) -> PathBuf {
    let stem = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| "frame".into());
    let ext = path.extension().map(|e| e.to_string_lossy().into_owned()).unwrap_or_else(|| "png".into());
    path.with_file_name(format!("{stem}_%06d.{ext}"))
}

/// Presets whose encoder passed the startup hardware probe, fastest first.
/// Empty when nothing works (no FFmpeg): the UI then disables export.
pub fn working_presets(probe: &EncoderProbe) -> Vec<ExportPreset> {
    probe.encoders.iter().filter_map(|e| ExportPreset::from_encoder(e)).collect()
}

/// Safety margin applied to size-capped exports (container overhead plus
/// average-bitrate overshoot).
pub const SIZE_BUDGET_MARGIN: f64 = 0.90;

/// Video bitrate (kbit/s) that keeps a muxed file of `duration_secs` under
/// `target_bytes`: `(target_bytes × 8 / duration) − audio`, times
/// [`SIZE_BUDGET_MARGIN`]. Floors at 150 kbit/s; check [`fits_size_budget`]
/// for clips too long to fit at any watchable quality.
pub fn size_budget_video_kbps(target_bytes: u64, duration_secs: f64, audio_kbps: u32) -> u32 {
    let duration = if duration_secs.is_nan() { 0.1 } else { duration_secs.max(0.1) };
    let total_kbps = target_bytes as f64 * 8.0 / 1000.0 / duration * SIZE_BUDGET_MARGIN;
    (total_kbps - audio_kbps as f64).floor().clamp(150.0, 60_000.0) as u32
}

/// Whether `video_kbps + audio_kbps` over `duration_secs` stays within `target_bytes`.
pub fn fits_size_budget(target_bytes: u64, duration_secs: f64, video_kbps: u32, audio_kbps: u32) -> bool {
    (video_kbps as f64 + audio_kbps as f64) * 1000.0 / 8.0 * duration_secs.max(0.0) <= target_bytes as f64
}

/// One-click gaming delivery presets (YouTube, Shorts/TikTok 9:16, Discord size budgets).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum GamingDeliveryPreset {
    #[default]
    ProjectDefault,
    YouTube1440p60,
    YouTube4k60,
    TikTokShorts1080x1920,
    DiscordUnder25Mb,
    DiscordUnder50Mb,
}

impl GamingDeliveryPreset {
    pub const ALL: [GamingDeliveryPreset; 6] = [
        GamingDeliveryPreset::ProjectDefault,
        GamingDeliveryPreset::YouTube1440p60,
        GamingDeliveryPreset::YouTube4k60,
        GamingDeliveryPreset::TikTokShorts1080x1920,
        GamingDeliveryPreset::DiscordUnder25Mb,
        GamingDeliveryPreset::DiscordUnder50Mb,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::ProjectDefault => "Match Project Format",
            Self::YouTube1440p60 => "YouTube 1440p60 (AV1/HEVC · 25 Mbps)",
            Self::YouTube4k60 => "YouTube 4K60 (AV1/HEVC · 40 Mbps)",
            Self::TikTokShorts1080x1920 => "Shorts / TikTok 9:16 (1080×1920 60fps · 18 Mbps)",
            Self::DiscordUnder25Mb => "Discord under 25 MB (720p60, auto bitrate)",
            Self::DiscordUnder50Mb => "Discord under 50 MB (1080p60, auto bitrate)",
        }
    }

    /// File-size cap in bytes for size-budgeted presets (decimal megabytes,
    /// the stricter reading of an upload limit).
    pub fn target_bytes(self) -> Option<u64> {
        match self {
            Self::DiscordUnder25Mb => Some(25_000_000),
            Self::DiscordUnder50Mb => Some(50_000_000),
            _ => None,
        }
    }

    /// AAC bitrate (kbit/s) this preset muxes.
    pub fn audio_kbps(self) -> u32 {
        match self {
            Self::DiscordUnder25Mb => 128,
            Self::DiscordUnder50Mb => 160,
            _ => 320,
        }
    }

    /// Platform loudness target: YouTube/Discord -14 LUFS, Shorts/TikTok -16 LUFS.
    pub fn loudness_target(self) -> Option<f32> {
        use nle_media::analysis::loudness::{TARGET_LUFS, TARGET_LUFS_SHORTS};
        match self {
            Self::ProjectDefault => None,
            Self::TikTokShorts1080x1920 => Some(TARGET_LUFS_SHORTS),
            _ => Some(TARGET_LUFS),
        }
    }

    /// Output frame size / rate, or `None` to keep the project format.
    pub fn frame(self) -> Option<(u32, u32, FrameRate)> {
        match self {
            Self::ProjectDefault => None,
            Self::YouTube1440p60 => Some((2560, 1440, FrameRate::FPS_60)),
            Self::YouTube4k60 => Some((3840, 2160, FrameRate::FPS_60)),
            Self::TikTokShorts1080x1920 => Some((1080, 1920, FrameRate::FPS_60)),
            Self::DiscordUnder25Mb => Some((1280, 720, FrameRate::FPS_60)),
            Self::DiscordUnder50Mb => Some((1920, 1080, FrameRate::FPS_60)),
        }
    }

    /// Target video bitrate (kbit/s) for a clip of `duration_secs`.
    pub fn video_kbps(self, duration_secs: f64) -> Option<u32> {
        match self {
            Self::ProjectDefault => None,
            Self::YouTube1440p60 => Some(25_000),
            Self::YouTube4k60 => Some(40_000),
            Self::TikTokShorts1080x1920 => Some(18_000),
            Self::DiscordUnder25Mb | Self::DiscordUnder50Mb => self.target_bytes().map(|b| size_budget_video_kbps(b, duration_secs, self.audio_kbps())),
        }
    }

    /// Codec families this preset wants, best first.
    fn codec_order(self) -> [ExportCodec; 3] {
        match self {
            // Shorts/TikTok and Discord: H.264 uploads and embeds everywhere;
            // AV1/HEVC may not preview inline.
            Self::TikTokShorts1080x1920 | Self::DiscordUnder25Mb | Self::DiscordUnder50Mb => [ExportCodec::H264, ExportCodec::Hevc, ExportCodec::Av1],
            _ => [ExportCodec::Av1, ExportCodec::Hevc, ExportCodec::H264],
        }
    }

    /// Best encoder for this preset out of `available` (which is ordered
    /// fastest backend first, so hardware always wins over `libx264`). Never
    /// `libx265`: too slow for a one-click preset.
    pub fn preferred_codec(self, available: &[ExportPreset]) -> ExportPreset {
        self.codec_order()
            .into_iter()
            .find_map(|codec| available.iter().copied().find(|p| p.codec() == codec && *p != ExportPreset::HevcSoftware))
            .or_else(|| available.first().copied())
            .unwrap_or(ExportPreset::H264Software)
    }

    /// Apply resolution, framerate, codec, and target bitrate to `settings`.
    pub fn apply(self, settings: &mut ExportSettings, project_format: nle_core::VideoFormat, available: &[ExportPreset]) {
        let dur_secs = (settings.end - settings.start).as_secs_f64();
        let Some((width, height, rate)) = self.frame() else {
            settings.width = project_format.width;
            settings.height = project_format.height;
            settings.frame_rate = project_format.rate;
            return;
        };
        settings.preset = self.preferred_codec(available);
        settings.width = width;
        settings.height = height;
        settings.frame_rate = rate;
        if let Some(kbps) = self.video_kbps(dur_secs) {
            settings.rate = RateControl::Bitrate(kbps);
        }
        settings.audio = AudioExport::Aac { kbps: self.audio_kbps() };
        settings.size_capped = self.target_bytes().is_some();
        settings.loudness_target = self.loudness_target();
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum RateControl {
    /// Constant quality: ICQ `global_quality` (QSV), CQP `qp` (VA-API), CRF (x264).
    Quality(u8),
    /// Average bitrate in kbit/s.
    Bitrate(u32),
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum AudioExport {
    Aac { kbps: u32 },
    Pcm,
    None,
}

#[derive(Clone, Debug)]
pub struct ExportSettings {
    pub path: PathBuf,
    pub preset: ExportPreset,
    pub rate: RateControl,
    pub audio: AudioExport,
    pub width: u32,
    pub height: u32,
    pub frame_rate: FrameRate,
    pub start: Time,
    pub end: Time,
    /// Size-capped delivery: clamp the peak rate to the average so the file
    /// cannot overshoot its byte budget.
    pub size_capped: bool,
    /// Normalize the program mix to this integrated loudness (LUFS) with a
    /// -1 dBFS look-ahead limiter; `None` exports the mix as-is.
    pub loudness_target: Option<f32>,
    /// 10-bit (Main10, P010) for HEVC / AV1.
    pub ten_bit: bool,
    /// Keep alpha (transparent background) for formats that carry it.
    pub alpha: bool,
}

impl ExportSettings {
    pub fn new(path: impl Into<PathBuf>, preset: ExportPreset, project: &Project) -> Self {
        let f = project.timeline.format;
        ExportSettings {
            path: path.into(),
            preset,
            rate: RateControl::Quality(preset.default_quality()),
            audio: AudioExport::Aac { kbps: 320 },
            width: f.width,
            height: f.height,
            frame_rate: f.rate,
            start: Time::ZERO,
            end: project.timeline.extent(),
            size_capped: false,
            loudness_target: None,
            ten_bit: false,
            alpha: false,
        }
    }

    /// How the renderer packs frames for this export.
    pub fn pack_format(&self) -> crate::PackFormat {
        use crate::PackFormat;
        if self.preset.is_mastering() {
            PackFormat::Rgba64 { alpha: self.alpha && self.preset.supports_alpha(), linear: self.preset == ExportPreset::ExrSequence }
        } else if self.ten_bit && self.preset.supports_ten_bit() {
            PackFormat::P010
        } else {
            PackFormat::Nv12
        }
    }

    /// Where FFmpeg writes: the file, or the numbered pattern for sequences.
    pub fn output_target(&self) -> PathBuf {
        if self.preset.is_image_sequence() {
            sequence_pattern(&self.path)
        } else {
            self.path.clone()
        }
    }

    /// Encoder dimensions (NV12 packing needs width % 4 == 0, height even).
    pub fn encoded_size(&self) -> (u32, u32) {
        ((self.width.max(4) / 4) * 4, (self.height.max(2) / 2) * 2)
    }

    pub fn total_frames(&self) -> u64 {
        let d = (self.end - self.start).max(Time::ZERO);
        d.frames_round(self.frame_rate).max(0) as u64
    }

    /// Full FFmpeg argument list. `audio_wav` is the pre-mixed audio file.
    pub fn ffmpeg_args(&self, platform: Platform, audio_wav: Option<&Path>) -> Vec<String> {
        let (w, h) = self.encoded_size();
        let mut a: Vec<String> = ["-hide_banner", "-loglevel", "error", "-y"].map(String::from).to_vec();
        match self.preset {
            p if p.is_qsv() => {
                a.extend(hwaccel::qsv_device_args(platform));
                a.extend(["-filter_hw_device", "hw"].map(String::from));
            }
            ExportPreset::H264Vaapi => a.extend(["-vaapi_device", "/dev/dri/renderD128"].map(String::from)),
            _ => {}
        }
        if self.preset.is_mastering() {
            return self.mastering_args(audio_wav);
        }
        let pack = self.pack_format();
        a.extend(["-f".into(), "rawvideo".into(), "-pix_fmt".into(), pack.ffmpeg_name().into()]);
        a.extend(["-s".into(), format!("{w}x{h}"), "-framerate".into(), format!("{}/{}", self.frame_rate.num, self.frame_rate.den)]);
        a.extend(["-i", "pipe:0"].map(String::from));
        let has_audio = audio_wav.is_some() && self.audio != AudioExport::None;
        if let (Some(wav), true) = (audio_wav, has_audio) {
            a.push("-i".into());
            a.push(wav.to_string_lossy().into_owned());
        }
        a.extend(["-map", "0:v:0"].map(String::from));
        if has_audio {
            a.extend(["-map", "1:a:0"].map(String::from));
        }
        // Tag BT.709 limited range on the frames *before* any hardware upload.
        // Output-side -colorspace/-color_range options make FFmpeg insert an
        // auto-scaler after hwupload, which cannot take QSV/VA-API surfaces.
        let tags = "setparams=range=tv:color_primaries=bt709:color_trc=bt709:colorspace=bt709";
        let vf = match self.preset {
            p if p.is_qsv() => format!("{tags},{}", hwaccel::qsv_upload_filter(platform)),
            ExportPreset::H264Vaapi => format!("{tags},format=nv12,hwupload"),
            ExportPreset::HevcSoftware if pack == crate::PackFormat::P010 => format!("{tags},format=yuv420p10le"),
            ExportPreset::HevcSoftware => format!("{tags},format=yuv420p"),
            _ => tags.to_string(),
        };
        let ten = pack == crate::PackFormat::P010;
        a.extend(["-vf".to_string(), vf]);
        a.extend(["-c:v".into(), self.preset.encoder().into()]);
        // Peak rate: 1.5× average normally, pinned to the average for size-capped exports.
        let peak = |k: u32| if self.size_capped { k } else { k * 3 / 2 };
        let bufsize = |k: u32| if self.size_capped { k } else { k * 2 };
        let abr = |k: u32| -> [String; 6] { ["-b:v".into(), format!("{k}k"), "-maxrate".into(), format!("{}k", peak(k)), "-bufsize".into(), format!("{}k", bufsize(k))] };
        match (self.preset, self.rate) {
            (p, RateControl::Quality(q)) if p.is_qsv() => a.extend(["-preset".into(), "slow".into(), "-global_quality".into(), q.to_string()]),
            (p, RateControl::Bitrate(k)) if p.is_qsv() => {
                a.extend(["-preset", "slow"].map(String::from));
                a.extend(abr(k));
            }
            (p, RateControl::Quality(q)) if p.is_nvenc() => {
                a.extend(["-preset", "p5", "-rc", "vbr", "-cq"].map(String::from));
                a.extend([q.to_string(), "-b:v".into(), "0".into()]);
            }
            (p, RateControl::Bitrate(k)) if p.is_nvenc() => {
                a.extend(["-preset", "p5", "-rc", "vbr"].map(String::from));
                a.extend(abr(k));
            }
            (p, RateControl::Quality(q)) if p.is_amf() => {
                a.extend(["-quality", "quality", "-rc", "cqp"].map(String::from));
                a.extend(["-qp_i".into(), q.to_string(), "-qp_p".into(), q.to_string()]);
            }
            (p, RateControl::Bitrate(k)) if p.is_amf() => {
                a.extend(["-quality", "quality", "-rc", "vbr_peak"].map(String::from));
                a.extend(abr(k));
            }
            (ExportPreset::H264Vaapi, RateControl::Quality(q)) => a.extend(["-rc_mode".into(), "CQP".into(), "-qp".into(), q.to_string()]),
            (ExportPreset::H264Vaapi, RateControl::Bitrate(k)) => a.extend(["-rc_mode".into(), "VBR".into(), "-b:v".into(), format!("{k}k")]),
            (_, RateControl::Quality(q)) => a.extend(["-preset".into(), "medium".into(), "-crf".into(), q.to_string()]),
            (_, RateControl::Bitrate(k)) => {
                a.extend(["-preset", "medium"].map(String::from));
                a.extend(abr(k));
            }
        }
        if ten && self.preset.codec() == ExportCodec::Hevc && (self.preset.is_nvenc() || self.preset.is_amf()) {
            a.extend(["-profile:v", "main10"].map(String::from));
        }
        if self.preset == ExportPreset::HevcSoftware {
            // stderr is only read at the end: x265's chatter could fill the pipe
            a.extend(["-x265-params", "log-level=error"].map(String::from));
        }
        if self.preset.codec() == ExportCodec::Hevc {
            a.extend(["-tag:v", "hvc1"].map(String::from));
        }
        if has_audio {
            match self.audio {
                AudioExport::Aac { kbps } => a.extend(["-c:a".into(), "aac".into(), "-b:a".into(), format!("{kbps}k")]),
                AudioExport::Pcm => a.extend(["-c:a", "pcm_s24le"].map(String::from)),
                AudioExport::None => {}
            }
        }
        let ext = self.path.extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default();
        if ext == "mp4" || ext == "m4v" || ext == "mov" {
            a.extend(["-movflags", "+faststart"].map(String::from));
        }
        a.push(self.path.to_string_lossy().into_owned());
        a
    }
}

impl ExportSettings {
    /// FFmpeg arguments for ProRes / DNxHR / image sequences: 16-bit RGBA in,
    /// converted on the CPU to the format's native layout.
    fn mastering_args(&self, audio_wav: Option<&Path>) -> Vec<String> {
        let (w, h) = self.encoded_size();
        let alpha = self.alpha && self.preset.supports_alpha();
        let seq = self.preset.is_image_sequence();
        let mut a: Vec<String> = ["-hide_banner", "-loglevel", "error", "-y"].map(String::from).to_vec();
        a.extend(["-f", "rawvideo", "-pix_fmt", "rgba64le"].map(String::from));
        a.extend(["-s".into(), format!("{w}x{h}"), "-framerate".into(), format!("{}/{}", self.frame_rate.num, self.frame_rate.den)]);
        a.extend(["-i", "pipe:0"].map(String::from));
        let has_audio = !seq && audio_wav.is_some() && self.audio != AudioExport::None;
        if let (Some(wav), true) = (audio_wav, has_audio) {
            a.push("-i".into());
            a.push(wav.to_string_lossy().into_owned());
        }
        a.extend(["-map", "0:v:0"].map(String::from));
        if has_audio {
            a.extend(["-map", "1:a:0"].map(String::from));
        }
        let yuv = |fmt: &str| format!("scale=out_color_matrix=bt709:out_range=tv,format={fmt},setparams=range=tv:color_primaries=bt709:color_trc=bt709:colorspace=bt709");
        let (vf, codec_args): (String, Vec<&str>) = match self.preset {
            ExportPreset::ProRes422 => (yuv("yuv422p10le"), vec!["-profile:v", "2", "-vendor", "apl0"]),
            ExportPreset::ProRes422Hq => (yuv("yuv422p10le"), vec!["-profile:v", "3", "-vendor", "apl0"]),
            ExportPreset::ProRes4444 if alpha => (yuv("yuva444p10le"), vec!["-profile:v", "4", "-vendor", "apl0", "-alpha_bits", "16"]),
            ExportPreset::ProRes4444 => (yuv("yuv444p10le"), vec!["-profile:v", "4", "-vendor", "apl0"]),
            ExportPreset::DnxhrHq => (yuv("yuv422p"), vec!["-profile:v", "dnxhr_hq"]),
            ExportPreset::DnxhrHqx => (yuv("yuv422p10le"), vec!["-profile:v", "dnxhr_hqx"]),
            ExportPreset::Dnxhr444 => (yuv("yuv444p10le"), vec!["-profile:v", "dnxhr_444"]),
            ExportPreset::PngSequence => (format!("format={}", if alpha { "rgba64be" } else { "rgb48be" }), vec![]),
            ExportPreset::TiffSequence => (format!("format={}", if alpha { "rgba64le" } else { "rgb48le" }), vec!["-compression_algo", "deflate"]),
            ExportPreset::ExrSequence => (format!("format={}", if alpha { "gbrapf32le" } else { "gbrpf32le" }), vec!["-compression", "zip16", "-format", "half"]),
            _ => unreachable!("not a mastering preset"),
        };
        a.extend(["-vf".into(), vf, "-c:v".into(), self.preset.encoder().into()]);
        a.extend(codec_args.into_iter().map(String::from));
        if has_audio {
            match self.audio {
                AudioExport::Aac { kbps } => a.extend(["-c:a".into(), "aac".into(), "-b:a".into(), format!("{kbps}k")]),
                AudioExport::Pcm => a.extend(["-c:a", "pcm_s24le"].map(String::from)),
                AudioExport::None => {}
            }
        }
        if seq {
            a.extend(["-f", "image2", "-start_number", "0"].map(String::from));
        }
        a.push(self.output_target().to_string_lossy().into_owned());
        a
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct ExportProgress {
    pub frame: u64,
    pub total: u64,
    pub fps: f32,
}

/// Write interleaved stereo f32 as a 32-bit float WAV.
pub fn write_wav_f32(path: &Path, rate: u32, samples: &[f32]) -> std::io::Result<()> {
    let mut f = std::io::BufWriter::new(std::fs::File::create(path)?);
    let data_len = (samples.len() * 4) as u32;
    f.write_all(b"RIFF")?;
    f.write_all(&(36 + data_len).to_le_bytes())?;
    f.write_all(b"WAVEfmt ")?;
    f.write_all(&16u32.to_le_bytes())?;
    f.write_all(&3u16.to_le_bytes())?; // IEEE float
    f.write_all(&2u16.to_le_bytes())?;
    f.write_all(&rate.to_le_bytes())?;
    f.write_all(&(rate * 8).to_le_bytes())?;
    f.write_all(&8u16.to_le_bytes())?;
    f.write_all(&32u16.to_le_bytes())?;
    f.write_all(b"data")?;
    f.write_all(&data_len.to_le_bytes())?;
    for s in samples {
        f.write_all(&s.to_le_bytes())?;
    }
    f.flush()
}

/// Mix the timeline's audio for `[start, end)` at 48 kHz stereo.
pub fn mix_audio(project: &Project, sources: &HashMap<AssetId, Arc<AudioBuffer>>, start: Time, end: Time) -> Vec<f32> {
    let mut mixer = TimelineMixer::new(48_000);
    let spans = eval::audio_spans(&project.timeline);
    // Look-ahead ducking: the same cached-envelope computation the preview uses,
    // over the whole timeline so the curve is identical to what was heard.
    if nle_media::audio::needs_duck_envelope(&spans) {
        let env = nle_media::audio::dialogue_envelope(&spans, sources, project.timeline.extent(), &|| false);
        mixer.set_duck_envelope(env.map(Arc::new));
    }
    // Noise reduction / voice isolation: rendered up front, exactly like the
    // preview's background job (same keys → same audio).
    let restored = nle_media::audio::restore_all(&spans, sources, &|| false).unwrap_or_default();
    mixer.set_spans(spans);
    for (id, buf) in sources {
        mixer.set_source(*id, buf.clone());
    }
    for (clip, r) in restored {
        mixer.set_restored(clip, r);
    }
    // Bus faders / mutes / inserts apply; solo is a monitoring aid only.
    mixer.set_mixer(&project.timeline.mixer);
    // Integer sample positions: audio never drifts against video PTS.
    let frames = (end - start).max(Time::ZERO).samples_round(48_000) as usize;
    let mut out = vec![0.0f32; frames * 2];
    const CHUNK: usize = 4_800;
    let mut i = 0;
    while i < frames {
        let n = CHUNK.min(frames - i);
        let t = start + Time::from_samples(i as i64, 48_000);
        mixer.render(t, 1.0, &mut out[i * 2..(i + n) * 2]);
        i += n;
    }
    out
}

/// Frame source backed by blocking full-resolution decoders.
pub struct ExportSource {
    server: FrameServer,
    assets: HashMap<AssetId, MediaAsset>,
    /// The first picture that could not be read.
    error: Option<String>,
}

impl ExportSource {
    pub fn new(project: &Project) -> Self {
        ExportSource { server: FrameServer::new(2, None), assets: project.assets.iter().map(|a| (a.id, a.clone())).collect(), error: None }
    }
}

impl FrameSource for ExportSource {
    fn frame_rate(&self, asset: AssetId) -> FrameRate {
        self.assets.get(&asset).map(|a| a.frame_rate()).unwrap_or_default()
    }
    fn frame(&mut self, asset: AssetId, index: i64) -> Option<VideoFrame> {
        let Some(a) = self.assets.get(&asset) else {
            self.error.get_or_insert_with(|| "a clip's media is no longer in the project".into());
            return None;
        };
        if !a.has_video() {
            return None;
        }
        let t = Time::from_frames(index, a.frame_rate());
        match self.server.frame_sync(a, t, false) {
            Ok(frame) => Some(frame),
            Err(e) => {
                self.error.get_or_insert_with(|| format!("picture of {}: {e}", a.path.display()));
                None
            }
        }
    }
    fn matte(&mut self, asset: AssetId, index: i64, kind: nle_core::MatteKind) -> Option<nle_media::matte::Matte> {
        nle_media::matte::load(self.assets.get(&asset)?, kind, index)
    }
    fn yuv_matrix(&self, asset: AssetId) -> Option<nle_core::YuvMatrix> {
        Some(self.assets.get(&asset)?.video.as_ref()?.yuv_matrix())
    }
}

/// `decoded`, plus the sound of any clip audible in `start..end` that is not
/// in it yet (media still being analysed, or just relinked).
fn export_audio(
    project: &Project,
    decoded: &HashMap<AssetId, Arc<AudioBuffer>>,
    start: Time,
    end: Time,
    cancel: &AtomicBool,
) -> Result<HashMap<AssetId, Arc<AudioBuffer>>, ExportError> {
    let mut sources = decoded.clone();
    for span in eval::audio_spans(&project.timeline) {
        let silent = span.params.muted || span.containers.iter().any(|c| c.params.muted);
        if silent || span.start >= end || span.end() <= start || sources.contains_key(&span.asset) {
            continue;
        }
        if cancel.load(Ordering::Relaxed) {
            return Err(ExportError::Cancelled);
        }
        let asset = project.asset(span.asset).ok_or_else(|| ExportError::Media("a clip's sound is no longer in the project".into()))?;
        if !asset.has_audio() {
            continue;
        }
        let sound = nle_media::audio::decode::decode_audio(asset).map_err(|e| ExportError::Media(format!("sound of {}: {e}", asset.path.display())))?;
        sources.insert(asset.id, Arc::new(sound));
    }
    Ok(sources)
}

/// Where `path` is written until it is complete.
fn partial_path(path: &Path, id: &str) -> PathBuf {
    let stem = path.file_stem().unwrap_or_default().to_string_lossy();
    let ext = path.extension().map(|e| format!(".{}", e.to_string_lossy())).unwrap_or_default();
    path.with_file_name(format!("{stem}.part-{id}{ext}"))
}

/// Run a complete export (blocking — call from a background thread).
///
/// The file appears at `settings.path` only once it is complete: cancelling or
/// failing leaves whatever was there before.
pub fn run_export(
    project: &Project,
    settings: &ExportSettings,
    gpu: GpuContext,
    audio_sources: &HashMap<AssetId, Arc<AudioBuffer>>,
    cancel: &AtomicBool,
    mut progress: impl FnMut(ExportProgress),
) -> Result<(), ExportError> {
    if !ffmpeg_cli::available() {
        return Err(ExportError::NoFfmpeg);
    }
    let (w, h) = settings.encoded_size();
    let total = settings.total_frames();
    let pack = settings.pack_format();
    let seq = settings.preset.is_image_sequence();
    if let Some(dir) = settings.path.parent().filter(|d| !d.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir)?;
    }

    // unique per export: queued / Copilot exports can run side by side
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let id = format!("{}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed));

    // sequences get the mix as a sidecar WAV next to the frames
    let wav = if settings.audio != AudioExport::None {
        let path = if seq { settings.path.with_extension("wav") } else { std::env::temp_dir().join(format!("rcp-export-{id}.wav")) };
        let sources = export_audio(project, audio_sources, settings.start, settings.end, cancel)?;
        let mut pcm = mix_audio(project, &sources, settings.start, settings.end);
        if let Some(target) = settings.loudness_target {
            use nle_media::analysis::loudness::{normalize_program, CEILING_DBFS};
            match normalize_program(&mut pcm, 2, 48_000, target, CEILING_DBFS) {
                Some(r) => log::info!("export: loudness {:.1} LUFS -> {target:.1} ({:+.1} dB, limiter {:.1} dB)", r.measured_lufs, r.gain_db, r.limited_db),
                None => log::info!("export: mix is silent; loudness normalization skipped"),
            }
        }
        write_wav_f32(&path, 48_000, &pcm)?;
        Some(path)
    } else {
        None
    };

    let mut output = settings.clone();
    if !seq {
        output.path = partial_path(&settings.path, &id);
    }
    let args = output.ffmpeg_args(Platform::current(), wav.as_deref());
    log::info!("export: ffmpeg {}", args.join(" "));
    let mut child = ffmpeg_cli::command(&ffmpeg_cli::ffmpeg_path()).args(&args).stdin(Stdio::piped()).stdout(Stdio::null()).stderr(Stdio::piped()).spawn()?;
    let mut stdin = child.stdin.take().ok_or_else(|| ExportError::Encoder("no stdin".into()))?;

    let mut renderer = Renderer::new(gpu, w, h);
    let mut source = ExportSource::new(project);
    let started = Instant::now();
    let mut result = Ok(());
    for n in 0..total {
        if cancel.load(Ordering::Relaxed) {
            result = Err(ExportError::Cancelled);
            break;
        }
        let t = settings.start + Time::from_frames(n as i64, settings.frame_rate);
        let comp = eval::evaluate(&project.timeline, t);
        let frame = renderer.render_packed(&comp, &mut source, RenderOptions::default(), pack);
        if let Some(e) = source.error.take() {
            result = Err(ExportError::Media(e));
            break;
        }
        if let Err(e) = stdin.write_all(&frame) {
            result = Err(ExportError::Encoder(format!("pipe closed: {e}")));
            break;
        }
        let secs = started.elapsed().as_secs_f32().max(1e-3);
        progress(ExportProgress { frame: n + 1, total, fps: (n + 1) as f32 / secs });
    }
    drop(stdin);
    let out = child.wait_with_output()?;
    if let Some(w) = wav.as_ref().filter(|_| !seq) {
        let _ = std::fs::remove_file(w);
    }
    let result = match result {
        Err(e @ (ExportError::Cancelled | ExportError::Media(_))) => Err(e),
        _ if !out.status.success() => {
            // FFmpeg's own diagnostics are more useful than "pipe closed"
            let stderr = String::from_utf8_lossy(&out.stderr);
            let msg = stderr.lines().rev().find(|l| !l.trim().is_empty()).unwrap_or("unknown error").trim().to_string();
            Err(ExportError::Encoder(msg))
        }
        other => other,
    };
    if seq {
        return result;
    }
    let result = result.and_then(|()| {
        std::fs::rename(&output.path, &settings.path)
            .map_err(|e| std::io::Error::new(e.kind(), format!("cannot replace {} (open in another program?): {e}", settings.path.display())).into())
    });
    if result.is_err() {
        // only ever this export's own partial file
        let _ = std::fs::remove_file(&output.path);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings(preset: ExportPreset, rate: RateControl) -> ExportSettings {
        let p = Project::new("t");
        let mut s = ExportSettings::new("/out/movie.mp4", preset, &p);
        s.rate = rate;
        s
    }

    #[test]
    fn av1_qsv_icq_args() {
        let s = settings(ExportPreset::Av1Qsv, RateControl::Quality(28));
        let a = s.ffmpeg_args(Platform::Linux, Some(Path::new("/tmp/a.wav"))).join(" ");
        assert!(a.contains("-c:v av1_qsv"));
        assert!(a.contains("-global_quality 28"));
        assert!(a.contains("qsv=hw@va"), "QSV derived from VA-API on Linux");
        assert!(a.contains("setparams=range=tv:color_primaries=bt709:color_trc=bt709:colorspace=bt709,hwupload=extra_hw_frames=64,format=qsv"));
        assert!(!a.contains("-colorspace"), "colour tags are set in the filter graph, not on the output");
        assert!(a.contains("-c:a aac -b:a 320k"));
        assert!(a.contains("-pix_fmt nv12 -s 1920x1080"));
    }

    #[test]
    fn hevc_bitrate_and_windows_device() {
        let s = settings(ExportPreset::HevcQsv, RateControl::Bitrate(40_000));
        let a = s.ffmpeg_args(Platform::Windows, None).join(" ");
        assert!(a.contains("d3d11va=dx:,vendor_id=0x8086"));
        assert!(a.contains("-b:v 40000k -maxrate 60000k"));
        assert!(a.contains("-tag:v hvc1"));
        assert!(!a.contains("-c:a"), "no audio input → no audio codec");
        assert!(a.contains(",hwupload,format=qsv"), "no fixed frame pool on D3D11");
    }

    #[test]
    fn vaapi_and_software() {
        let a = settings(ExportPreset::H264Vaapi, RateControl::Quality(20)).ffmpeg_args(Platform::Linux, None).join(" ");
        assert!(a.contains("-vaapi_device /dev/dri/renderD128") && a.contains("-rc_mode CQP -qp 20"));
        let mut s = settings(ExportPreset::H264Software, RateControl::Quality(18));
        s.audio = AudioExport::Pcm;
        let a = s.ffmpeg_args(Platform::Windows, Some(Path::new("a.wav"))).join(" ");
        assert!(a.contains("-c:v libx264") && a.contains("-crf 18") && a.contains("pcm_s24le"));
        assert!(!ExportPreset::H264Vaapi.supported_on(Platform::Windows));
    }

    #[test]
    fn wav_header() {
        let p = std::env::temp_dir().join(format!("rcp-wav-test-{}.wav", std::process::id()));
        write_wav_f32(&p, 48_000, &[0.0, 0.5, -0.5, 1.0]).unwrap();
        let b = std::fs::read(&p).unwrap();
        assert_eq!(&b[0..4], b"RIFF");
        assert_eq!(u16::from_le_bytes([b[20], b[21]]), 3);
        assert_eq!(b.len(), 44 + 16);
        let _ = std::fs::remove_file(p);
    }

    #[test]
    fn nvenc_and_amf_args() {
        let a = settings(ExportPreset::HevcNvenc, RateControl::Quality(24)).ffmpeg_args(Platform::Windows, None).join(" ");
        assert!(a.contains("-c:v hevc_nvenc -preset p5 -rc vbr -cq 24 -b:v 0") && a.contains("-tag:v hvc1"), "{a}");
        assert!(!a.contains("hwupload") && !a.contains("init_hw_device"));
        let a = settings(ExportPreset::Av1Nvenc, RateControl::Bitrate(25_000)).ffmpeg_args(Platform::Windows, None).join(" ");
        assert!(a.contains("-c:v av1_nvenc") && a.contains("-b:v 25000k -maxrate 37500k -bufsize 50000k"), "{a}");
        let a = settings(ExportPreset::H264Amf, RateControl::Quality(22)).ffmpeg_args(Platform::Windows, None).join(" ");
        assert!(a.contains("-c:v h264_amf -quality quality -rc cqp -qp_i 22 -qp_p 22"), "{a}");
        let mut s = settings(ExportPreset::HevcAmf, RateControl::Bitrate(8_000));
        s.size_capped = true;
        let a = s.ffmpeg_args(Platform::Windows, None).join(" ");
        assert!(a.contains("-rc vbr_peak -b:v 8000k -maxrate 8000k -bufsize 8000k"), "{a}");
    }

    #[test]
    fn ten_bit_hevc_and_av1_pack_p010() {
        let mut s = settings(ExportPreset::HevcQsv, RateControl::Quality(24));
        s.ten_bit = true;
        let a = s.ffmpeg_args(Platform::Windows, None).join(" ");
        assert!(a.contains("-pix_fmt p010le") && a.contains("-c:v hevc_qsv"), "{a}");
        let mut s = settings(ExportPreset::HevcNvenc, RateControl::Quality(24));
        s.ten_bit = true;
        assert!(s.ffmpeg_args(Platform::Windows, None).join(" ").contains("-profile:v main10"));
        // H.264 has no 10-bit option: the flag is ignored
        let mut s = settings(ExportPreset::H264Software, RateControl::Quality(20));
        s.ten_bit = true;
        assert_eq!(s.pack_format(), crate::PackFormat::Nv12);
    }

    #[test]
    fn mastering_formats_convert_from_rgba64() {
        let mut s = settings(ExportPreset::ProRes4444, RateControl::Quality(0));
        s.path = PathBuf::from("master.mov");
        s.alpha = true;
        s.audio = AudioExport::Pcm;
        let a = s.ffmpeg_args(Platform::Windows, Some(Path::new("a.wav"))).join(" ");
        assert!(a.contains("-pix_fmt rgba64le") && a.contains("format=yuva444p10le") && a.contains("-c:v prores_ks -profile:v 4"), "{a}");
        assert!(a.contains("-alpha_bits 16") && a.contains("pcm_s24le") && a.ends_with("master.mov"), "{a}");
        assert!(!a.contains("qsv"), "mastering never touches the GPU encoder: {a}");
        let s = settings(ExportPreset::DnxhrHqx, RateControl::Quality(0));
        let a = s.ffmpeg_args(Platform::Windows, None).join(" ");
        assert!(a.contains("format=yuv422p10le") && a.contains("-c:v dnxhd -profile:v dnxhr_hqx"), "{a}");
        assert_eq!(s.pack_format(), crate::PackFormat::Rgba64 { alpha: false, linear: false });
    }

    #[test]
    fn image_sequences_use_a_numbered_pattern_and_no_audio_stream() {
        let mut s = settings(ExportPreset::ExrSequence, RateControl::Quality(0));
        s.path = PathBuf::from("out/shot.exr");
        let a = s.ffmpeg_args(Platform::Windows, Some(Path::new("a.wav")));
        let j = a.join(" ");
        assert!(j.contains("-f image2 -start_number 0") && j.contains("-c:v exr") && !j.contains("1:a:0"), "{j}");
        assert_eq!(Path::new(a.last().unwrap()), Path::new("out/shot_%06d.exr"));
        assert_eq!(s.pack_format(), crate::PackFormat::Rgba64 { alpha: false, linear: true });
        assert_eq!(sequence_pattern(Path::new("frames.png")), PathBuf::from("frames_%06d.png"));
    }

    #[test]
    fn working_presets_follow_probe_order() {
        use nle_media::encoder_probe::{probe_with, EncoderBackend};
        let nvidia = probe_with(&HwCaps::default(), |_, b| matches!(b, EncoderBackend::Nvenc | EncoderBackend::Cpu));
        let w = working_presets(&nvidia);
        assert_eq!(w, [ExportPreset::Av1Nvenc, ExportPreset::HevcNvenc, ExportPreset::H264Nvenc, ExportPreset::H264Software, ExportPreset::HevcSoftware]);
        assert_eq!(GamingDeliveryPreset::YouTube4k60.preferred_codec(&w), ExportPreset::Av1Nvenc);
        assert_eq!(GamingDeliveryPreset::TikTokShorts1080x1920.preferred_codec(&w), ExportPreset::H264Nvenc);
        // CPU-only machine and "nothing works" both degrade without panicking
        let cpu = working_presets(&probe_with(&HwCaps::default(), |_, b| b == EncoderBackend::Cpu));
        assert_eq!(GamingDeliveryPreset::YouTube1440p60.preferred_codec(&cpu), ExportPreset::H264Software);
        assert!(working_presets(&nle_media::EncoderProbe::default()).is_empty());
        assert_eq!(GamingDeliveryPreset::DiscordUnder25Mb.preferred_codec(&[]), ExportPreset::H264Software);
        for p in ExportPreset::ALL {
            assert_eq!(ExportPreset::from_encoder(p.encoder()), Some(p));
        }
    }
}
