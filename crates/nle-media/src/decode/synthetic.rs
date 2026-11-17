//! Procedural NV12 test pattern decoder (colour bars + moving sweep).

use super::{scaled_dims, VideoDecoder};
use crate::frame::{PixelFormat, VideoFrame};
use crate::hwaccel::HwBackend;
use crate::MediaError;
use nle_core::{FrameRate, MediaAsset, Time};

pub struct SyntheticDecoder {
    width: u32,
    height: u32,
    rate: FrameRate,
    hue: f32,
    duration: Time,
}

impl SyntheticDecoder {
    pub fn new(asset: &MediaAsset, max_h: Option<u32>) -> Self {
        let (w, h) = asset.video.as_ref().map(|v| (v.width, v.height)).unwrap_or((1920, 1080));
        let (w, h) = scaled_dims(w, h, max_h.or(Some(1080)));
        SyntheticDecoder { width: w, height: h, rate: asset.frame_rate(), hue: crate::probe::synthetic_hue(asset), duration: asset.duration }
    }
}

fn rgb_to_yuv709(r: f32, g: f32, b: f32) -> (u8, u8, u8) {
    let y = 0.2126 * r + 0.7152 * g + 0.0722 * b;
    let u = (b - y) / 1.8556;
    let v = (r - y) / 1.5748;
    let q = |x: f32| x.round().clamp(0.0, 255.0) as u8;
    (q(16.0 + 219.0 * y), q(128.0 + 224.0 * u), q(128.0 + 224.0 * v))
}

fn hsv(h: f32, s: f32, v: f32) -> (f32, f32, f32) {
    let h = h.rem_euclid(1.0) * 6.0;
    let i = h.floor();
    let f = h - i;
    let (p, q, t) = (v * (1.0 - s), v * (1.0 - s * f), v * (1.0 - s * (1.0 - f)));
    match i as i32 {
        0 => (v, t, p),
        1 => (q, v, p),
        2 => (p, v, t),
        3 => (p, q, v),
        4 => (t, p, v),
        _ => (v, p, q),
    }
}

impl VideoDecoder for SyntheticDecoder {
    fn width(&self) -> u32 {
        self.width
    }
    fn height(&self) -> u32 {
        self.height
    }
    fn format(&self) -> PixelFormat {
        PixelFormat::Nv12
    }
    fn frame_rate(&self) -> FrameRate {
        self.rate
    }
    fn backend(&self) -> HwBackend {
        HwBackend::Software
    }

    fn frame_at(&mut self, t: Time) -> Result<VideoFrame, MediaError> {
        let t = t.max(Time::ZERO).min(self.duration);
        let frame_idx = t.frames_floor(self.rate);
        let pts = Time::from_frames(frame_idx, self.rate);
        let (w, h) = (self.width as usize, self.height as usize);
        let mut data = vec![0u8; PixelFormat::Nv12.frame_size(self.width, self.height)];
        let secs = pts.as_secs_f64() as f32;
        let progress = if self.duration.is_positive() { (pts.as_secs_f64() / self.duration.as_secs_f64()) as f32 } else { 0.0 };
        // seven bars tinted by hue
        let bars: Vec<(u8, u8, u8)> = (0..7)
            .map(|i| {
                let (r, g, b) = hsv(self.hue + i as f32 / 7.0, 0.55, 0.85);
                rgb_to_yuv709(r, g, b)
            })
            .collect();
        let sweep_x = ((secs * 0.25).fract() * w as f32) as usize;
        let box_y = ((h / 2) as isize + ((secs * 2.0).sin() * h as f32 * 0.2) as isize).max(0) as usize;
        let (y_plane, uv_plane) = data.split_at_mut(w * h);
        for y in 0..h {
            let lower = y > h * 3 / 4;
            for x in 0..w {
                let (mut yy, mut u, mut v) = if lower {
                    // progress bar at the bottom
                    if (x as f32) < progress * w as f32 {
                        rgb_to_yuv709(0.04, 0.52, 1.0)
                    } else {
                        rgb_to_yuv709(0.08, 0.08, 0.09)
                    }
                } else {
                    bars[(x * 7 / w).min(6)]
                };
                if x.abs_diff(sweep_x) < 6 {
                    (yy, u, v) = rgb_to_yuv709(1.0, 1.0, 1.0);
                }
                if !lower && x.abs_diff(sweep_x) < h / 10 && y.abs_diff(box_y) < h / 10 {
                    (yy, u, v) = rgb_to_yuv709(0.95, 0.3, 0.2);
                }
                y_plane[y * w + x] = yy;
                if y % 2 == 0 && x % 2 == 0 {
                    let i = (y / 2) * w + x;
                    uv_plane[i] = u;
                    uv_plane[i + 1] = v;
                }
            }
        }
        Ok(VideoFrame::new(self.width, self.height, PixelFormat::Nv12, data, pts))
    }
}
