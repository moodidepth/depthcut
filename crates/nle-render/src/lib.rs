//! # nle-render
//!
//! Native `wgpu` render engine. Every pixel operation runs in WGSL compute
//! shaders on the GPU:
//!
//! * NV12 / P010 → RGBA conversion (`yuv.wgsl`)
//! * colour management: camera log / HDR / sRGB input transforms, input LUTs,
//!   Rec.709-gamma or ACEScct working space with highlight roll-off
//!   (`colorspace.wgsl`, CPU mirror in [`colormath`])
//! * colour: exposure, white balance, 3-way wheels, contrast, saturation,
//!   `.cube` 3D LUT with trilinear sampling (`grade.wgsl`), then stacked
//!   correction nodes with log wheels, curves, HSL qualifier and shape masks
//!   (`correct.wgsl`); shape-masked effects (`mask.wgsl`)
//! * transforms, crop, Ken Burns, blend modes, opacity (`composite.wgsl`)
//! * transitions: dissolve, dips, whip pan, slide, push, luma wipe
//! * slow-motion frame blending and block-matching motion-compensated
//!   interpolation (`motion.wgsl` + `frameblend.wgsl`)
//! * titles / lower thirds with shadow and pill background (`title.wgsl`)
//! * NV12 packing for hardware export (`nv12.wgsl`, [`export`])
//! * video scopes: waveform, RGB parade, vectorscope, histogram (`scopes.wgsl`, [`scopes`])
//!
//! Decoded frames live in VRAM in an LRU cache capped at 2.5 GB
//! ([`cache::VRAM_FRAME_CACHE_BUDGET`]) for the 8 GB Arc A750.

pub mod cache;
pub mod colormath;
pub mod export;
pub mod gpu;
pub mod lut;
pub mod pipelines;
pub mod reframe;
pub mod renderer;
pub mod scopes;
pub mod text;
pub mod transform;

pub use cache::VRAM_FRAME_CACHE_BUDGET;
pub use export::{ExportPreset, ExportSettings, GamingDeliveryPreset};
pub use gpu::GpuContext;
pub use renderer::{FrameKey, FrameSource, Highlight, RenderOptions, Renderer};
pub use scopes::{ScopeKind, ScopeSettings};
pub use wgpu;
