//! Compute pipelines and their bind-group layouts.

pub const WORKING_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba16Float;
pub const DISPLAY_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8Unorm;

pub const SHADER_YUV: &str = include_str!("shaders/yuv.wgsl");
/// Shared colour-space helpers, prepended to the grade and correction shaders.
pub const SHADER_COLORSPACE: &str = include_str!("shaders/colorspace.wgsl");
pub const SHADER_GRADE: &str = concat!(include_str!("shaders/colorspace.wgsl"), include_str!("shaders/grade.wgsl"));
pub const SHADER_CORRECT: &str = concat!(include_str!("shaders/colorspace.wgsl"), include_str!("shaders/mask_common.wgsl"), include_str!("shaders/correct.wgsl"));
pub const SHADER_MASK: &str = concat!(include_str!("shaders/mask_common.wgsl"), include_str!("shaders/mask.wgsl"));
pub const SHADER_COMPOSITE: &str = include_str!("shaders/composite.wgsl");
pub const SHADER_TRANSITION: &str = include_str!("shaders/transition.wgsl");
pub const SHADER_OPTICAL_FLOW: &str = include_str!("shaders/optical_flow.wgsl");
pub const SHADER_INTERPOLATE: &str = include_str!("shaders/interpolate_frame.wgsl");
pub const SHADER_MOTION_BLUR: &str = include_str!("shaders/motion_blur.wgsl");
pub const SHADER_EFFECTS: &str = include_str!("shaders/effects.wgsl");
pub const SHADER_FRAMEBLEND: &str = include_str!("shaders/frameblend.wgsl");
pub const SHADER_TITLE: &str = include_str!("shaders/title.wgsl");
pub const SHADER_OUTPUT: &str = include_str!("shaders/output.wgsl");
pub const SHADER_NV12: &str = include_str!("shaders/nv12.wgsl");
pub const SHADER_PACK: &str = include_str!("shaders/pack.wgsl");
pub const SHADER_FILL: &str = include_str!("shaders/fill.wgsl");
pub const SHADER_SCOPES: &str = include_str!("shaders/scopes.wgsl");
pub const SHADER_WARP: &str = include_str!("shaders/warp.wgsl");

/// Every shader with its entry point (used by the WGSL validation test).
pub const ALL_SHADERS: &[(&str, &str, &str)] = &[
    ("yuv", SHADER_YUV, "main"),
    ("grade", SHADER_GRADE, "main"),
    ("composite", SHADER_COMPOSITE, "main"),
    ("transition", SHADER_TRANSITION, "main"),
    ("optical_flow", SHADER_OPTICAL_FLOW, "lk_iter"),
    ("interpolate_frame", SHADER_INTERPOLATE, "main"),
    ("motion_blur", SHADER_MOTION_BLUR, "main"),
    ("effects", SHADER_EFFECTS, "chroma_key"),
    ("frameblend", SHADER_FRAMEBLEND, "main"),
    ("title", SHADER_TITLE, "main"),
    ("output", SHADER_OUTPUT, "to_display"),
    ("nv12", SHADER_NV12, "main"),
    ("pack_p010", SHADER_PACK, "p010"),
    ("pack_rgba64", SHADER_PACK, "rgba64"),
    ("fill", SHADER_FILL, "main"),
    ("scopes", SHADER_SCOPES, "accumulate"),
    ("correct", SHADER_CORRECT, "main"),
    ("mask", SHADER_MASK, "mask_mix"),
    ("warp", SHADER_WARP, "main"),
    ("optical_flow_median", SHADER_OPTICAL_FLOW, "vec_median"),
];

/// Every compute entry point of `scopes.wgsl`.
pub const SCOPE_ENTRY_POINTS: &[&str] = &["clear", "accumulate", "hist_max", "draw_waveform", "draw_parade", "draw_vector", "draw_histogram"];

/// Every compute entry point of `effects.wgsl` the renderer builds a pipeline
/// for (validated individually so a typo in a new effect fails the test).
pub const EFFECT_ENTRY_POINTS: &[&str] = &[
    "bright_pass",
    "blur_h",
    "blur_v",
    "combine",
    "shake",
    "film_grain",
    "directional_blur",
    "zoom_blur",
    "chroma_key",
    "rgb_split_glitch",
    "impact_flash",
    "neon_outline",
    "gaming_reframe_916",
    "downsample",
    "gblur_h",
    "gblur_v",
    "radial_blur",
    "vignette",
    "sharpen",
    "pixelate",
    "lens_distortion",
    "hue_sat_lightness",
    "levels",
    "person_matte",
    "depth_map",
];

#[derive(Clone, Copy)]
enum B {
    Tex,
    Tex3d,
    Samp,
    Uniform,
    StorageRo,
    StorageRw,
    Store(wgpu::TextureFormat),
}

fn layout(device: &wgpu::Device, label: &str, spec: &[B]) -> wgpu::BindGroupLayout {
    let entries: Vec<wgpu::BindGroupLayoutEntry> = spec
        .iter()
        .enumerate()
        .map(|(i, b)| wgpu::BindGroupLayoutEntry {
            binding: i as u32,
            visibility: wgpu::ShaderStages::COMPUTE,
            ty: match *b {
                B::Tex => wgpu::BindingType::Texture {
                    sample_type: wgpu::TextureSampleType::Float { filterable: true },
                    view_dimension: wgpu::TextureViewDimension::D2,
                    multisampled: false,
                },
                B::Tex3d => wgpu::BindingType::Texture {
                    sample_type: wgpu::TextureSampleType::Float { filterable: true },
                    view_dimension: wgpu::TextureViewDimension::D3,
                    multisampled: false,
                },
                B::Samp => wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                B::Uniform => wgpu::BindingType::Buffer { ty: wgpu::BufferBindingType::Uniform, has_dynamic_offset: false, min_binding_size: None },
                B::StorageRo => wgpu::BindingType::Buffer { ty: wgpu::BufferBindingType::Storage { read_only: true }, has_dynamic_offset: false, min_binding_size: None },
                B::StorageRw => wgpu::BindingType::Buffer { ty: wgpu::BufferBindingType::Storage { read_only: false }, has_dynamic_offset: false, min_binding_size: None },
                B::Store(format) => wgpu::BindingType::StorageTexture { access: wgpu::StorageTextureAccess::WriteOnly, format, view_dimension: wgpu::TextureViewDimension::D2 },
            },
            count: None,
        })
        .collect();
    device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor { label: Some(label), entries: &entries })
}

pub struct Pipe {
    pub pipeline: wgpu::ComputePipeline,
    pub layout: wgpu::BindGroupLayout,
}

fn module(device: &wgpu::Device, label: &str, src: &str) -> wgpu::ShaderModule {
    device.create_shader_module(wgpu::ShaderModuleDescriptor { label: Some(label), source: wgpu::ShaderSource::Wgsl(src.into()) })
}

fn pipe(device: &wgpu::Device, cache: Option<&wgpu::PipelineCache>, label: &str, src: &str, entry: &str, spec: &[B]) -> Pipe {
    pipe_m(device, cache, label, &module(device, label, src), entry, spec)
}

fn pipe_m(device: &wgpu::Device, cache: Option<&wgpu::PipelineCache>, label: &str, module: &wgpu::ShaderModule, entry: &str, spec: &[B]) -> Pipe {
    let bgl = layout(device, label, spec);
    let pl = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor { label: Some(label), bind_group_layouts: &[&bgl], push_constant_ranges: &[] });
    let pipeline = device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
        label: Some(label),
        layout: Some(&pl),
        module,
        entry_point: Some(entry),
        compilation_options: wgpu::PipelineCompilationOptions::default(),
        cache,
    });
    Pipe { pipeline, layout: bgl }
}

pub struct Pipelines {
    pub yuv: Pipe,
    pub grade: Pipe,
    /// One stacked colour correction node.
    pub correct: Pipe,
    /// Shape-masked mix of a processed image over the original (and the wipe after nodes).
    pub mask_mix: Pipe,
    pub composite: Pipe,
    pub transition: Pipe,
    /// Optical flow passes: luma pyramid, block matching, upsampling, Lucas-Kanade.
    pub flow_luma: Pipe,
    pub flow_down: Pipe,
    pub flow_init: Pipe,
    pub flow_up: Pipe,
    pub flow_lk: Pipe,
    pub interpolate: Pipe,
    /// 3×3 vector-median filter of a flow field (Speed Warp).
    pub flow_median: Pipe,
    /// Projective warp (stabilization).
    pub warp: Pipe,
    pub motion_blur: Pipe,
    pub fx_bright: Pipe,
    pub fx_blur_h: Pipe,
    pub fx_blur_v: Pipe,
    pub fx_combine: Pipe,
    pub fx_shake: Pipe,
    pub fx_grain: Pipe,
    pub fx_dir_blur: Pipe,
    pub fx_zoom_blur: Pipe,
    pub fx_chroma_key: Pipe,
    pub fx_rgb_split_glitch: Pipe,
    pub fx_impact_flash: Pipe,
    pub fx_neon_outline: Pipe,
    pub fx_gaming_reframe_916: Pipe,
    pub fx_downsample: Pipe,
    pub fx_gblur_h: Pipe,
    pub fx_gblur_v: Pipe,
    pub fx_radial_blur: Pipe,
    pub fx_vignette: Pipe,
    pub fx_sharpen: Pipe,
    pub fx_pixelate: Pipe,
    pub fx_lens_distortion: Pipe,
    pub fx_hue_sat_lightness: Pipe,
    pub fx_levels: Pipe,
    pub fx_person_matte: Pipe,
    pub fx_depth_map: Pipe,
    pub flow_format: wgpu::TextureFormat,
    pub frameblend: Pipe,
    pub title: Pipe,
    pub to_display: Pipe,
    pub nv12: Pipe,
    pub pack_p010: Pipe,
    pub pack_rgba64: Pipe,
    pub fill: Pipe,
    /// Video scopes: clear, accumulate, histogram max, then one draw pass per scope.
    pub scope_clear: Pipe,
    pub scope_accumulate: Pipe,
    pub scope_hist_max: Pipe,
    pub scope_waveform: Pipe,
    pub scope_parade: Pipe,
    pub scope_vector: Pipe,
    pub scope_histogram: Pipe,
}

impl Pipelines {
    /// `flow_format` is the optical-flow storage format (`Rg16Float` when
    /// supported, else `Rgba16Float`; the WGSL is rewritten to match).
    pub fn new(device: &wgpu::Device, flow_format: wgpu::TextureFormat, cache: Option<&wgpu::PipelineCache>) -> Self {
        let w = B::Store(WORKING_FORMAT);
        let flow_src = if flow_format == wgpu::TextureFormat::Rg16Float { SHADER_OPTICAL_FLOW.to_string() } else { SHADER_OPTICAL_FLOW.replace("rg16float", "rgba16float") };
        let flow = module(device, "optical_flow", &flow_src);
        let fspec = [B::Tex, B::Tex, B::Tex, B::Samp, B::Uniform, B::Store(flow_format)];
        let fx = module(device, "effects", SHADER_EFFECTS);
        let xspec = [B::Tex, B::Tex, B::Samp, B::Uniform, w];
        let sc = module(device, "scopes", SHADER_SCOPES);
        let sspec = [B::Tex, B::Uniform, B::StorageRw, B::Store(DISPLAY_FORMAT)];
        Pipelines {
            flow_luma: pipe_m(device, cache, "flow_luma", &flow, "luma", &fspec),
            flow_down: pipe_m(device, cache, "flow_down", &flow, "downsample", &fspec),
            flow_init: pipe_m(device, cache, "flow_init", &flow, "block_init", &fspec),
            flow_up: pipe_m(device, cache, "flow_up", &flow, "upsample", &fspec),
            flow_lk: pipe_m(device, cache, "flow_lk", &flow, "lk_iter", &fspec),
            interpolate: pipe(device, cache, "interpolate_frame", SHADER_INTERPOLATE, "main", &[B::Tex, B::Tex, B::Tex, B::Tex, B::Samp, B::Uniform, w]),
            flow_median: pipe_m(device, cache, "flow_median", &flow, "vec_median", &fspec),
            warp: pipe(device, cache, "warp", SHADER_WARP, "main", &[B::Tex, B::Samp, B::Uniform, w]),
            motion_blur: pipe(device, cache, "motion_blur", SHADER_MOTION_BLUR, "main", &xspec),
            fx_bright: pipe_m(device, cache, "fx_bright", &fx, "bright_pass", &xspec),
            fx_blur_h: pipe_m(device, cache, "fx_blur_h", &fx, "blur_h", &xspec),
            fx_blur_v: pipe_m(device, cache, "fx_blur_v", &fx, "blur_v", &xspec),
            fx_combine: pipe_m(device, cache, "fx_combine", &fx, "combine", &xspec),
            fx_shake: pipe_m(device, cache, "fx_shake", &fx, "shake", &xspec),
            fx_grain: pipe_m(device, cache, "fx_grain", &fx, "film_grain", &xspec),
            fx_dir_blur: pipe_m(device, cache, "fx_dir_blur", &fx, "directional_blur", &xspec),
            fx_zoom_blur: pipe_m(device, cache, "fx_zoom_blur", &fx, "zoom_blur", &xspec),
            fx_chroma_key: pipe_m(device, cache, "fx_chroma_key", &fx, "chroma_key", &xspec),
            fx_rgb_split_glitch: pipe_m(device, cache, "fx_rgb_split_glitch", &fx, "rgb_split_glitch", &xspec),
            fx_impact_flash: pipe_m(device, cache, "fx_impact_flash", &fx, "impact_flash", &xspec),
            fx_neon_outline: pipe_m(device, cache, "fx_neon_outline", &fx, "neon_outline", &xspec),
            fx_gaming_reframe_916: pipe_m(device, cache, "fx_gaming_reframe_916", &fx, "gaming_reframe_916", &xspec),
            fx_downsample: pipe_m(device, cache, "fx_downsample", &fx, "downsample", &xspec),
            fx_gblur_h: pipe_m(device, cache, "fx_gblur_h", &fx, "gblur_h", &xspec),
            fx_gblur_v: pipe_m(device, cache, "fx_gblur_v", &fx, "gblur_v", &xspec),
            fx_radial_blur: pipe_m(device, cache, "fx_radial_blur", &fx, "radial_blur", &xspec),
            fx_vignette: pipe_m(device, cache, "fx_vignette", &fx, "vignette", &xspec),
            fx_sharpen: pipe_m(device, cache, "fx_sharpen", &fx, "sharpen", &xspec),
            fx_pixelate: pipe_m(device, cache, "fx_pixelate", &fx, "pixelate", &xspec),
            fx_lens_distortion: pipe_m(device, cache, "fx_lens_distortion", &fx, "lens_distortion", &xspec),
            fx_hue_sat_lightness: pipe_m(device, cache, "fx_hue_sat_lightness", &fx, "hue_sat_lightness", &xspec),
            fx_levels: pipe_m(device, cache, "fx_levels", &fx, "levels", &xspec),
            fx_person_matte: pipe_m(device, cache, "fx_person_matte", &fx, "person_matte", &xspec),
            fx_depth_map: pipe_m(device, cache, "fx_depth_map", &fx, "depth_map", &xspec),
            flow_format,
            yuv: pipe(device, cache, "yuv", SHADER_YUV, "main", &[B::StorageRo, B::Uniform, w]),
            grade: pipe(device, cache, "grade", SHADER_GRADE, "main", &[B::Tex, B::Tex3d, B::Samp, B::Uniform, w, B::Tex3d]),
            correct: pipe(device, cache, "correct", SHADER_CORRECT, "main", &[B::Tex, B::Uniform, B::StorageRo, w]),
            mask_mix: pipe(device, cache, "mask_mix", SHADER_MASK, "mask_mix", &[B::Tex, B::Tex, B::Uniform, w]),
            composite: pipe(device, cache, "composite", SHADER_COMPOSITE, "main", &[B::Tex, B::Tex, B::Samp, B::Uniform, w]),
            transition: pipe(device, cache, "transition", SHADER_TRANSITION, "main", &[B::Tex, B::Tex, B::Samp, B::Uniform, w]),
            frameblend: pipe(device, cache, "frameblend", SHADER_FRAMEBLEND, "main", &[B::Tex, B::Tex, B::Tex, B::Samp, B::Uniform, w]),
            title: pipe(device, cache, "title", SHADER_TITLE, "main", &[B::Tex, B::Samp, B::Uniform, w]),
            to_display: pipe(device, cache, "to_display", SHADER_OUTPUT, "to_display", &[B::Tex, B::Store(DISPLAY_FORMAT)]),
            nv12: pipe(device, cache, "nv12", SHADER_NV12, "main", &[B::Tex, B::Uniform, B::StorageRw]),
            pack_p010: pipe(device, cache, "pack_p010", SHADER_PACK, "p010", &[B::Tex, B::Uniform, B::StorageRw]),
            pack_rgba64: pipe(device, cache, "pack_rgba64", SHADER_PACK, "rgba64", &[B::Tex, B::Uniform, B::StorageRw]),
            fill: pipe(device, cache, "fill", SHADER_FILL, "main", &[B::Uniform, w]),
            scope_clear: pipe_m(device, cache, "scope_clear", &sc, "clear", &sspec),
            scope_accumulate: pipe_m(device, cache, "scope_accumulate", &sc, "accumulate", &sspec),
            scope_hist_max: pipe_m(device, cache, "scope_hist_max", &sc, "hist_max", &sspec),
            scope_waveform: pipe_m(device, cache, "scope_waveform", &sc, "draw_waveform", &sspec),
            scope_parade: pipe_m(device, cache, "scope_parade", &sc, "draw_parade", &sspec),
            scope_vector: pipe_m(device, cache, "scope_vector", &sc, "draw_vector", &sspec),
            scope_histogram: pipe_m(device, cache, "scope_histogram", &sc, "draw_histogram", &sspec),
        }
    }
}
