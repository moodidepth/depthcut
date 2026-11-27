//! In-process decoding through `ffmpeg-next` (feature `ffmpeg`).
//!
//! Uses FFmpeg's generic hwaccel on the Intel Arc: a D3D11VA device on
//! Windows, VA-API (`iHD`, `/dev/dri/renderD128`) on Linux — the same media
//! engines Quick Sync drives. Hardware frames are transferred to system
//! memory as NV12 (8-bit) or P010 (10-bit). If the device cannot be created
//! the decoder silently falls back to multi-threaded software decoding.
//!
//! Requires FFmpeg 9.x development libraries and libclang at build time.

use super::{DecodeOptions, VideoDecoder};
use crate::frame::{PixelFormat, VideoFrame};
use crate::hwaccel::HwBackend;
use crate::MediaError;
use ff::ffi;
use ffmpeg_next as ff;
use nle_core::{AssetKind, FrameRate, MediaAsset, Time};
use std::path::PathBuf;
use std::ptr;

pub struct LibavDecoder {
    input: ff::format::context::Input,
    decoder: ff::decoder::Video,
    stream: usize,
    time_base: f64,
    rate: FrameRate,
    out_w: u32,
    out_h: u32,
    format: PixelFormat,
    backend: HwBackend,
    scaler: Option<(ff::format::Pixel, u32, u32, ff::software::scaling::Context)>,
    /// Last decoded frame (index at the stream rate) for sequential reads.
    last: Option<(i64, VideoFrame)>,
    /// The file's final frame, once it has been found.
    end: Option<(i64, VideoFrame)>,
    /// No later than this is the picture expected to end.
    expected_end: i64,
    /// The container's first timestamp, which is time zero, in seconds.
    origin: f64,
    /// Frames decoded ahead of a seek target.
    preroll: i64,
    /// The decoder has been sent the end of the stream.
    drained: bool,
}

// The FFmpeg contexts are only used from the owning decode worker thread.
unsafe impl Send for LibavDecoder {}

fn err(e: impl std::fmt::Display) -> MediaError {
    MediaError::Decode(e.to_string())
}

/// Attach a hardware device context to the codec context before opening.
/// Returns the backend that was set up.
fn attach_hw_device(ctx: &mut ff::codec::Context) -> Option<HwBackend> {
    #[cfg(target_os = "windows")]
    let (kind, device, backend) = (ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_D3D11VA, None::<&str>, HwBackend::D3d11va);
    #[cfg(not(target_os = "windows"))]
    let (kind, device, backend) = (ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_VAAPI, Some("/dev/dri/renderD128"), HwBackend::Vaapi);

    let dev = match device.map(std::ffi::CString::new).transpose() {
        Ok(d) => d,
        Err(_) => return None, // device path with interior NUL → software decode
    };
    unsafe {
        let mut opts: *mut ffi::AVDictionary = ptr::null_mut();
        let gpu = crate::hwaccel::render_gpu();
        if cfg!(not(target_os = "windows")) {
            // Intel needs the iHD (intel-media-driver) VA-API driver; libva
            // picks Mesa's driver for AMD by itself
            if gpu.is_none_or(|g| g.vendor == crate::hwaccel::GpuVendor::Intel) {
                ffi::av_dict_set(&mut opts, c"driver".as_ptr(), c"iHD".as_ptr(), 0);
            }
        } else if let Some(g) = gpu.filter(|g| g.vendor_id != 0) {
            // decode on the render GPU; without one D3D11 takes the default adapter
            if let Ok(id) = std::ffi::CString::new(format!("0x{:04x}", g.vendor_id)) {
                ffi::av_dict_set(&mut opts, c"vendor_id".as_ptr(), id.as_ptr(), 0);
            }
        }
        let mut hw: *mut ffi::AVBufferRef = ptr::null_mut();
        let rc = ffi::av_hwdevice_ctx_create(&mut hw, kind, dev.as_ref().map_or(ptr::null(), |d| d.as_ptr()), opts, 0);
        ffi::av_dict_free(&mut opts);
        if rc < 0 || hw.is_null() {
            return None;
        }
        (*ctx.as_mut_ptr()).hw_device_ctx = ffi::av_buffer_ref(hw);
        ffi::av_buffer_unref(&mut hw);
    }
    Some(backend)
}

impl LibavDecoder {
    pub fn open(asset: &MediaAsset, opts: DecodeOptions) -> Result<Self, MediaError> {
        ff::init().map_err(err)?;
        let path: PathBuf = match (&asset.proxy_path, opts.use_proxy) {
            (Some(p), true) if p.exists() => p.clone(),
            _ => asset.path.clone(),
        };
        let input = ff::format::input(&path).map_err(err)?;
        let st = input.streams().best(ff::media::Type::Video).ok_or_else(|| MediaError::Unsupported("no video stream".into()))?;
        let stream = st.index();
        let tb = st.time_base();
        let time_base = tb.numerator() as f64 / tb.denominator().max(1) as f64;
        let afr = st.avg_frame_rate();
        let rate = if afr.numerator() > 0 && afr.denominator() > 0 { FrameRate::new(afr.numerator() as u32, afr.denominator() as u32) } else { asset.frame_rate() };

        let mut ctx = ff::codec::Context::from_parameters(st.parameters()).map_err(err)?;
        let backend = attach_hw_device(&mut ctx).unwrap_or(HwBackend::Software);
        // software path: use all decode threads the codec supports
        ctx.set_threading(ff::codec::threading::Config { kind: ff::codec::threading::Type::Frame, count: 0 });
        let decoder = ctx.decoder().video().map_err(err)?;

        let ten_bit = asset.video.as_ref().is_some_and(|v| v.bit_depth > 8);
        let format = if ten_bit { PixelFormat::P010 } else { PixelFormat::Nv12 };
        let (w, h) = (decoder.width(), decoder.height());
        let (out_w, out_h) = super::scaled_dims(w, h, opts.max_height);
        log::info!("libav decoder for {}: {}x{} → {}x{} via {}", asset.name, w, h, out_w, out_h, backend.label());
        let start = unsafe { (*input.as_ptr()).start_time };
        let origin = if start == ffi::AV_NOPTS_VALUE { 0.0 } else { start as f64 / ffi::AV_TIME_BASE as f64 };
        let expected_end = if asset.kind == AssetKind::Image { 0 } else { asset.duration.frames_floor(rate).max(0) };
        let preroll = super::preroll_frames(&path, rate);
        Ok(LibavDecoder {
            input,
            decoder,
            stream,
            time_base,
            rate,
            out_w,
            out_h,
            format,
            backend,
            scaler: None,
            last: None,
            end: None,
            expected_end,
            origin,
            preroll,
            drained: false,
        })
    }

    /// Position the demuxer on a keyframe at or before frame `index`.
    fn seek(&mut self, index: i64) -> Result<(), MediaError> {
        let ts = ((self.origin + Time::from_frames(index, self.rate).as_secs_f64()) * ffi::AV_TIME_BASE as f64) as i64;
        if index > 0 {
            self.input.seek(ts, ..ts).map_err(err)?;
        } else {
            // The top: a transport stream needs aiming before its first
            // timestamp, or it starts on the second keyframe; FLV refuses that.
            let before = ts - i64::from(ffi::AV_TIME_BASE);
            self.input.seek(before, ..).or_else(|_| self.input.seek(ts, ..)).map_err(err)?;
        }
        self.decoder.flush();
        self.last = None;
        self.drained = false;
        Ok(())
    }

    /// Download a (possibly hardware) frame and convert it to NV12/P010.
    fn convert(&mut self, frame: &ff::frame::Video) -> Result<VideoFrame, MediaError> {
        let mut sw = ff::frame::Video::empty();
        let src = unsafe {
            if !(*frame.as_ptr()).hw_frames_ctx.is_null() {
                if ffi::av_hwframe_transfer_data(sw.as_mut_ptr(), frame.as_ptr(), 0) < 0 {
                    return Err(MediaError::Decode("hardware frame transfer failed".into()));
                }
                &sw
            } else {
                frame
            }
        };
        let target = if self.format == PixelFormat::P010 { ff::format::Pixel::P010LE } else { ff::format::Pixel::NV12 };
        let (w, h) = (self.out_w, self.out_h);
        let needs_scale = src.format() != target || src.width() != w || src.height() != h;
        let mut out = ff::frame::Video::empty();
        let img = if needs_scale {
            let key = (src.format(), src.width(), src.height());
            let rebuild = !matches!(&self.scaler, Some((f, sw_, sh, _)) if (*f, *sw_, *sh) == key);
            if rebuild {
                let ctx = ff::software::scaling::Context::get(src.format(), src.width(), src.height(), target, w, h, ff::software::scaling::Flags::BILINEAR).map_err(err)?;
                self.scaler = Some((key.0, key.1, key.2, ctx));
            }
            let Some((_, _, _, sc)) = self.scaler.as_mut() else {
                return Err(MediaError::Decode("scaler unavailable".into()));
            };
            sc.run(src, &mut out).map_err(err)?;
            &out
        } else {
            src
        };
        // pack planes tightly (strides may be padded)
        let bps = self.format.bytes_per_sample();
        let row = w as usize * bps;
        let mut data = Vec::with_capacity(self.format.frame_size(w, h));
        for y in 0..h as usize {
            let s = img.stride(0);
            data.extend_from_slice(&img.data(0)[y * s..y * s + row]);
        }
        for y in 0..(h as usize).div_ceil(2) {
            let s = img.stride(1);
            data.extend_from_slice(&img.data(1)[y * s..y * s + row]);
        }
        let pts = frame.timestamp().or(frame.pts()).map(|p| Time::from_secs_f64(p as f64 * self.time_base)).unwrap_or(Time::ZERO);
        Ok(VideoFrame::new(w, h, self.format, data, pts))
    }

    /// Decode forward to the frame at or after `target`. If the stream ends
    /// first that is its final frame. `None`: it held nothing at all, or began
    /// after `target` when it might have begun earlier (`may_overshoot` false).
    fn decode_until(&mut self, target: i64, may_overshoot: bool) -> Result<Option<(i64, VideoFrame)>, MediaError> {
        let mut frame = ff::frame::Video::empty();
        let mut passed = None;
        let fps = self.rate.fps();
        loop {
            while self.decoder.receive_frame(&mut frame).is_ok() {
                // timestamps may be rounded (to milliseconds in Matroska): nearest frame
                let idx = match frame.timestamp().or(frame.pts()) {
                    Some(pts) => ((pts as f64 * self.time_base - self.origin) * fps).round() as i64,
                    None => 0,
                };
                if idx >= target {
                    if idx > target && passed.is_none() && !may_overshoot {
                        return Ok(None);
                    }
                    return Ok(Some((idx, self.convert(&frame)?)));
                }
                passed = Some((idx, std::mem::replace(&mut frame, ff::frame::Video::empty())));
            }
            if self.drained {
                return match passed {
                    Some((idx, f)) => Ok(Some((idx, self.convert(&f)?))),
                    None => Ok(self.last.clone()),
                };
            }
            let mut got = false;
            for (st, packet) in self.input.packets() {
                if st.index() == self.stream {
                    // as FFmpeg itself does: after a seek, and at the cut-off
                    // end of a stream, some packets cannot be decoded
                    if let Err(e) = self.decoder.send_packet(&packet) {
                        log::debug!("skipping a packet the decoder refused: {e}");
                    }
                    got = true;
                    break;
                }
            }
            if !got {
                self.decoder.send_eof().map_err(err)?;
                self.drained = true;
            }
        }
    }
}

impl VideoDecoder for LibavDecoder {
    fn width(&self) -> u32 {
        self.out_w
    }
    fn height(&self) -> u32 {
        self.out_h
    }
    fn format(&self) -> PixelFormat {
        self.format
    }
    fn frame_rate(&self) -> FrameRate {
        self.rate
    }
    fn backend(&self) -> HwBackend {
        self.backend
    }
    fn frame_at(&mut self, t: Time) -> Result<VideoFrame, MediaError> {
        let target = t.max(Time::ZERO).frames_floor(self.rate);
        if let Some((idx, f)) = &self.end {
            if target >= *idx {
                return Ok(f.clone());
            }
        }
        let from = target.min(self.expected_end);
        let mut back = self.preroll.max(1);
        match &self.last {
            Some((idx, f)) if *idx == target => return Ok(f.clone()),
            // short forward steps (playback) decode sequentially; anything else seeks
            Some((idx, _)) if target > *idx && target - *idx <= 30 => {}
            _ => self.seek((from - back).max(0))?,
        }
        loop {
            let may_overshoot = self.last.is_some() || back >= from;
            if let Some((idx, f)) = self.decode_until(target, may_overshoot)? {
                self.last = Some((idx, f.clone()));
                if idx < target {
                    self.end = self.last.clone();
                }
                return Ok(f);
            }
            if back >= from {
                return Err(MediaError::Eof);
            }
            // the picture ends before the sound does, or the seek landed too late
            back *= 4;
            self.seek((from - back).max(0))?;
        }
    }
}
