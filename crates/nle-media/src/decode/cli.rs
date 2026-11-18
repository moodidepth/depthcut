//! Decoder that streams raw frames out of the `ffmpeg` executable.
//!
//! The process is kept alive while frames are requested sequentially
//! (playback), and restarted with a fast input seek when the requested
//! frame jumps (scrubbing). If a hardware path fails to produce a frame
//! the next plan (QSV → VA-API/D3D11VA → software) is tried silently.

use super::{scaled_dims, DecodeOptions, VideoDecoder};
use crate::ffmpeg_cli;
use crate::frame::{PixelFormat, VideoFrame};
use crate::hwaccel::{self, Chroma, DecoderPlan, HwBackend, Platform, VideoCodec, INTEL_ARC_A750};
use crate::MediaError;
use nle_core::{FrameRate, MediaAsset, Time};
use std::io::Read;
use std::path::PathBuf;
use std::process::{Child, ChildStdout, Stdio};

/// How far ahead (in frames) we read forward instead of re-seeking.
const MAX_FORWARD_READ_SECS: i64 = 2;

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
    child: Option<Child>,
    stdout: Option<ChildStdout>,
    next_index: i64,
    last: Option<(i64, VideoFrame)>,
}

impl CliDecoder {
    pub fn open(asset: &MediaAsset, opts: DecodeOptions) -> Result<Self, MediaError> {
        let v = asset.video.as_ref().ok_or_else(|| MediaError::Unsupported("no video stream".into()))?;
        let use_proxy = opts.use_proxy && asset.proxy_path.as_ref().is_some_and(|p| p.exists());
        let path = if use_proxy { asset.proxy_path.clone().unwrap_or_else(|| asset.path.clone()) } else { asset.path.clone() };
        let codec = if use_proxy { VideoCodec::H264 } else { VideoCodec::from_ffmpeg_name(&v.codec) };
        let depth = if use_proxy { 8 } else { v.bit_depth };
        let chroma = if use_proxy { Chroma::Yuv420 } else { Chroma::from_pix_fmt(&v.pix_fmt) };
        let plans = hwaccel::plan_decode(codec, depth, chroma, Platform::current(), hwaccel::probe_caps(), &INTEL_ARC_A750);
        let (width, height) = scaled_dims(v.width, v.height, opts.max_height);
        log::debug!("{}: decode plan {:?}", asset.name, plans.iter().map(|p| p.backend).collect::<Vec<_>>());
        Ok(CliDecoder {
            path,
            plans,
            plan_idx: 0,
            src_w: v.width,
            src_h: v.height,
            width,
            height,
            format: PixelFormat::for_bit_depth(depth),
            chroma,
            rate: v.frame_rate,
            child: None,
            stdout: None,
            next_index: -1,
            last: None,
        })
    }

    fn needs_scale(&self) -> bool {
        (self.width, self.height) != (self.src_w, self.src_h)
    }

    /// Output-side filter graph for the active plan.
    fn filter(&self, out_fmt: &str) -> (Vec<String>, String) {
        let plan = &self.plans[self.plan_idx];
        let (w, h) = (self.width, self.height);
        let sw_fmt = self.chroma.hw_download_format(if self.format == PixelFormat::P010 { 10 } else { 8 });
        let scale = self.needs_scale();
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
        let t = Time::from_frames(frame_index.max(0), self.rate).as_secs_f64();
        let (hw_out, vf) = self.filter(self.format.ffmpeg_name());
        let mut cmd = ffmpeg_cli::command(&ffmpeg_cli::ffmpeg_path());
        cmd.args(["-hide_banner", "-loglevel", "error", "-nostdin"]);
        cmd.args(&plan.input_args);
        cmd.args(&hw_out);
        cmd.args(["-ss", &format!("{t:.6}"), "-i"]).arg(&self.path);
        cmd.args(["-map", "0:v:0", "-an", "-sn", "-vf", &vf]);
        cmd.args(["-fps_mode", "cfr", "-r", &format!("{}/{}", self.rate.num, self.rate.den)]);
        cmd.args(["-pix_fmt", self.format.ffmpeg_name(), "-f", "rawvideo", "pipe:1"]);
        cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null());
        let mut child = cmd.spawn().map_err(MediaError::Io)?;
        self.stdout = child.stdout.take();
        self.child = Some(child);
        self.next_index = frame_index.max(0);
        Ok(())
    }

    fn read_frame(&mut self) -> Result<VideoFrame, MediaError> {
        let size = self.format.frame_size(self.width, self.height);
        let out = self.stdout.as_mut().ok_or(MediaError::Eof)?;
        let mut buf = vec![0u8; size];
        out.read_exact(&mut buf).map_err(|_| MediaError::Eof)?;
        let pts = Time::from_frames(self.next_index, self.rate);
        self.next_index += 1;
        Ok(VideoFrame::new(self.width, self.height, self.format, buf, pts))
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
        loop {
            match self.decode_index(idx) {
                Ok(f) => {
                    self.last = Some((idx, f.clone()));
                    return Ok(f);
                }
                Err(MediaError::Eof) if self.last.is_some() && self.next_index > idx.min(1) && self.plan_idx + 1 >= self.plans.len() => {
                    // past the end of the media: hold the last frame
                    return self.last.as_ref().map(|(_, f)| f.clone()).ok_or(MediaError::Eof);
                }
                Err(e) => {
                    // a hardware path that produces nothing → try the next plan
                    let produced_any = self.last.is_some();
                    if !produced_any && self.plan_idx + 1 < self.plans.len() {
                        log::warn!("{:?} decode failed for {} ({e}); falling back", self.backend(), self.path.display());
                        self.plan_idx += 1;
                        self.kill();
                        continue;
                    }
                    if let Some((_, f)) = &self.last {
                        return Ok(f.clone());
                    }
                    return Err(e);
                }
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
        cmd.args(["-ss", &format!("{:.6}", t.max(Time::ZERO).as_secs_f64()), "-i"]).arg(&self.path);
        cmd.args(["-map", "0:v:0", "-frames:v", "1", "-vf", &format!("scale={tw}:{th}:flags=area"), "-pix_fmt", "rgba", "-f", "rawvideo", "pipe:1"]);
        let out = cmd.stdin(Stdio::null()).stderr(Stdio::null()).output().map_err(MediaError::Io)?;
        if out.stdout.len() == (tw * th * 4) as usize {
            Ok((tw, th, out.stdout))
        } else {
            // fall back to the streaming path
            let f = self.frame_at(t)?;
            Ok(f.thumbnail_rgba(max_w, max_h))
        }
    }
}
