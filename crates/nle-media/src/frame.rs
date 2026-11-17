//! Decoded media buffers.

use nle_core::Time;
use std::sync::Arc;

/// CPU-side pixel layouts handed to the GPU. NV12/P010 stay in YUV until
/// the WGSL conversion shader.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PixelFormat {
    /// 8-bit 4:2:0, Y plane then interleaved UV plane.
    Nv12,
    /// 10-bit 4:2:0 in 16-bit little-endian words (MSB aligned).
    P010,
    /// 8-bit RGBA.
    Rgba8,
}

impl PixelFormat {
    pub fn ffmpeg_name(self) -> &'static str {
        match self {
            PixelFormat::Nv12 => "nv12",
            PixelFormat::P010 => "p010le",
            PixelFormat::Rgba8 => "rgba",
        }
    }
    pub fn bytes_per_sample(self) -> usize {
        match self {
            PixelFormat::Nv12 => 1,
            PixelFormat::P010 => 2,
            PixelFormat::Rgba8 => 4,
        }
    }
    /// Total byte size of a tightly packed frame.
    pub fn frame_size(self, width: u32, height: u32) -> usize {
        let (w, h) = (width as usize, height as usize);
        match self {
            PixelFormat::Nv12 => w * h + w * h.div_ceil(2),
            PixelFormat::P010 => (w * h + w * h.div_ceil(2)) * 2,
            PixelFormat::Rgba8 => w * h * 4,
        }
    }
    pub fn for_bit_depth(bits: u8) -> Self {
        if bits > 8 {
            PixelFormat::P010
        } else {
            PixelFormat::Nv12
        }
    }
}

/// A decoded, tightly packed video frame.
#[derive(Clone, Debug)]
pub struct VideoFrame {
    pub width: u32,
    pub height: u32,
    pub format: PixelFormat,
    pub data: Arc<Vec<u8>>,
    /// Presentation time in source media time.
    pub pts: Time,
}

impl VideoFrame {
    pub fn new(width: u32, height: u32, format: PixelFormat, data: Vec<u8>, pts: Time) -> Self {
        debug_assert_eq!(data.len(), format.frame_size(width, height));
        VideoFrame { width, height, format, data: Arc::new(data), pts }
    }

    /// Bytes per row of the luma (or RGBA) plane.
    pub fn stride(&self) -> usize {
        self.width as usize * self.format.bytes_per_sample()
    }

    /// Byte offset of the interleaved chroma plane.
    pub fn uv_offset(&self) -> usize {
        self.width as usize * self.height as usize * self.format.bytes_per_sample()
    }

    pub fn byte_len(&self) -> usize {
        self.data.len()
    }

    /// Sample one pixel as 8-bit RGB (BT.709 limited range). Used only for
    /// small CPU thumbnails; the viewer/export path converts on the GPU.
    pub fn rgb_at(&self, x: u32, y: u32) -> [u8; 3] {
        let (x, y) = (x.min(self.width - 1) as usize, y.min(self.height - 1) as usize);
        let w = self.width as usize;
        let d = &self.data;
        match self.format {
            PixelFormat::Rgba8 => {
                let i = (y * w + x) * 4;
                [d[i], d[i + 1], d[i + 2]]
            }
            PixelFormat::Nv12 | PixelFormat::P010 => {
                let bps = self.format.bytes_per_sample();
                let rd = |i: usize| -> f32 {
                    if bps == 1 {
                        d[i] as f32 / 255.0
                    } else {
                        u16::from_le_bytes([d[i], d[i + 1]]) as f32 / 65535.0
                    }
                };
                let yv = rd((y * w + x) * bps);
                let uv_base = self.uv_offset() + ((y / 2) * w + (x & !1)) * bps;
                let u = rd(uv_base);
                let v = rd(uv_base + bps);
                let rgb = yuv709_to_rgb(yv, u, v);
                rgb.map(|c| (c.clamp(0.0, 1.0) * 255.0 + 0.5) as u8)
            }
        }
    }

    /// Nearest-neighbour RGBA thumbnail of at most `max_w × max_h`.
    pub fn thumbnail_rgba(&self, max_w: u32, max_h: u32) -> (u32, u32, Vec<u8>) {
        let scale = (max_w as f32 / self.width as f32).min(max_h as f32 / self.height as f32).min(1.0);
        let tw = ((self.width as f32 * scale).round() as u32).max(1);
        let th = ((self.height as f32 * scale).round() as u32).max(1);
        let mut out = Vec::with_capacity((tw * th * 4) as usize);
        for ty in 0..th {
            for tx in 0..tw {
                let sx = ((tx as f32 + 0.5) / scale) as u32;
                let sy = ((ty as f32 + 0.5) / scale) as u32;
                let [r, g, b] = self.rgb_at(sx, sy);
                out.extend_from_slice(&[r, g, b, 255]);
            }
        }
        (tw, th, out)
    }
}

/// BT.709 limited-range Y'CbCr (normalised 0..1) to R'G'B'.
pub fn yuv709_to_rgb(y: f32, u: f32, v: f32) -> [f32; 3] {
    let y = (y - 16.0 / 255.0) * (255.0 / 219.0);
    let u = (u - 128.0 / 255.0) * (255.0 / 224.0);
    let v = (v - 128.0 / 255.0) * (255.0 / 224.0);
    [y + 1.5748 * v, y - 0.1873 * u - 0.4681 * v, y + 1.8556 * u]
}

/// Interleaved floating-point PCM.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct AudioBuffer {
    pub sample_rate: u32,
    pub channels: u16,
    pub samples: Vec<f32>,
}

impl AudioBuffer {
    pub fn new(sample_rate: u32, channels: u16, samples: Vec<f32>) -> Self {
        AudioBuffer { sample_rate, channels, samples }
    }
    pub fn frames(&self) -> usize {
        self.samples.len() / self.channels.max(1) as usize
    }
    pub fn duration(&self) -> Time {
        Time::from_secs_f64(self.frames() as f64 / self.sample_rate.max(1) as f64)
    }
    /// Stereo sample pair at a fractional frame position (linear interpolation).
    pub fn stereo_at(&self, pos: f64) -> (f32, f32) {
        if pos < 0.0 {
            return (0.0, 0.0);
        }
        let n = self.frames();
        let i = pos.floor() as usize;
        if i + 1 >= n {
            if i < n {
                return self.frame(i);
            }
            return (0.0, 0.0);
        }
        let f = (pos - i as f64) as f32;
        let (l0, r0) = self.frame(i);
        let (l1, r1) = self.frame(i + 1);
        (l0 + (l1 - l0) * f, r0 + (r1 - r0) * f)
    }
    fn frame(&self, i: usize) -> (f32, f32) {
        let ch = self.channels as usize;
        match ch {
            0 => (0.0, 0.0),
            1 => (self.samples[i], self.samples[i]),
            _ => (self.samples[i * ch], self.samples[i * ch + 1]),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nv12_sizes_and_colors() {
        assert_eq!(PixelFormat::Nv12.frame_size(1920, 1080), 1920 * 1080 * 3 / 2);
        assert_eq!(PixelFormat::P010.frame_size(3840, 2160), 3840 * 2160 * 3);
        // limited-range white / black
        let w = yuv709_to_rgb(235.0 / 255.0, 0.5019608, 0.5019608);
        assert!(w.iter().all(|c| (c - 1.0).abs() < 1e-3));
        let b = yuv709_to_rgb(16.0 / 255.0, 0.5019608, 0.5019608);
        assert!(b.iter().all(|c| c.abs() < 1e-3));
    }
}
