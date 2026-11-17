//! The GPU render graph executor.
//!
//! Turns an [`nle_core::Composition`] into pixels with compute passes only:
//! decoded NV12/P010 frames are uploaded once into VRAM (LRU-cached under
//! the 2.5 GB budget), converted, interpolated with optical flow for slow
//! motion and frame-rate conversion, graded, run through the clip's effect
//! stack (vector motion blur, halation, glow, shake, grain, blurs, keying),
//! transitioned and composited — nothing is read back to the CPU except the
//! final NV12 frame during export.

use crate::cache::{LruCache, Weighted, VRAM_FRAME_CACHE_BUDGET};
use crate::gpu::GpuContext;
use crate::lut::CubeLut;
use crate::pipelines::{Pipe, Pipelines, DISPLAY_FORMAT, WORKING_FORMAT};
use crate::text::{self, FontBook};
use crate::transform::{layer_to_output, Affine};
use bytemuck::{Pod, Zeroable};
use nle_core::eval::{Layer, LayerSource, RenderNode, ResolvedColor, ResolvedTransition};
use nle_core::{AssetId, Composition, EffectKind, FrameBlendMode, FrameRate, ResolvedEffect, TitleAnim, TitleParams};
use nle_media::{PixelFormat, VideoFrame};
use std::collections::HashMap;
use std::sync::Arc;
use wgpu::util::DeviceExt;

/// Supplies decoded frames to the renderer.
pub trait FrameSource {
    /// Native frame rate of an asset (to turn source time into frame index).
    fn frame_rate(&self, asset: AssetId) -> FrameRate;
    /// The decoded frame `index` of `asset`, if available right now.
    fn frame(&mut self, asset: AssetId, index: i64) -> Option<VideoFrame>;
}

#[derive(Clone, Copy, Debug)]
pub struct RenderOptions {
    /// Before/after split position (0..1) for the colour grade, if active.
    pub wipe: Option<f32>,
    /// Apply colour grades (false = bypass for performance or comparison).
    pub color: bool,
}

impl Default for RenderOptions {
    fn default() -> Self {
        RenderOptions { wipe: None, color: true }
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct RenderStats {
    pub layers: u32,
    pub uploads: u32,
    pub missing_frames: u32,
    pub cache_bytes: u64,
    pub cache_frames: usize,
    /// Optical-flow fields computed this frame.
    pub flow_fields: u32,
    /// Effect passes run this frame.
    pub effects: u32,
    /// VRAM held by pooled working targets, flow fields and title masks.
    pub working_bytes: u64,
}

/// Strict 2,560 MB (2.5 GB) LRU VRAM pool budget across frames and working textures.
pub const MAX_VRAM_POOL_BYTES: usize = VRAM_FRAME_CACHE_BUDGET as usize;

/// Longest side of the optical-flow field (finer adds cost, not much quality).
pub const FLOW_MAX_WIDTH: u32 = 960;

// ---------------------------------------------------------------------------

pub struct Target {
    pub texture: wgpu::Texture,
    pub view: wgpu::TextureView,
    pub width: u32,
    pub height: u32,
}

fn make_tex(device: &wgpu::Device, w: u32, h: u32, format: wgpu::TextureFormat, label: &str) -> Target {
    let texture = device.create_texture(&wgpu::TextureDescriptor {
        label: Some(label),
        size: wgpu::Extent3d { width: w.max(1), height: h.max(1), depth_or_array_layers: 1 },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::STORAGE_BINDING,
        view_formats: &[],
    });
    let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
    Target { texture, view, width: w.max(1), height: h.max(1) }
}

/// Recycles same-sized textures of one format between frames.
struct Pool {
    format: wgpu::TextureFormat,
    bytes_per_px: u64,
    free: HashMap<(u32, u32), Vec<Target>>,
}

impl Pool {
    fn new(format: wgpu::TextureFormat) -> Self {
        let bytes_per_px = format.block_copy_size(None).unwrap_or(8) as u64;
        Pool { format, bytes_per_px, free: HashMap::new() }
    }
    fn get(&mut self, device: &wgpu::Device, w: u32, h: u32) -> Target {
        let (w, h) = (w.max(1), h.max(1));
        self.free.get_mut(&(w, h)).and_then(|v| v.pop()).unwrap_or_else(|| make_tex(device, w, h, self.format, "rcp-pooled"))
    }
    fn put(&mut self, t: Target) {
        let v = self.free.entry((t.width, t.height)).or_default();
        if v.len() < 6 {
            v.push(t);
        }
    }
    fn clear(&mut self) {
        self.free.clear();
    }
    /// Bytes held by idle pooled textures.
    fn bytes(&self) -> u64 {
        self.free.iter().map(|((w, h), v)| *w as u64 * *h as u64 * self.bytes_per_px * v.len() as u64).sum()
    }
}

/// A decoded frame resident in VRAM as raw YUV.
pub struct GpuFrame {
    buffer: wgpu::Buffer,
    width: u32,
    height: u32,
    format: PixelFormat,
    bytes: u64,
}

impl Weighted for Arc<GpuFrame> {
    fn weight(&self) -> u64 {
        self.bytes
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct FrameKey {
    pub asset: AssetId,
    pub index: i64,
}

enum Img {
    Pooled(Target),
}

impl Img {
    fn view(&self) -> &wgpu::TextureView {
        match self {
            Img::Pooled(t) => &t.view,
        }
    }
    fn size(&self) -> (u32, u32) {
        match self {
            Img::Pooled(t) => (t.width, t.height),
        }
    }
}

struct GpuLut {
    _texture: wgpu::Texture,
    view: wgpu::TextureView,
    size: u32,
}

/// Rasterised title mask in VRAM.
struct MaskTex {
    _texture: wgpu::Texture,
    view: wgpu::TextureView,
    rect: [f32; 4],
    pill: [f32; 4],
    bytes: u64,
}

/// Per-composition context for drawing layers.
#[derive(Clone, Copy)]
struct Ctx<'a> {
    /// Composition time in seconds (animates grain / shake).
    time: f32,
    /// The composition one frame earlier (adjustment-layer motion blur).
    prev: Option<&'a Composition>,
}

// ----- uniforms ------------------------------------------------------------

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct YuvParams {
    width: u32,
    height: u32,
    uv_offset: u32,
    format: u32,
    matrix: u32,
    full_range: u32,
    _pad: [u32; 2],
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct GradeU {
    exposure: f32,
    contrast: f32,
    saturation: f32,
    temperature: f32,
    tint: f32,
    lut_mix: f32,
    wipe: f32,
    lut_size: f32,
    lift: [f32; 4],
    gamma: [f32; 4],
    gain: [f32; 4],
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct CompU {
    inv_row0: [f32; 4],
    inv_row1: [f32; 4],
    crop: [f32; 4],
    src_rect: [f32; 4],
    opacity: f32,
    blend: u32,
    _pad: [u32; 2],
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct TrU {
    dir: [f32; 2],
    progress: f32,
    softness: f32,
    kind: u32,
    _pad: [u32; 3],
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct BlendU {
    t: f32,
    mode: u32,
    block: f32,
    _pad: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct TitleU {
    text_color: [f32; 4],
    shadow_color: [f32; 4],
    pill_color: [f32; 4],
    pill_rect: [f32; 4],
    mask_rect: [f32; 4],
    outline_color: [f32; 4],
    highlight_color: [f32; 4],
    dim_color: [f32; 4],
    shadow_offset: [f32; 2],
    shadow_blur: f32,
    pill_radius: f32,
    anim_offset: [f32; 2],
    anim_scale: f32,
    opacity: f32,
    outline_width: f32,
    glitch: f32,
    glitch_seed: f32,
    flags: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct SizeU {
    width: u32,
    height: u32,
    _pad: [u32; 2],
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct FlowU {
    swap: u32,
    radius: i32,
    search: i32,
    _pad: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct InterpU {
    t: f32,
    sigma: f32,
    _pad: [f32; 2],
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct BlurU {
    shutter: f32,
    samples: u32,
    threshold: f32,
    quality: u32,
    scale: f32,
    amount: f32,
    sensitivity: f32,
    _pad: f32,
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable, Default)]
struct FxU {
    a: [f32; 4],
    b: [f32; 4],
    c: [f32; 4],
    x: [f32; 4],
    dims: [f32; 2],
    time: f32,
    k: f32,
}

// ----- camera-shake noise (CPU, deterministic) ------------------------------

fn hash1(i: i32, seed: f32) -> f32 {
    let s = ((i as f32) * 127.1 + seed * 311.7).sin() * 43_758.547;
    s - s.floor()
}

/// 1-D gradient (Perlin) noise in -1..1.
fn perlin1(x: f32, seed: f32) -> f32 {
    let i = x.floor();
    let f = x - i;
    let g0 = hash1(i as i32, seed) * 2.0 - 1.0;
    let g1 = hash1(i as i32 + 1, seed) * 2.0 - 1.0;
    let u = f * f * f * (f * (f * 6.0 - 15.0) + 10.0);
    let n = g0 * f + (g1 * (f - 1.0) - g0 * f) * u;
    n * 2.0
}

/// Multi-octave (fractal) noise, roughly -1..1.
pub fn fbm1(x: f32, seed: f32, octaves: u32) -> f32 {
    let (mut sum, mut amp, mut freq, mut norm) = (0.0, 1.0, 1.0, 0.0);
    for o in 0..octaves {
        sum += perlin1(x * freq, seed + o as f32 * 19.19) * amp;
        norm += amp;
        amp *= 0.5;
        freq *= 2.03;
    }
    (sum / norm).clamp(-1.0, 1.0)
}

/// Camera-shake transform at time `t`: (dx, dy) in uv, rotation (radians), zoom.
pub fn shake_transform(e: &ResolvedEffect, t: f32) -> [f32; 4] {
    let v = &e.values;
    let (scale, speed, rot, bias, impact, seed) = (v[0], v[1], v[2], v[3], v[4] > 0.5, v[5]);
    let (decay_rate, zoom_punch, tilt_deg) = (v[6].max(0.0), v[7].max(1.0), v[8].max(0.0));
    let (x, env) = if decay_rate > 0.01 {
        (t * speed.max(0.2) * 3.0, (-t.max(0.0) * decay_rate).exp())
    } else if impact {
        // sharp hits every 2/speed seconds that decay quickly
        let period = 2.0 / speed.max(0.05);
        let phase = (t / period).fract();
        (t * speed * 8.0, (-phase * 6.0).exp())
    } else {
        (t * speed, 1.0)
    };
    let amp = scale * 0.035 * env;
    let mut dx = fbm1(x, seed, 4) * amp;
    let mut dy = fbm1(x, seed + 41.3, 4) * amp;
    // bias −1 → horizontal sway only, +1 → vertical bob only
    dx *= 1.0 - bias.max(0.0) * 0.85;
    dy *= 1.0 + bias.min(0.0) * 0.85;
    let r = fbm1(x * 0.8, seed + 83.7, 3) * (rot * 3f32.to_radians() + tilt_deg.to_radians()) * env;
    let zoom = 1.0 + scale * 0.03 * env + (zoom_punch - 1.0) * env;
    [dx, dy, r, zoom]
}

// ---------------------------------------------------------------------------

pub struct Renderer {
    gpu: GpuContext,
    p: Pipelines,
    sampler: wgpu::Sampler,
    width: u32,
    height: u32,
    display: wgpu::Texture,
    display_storage: wgpu::TextureView,
    display_srgb_tex: wgpu::Texture,
    display_srgb: wgpu::TextureView,
    pool: Pool,
    flow_pool: Pool,
    frames: LruCache<FrameKey, Arc<GpuFrame>>,
    latest: HashMap<AssetId, FrameKey>,
    luts: HashMap<String, Option<Arc<GpuLut>>>,
    identity_lut: Arc<GpuLut>,
    masks: HashMap<u64, Arc<MaskTex>>,
    fonts: FontBook,
    stats: RenderStats,
    /// Automatic preview proxy scale factor during fast J/K/L timeline shuttling (1.0 = full).
    pub shuttle_proxy_scale: f32,
}

fn ceil8(x: u32) -> u32 {
    x.div_ceil(8)
}

fn tv(v: &wgpu::TextureView) -> wgpu::BindingResource<'_> {
    wgpu::BindingResource::TextureView(v)
}

fn entry(binding: u32, resource: wgpu::BindingResource<'_>) -> wgpu::BindGroupEntry<'_> {
    wgpu::BindGroupEntry { binding, resource }
}

/// Optical-flow field size for a `w × h` image (≤ [`FLOW_MAX_WIDTH`] wide).
pub fn flow_size(w: u32, h: u32) -> (u32, u32) {
    let f = (FLOW_MAX_WIDTH as f32 / w.max(1) as f32).min(1.0);
    (((w as f32 * f).ceil() as u32).max(8), ((h as f32 * f).ceil() as u32).max(8))
}

/// Pyramid level sizes, finest first (coarsest ≥ 24 px, at most 5 levels).
pub fn pyramid_sizes(w: u32, h: u32) -> Vec<(u32, u32)> {
    let mut v = vec![(w, h)];
    while v.len() < 5 {
        let (lw, lh) = *v.last().unwrap();
        if lw.min(lh) < 48 {
            break;
        }
        v.push((lw.div_ceil(2), lh.div_ceil(2)));
    }
    v
}

impl Renderer {
    pub fn new(gpu: GpuContext, width: u32, height: u32) -> Self {
        let p = {
            let device = &gpu.device;
            let cache = gpu.pipeline_cache.as_deref();
            Pipelines::new(device, gpu.flow_format, cache)
        };
        gpu.save_pipeline_cache();
        let device = &gpu.device;
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("rcp-linear"),
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            address_mode_w: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            mipmap_filter: wgpu::FilterMode::Nearest,
            ..Default::default()
        });
        let id = CubeLut::identity(2);
        let tex = id.upload(device, &gpu.queue);
        let view = tex.create_view(&wgpu::TextureViewDescriptor::default());
        let identity_lut = Arc::new(GpuLut { _texture: tex, view, size: 2 });
        let (display, display_storage, display_srgb_tex, display_srgb) = Self::make_display(device, width, height);
        Renderer {
            sampler,
            width,
            height,
            display,
            display_storage,
            display_srgb_tex,
            display_srgb,
            pool: Pool::new(WORKING_FORMAT),
            flow_pool: Pool::new(gpu.flow_format),
            p,
            frames: LruCache::new(VRAM_FRAME_CACHE_BUDGET),
            latest: HashMap::new(),
            luts: HashMap::new(),
            identity_lut,
            masks: HashMap::new(),
            fonts: FontBook::system(),
            stats: RenderStats::default(),
            shuttle_proxy_scale: 1.0,
            gpu,
        }
    }

    /// The display output is a plain `rgba8unorm` storage texture plus an
    /// `rgba8unorm-srgb` copy for sampling by egui. A copy is used instead of
    /// an sRGB view of the storage texture: that reinterpretation loses the
    /// device on Intel Arc under DX12.
    fn make_display(device: &wgpu::Device, w: u32, h: u32) -> (wgpu::Texture, wgpu::TextureView, wgpu::Texture, wgpu::TextureView) {
        let size = wgpu::Extent3d { width: w.max(1), height: h.max(1), depth_or_array_layers: 1 };
        let display = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("rcp-display"),
            size,
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: DISPLAY_FORMAT,
            usage: wgpu::TextureUsages::STORAGE_BINDING | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let storage = display.create_view(&wgpu::TextureViewDescriptor::default());
        let srgb_tex = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("rcp-display-srgb"),
            size,
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8UnormSrgb,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let srgb = srgb_tex.create_view(&wgpu::TextureViewDescriptor::default());
        (display, storage, srgb_tex, srgb)
    }

    pub fn gpu(&self) -> &GpuContext {
        &self.gpu
    }
    pub fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }
    pub fn stats(&self) -> RenderStats {
        self.stats
    }
    /// Storage format used for motion vectors (`Rg16Float` when supported).
    pub fn flow_format(&self) -> wgpu::TextureFormat {
        self.p.flow_format
    }

    /// Resize the output (e.g. preview quality change). Clears targets.
    pub fn resize(&mut self, width: u32, height: u32) {
        // A minimised window or a mid-resize DPI change can report 0×0;
        // zero-sized textures are a wgpu validation panic.
        let (width, height) = (width.max(1), height.max(1));
        if (width, height) == (self.width, self.height) {
            return;
        }
        self.width = width;
        self.height = height;
        let (d, s, t, v) = Self::make_display(&self.gpu.device, width, height);
        self.display = d;
        self.display_storage = s;
        self.display_srgb_tex = t;
        self.display_srgb = v;
        self.pool.clear();
        self.flow_pool.clear();
        self.masks.clear();
    }

    /// sRGB view of the final frame, for sampling by the UI (egui).
    pub fn display_view(&self) -> &wgpu::TextureView {
        &self.display_srgb
    }

    /// The sRGB display texture (what [`Self::display_view`] views).
    pub fn display_texture(&self) -> &wgpu::Texture {
        &self.display_srgb_tex
    }

    pub fn clear_cache(&mut self) {
        self.frames.clear();
        self.latest.clear();
        self.masks.clear();
    }

    /// Drop cached frames of one asset (e.g. after toggling proxy media).
    pub fn evict_asset(&mut self, asset: AssetId) {
        self.frames.retain(|k| k.asset != asset);
        self.latest.remove(&asset);
    }

    pub fn is_cached(&self, key: FrameKey) -> bool {
        self.frames.contains(&key)
    }

    /// Upload a decoded frame into the VRAM cache ahead of time.
    pub fn upload_frame(&mut self, key: FrameKey, frame: &VideoFrame) {
        if self.frames.contains(&key) {
            return;
        }
        let mut bytes = frame.data.as_slice().to_vec();
        while !bytes.len().is_multiple_of(4) {
            bytes.push(0);
        }
        let buffer = self.gpu.device.create_buffer_init(&wgpu::util::BufferInitDescriptor { label: Some("rcp-frame"), contents: &bytes, usage: wgpu::BufferUsages::STORAGE });
        let gf = Arc::new(GpuFrame { buffer, width: frame.width, height: frame.height, format: frame.format, bytes: bytes.len() as u64 });
        self.frames.insert(key, gf);
        self.stats.uploads += 1;
        match self.latest.get(&key.asset) {
            Some(k) if k.index > key.index => {}
            _ => {
                self.latest.insert(key.asset, key);
            }
        }
    }

    /// Keep the whole renderer (frame cache + working textures) inside the
    /// 2.5 GB VRAM budget: the frame cache gets what the working set leaves.
    fn enforce_vram_budget(&mut self) {
        let working = self.pool.bytes() + self.flow_pool.bytes() + self.masks.values().map(|m| m.bytes).sum::<u64>();
        let floor = 256 * 1024 * 1024;
        self.frames.set_budget(VRAM_FRAME_CACHE_BUDGET.saturating_sub(working).max(floor));
        self.stats.working_bytes = working;
        self.stats.cache_bytes = self.frames.used();
        self.stats.cache_frames = self.frames.len();
    }

    // ----- helpers -----------------------------------------------------------

    fn uniform<T: Pod>(&self, v: &T) -> wgpu::Buffer {
        self.gpu.device.create_buffer_init(&wgpu::util::BufferInitDescriptor { label: Some("rcp-uniform"), contents: bytemuck::bytes_of(v), usage: wgpu::BufferUsages::UNIFORM })
    }

    fn dispatch(&self, enc: &mut wgpu::CommandEncoder, pipe: &Pipe, entries: &[wgpu::BindGroupEntry], gx: u32, gy: u32) {
        let bg = self.gpu.device.create_bind_group(&wgpu::BindGroupDescriptor { label: None, layout: &pipe.layout, entries });
        let mut pass = enc.begin_compute_pass(&wgpu::ComputePassDescriptor { label: None, timestamp_writes: None });
        pass.set_pipeline(&pipe.pipeline);
        pass.set_bind_group(0, &bg, &[]);
        pass.dispatch_workgroups(gx.max(1), gy.max(1), 1);
    }

    fn fill(&mut self, enc: &mut wgpu::CommandEncoder, t: &Target, color: [f32; 4]) {
        let u = self.uniform(&color);
        self.dispatch(enc, &self.p.fill, &[entry(0, u.as_entire_binding()), entry(1, tv(&t.view))], ceil8(t.width), ceil8(t.height));
    }

    fn convert(&mut self, enc: &mut wgpu::CommandEncoder, f: &GpuFrame) -> Target {
        let out = self.pool.get(&self.gpu.device, f.width, f.height);
        let bps = if f.format == PixelFormat::P010 { 2 } else { 1 };
        let params = YuvParams {
            width: f.width,
            height: f.height,
            uv_offset: f.width * f.height * bps,
            format: match f.format {
                PixelFormat::Nv12 => 0,
                PixelFormat::P010 => 1,
                PixelFormat::Rgba8 => 2,
            },
            matrix: if f.height > 1100 && f.format == PixelFormat::P010 { 2 } else { 0 },
            full_range: 0,
            _pad: [0; 2],
        };
        let u = self.uniform(&params);
        self.dispatch(enc, &self.p.yuv, &[entry(0, f.buffer.as_entire_binding()), entry(1, u.as_entire_binding()), entry(2, tv(&out.view))], ceil8(f.width), ceil8(f.height));
        out
    }

    fn lut(&mut self, path: &str) -> Arc<GpuLut> {
        if !self.luts.contains_key(path) {
            let loaded = match CubeLut::load(std::path::Path::new(path)) {
                Ok(l) => {
                    let tex = l.upload(&self.gpu.device, &self.gpu.queue);
                    let view = tex.create_view(&wgpu::TextureViewDescriptor::default());
                    Some(Arc::new(GpuLut { _texture: tex, view, size: l.size as u32 }))
                }
                Err(e) => {
                    log::warn!("LUT {path}: {e}");
                    None
                }
            };
            self.luts.insert(path.to_string(), loaded);
        }
        self.luts.get(path).cloned().flatten().unwrap_or_else(|| self.identity_lut.clone())
    }

    fn grade(&mut self, enc: &mut wgpu::CommandEncoder, src: &wgpu::TextureView, size: (u32, u32), c: &ResolvedColor, wipe: Option<f32>) -> Target {
        let out = self.pool.get(&self.gpu.device, size.0, size.1);
        let (lut, mix) = match &c.lut {
            Some(l) if l.intensity > 0.0 => (self.lut(&l.path), l.intensity),
            _ => (self.identity_lut.clone(), 0.0),
        };
        let u = self.uniform(&GradeU {
            exposure: c.exposure,
            contrast: c.contrast,
            saturation: c.saturation,
            temperature: c.temperature,
            tint: c.tint,
            lut_mix: if lut.size > 2 || mix == 0.0 { mix } else { 0.0 },
            wipe: wipe.unwrap_or(-1.0),
            lut_size: lut.size as f32,
            lift: c.lift,
            gamma: c.gamma,
            gain: c.gain,
        });
        self.dispatch(
            enc,
            &self.p.grade,
            &[entry(0, tv(src)), entry(1, tv(&lut.view)), entry(2, wgpu::BindingResource::Sampler(&self.sampler)), entry(3, u.as_entire_binding()), entry(4, tv(&out.view))],
            ceil8(size.0),
            ceil8(size.1),
        );
        out
    }

    fn composite(&mut self, enc: &mut wgpu::CommandEncoder, bg: &Target, img: &wgpu::TextureView, inv: Affine, layer: &Layer) -> Target {
        let out = self.pool.get(&self.gpu.device, bg.width, bg.height);
        let v = &layer.video;
        let u = self.uniform(&CompU {
            inv_row0: [inv.m[0][0], inv.m[0][1], inv.m[0][2], 0.0],
            inv_row1: [inv.m[1][0], inv.m[1][1], inv.m[1][2], 0.0],
            crop: v.crop,
            src_rect: v.source_rect,
            opacity: v.opacity,
            blend: v.blend.shader_index(),
            _pad: [0; 2],
        });
        self.dispatch(
            enc,
            &self.p.composite,
            &[entry(0, tv(&bg.view)), entry(1, tv(img)), entry(2, wgpu::BindingResource::Sampler(&self.sampler)), entry(3, u.as_entire_binding()), entry(4, tv(&out.view))],
            ceil8(bg.width),
            ceil8(bg.height),
        );
        out
    }

    fn transition(&mut self, enc: &mut wgpu::CommandEncoder, a: &Target, b: &Target, t: &ResolvedTransition) -> Target {
        let out = self.pool.get(&self.gpu.device, a.width, a.height);
        let u = self.uniform(&TrU { dir: t.direction.vector(), progress: t.progress, softness: t.softness, kind: t.kind.shader_index(), _pad: [0; 3] });
        self.dispatch(
            enc,
            &self.p.transition,
            &[entry(0, tv(&a.view)), entry(1, tv(&b.view)), entry(2, wgpu::BindingResource::Sampler(&self.sampler)), entry(3, u.as_entire_binding()), entry(4, tv(&out.view))],
            ceil8(a.width),
            ceil8(a.height),
        );
        out
    }

    // ----- optical flow --------------------------------------------------------

    #[allow(clippy::too_many_arguments)] // one GPU pass: encoder, targets and per-pass state
    fn flow_pass(&mut self, enc: &mut wgpu::CommandEncoder, pipe: &str, a: &wgpu::TextureView, b: &wgpu::TextureView, f: &wgpu::TextureView, u: FlowU, out: &Target) {
        let ub = self.uniform(&u);
        let p = match pipe {
            "luma" => &self.p.flow_luma,
            "down" => &self.p.flow_down,
            "init" => &self.p.flow_init,
            "up" => &self.p.flow_up,
            _ => &self.p.flow_lk,
        };
        self.dispatch(
            enc,
            p,
            &[
                entry(0, tv(a)),
                entry(1, tv(b)),
                entry(2, tv(f)),
                entry(3, wgpu::BindingResource::Sampler(&self.sampler)),
                entry(4, ub.as_entire_binding()),
                entry(5, tv(&out.view)),
            ],
            ceil8(out.width),
            ceil8(out.height),
        );
    }

    /// Dense optical flow between two images of `size`: multi-scale
    /// pyramidal block matching, refined with `iters` Lucas-Kanade steps per
    /// level. `backward` estimates B → A instead of A → B. The returned field
    /// (in the flow pool's format) holds vectors in its own pixel units.
    fn optical_flow(&mut self, enc: &mut wgpu::CommandEncoder, a: &wgpu::TextureView, b: &wgpu::TextureView, size: (u32, u32), backward: bool, iters: u32) -> Target {
        self.stats.flow_fields += 1;
        let dev = self.gpu.device.clone();
        let (fw, fh) = flow_size(size.0, size.1);
        let sizes = pyramid_sizes(fw, fh);
        let base = FlowU { swap: u32::from(backward), radius: 3, search: 4, _pad: 0 };
        let mut pyr: Vec<Target> = Vec::with_capacity(sizes.len());
        let l0 = self.flow_pool.get(&dev, sizes[0].0, sizes[0].1);
        self.flow_pass(enc, "luma", a, b, a, base, &l0);
        pyr.push(l0);
        for &(w, h) in &sizes[1..] {
            let next = self.flow_pool.get(&dev, w, h);
            let prev = &pyr[pyr.len() - 1].view;
            self.flow_pass(enc, "down", prev, prev, prev, base, &next);
            pyr.push(next);
        }
        let top = sizes.len() - 1;
        let mut flow = self.flow_pool.get(&dev, sizes[top].0, sizes[top].1);
        self.flow_pass(enc, "init", &pyr[top].view, &pyr[top].view, &pyr[top].view, base, &flow);
        for level in (0..=top).rev() {
            if level < top {
                let up = self.flow_pool.get(&dev, sizes[level].0, sizes[level].1);
                self.flow_pass(enc, "up", &pyr[level].view, &pyr[level].view, &flow.view, base, &up);
                self.flow_pool.put(std::mem::replace(&mut flow, up));
            }
            for _ in 0..iters.max(1) {
                let next = self.flow_pool.get(&dev, sizes[level].0, sizes[level].1);
                self.flow_pass(enc, "lk", &pyr[level].view, &pyr[level].view, &flow.view, base, &next);
                self.flow_pool.put(std::mem::replace(&mut flow, next));
            }
        }
        for t in pyr {
            self.flow_pool.put(t);
        }
        flow
    }

    /// Optical-flow in-between frame at `t` (0..1) between `a` and `b`.
    fn interpolate(&mut self, enc: &mut wgpu::CommandEncoder, a: &Target, b: &Target, t: f32) -> Target {
        let size = (a.width, a.height);
        let fwd = self.optical_flow(enc, &a.view, &b.view, size, false, 2);
        let bwd = self.optical_flow(enc, &a.view, &b.view, size, true, 2);
        let out = self.pool.get(&self.gpu.device, a.width, a.height);
        let u = self.uniform(&InterpU { t, sigma: 1.5, _pad: [0.0; 2] });
        self.dispatch(
            enc,
            &self.p.interpolate,
            &[
                entry(0, tv(&a.view)),
                entry(1, tv(&b.view)),
                entry(2, tv(&fwd.view)),
                entry(3, tv(&bwd.view)),
                entry(4, wgpu::BindingResource::Sampler(&self.sampler)),
                entry(5, u.as_entire_binding()),
                entry(6, tv(&out.view)),
            ],
            ceil8(a.width),
            ceil8(a.height),
        );
        self.flow_pool.put(fwd);
        self.flow_pool.put(bwd);
        out
    }

    /// Synthesize an in-between frame (`t` in 0..1) from two source frames.
    fn frame_blend(&mut self, enc: &mut wgpu::CommandEncoder, a: &Target, b: &Target, t: f32, mode: FrameBlendMode, flow: Option<&Target>) -> Target {
        if mode == FrameBlendMode::OpticalFlow {
            return self.interpolate(enc, a, b, t);
        }
        let (w, h) = (a.width, a.height);
        let out = self.pool.get(&self.gpu.device, w, h);
        let (shader_mode, mv_view) = match (mode, flow) {
            (FrameBlendMode::WeightedAccumulation, Some(f)) => (3, &f.view),
            _ => (1, &a.view),
        };
        let u = self.uniform(&BlendU { t, mode: shader_mode, block: 4.0, _pad: 0 });
        self.dispatch(
            enc,
            &self.p.frameblend,
            &[
                entry(0, tv(&a.view)),
                entry(1, tv(&b.view)),
                entry(2, tv(mv_view)),
                entry(3, wgpu::BindingResource::Sampler(&self.sampler)),
                entry(4, u.as_entire_binding()),
                entry(5, tv(&out.view)),
            ],
            ceil8(w),
            ceil8(h),
        );
        out
    }

    // ----- effects -------------------------------------------------------------

    fn fx(&mut self, enc: &mut wgpu::CommandEncoder, pipe: &str, src: &wgpu::TextureView, aux: &wgpu::TextureView, u: FxU, out: &Target) {
        let ub = self.uniform(&u);
        let p = match pipe {
            "bright" => &self.p.fx_bright,
            "blur_h" => &self.p.fx_blur_h,
            "blur_v" => &self.p.fx_blur_v,
            "combine" => &self.p.fx_combine,
            "shake" => &self.p.fx_shake,
            "grain" => &self.p.fx_grain,
            "dir_blur" => &self.p.fx_dir_blur,
            "zoom_blur" => &self.p.fx_zoom_blur,
            "rgb_split" => &self.p.fx_rgb_split_glitch,
            "impact_flash" => &self.p.fx_impact_flash,
            "neon_outline" => &self.p.fx_neon_outline,
            "reframe_916" => &self.p.fx_gaming_reframe_916,
            _ => &self.p.fx_chroma_key,
        };
        self.dispatch(
            enc,
            p,
            &[entry(0, tv(src)), entry(1, tv(aux)), entry(2, wgpu::BindingResource::Sampler(&self.sampler)), entry(3, ub.as_entire_binding()), entry(4, tv(&out.view))],
            ceil8(out.width),
            ceil8(out.height),
        );
    }

    /// Glow / halation: soft-knee threshold → two-scale separable Gaussian at
    /// half resolution (a tight core plus a wide halo that reaches `radius`)
    /// → two combine passes (additive, or screen when `tint[3] = 1`).
    ///
    /// A single wide Gaussian spreads sparse highlights so thin that a
    /// typical preset (0.78 / 32 px / 0.35) changed the frame by < 1 %; the
    /// core keeps the light dense next to the highlight while the halo
    /// carries it out to the radius.
    #[allow(clippy::too_many_arguments)]
    fn bloom(
        &mut self,
        enc: &mut wgpu::CommandEncoder,
        src: &wgpu::TextureView,
        size: (u32, u32),
        base: FxU,
        threshold: f32,
        mask: bool,
        sigma_full: f32,
        tint: [f32; 4],
    ) -> Target {
        const CORE_GAIN: f32 = 1.6;
        const HALO_GAIN: f32 = 2.4;
        let dev = self.gpu.device.clone();
        let (hw, hh) = (size.0.div_ceil(2), size.1.div_ceil(2));
        let h1 = self.pool.get(&dev, hw, hh);
        let h2 = self.pool.get(&dev, hw, hh);
        let core = self.pool.get(&dev, hw, hh);
        self.fx(enc, "bright", src, src, FxU { x: [threshold, if mask { 1.0 } else { 0.0 }, 0.0, 0.0], ..base }, &h1);
        // half-res sigmas; core² + halo² = radius² so the halo ends at `radius`
        let sigma = (sigma_full * 0.5).max(0.5);
        let (s_core, s_halo) = ((sigma * 0.3).max(0.5), (sigma * (1.0f32 - 0.09).sqrt()).max(0.5));
        let blur = |s: f32| FxU { x: [s, 0.0, 0.0, 0.0], ..base };
        self.fx(enc, "blur_h", &h1.view, &h1.view, blur(s_core), &h2);
        self.fx(enc, "blur_v", &h2.view, &h2.view, blur(s_core), &core);
        self.fx(enc, "blur_h", &core.view, &core.view, blur(s_halo), &h2);
        self.fx(enc, "blur_v", &h2.view, &h2.view, blur(s_halo), &h1);
        let scaled = |g: f32| [tint[0] * g, tint[1] * g, tint[2] * g, tint[3]];
        let mid = self.pool.get(&dev, size.0, size.1);
        self.fx(enc, "combine", src, &core.view, FxU { x: scaled(CORE_GAIN), ..base }, &mid);
        let out = self.pool.get(&dev, size.0, size.1);
        self.fx(enc, "combine", &mid.view, &h1.view, FxU { x: scaled(HALO_GAIN), ..base }, &out);
        for t in [h1, h2, core, mid] {
            self.pool.put(t);
        }
        out
    }

    fn motion_blur(&mut self, enc: &mut wgpu::CommandEncoder, src: &wgpu::TextureView, size: (u32, u32), e: &ResolvedEffect, flow: &Target, vel_scale: f32) -> Target {
        let k = size.1 as f32 / 1080.0;
        let out = self.pool.get(&self.gpu.device, size.0, size.1);
        let v = &e.values;
        let u = self.uniform(&BlurU {
            shutter: v[0] / 360.0,
            samples: v[1].clamp(2.0, 64.0) as u32,
            threshold: v[2] * k,
            quality: v[3] as u32,
            scale: vel_scale,
            amount: v[4].clamp(0.0, 1.5),
            sensitivity: (v[5] / 100.0).clamp(0.0, 1.0),
            _pad: 0.0,
        });
        self.dispatch(
            enc,
            &self.p.motion_blur,
            &[entry(0, tv(src)), entry(1, tv(&flow.view)), entry(2, wgpu::BindingResource::Sampler(&self.sampler)), entry(3, u.as_entire_binding()), entry(4, tv(&out.view))],
            ceil8(size.0),
            ceil8(size.1),
        );
        out
    }

    /// Gaming 9:16 reframe: a quarter-res Gaussian-blurred copy of the source
    /// feeds the background fill; the layout shader writes a 9:16 layer
    /// (`resize`) or, on adjustment layers, a frame of the same size. Either
    /// way every region is scaled uniformly (see [`crate::reframe`]).
    fn reframe_916(&mut self, enc: &mut wgpu::CommandEncoder, src: &wgpu::TextureView, size: (u32, u32), base: FxU, resize: bool) -> Target {
        let dev = self.gpu.device.clone();
        let (qw, qh) = (size.0.div_ceil(4).max(1), size.1.div_ceil(4).max(1));
        let q1 = self.pool.get(&dev, qw, qh);
        let q2 = self.pool.get(&dev, qw, qh);
        let sigma = size.1 as f32 * 0.025;
        self.fx(enc, "blur_h", src, src, FxU { x: [sigma, 0.0, 0.0, 0.0], ..base }, &q1);
        self.fx(enc, "blur_v", &q1.view, &q1.view, FxU { x: [sigma * 0.25, 0.0, 0.0, 0.0], ..base }, &q2);
        let (ow, oh) = if resize { crate::reframe::output_size(size, self.height) } else { size };
        let out = self.pool.get(&dev, ow, oh);
        self.fx(enc, "reframe_916", src, &q2.view, FxU { dims: [ow as f32, oh as f32], k: oh as f32 / 1080.0, ..base }, &out);
        self.pool.put(q1);
        self.pool.put(q2);
        out
    }

    /// One effect from `src` into a new pooled target (`None` = skipped).
    #[allow(clippy::too_many_arguments)]
    fn apply_effect(
        &mut self,
        enc: &mut wgpu::CommandEncoder,
        src: &wgpu::TextureView,
        size: (u32, u32),
        e: &ResolvedEffect,
        flow: Option<&Target>,
        vel_scale: f32,
        time: f32,
        resize: bool,
    ) -> Option<Target> {
        let v = e.values;
        let k = size.1 as f32 / 1080.0;
        let base = FxU { a: [v[0], v[1], v[2], v[3]], b: [v[4], v[5], v[6], v[7]], c: [v[8], v[9], v[10], v[11]], x: [0.0; 4], dims: [size.0 as f32, size.1 as f32], time, k };
        let simple = |r: &mut Self, enc: &mut wgpu::CommandEncoder, pipe: &str, u: FxU| {
            let out = r.pool.get(&r.gpu.device, size.0, size.1);
            r.fx(enc, pipe, src, src, u, &out);
            out
        };
        self.stats.effects += 1;
        Some(match e.kind {
            EffectKind::MotionBlur => self.motion_blur(enc, src, size, e, flow?, vel_scale),
            EffectKind::Halation => {
                let w = v[3];
                // white → warm red (1.0, 0.32, 0.1) by warmth `w`; red stays at 1.0
                let tint = [1.0, 1.0 + (0.32 - 1.0) * w, 1.0 + (0.1 - 1.0) * w];
                // bloom's core + halo gains sum to 4; halation keeps its unit gain
                let s = v[2] * 0.25;
                self.bloom(enc, src, size, base, v[0], false, v[1] * k, [tint[0] * s, tint[1] * s, tint[2] * s, 1.0])
            }
            EffectKind::Glow => self.bloom(enc, src, size, base, v[0], true, v[1] * k, [v[2] * v[3], v[2] * v[4], v[2] * v[5], v[6]]),
            EffectKind::CameraShake => simple(self, enc, "shake", FxU { x: shake_transform(e, time), ..base }),
            EffectKind::FilmGrain => simple(self, enc, "grain", base),
            EffectKind::DirectionalBlur => simple(self, enc, "dir_blur", base),
            EffectKind::ZoomBlur => simple(self, enc, "zoom_blur", base),
            EffectKind::ChromaKey => simple(self, enc, "chroma", base),
            EffectKind::RgbSplitGlitch => simple(self, enc, "rgb_split", base),
            EffectKind::ImpactFlash => simple(self, enc, "impact_flash", base),
            EffectKind::NeonOutline => simple(self, enc, "neon_outline", base),
            EffectKind::GamingReframe916 => self.reframe_916(enc, src, size, base, resize),
        })
    }

    /// Run an effect stack over `src`. Returns `None` if nothing was applied.
    /// `resize` lets an effect change the layer size (9:16 reframe on a clip);
    /// adjustment layers keep the frame size.
    #[allow(clippy::too_many_arguments)]
    fn apply_effects(
        &mut self,
        enc: &mut wgpu::CommandEncoder,
        src: &wgpu::TextureView,
        size: (u32, u32),
        effects: &[ResolvedEffect],
        flow: Option<&Target>,
        vel_scale: f32,
        time: f32,
        resize: bool,
    ) -> Option<Target> {
        let mut cur: Option<Target> = None;
        for e in effects {
            let out = {
                let (view, size) = cur.as_ref().map_or((src, size), |t| (&t.view, (t.width, t.height)));
                self.apply_effect(enc, view, size, e, flow, vel_scale, time, resize)
            };
            if let Some(o) = out {
                if let Some(old) = cur.replace(o) {
                    self.pool.put(old);
                }
            }
        }
        cur
    }

    /// Motion-blur quality → Lucas-Kanade iterations per pyramid level.
    fn blur_iters(effects: &[ResolvedEffect]) -> u32 {
        effects.iter().find(|e| e.kind == EffectKind::MotionBlur).map_or(1, |e| if e.values[3] >= 0.5 { 3 } else { 1 })
    }

    // ----- titles ----------------------------------------------------------------

    fn title_mask(&mut self, p: &TitleParams, anim: &TitleAnim) -> Option<Arc<MaskTex>> {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        format!("{p:?}").hash(&mut h);
        anim.layout_key().hash(&mut h);
        anim.highlight_word.hash(&mut h);
        anim.dim_from.hash(&mut h);
        ((anim.word_scale * 64.0).round() as i32).hash(&mut h);
        ((anim.word_lift * 4.0).round() as i32).hash(&mut h);
        if p.word_pill {
            anim.pill_from.hash(&mut h);
            ((anim.pill_glide * 64.0).round() as i32).hash(&mut h);
        }
        (self.width, self.height).hash(&mut h);
        let key = h.finish();
        if let Some(m) = self.masks.get(&key) {
            return Some(m.clone());
        }
        let mask = text::rasterize(p, anim, self.width, self.height, &self.fonts)?;
        let texture = self.gpu.device.create_texture_with_data(
            &self.gpu.queue,
            &wgpu::TextureDescriptor {
                label: Some("rcp-title-mask"),
                size: wgpu::Extent3d { width: mask.width, height: mask.height, depth_or_array_layers: 1 },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::Rgba8Unorm,
                usage: wgpu::TextureUsages::TEXTURE_BINDING,
                view_formats: &[],
            },
            wgpu::util::TextureDataOrder::LayerMajor,
            &mask.data,
        );
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        if self.masks.len() > 96 {
            self.masks.clear();
        }
        let m = Arc::new(MaskTex { _texture: texture, view, rect: mask.rect, pill: mask.pill, bytes: mask.data.len() as u64 });
        self.masks.insert(key, m.clone());
        Some(m)
    }

    fn title_layer(&mut self, enc: &mut wgpu::CommandEncoder, p: &TitleParams, anim: &TitleAnim) -> Option<Target> {
        if anim.opacity <= 0.0 || anim.scale <= 0.0 {
            return None;
        }
        let mask = self.title_mask(p, anim)?;
        let k = self.height as f32 / 1080.0;
        let out = self.pool.get(&self.gpu.device, self.width, self.height);
        let u = self.uniform(&TitleU {
            text_color: p.color,
            shadow_color: p.shadow_color,
            pill_color: p.pill_color,
            pill_rect: mask.pill,
            mask_rect: mask.rect,
            outline_color: p.outline_color,
            highlight_color: p.highlight_color,
            dim_color: p.dim_color.unwrap_or(p.color),
            shadow_offset: [p.shadow_offset[0] * k, p.shadow_offset[1] * k],
            shadow_blur: p.shadow_blur * k,
            pill_radius: p.pill_radius * k,
            anim_offset: [anim.offset[0] * k, anim.offset[1] * k],
            anim_scale: anim.scale,
            opacity: anim.opacity,
            outline_width: p.outline_width * k,
            glitch: anim.glitch,
            glitch_seed: anim.glitch_seed,
            flags: u32::from(p.shadow_enabled)
                | (u32::from(p.pill_enabled && mask.pill[2] > mask.pill[0]) << 1)
                | (u32::from(p.outline_enabled) << 2)
                // warm glow around the spoken word
                | (u32::from(anim.highlight_word.is_some()) << 3),
        });
        self.dispatch(
            enc,
            &self.p.title,
            &[entry(0, tv(&mask.view)), entry(1, wgpu::BindingResource::Sampler(&self.sampler)), entry(2, u.as_entire_binding()), entry(3, tv(&out.view))],
            ceil8(self.width),
            ceil8(self.height),
        );
        Some(out)
    }

    // ----- layers ----------------------------------------------------------------

    fn media_frame(&mut self, enc: &mut wgpu::CommandEncoder, asset: AssetId, index: i64, src: &mut dyn FrameSource, exact: bool) -> Option<Target> {
        let key = FrameKey { asset, index };
        if !self.frames.contains(&key) {
            if let Some(f) = src.frame(asset, index) {
                self.upload_frame(key, &f);
            }
        }
        let gf = match self.frames.get(&key) {
            Some(f) => f.clone(),
            None if !exact => {
                // not decoded yet: show the most recent frame of this asset
                self.stats.missing_frames += 1;
                let k = *self.latest.get(&asset)?;
                self.frames.get(&k)?.clone()
            }
            None => return None,
        };
        Some(self.convert(enc, &gf))
    }

    /// The layer's pixels before grading, plus a motion field (and velocity
    /// scale) when the layer's effect stack needs one.
    fn layer_source(&mut self, enc: &mut wgpu::CommandEncoder, layer: &Layer, src: &mut dyn FrameSource, opts: RenderOptions, depth: u32) -> Option<(Img, Option<Target>, f32)> {
        Some(match &layer.source {
            LayerSource::Media { asset, source_time, speed, blend } => {
                let rate = src.frame_rate(*asset);
                let pos = source_time.as_secs_f64() * rate.fps();
                let idx = pos.floor() as i64;
                let frac = (pos - idx as f64) as f32;
                let a = self.media_frame(enc, *asset, idx, src, false)?;
                // Rate conform interpolates whenever the timeline samples between
                // source frames — at any speed (24/30/60 fps on a 120 fps timeline).
                let wants_blend = *blend != FrameBlendMode::FloorFrame && frac > 0.02;
                let wants_motion = layer.has_effect(EffectKind::MotionBlur) || (*blend == FrameBlendMode::WeightedAccumulation && wants_blend);
                let b = if wants_blend || wants_motion { self.media_frame(enc, *asset, idx + 1, src, true) } else { None };
                let b = match b {
                    Some(b) if (b.width, b.height) == (a.width, a.height) => Some(b),
                    Some(b) => {
                        self.pool.put(b);
                        None
                    }
                    None => None,
                };
                let Some(b) = b else { return Some((Img::Pooled(a), None, 0.0)) };
                let flow = if wants_motion { Some(self.optical_flow(enc, &a.view, &b.view, (a.width, a.height), false, Self::blur_iters(&layer.effects))) } else { None };
                let img = if wants_blend {
                    let o = self.frame_blend(enc, &a, &b, frac, *blend, flow.as_ref());
                    self.pool.put(a);
                    o
                } else {
                    a
                };
                self.pool.put(b);
                (Img::Pooled(img), flow, speed.abs().max(0.05) as f32)
            }
            LayerSource::Title(p, anim) => (Img::Pooled(self.title_layer(enc, p, anim)?), None, 0.0),
            LayerSource::Solid(c) => {
                let t = self.pool.get(&self.gpu.device, self.width, self.height);
                self.fill(enc, &t, *c);
                (Img::Pooled(t), None, 0.0)
            }
            LayerSource::Compound(inner) => {
                if depth > 16 {
                    return None;
                }
                (Img::Pooled(self.render_comp(enc, inner, src, opts, depth + 1)), None, 0.0)
            }
        })
    }

    fn layer_image(&mut self, enc: &mut wgpu::CommandEncoder, layer: &Layer, src: &mut dyn FrameSource, opts: RenderOptions, depth: u32, ctx: Ctx) -> Option<Img> {
        let (img, flow, vel) = self.layer_source(enc, layer, src, opts, depth)?;
        let img = self.maybe_grade(enc, img, layer, opts);
        let out = if layer.effects.is_empty() {
            img
        } else {
            let size = img.size();
            match self.apply_effects(enc, img.view(), size, &layer.effects, flow.as_ref(), vel, ctx.time, true) {
                Some(t) => {
                    let Img::Pooled(old) = img;
                    {
                        self.pool.put(old);
                    }
                    Img::Pooled(t)
                }
                None => img,
            }
        };
        if let Some(f) = flow {
            self.flow_pool.put(f);
        }
        Some(out)
    }

    fn maybe_grade(&mut self, enc: &mut wgpu::CommandEncoder, img: Img, layer: &Layer, opts: RenderOptions) -> Img {
        if opts.color && layer.color.enabled {
            let size = img.size();
            let graded = self.grade(enc, img.view(), size, &layer.color, opts.wipe);
            let Img::Pooled(t) = img;
            {
                self.pool.put(t);
            }
            return Img::Pooled(graded);
        }
        img
    }

    /// The previous frame's composite of everything below `lane` (for
    /// adjustment-layer motion vectors).
    fn render_prev_below(&mut self, enc: &mut wgpu::CommandEncoder, prev: &Composition, lane: i32, src: &mut dyn FrameSource, opts: RenderOptions, depth: u32) -> Target {
        let nodes = prev
            .nodes
            .iter()
            .filter(|n| match n {
                RenderNode::Layer(l) => l.lane < lane && !l.adjustment,
                RenderNode::Transition { .. } => 0 < lane,
            })
            .cloned()
            .collect();
        self.render_comp(enc, &Composition::new(prev.time, nodes), src, opts, depth + 1)
    }

    /// An adjustment layer: grade + effects applied to everything beneath.
    #[allow(clippy::too_many_arguments)] // one GPU pass: encoder, targets and per-pass state
    fn draw_adjustment(&mut self, enc: &mut wgpu::CommandEncoder, bg: Target, layer: &Layer, src: &mut dyn FrameSource, opts: RenderOptions, depth: u32, ctx: Ctx) -> Target {
        let grade = opts.color && layer.color.enabled;
        if !grade && layer.effects.is_empty() {
            return bg;
        }
        let size = (bg.width, bg.height);
        let graded = if grade { Some(self.grade(enc, &bg.view, size, &layer.color, opts.wipe)) } else { None };
        // motion vectors of the composite: previous frame → this one
        let flow = match ctx.prev {
            Some(prev) if layer.has_effect(EffectKind::MotionBlur) && depth < 16 => {
                let before = self.render_prev_below(enc, prev, layer.lane, src, opts, depth);
                let f = self.optical_flow(enc, &before.view, &bg.view, size, true, Self::blur_iters(&layer.effects));
                self.pool.put(before);
                Some(f)
            }
            _ => None,
        };
        let fx = {
            let view = graded.as_ref().map_or(&bg.view, |t| &t.view);
            self.apply_effects(enc, view, size, &layer.effects, flow.as_ref(), 1.0, ctx.time, false)
        };
        if let Some(f) = flow {
            self.flow_pool.put(f);
        }
        let result = match (fx, graded) {
            (Some(f), Some(g)) => {
                self.pool.put(g);
                f
            }
            (Some(f), None) => f,
            (None, Some(g)) => g,
            (None, None) => return bg,
        };
        // respect the adjustment layer's opacity by compositing the processed copy
        let id = layer_to_output(&nle_core::eval::ResolvedVideo::default(), [bg.width as f32, bg.height as f32], [bg.width as f32, bg.height as f32]);
        let mut l2 = layer.clone();
        l2.video.crop = [0.0; 4];
        l2.video.source_rect = [0.0, 0.0, 1.0, 1.0];
        let out = self.composite(enc, &bg, &result.view, id.inverse().expect("identity"), &l2);
        self.pool.put(result);
        self.pool.put(bg);
        out
    }

    /// Composite `layer` over `bg`, returning the new accumulator.
    #[allow(clippy::too_many_arguments)] // one GPU pass: encoder, targets and per-pass state
    fn draw_layer(&mut self, enc: &mut wgpu::CommandEncoder, bg: Target, layer: &Layer, src: &mut dyn FrameSource, opts: RenderOptions, depth: u32, ctx: Ctx) -> Target {
        self.stats.layers += 1;
        if layer.adjustment {
            return self.draw_adjustment(enc, bg, layer, src, opts, depth, ctx);
        }
        let Some(img) = self.layer_image(enc, layer, src, opts, depth, ctx) else {
            return bg;
        };
        let (lw, lh) = img.size();
        let fwd = layer_to_output(&layer.video, [lw as f32, lh as f32], [bg.width as f32, bg.height as f32]);
        let Some(inv) = fwd.inverse() else {
            let Img::Pooled(t) = img;
            {
                self.pool.put(t);
            }
            return bg;
        };
        let out = self.composite(enc, &bg, img.view(), inv, layer);
        let Img::Pooled(t) = img;
        {
            self.pool.put(t);
        }
        self.pool.put(bg);
        out
    }

    fn render_comp(&mut self, enc: &mut wgpu::CommandEncoder, comp: &Composition, src: &mut dyn FrameSource, opts: RenderOptions, depth: u32) -> Target {
        let (w, h) = (self.width, self.height);
        let ctx = Ctx { time: comp.time.as_secs_f64() as f32, prev: comp.prev.as_deref() };
        let mut acc = self.pool.get(&self.gpu.device, w, h);
        self.fill(enc, &acc, [0.0, 0.0, 0.0, 1.0]);
        for node in &comp.nodes {
            match node {
                RenderNode::Layer(l) => acc = self.draw_layer(enc, acc, l, src, opts, depth, ctx),
                RenderNode::Transition { from, to, transition } => {
                    // Each side is the current composite plus that side's layer,
                    // so lower lanes stay visible through the transition.
                    let side = |r: &mut Self, enc: &mut wgpu::CommandEncoder, l: &Option<Box<Layer>>, acc: &Target, src: &mut dyn FrameSource| -> Target {
                        let base = r.pool.get(&r.gpu.device, w, h);
                        // copy acc → base with an identity, opaque, normal-blend composite
                        r.fill(enc, &base, [0.0, 0.0, 0.0, 1.0]);
                        let copy_layer = Layer::new(nle_core::ClipId(0), 0, LayerSource::Solid([0.0; 4]));
                        let id = layer_to_output(&copy_layer.video, [w as f32, h as f32], [w as f32, h as f32]).inverse().expect("identity");
                        let copied = r.composite(enc, &base, &acc.view, id, &copy_layer);
                        r.pool.put(base);
                        match l {
                            Some(layer) => r.draw_layer(enc, copied, layer, src, opts, depth, ctx),
                            None => {
                                // dip/fade to black: the side is black
                                r.fill(enc, &copied, [0.0, 0.0, 0.0, 1.0]);
                                copied
                            }
                        }
                    };
                    let a = side(self, enc, from, &acc, src);
                    let b = side(self, enc, to, &acc, src);
                    let out = self.transition(enc, &a, &b, transition);
                    self.pool.put(a);
                    self.pool.put(b);
                    self.pool.put(acc);
                    acc = out;
                }
            }
        }
        acc
    }

    /// Render a composition into the display texture.
    pub fn render(&mut self, comp: &Composition, src: &mut dyn FrameSource, opts: RenderOptions) {
        self.stats = RenderStats::default();
        let mut enc = self.gpu.device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("rcp-frame") });
        let scale = self.shuttle_proxy_scale.clamp(0.25, 1.0);
        let (orig_w, orig_h) = (self.width, self.height);
        let out = if scale < 0.98 && orig_w >= 64 && orig_h >= 64 {
            let sw = ((orig_w as f32 * scale).round() as u32).max(16) & !1;
            let sh = ((orig_h as f32 * scale).round() as u32).max(16) & !1;
            self.width = sw;
            self.height = sh;
            let small = self.render_comp(&mut enc, comp, src, opts, 0);
            self.width = orig_w;
            self.height = orig_h;
            let bg = self.pool.get(&self.gpu.device, orig_w, orig_h);
            self.fill(&mut enc, &bg, [0.0, 0.0, 0.0, 1.0]);
            let copy_layer = Layer::new(nle_core::ClipId(0), 0, LayerSource::Solid([0.0; 4]));
            let id = layer_to_output(&copy_layer.video, [sw as f32, sh as f32], [orig_w as f32, orig_h as f32]).inverse().expect("identity");
            let upscaled = self.composite(&mut enc, &bg, &small.view, id, &copy_layer);
            self.pool.put(small);
            self.pool.put(bg);
            upscaled
        } else {
            self.render_comp(&mut enc, comp, src, opts, 0)
        };
        let (w, h) = (self.width, self.height);
        self.dispatch(&mut enc, &self.p.to_display, &[entry(0, tv(&out.view)), entry(1, tv(&self.display_storage))], ceil8(w), ceil8(h));
        enc.copy_texture_to_texture(
            self.display.as_image_copy(),
            self.display_srgb_tex.as_image_copy(),
            wgpu::Extent3d { width: w.max(1), height: h.max(1), depth_or_array_layers: 1 },
        );
        self.gpu.queue.submit(Some(enc.finish()));
        self.pool.put(out);
        self.enforce_vram_budget();
    }

    /// Render and read back the frame as packed NV12 (for the encoder).
    /// Width must be a multiple of 4 and height even.
    pub fn render_nv12(&mut self, comp: &Composition, src: &mut dyn FrameSource, opts: RenderOptions) -> Vec<u8> {
        self.stats = RenderStats::default();
        let (w, h) = (self.width, self.height);
        let size = (w as u64 * h as u64 * 3 / 2).next_multiple_of(4);
        let device = self.gpu.device.clone();
        let storage = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("rcp-nv12"),
            size,
            usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_SRC,
            mapped_at_creation: false,
        });
        let readback = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("rcp-nv12-readback"),
            size,
            usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let mut enc = device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("rcp-export") });
        let out = self.render_comp(&mut enc, comp, src, opts, 0);
        let u = self.uniform(&SizeU { width: w, height: h, _pad: [0; 2] });
        self.dispatch(
            &mut enc,
            &self.p.nv12,
            &[entry(0, tv(&out.view)), entry(1, u.as_entire_binding()), entry(2, storage.as_entire_binding())],
            (w / 4).div_ceil(8),
            (h / 2).div_ceil(8),
        );
        enc.copy_buffer_to_buffer(&storage, 0, &readback, 0, size);
        self.gpu.queue.submit(Some(enc.finish()));
        self.pool.put(out);
        self.enforce_vram_budget();
        let slice = readback.slice(..);
        let (tx, rx) = std::sync::mpsc::channel();
        slice.map_async(wgpu::MapMode::Read, move |r| {
            let _ = tx.send(r);
        });
        let _ = device.poll(wgpu::Maintain::Wait);
        let ok = matches!(rx.recv(), Ok(Ok(())));
        let mut data = if ok { slice.get_mapped_range().to_vec() } else { vec![0u8; size as usize] };
        if ok {
            readback.unmap();
        }
        data.truncate((w as usize * h as usize * 3) / 2);
        data
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flow_sizes_and_pyramid() {
        assert_eq!(flow_size(3840, 2160), (960, 540));
        assert_eq!(flow_size(640, 360), (640, 360));
        let p = pyramid_sizes(960, 540);
        assert_eq!(p.first(), Some(&(960, 540)));
        assert!(p.len() >= 4 && p.len() <= 5);
        assert!(p.last().unwrap().1 >= 24);
        assert_eq!(pyramid_sizes(40, 30), vec![(40, 30)]);
    }

    #[test]
    fn noise_is_bounded_and_deterministic() {
        for i in 0..500 {
            let x = i as f32 * 0.037;
            let n = fbm1(x, 7.0, 4);
            assert!((-1.0..=1.0).contains(&n));
            assert_eq!(n, fbm1(x, 7.0, 4));
        }
        let e = nle_core::Effect::new(EffectKind::CameraShake).resolve(nle_core::Time::ZERO);
        let a = shake_transform(&e, 0.5);
        let b = shake_transform(&e, 0.9);
        assert_ne!(a, b, "shake moves over time");
        assert!(a[0].abs() < 0.05 && a[1].abs() < 0.05 && a[3] >= 1.0);
    }
}
