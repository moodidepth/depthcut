//! GPU video scopes: waveform (luma / RGB), RGB parade, vectorscope with the
//! skin-tone line, and histogram.
//!
//! The renderer bins the final frame into an atomic count buffer on the GPU
//! (`scopes.wgsl`) and draws each scope into a small texture that egui shows
//! directly, so nothing the size of a frame is ever read back to the CPU.
//! The constants and binning helpers below mirror the shader exactly; the
//! GPU tests compare the two.

use crate::pipelines::DISPLAY_FORMAT;
use bytemuck::{Pod, Zeroable};

/// Waveform columns (horizontal resolution of the waveform and parade).
pub const WAVE_COLS: u32 = 384;
/// Signal levels (8-bit).
pub const LEVELS: u32 = 256;
/// Vectorscope bins per axis.
pub const VEC_N: u32 = 256;
pub const VEC_OFF: u32 = 4 * WAVE_COLS * LEVELS;
pub const HIST_OFF: u32 = VEC_OFF + VEC_N * VEC_N;
pub const MAX_OFF: u32 = HIST_OFF + 4 * LEVELS;
/// Length of the count buffer in `u32`s.
pub const COUNT_LEN: u32 = MAX_OFF + 4;
/// Frames wider than this are sampled every n-th pixel (both axes).
pub const MAX_SAMPLE_WIDTH: u32 = 960;
/// Skin-tone line angle in the vectorscope (degrees from +Cb).
pub const SKIN_TONE_DEG: f32 = 123.0;

const LUMA: [f32; 3] = [0.2126, 0.7152, 0.0722];

/// Waveform / histogram channel order in the count buffer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Channel {
    Luma = 0,
    Red = 1,
    Green = 2,
    Blue = 3,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ScopeKind {
    Waveform,
    Parade,
    Vectorscope,
    Histogram,
}

impl ScopeKind {
    pub const ALL: [ScopeKind; 4] = [ScopeKind::Waveform, ScopeKind::Parade, ScopeKind::Vectorscope, ScopeKind::Histogram];

    pub fn label(self) -> &'static str {
        match self {
            ScopeKind::Waveform => "Waveform",
            ScopeKind::Parade => "RGB Parade",
            ScopeKind::Vectorscope => "Vectorscope",
            ScopeKind::Histogram => "Histogram",
        }
    }

    /// Texture size of the drawn scope.
    pub fn size(self) -> (u32, u32) {
        match self {
            ScopeKind::Waveform => (WAVE_COLS, LEVELS),
            ScopeKind::Parade => (3 * 256, LEVELS),
            ScopeKind::Vectorscope => (VEC_N, VEC_N),
            ScopeKind::Histogram => (LEVELS, 128),
        }
    }

    fn index(self) -> usize {
        self as usize
    }
}

/// Display options for the scopes.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ScopeSettings {
    /// Waveform shows R, G and B overlaid instead of luma.
    pub rgb_waveform: bool,
    /// Trace brightness (1 = default).
    pub gain: f32,
    /// Vectorscope magnification (1 = 100 % saturation at the rim, 2 = 50 %).
    pub vec_zoom: f32,
}

impl Default for ScopeSettings {
    fn default() -> Self {
        ScopeSettings { rgb_waveform: false, gain: 1.0, vec_zoom: 1.0 }
    }
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
pub(crate) struct ScopeParams {
    pub stride: u32,
    pub wave_mode: u32,
    pub gain: f32,
    pub vec_zoom: f32,
    pub samples: f32,
    pub _pad: [f32; 3],
}

impl ScopeParams {
    pub fn new(s: &ScopeSettings, width: u32, height: u32) -> Self {
        let stride = sample_stride(width);
        ScopeParams {
            stride,
            wave_mode: s.rgb_waveform as u32,
            gain: s.gain.clamp(0.05, 20.0),
            vec_zoom: s.vec_zoom.clamp(0.25, 8.0),
            samples: sample_count(width, height, stride) as f32,
            _pad: [0.0; 3],
        }
    }
}

/// Pixel stride used for a frame `width` pixels wide.
pub fn sample_stride(width: u32) -> u32 {
    width.div_ceil(MAX_SAMPLE_WIDTH).max(1)
}

/// Number of pixels sampled from a `width × height` frame at `stride`.
pub fn sample_count(width: u32, height: u32, stride: u32) -> u32 {
    width.div_ceil(stride) * height.div_ceil(stride)
}

// ----- CPU mirrors of the shader's binning ------------------------------------

/// 8-bit level of a 0..1 value (clamped).
pub fn level(v: f32) -> u32 {
    (v.clamp(0.0, 1.0) * 255.0 + 0.5) as u32
}

pub fn luma(rgb: [f32; 3]) -> f32 {
    rgb[0] * LUMA[0] + rgb[1] * LUMA[1] + rgb[2] * LUMA[2]
}

/// BT.709 Cb / Cr, each in −0.5..0.5.
pub fn cbcr(rgb: [f32; 3]) -> [f32; 2] {
    let y = luma(rgb);
    [(rgb[2] - y) / 1.8556, (rgb[0] - y) / 1.5748]
}

/// Vectorscope bin `(x, y)` of a colour (x = Cb right, y = Cr up).
pub fn vec_bin(rgb: [f32; 3], zoom: f32) -> (u32, u32) {
    let rgb = rgb.map(|v| v.clamp(0.0, 1.0));
    let [cb, cr] = cbcr(rgb);
    let n = VEC_N as f32;
    let x = ((cb * zoom + 0.5) * n).floor().clamp(0.0, n - 1.0);
    let y = ((0.5 - cr * zoom) * n).floor().clamp(0.0, n - 1.0);
    (x as u32, y as u32)
}

/// Index of a waveform bin in the count buffer.
pub fn wave_index(ch: Channel, col: u32, lv: u32) -> usize {
    ((ch as u32 * WAVE_COLS + col) * LEVELS + lv) as usize
}

/// Index of a vectorscope bin in the count buffer.
pub fn vec_index(x: u32, y: u32) -> usize {
    (VEC_OFF + y * VEC_N + x) as usize
}

/// Index of a histogram bin in the count buffer.
pub fn hist_index(ch: Channel, lv: u32) -> usize {
    (HIST_OFF + ch as u32 * LEVELS + lv) as usize
}

// ----- GPU resources ----------------------------------------------------------

pub(crate) struct ScopeImage {
    pub storage: wgpu::TextureView,
    pub storage_tex: wgpu::Texture,
    pub srgb_tex: wgpu::Texture,
    pub srgb: wgpu::TextureView,
}

/// Count buffer and the four drawn scope textures. Created only while the
/// scopes are shown.
pub(crate) struct ScopeTargets {
    pub counts: wgpu::Buffer,
    pub images: [ScopeImage; 4],
}

impl ScopeTargets {
    pub fn new(device: &wgpu::Device) -> Self {
        let counts = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("rcp-scope-counts"),
            size: COUNT_LEN as u64 * 4,
            usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_SRC,
            mapped_at_creation: false,
        });
        let images = ScopeKind::ALL.map(|k| {
            let (w, h) = k.size();
            let size = wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 };
            // same storage → sRGB copy scheme as the display texture
            // (an sRGB view of a storage texture loses the device on Arc/DX12)
            let storage_tex = device.create_texture(&wgpu::TextureDescriptor {
                label: Some("rcp-scope"),
                size,
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: DISPLAY_FORMAT,
                usage: wgpu::TextureUsages::STORAGE_BINDING | wgpu::TextureUsages::COPY_SRC,
                view_formats: &[],
            });
            let srgb_tex = device.create_texture(&wgpu::TextureDescriptor {
                label: Some("rcp-scope-srgb"),
                size,
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::Rgba8UnormSrgb,
                usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST | wgpu::TextureUsages::COPY_SRC,
                view_formats: &[],
            });
            ScopeImage {
                storage: storage_tex.create_view(&wgpu::TextureViewDescriptor::default()),
                storage_tex,
                srgb: srgb_tex.create_view(&wgpu::TextureViewDescriptor::default()),
                srgb_tex,
            }
        });
        ScopeTargets { counts, images }
    }

    pub fn image(&self, kind: ScopeKind) -> &ScopeImage {
        &self.images[kind.index()]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout_matches_the_shader_constants() {
        let src = crate::pipelines::SHADER_SCOPES;
        for (name, v) in
            [("WAVE_COLS", WAVE_COLS), ("LEVELS", LEVELS), ("VEC_N", VEC_N), ("VEC_OFF", VEC_OFF), ("HIST_OFF", HIST_OFF), ("MAX_OFF", MAX_OFF), ("COUNT_LEN", COUNT_LEN)]
        {
            assert!(src.contains(&format!("const {name}: u32 = {v}u;")), "scopes.wgsl {name} ≠ {v}");
        }
        assert!(src.contains(&format!("const SKIN_DEG: f32 = {SKIN_TONE_DEG:.1};")));
    }

    #[test]
    fn binning_of_reference_colours() {
        assert_eq!(level(0.0), 0);
        assert_eq!(level(1.0), 255);
        assert_eq!(level(1.7), 255);
        assert_eq!(level(-0.2), 0);
        assert_eq!(level(0.5), 128);
        // neutral greys sit in the centre of the vectorscope
        assert_eq!(vec_bin([0.5, 0.5, 0.5], 1.0), (128, 128));
        assert_eq!(vec_bin([0.0; 3], 1.0), (128, 128));
        // 100 % red: Cr = +0.5 (top edge), Cb = -0.2126 / 1.8556 = -0.115
        assert_eq!(vec_bin([1.0, 0.0, 0.0], 1.0), (98, 0));
        // 100 % blue: Cb = +0.5 (right edge)
        assert_eq!(vec_bin([0.0, 0.0, 1.0], 1.0).0, 255);
        // the skin-tone line runs between the red and yellow targets
        let ang = |c: [f32; 3]| {
            let [cb, cr] = cbcr(c);
            cr.atan2(cb).to_degrees()
        };
        assert!(ang([0.75, 0.0, 0.0]) < SKIN_TONE_DEG && SKIN_TONE_DEG < ang([0.75, 0.75, 0.0]));
        // a typical skin tone lands near the line
        let skin = ang([0.84, 0.62, 0.52]);
        assert!((skin - SKIN_TONE_DEG).abs() < 12.0, "skin at {skin}°");
    }

    #[test]
    fn stride_keeps_sampling_under_the_width_cap() {
        assert_eq!(sample_stride(640), 1);
        assert_eq!(sample_stride(960), 1);
        assert_eq!(sample_stride(1920), 2);
        assert_eq!(sample_stride(3840), 4);
        assert_eq!(sample_count(1920, 1080, 2), 960 * 540);
        assert_eq!(sample_count(5, 3, 2), 3 * 2);
    }
}
