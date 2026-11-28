//! Decoder that streams raw frames out of the `ffmpeg` executable.
//!
//! The process is kept alive while frames are requested sequentially
//! (playback), and restarted with a fast input seek when the requested
//! frame jumps (scrubbing). If a hardware path fails to produce a frame
//! the next plan (see [`hwaccel::plan_decode`]) is tried silently.
//! Past the end of the picture the last frame is held; a file that cannot be
//! read is an error, never a held frame.

use super::{alpha_decoder, deinterlace_for, preroll_frames, proxy_usable, scaled_dims, DecodeOptions, VideoDecoder};
use crate::ffmpeg_cli;
use crate::frame::{PixelFormat, VideoFrame};
use crate::hwaccel::{self, Chroma, DecoderPlan, HwBackend, Platform, VideoCodec};
use crate::MediaError;
use nle_core::{AssetKind, FrameRate, MediaAsset, Time, YuvMatrix};
use std::io::{ErrorKind, Read};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdout, Stdio};

/// How far ahead (in frames) we read forward instead of re-seeking.
const MAX_FORWARD_READ_SECS: i64 = 2;

/// The `-i` of `path` with the `-ss` before and after it that start exactly on
/// frame `index`.
fn seek_args(cmd: &mut std::process::Command, path: &Path, index: i64, rate: FrameRate) {
    let index = index.max(0);
    let preroll = preroll_frames(path, rate);
    // Near the top, decode from it: FLV cannot seek to before its first timestamp.
    let trim = if index < 2 * preroll { index } else { preroll };
    let secs = |frames: i64| Time::from_frames(frames, rate).as_secs_f64();
    if index > trim {
        cmd.args(["-ss", &format!("{:.6}", secs(index - trim))]);
    }
    cmd.arg("-i").arg(path);
    if trim > 0 {
        // a quarter frame early: Matroska and FLV round times to milliseconds,
        // which can put the wanted frame just before its exact time
        cmd.args(["-ss", &format!("{:.6}", secs(trim) - 0.25 / rate.fps())]);
    }
}

pub struct CliDecoder {
    path: PathBuf,
    plans: Vec<DecoderPlan>,
    plan_idx: usize,
    src_w: u32,
    src_h: u32,
    width: u32,
    height: u32,
    format: PixelFormat,
    /// Source chroma layout (picks the hardware download format).
    chroma: Chroma,
    rate: FrameRate,
    /// Variable-rate source: resampled by timestamp.
    resample: bool,
    child: Option<Child>,
    stdout: Option<ChildStdout>,
    next_index: i64,
    last: Option<(i64, VideoFrame)>,
    /// No later than this is the picture expected to end (the container's
    /// length; the first frame for a still).
    expected_end: i64,
    /// The file's final frame, once it has been found.
    end: Option<(i64, VideoFrame)>,
    /// `bwdif` filter for interlaced sources.
    deinterlace: Option<String>,
    /// The source has an alpha channel: decoded on the CPU to straight RGBA
    /// (hardware decoders and the 4:2:0 formats have no alpha).
    alpha: bool,
    /// YCbCr matrix for the RGBA conversion of an alpha source.
    matrix: YuvMatrix,
    /// Decoding the proxy: its size differs from the source's, so the
    /// picture is always scaled to `width` x `height`.
    from_proxy: bool,
}

impl CliDecoder {
    pub fn open(asset: &MediaAsset, opts: DecodeOptions) -> Result<Self, MediaError> {
        let v = asset.video.as_ref().ok_or_else(|| MediaError::Unsupported("no video stream".into()))?;
        let use_proxy = proxy_usable(asset, opts);
        let path = if use_proxy { asset.proxy_path.clone().unwrap_or_else(|| asset.path.clone()) } else { asset.path.clone() };
        let codec = if use_proxy { VideoCodec::H264 } else { VideoCodec::from_ffmpeg_name(&v.codec) };
        let depth = if use_proxy { 8 } else { v.bit_depth };
        let chroma = if use_proxy { Chroma::Yuv420 } else { Chroma::from_pix_fmt(&v.pix_fmt) };
        let alpha = v.alpha();
        let mut plans = hwaccel::plan_decode(codec, depth, chroma, Platform::current(), hwaccel::probe_caps(), hwaccel::active_gpu());
        if alpha {
            plans.retain(|p| p.backend == HwBackend::Software);
            // the alpha proxy is ProRes 4444, which FFmpeg's own decoder reads with alpha
            if let Some(dec) = alpha_decoder(&v.codec).filter(|_| !use_proxy) {
                for p in &mut plans {
                    p.decoder = Some(dec.into());
                    p.input_args.extend(["-c:v".to_string(), dec.to_string()]);
                }
            }
            log::info!("{}: alpha channel ({}) — software RGBA decode{}", asset.name, v.pix_fmt, if use_proxy { " of the ProRes 4444 proxy" } else { "" });
        }
        let (width, height) = scaled_dims(v.width, v.height, opts.max_height);
        log::debug!("{}: decode plan {:?}", asset.name, plans.iter().map(|p| p.backend).collect::<Vec<_>>());
        let deinterlace = deinterlace_for(asset, opts);
        Ok(CliDecoder {
            path,
            plans,
            plan_idx: 0,
            src_w: v.width,
            src_h: v.height,
            width,
            height,
            format: if alpha { PixelFormat::Rgba8 } else { PixelFormat::for_bit_depth(depth) },
            chroma,
            rate: v.frame_rate,
            resample: v.is_vfr,
            child: None,
            stdout: None,
            next_index: -1,
            last: None,
            expected_end: if asset.kind == AssetKind::Image { 0 } else { asset.duration.frames_floor(v.frame_rate).max(0) },
            end: None,
            deinterlace,
            alpha,
            matrix: v.yuv_matrix(),
            from_proxy: use_proxy,
        })
    }

    fn needs_scale(&self) -> bool {
        self.from_proxy || (self.width, self.height) != (self.src_w, self.src_h)
    }

    /// Output-side filter graph for the active plan.
    fn filter(&self, out_fmt: &str) -> (Vec<String>, String) {
        let plan = &self.plans[self.plan_idx];
        let (w, h) = (self.width, self.height);
        let sw_fmt = self.chroma.hw_download_format(if self.format == PixelFormat::P010 { 10 } else { 8 });
        let scale = self.needs_scale();
        if self.alpha {
            // software only: deinterlace, scale and convert to straight RGBA
            // in one swscale pass with the source's matrix (ignored for RGB
            // sources). Bilinear on straight alpha can tint the outermost
            // edge pixel a little toward its transparent neighbours.
            let di = self.deinterlace.as_ref().map(|d| format!("{d},")).unwrap_or_default();
            let m = match self.matrix {
                YuvMatrix::Bt709 => "bt709",
                YuvMatrix::Bt601 => "bt601",
                YuvMatrix::Bt2020 => "bt2020",
            };
            let (w, h) = if scale { (w.to_string(), h.to_string()) } else { ("iw".into(), "ih".into()) };
            return (Vec::new(), format!("{di}scale={w}:{h}:flags=bilinear:in_color_matrix={m},format={out_fmt}"));
        }
        if let Some(di) = &self.deinterlace {
            // fields must be separated before any vertical scaling, so the
            // picture is downloaded first and scaled on the CPU
            let sc = if scale { format!("scale={w}:{h}:flags=bilinear,") } else { String::new() };
            return match plan.backend {
                HwBackend::Qsv => (vec!["-hwaccel_output_format".into(), "qsv".into()], format!("hwdownload,format={sw_fmt},{di},{sc}format={out_fmt}")),
                HwBackend::Vaapi => (vec!["-hwaccel_output_format".into(), "vaapi".into()], format!("hwdownload,format={sw_fmt},{di},{sc}format={out_fmt}")),
                _ => (Vec::new(), format!("{di},{sc}format={out_fmt}")),
            };
        }
        // QSV / VA-API surfaces stay on the GPU (scaled there when needed) and
        // are downloaded explicitly. Letting FFmpeg auto-download QSV frames
        // fails for HEVC Main10 and AV1 ("Impossible to convert between the
        // formats"), verified on an Arc A750 with FFmpeg 9.0.
        match plan.backend {
            HwBackend::Qsv => {
                let vpp = if scale { format!("vpp_qsv=w={w}:h={h},") } else { String::new() };
                (vec!["-hwaccel_output_format".into(), "qsv".into()], format!("{vpp}hwdownload,format={sw_fmt},format={out_fmt}"))
            }
            HwBackend::Vaapi => {
                let sc = if scale { format!("scale_vaapi=w={w}:h={h},") } else { String::new() };
                (vec!["-hwaccel_output_format".into(), "vaapi".into()], format!("{sc}hwdownload,format={sw_fmt},format={out_fmt}"))
            }
            _ if scale => (Vec::new(), format!("scale={w}:{h}:flags=bilinear,format={out_fmt}")),
            _ => (Vec::new(), format!("format={out_fmt}")),
        }
    }

    fn kill(&mut self) {
        self.stdout = None;
        if let Some(mut c) = self.child.take() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }

    fn spawn(&mut self, frame_index: i64) -> Result<(), MediaError> {
        self.kill();
        let plan = self.plans[self.plan_idx].clone();
        let (hw_out, mut vf) = self.filter(self.format.ffmpeg_name());
        if self.resample {
            // `-fps_mode cfr` alone lets a picture lag over a frame before it
            // drops one, which adds up to 100 ms behind the sound
            vf = format!("fps={}/{},{vf}", self.rate.num, self.rate.den);
        }
        let mut cmd = ffmpeg_cli::command(&ffmpeg_cli::ffmpeg_path());
        cmd.args(["-hide_banner", "-loglevel", "error", "-nostdin"]);
        cmd.args(&plan.input_args);
        cmd.args(&hw_out);
        seek_args(&mut cmd, &self.path, frame_index, self.rate);
        cmd.args(["-map", "0:v:0", "-an", "-sn", "-vf", &vf]);
        cmd.args(["-fps_mode", "cfr", "-r", &format!("{}/{}", self.rate.num, self.rate.den)]);
        cmd.args(["-pix_fmt", self.format.ffmpeg_name(), "-f", "rawvideo", "pipe:1"]);
        cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null());
        let mut child = ffmpeg_cli::spawn_confined(&mut cmd).map_err(MediaError::Io)?;
        self.stdout = child.stdout.take();
        self.child = Some(child);
        self.next_index = frame_index.max(0);
        Ok(())
    }

    fn read_frame(&mut self) -> Result<VideoFrame, MediaError> {
        let size = self.format.frame_size(self.width, self.height);
        let out = self.stdout.as_mut().ok_or(MediaError::Eof)?;
        let mut buf = vec![0u8; size];
        if let Err(e) = out.read_exact(&mut buf) {
            return Err(if e.kind() == ErrorKind::UnexpectedEof { self.stream_ended() } else { MediaError::Io(e) });
        }
        let pts = Time::from_frames(self.next_index, self.rate);
        self.next_index += 1;
        Ok(VideoFrame::new(self.width, self.height, self.format, buf, pts))
    }

    /// What an exhausted stream means: the end of the picture if FFmpeg
    /// exited cleanly, otherwise a file it could not read.
    fn stream_ended(&mut self) -> MediaError {
        self.stdout = None;
        match self.child.take().map(|mut c| c.wait()) {
            Some(Ok(status)) if !status.success() => MediaError::Decode(format!("FFmpeg could not read {} ({status})", self.path.display())),
            Some(Err(e)) => MediaError::Io(e),
            _ => MediaError::Eof,
        }
    }

    /// The last frame of the picture, for a request past it (`idx`). Reads to
    /// the end from a little before it, stepping further back while that
    /// finds nothing (the sound can outlast the picture).
    fn final_frame(&mut self, idx: i64) -> Result<(i64, VideoFrame), MediaError> {
        let from = idx.min(self.expected_end);
        let mut back = self.rate.nominal().max(1);
        loop {
            let start = (from - back).max(0);
            self.spawn(start)?;
            let mut found = None;
            loop {
                match self.read_frame() {
                    Ok(f) => found = Some((self.next_index - 1, f)),
                    Err(MediaError::Eof) => break,
                    Err(e) => return Err(e),
                }
            }
            match found {
                Some(end) => return Ok(end),
                None if start == 0 => return Err(MediaError::Eof),
                None => back *= 4,
            }
        }
    }

    fn decode_index(&mut self, idx: i64) -> Result<VideoFrame, MediaError> {
        let max_skip = MAX_FORWARD_READ_SECS * self.rate.nominal();
        let streaming = self.stdout.is_some() && idx >= self.next_index && idx - self.next_index <= max_skip;
        if !streaming {
            self.spawn(idx)?;
        }
        loop {
            match self.read_frame() {
                Ok(f) if self.next_index > idx => return Ok(f),
                Ok(_) => continue,
                Err(e) => return Err(e),
            }
        }
    }
}

impl Drop for CliDecoder {
    fn drop(&mut self) {
        self.kill();
    }
}

impl VideoDecoder for CliDecoder {
    fn width(&self) -> u32 {
        self.width
    }
    fn height(&self) -> u32 {
        self.height
    }
    fn format(&self) -> PixelFormat {
        self.format
    }
    fn frame_rate(&self) -> FrameRate {
        self.rate
    }
    fn backend(&self) -> HwBackend {
        self.plans[self.plan_idx].backend
    }

    fn frame_at(&mut self, t: Time) -> Result<VideoFrame, MediaError> {
        let idx = t.max(Time::ZERO).frames_floor(self.rate);
        if let Some((i, f)) = &self.last {
            if *i == idx {
                return Ok(f.clone());
            }
        }
        if let Some((i, f)) = &self.end {
            if idx >= *i {
                return Ok(f.clone());
            }
        }
        loop {
            let decoded = match self.decode_index(idx) {
                Ok(f) => Ok((idx, f)),
                Err(MediaError::Eof) => self.final_frame(idx).inspect(|end| self.end = Some(end.clone())),
                Err(e) => Err(e),
            };
            match decoded {
                Ok((i, f)) => {
                    self.last = Some((i, f.clone()));
                    return Ok(f);
                }
                // this path produced nothing → try the next plan
                Err(e) if self.plan_idx + 1 < self.plans.len() => {
                    log::warn!("{:?} decode failed for {} ({e}); falling back", self.backend(), self.path.display());
                    self.plan_idx += 1;
                    self.kill();
                }
                Err(e) => return Err(e),
            }
        }
    }

    fn thumbnail_at(&mut self, t: Time, max_w: u32, max_h: u32) -> Result<(u32, u32, Vec<u8>), MediaError> {
        let scale = (max_w as f32 / self.src_w as f32).min(max_h as f32 / self.src_h as f32).min(1.0);
        let tw = ((self.src_w as f32 * scale) as u32).max(2) & !1;
        let th = ((self.src_h as f32 * scale) as u32).max(2) & !1;
        let plan = self.plans[self.plan_idx].clone();
        let mut cmd = ffmpeg_cli::command(&ffmpeg_cli::ffmpeg_path());
        cmd.args(["-hide_banner", "-loglevel", "error", "-nostdin"]);
        cmd.args(&plan.input_args);
        seek_args(&mut cmd, &self.path, t.frames_floor(self.rate), self.rate);
        let di = self.deinterlace.as_ref().map(|d| format!("{d},")).unwrap_or_default();
        cmd.args(["-map", "0:v:0", "-frames:v", "1", "-vf", &format!("{di}scale={tw}:{th}:flags=area"), "-pix_fmt", "rgba", "-f", "rawvideo", "pipe:1"]);
        let out = ffmpeg_cli::output_confined(&mut cmd).map_err(MediaError::Io)?;
        if out.stdout.len() == (tw * th * 4) as usize {
            Ok((tw, th, out.stdout))
        } else {
            // fall back to the streaming path
            let f = self.frame_at(t)?;
            Ok(f.thumbnail_rgba(max_w, max_h))
        }
    }
}
