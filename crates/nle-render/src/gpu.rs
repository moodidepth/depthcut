//! wgpu device selection: prefer the Intel Arc (vendor 0x8086, discrete)
//! on Vulkan (Linux) or DX12/Vulkan (Windows).

use std::sync::Arc;
use thiserror::Error;

pub const INTEL_VENDOR_ID: u32 = 0x8086;

#[derive(Debug, Error)]
pub enum GpuError {
    #[error("no suitable GPU adapter found")]
    NoAdapter,
    #[error("device request failed: {0}")]
    Device(#[from] wgpu::RequestDeviceError),
}

/// Shared device + queue. Cloning is cheap.
#[derive(Clone)]
pub struct GpuContext {
    pub device: Arc<wgpu::Device>,
    pub queue: Arc<wgpu::Queue>,
    pub info: wgpu::AdapterInfo,
    /// Storage format of optical-flow textures: `Rg16Float` when the adapter
    /// can write it from compute shaders, otherwise `Rgba16Float`.
    pub flow_format: wgpu::TextureFormat,
    /// Persistent GPU pipeline cache stored at `%LOCALAPPDATA%/Argon/shader_cache.bin`.
    pub pipeline_cache: Option<Arc<wgpu::PipelineCache>>,
}

/// Device features worth requesting: adapter-specific format features let
/// compute shaders write `Rg16Float` motion vectors (half the bandwidth),
/// and `PIPELINE_CACHE` enables persistent shader compilation caching.
pub fn wanted_features(adapter: &wgpu::Adapter) -> wgpu::Features {
    adapter.features() & (wgpu::Features::TEXTURE_ADAPTER_SPECIFIC_FORMAT_FEATURES | wgpu::Features::PIPELINE_CACHE)
}

/// Load or initialize the persistent wgpu pipeline cache from `%LOCALAPPDATA%/Argon/shader_cache.bin`.
pub fn load_or_create_pipeline_cache(device: &wgpu::Device) -> Option<Arc<wgpu::PipelineCache>> {
    if !device.features().contains(wgpu::Features::PIPELINE_CACHE) {
        return None;
    }
    let path = nle_media::paths::shader_cache_path();
    let bytes = std::fs::read(&path).ok();
    let cache = unsafe { device.create_pipeline_cache(&wgpu::PipelineCacheDescriptor { label: Some("argon-shader-cache"), data: bytes.as_deref(), fallback: true }) };
    Some(Arc::new(cache))
}

/// The best flow-texture format the device + adapter support.
pub fn flow_format_for(device: &wgpu::Device, adapter: Option<&wgpu::Adapter>) -> wgpu::TextureFormat {
    let ok = adapter.is_some_and(|a| {
        device.features().contains(wgpu::Features::TEXTURE_ADAPTER_SPECIFIC_FORMAT_FEATURES)
            && a.get_texture_format_features(wgpu::TextureFormat::Rg16Float).allowed_usages.contains(wgpu::TextureUsages::STORAGE_BINDING)
    });
    if ok && std::env::var_os("RCP_NO_RG16").is_none() {
        wgpu::TextureFormat::Rg16Float
    } else {
        wgpu::TextureFormat::Rgba16Float
    }
}

/// Backends to try on this OS, in priority order. Override with
/// `RCP_WGPU_BACKEND=vulkan|dx12|gl`.
pub fn preferred_backends() -> wgpu::Backends {
    if let Ok(b) = std::env::var("RCP_WGPU_BACKEND") {
        match b.to_ascii_lowercase().as_str() {
            "vulkan" | "vk" => return wgpu::Backends::VULKAN,
            "dx12" | "d3d12" => return wgpu::Backends::DX12,
            "gl" | "opengl" => return wgpu::Backends::GL,
            _ => {}
        }
    }
    if cfg!(target_os = "windows") {
        wgpu::Backends::DX12 | wgpu::Backends::VULKAN
    } else if cfg!(target_os = "macos") {
        wgpu::Backends::METAL
    } else {
        wgpu::Backends::VULKAN
    }
}

/// Score adapters: discrete Intel Arc > other discrete > integrated > rest.
pub fn adapter_score(info: &wgpu::AdapterInfo) -> i32 {
    let mut s = match info.device_type {
        wgpu::DeviceType::DiscreteGpu => 100,
        wgpu::DeviceType::IntegratedGpu => 50,
        wgpu::DeviceType::VirtualGpu => 20,
        wgpu::DeviceType::Cpu => 1,
        wgpu::DeviceType::Other => 10,
    };
    if info.vendor == INTEL_VENDOR_ID {
        s += 40;
    }
    if info.name.to_ascii_lowercase().contains("arc") {
        s += 40;
    }
    match info.backend {
        wgpu::Backend::Dx12 if cfg!(target_os = "windows") => s += 5,
        wgpu::Backend::Vulkan => s += 3,
        _ => {}
    }
    s
}

/// Pick the best adapter from a list (used by the UI's adapter selector too).
pub fn pick_adapter(adapters: Vec<wgpu::Adapter>) -> Option<wgpu::Adapter> {
    adapters.into_iter().max_by_key(|a| adapter_score(&a.get_info()))
}

impl GpuContext {
    /// Wrap an existing device (e.g. eframe's), so textures can be shared
    /// with the UI without copies.
    pub fn from_existing(device: Arc<wgpu::Device>, queue: Arc<wgpu::Queue>, info: wgpu::AdapterInfo) -> Self {
        let pipeline_cache = load_or_create_pipeline_cache(&device);
        GpuContext { device, queue, info, flow_format: wgpu::TextureFormat::Rgba16Float, pipeline_cache }
    }

    /// Detect adapter-specific capabilities (`Rg16Float` storage for flow).
    pub fn with_adapter(mut self, adapter: &wgpu::Adapter) -> Self {
        self.flow_format = flow_format_for(&self.device, Some(adapter));
        self
    }

    /// Create a standalone (headless) device, preferring the Intel Arc.
    pub fn new_headless() -> Result<Self, GpuError> {
        let backends = preferred_backends();
        let instance = wgpu::Instance::new(&wgpu::InstanceDescriptor { backends, ..Default::default() });
        let adapter = pick_adapter(instance.enumerate_adapters(backends)).ok_or(GpuError::NoAdapter)?;
        let info = adapter.get_info();
        log::info!("render device: {} ({:?}, {:?})", info.name, info.backend, info.device_type);
        let (device, queue) = pollster::block_on(adapter.request_device(
            &wgpu::DeviceDescriptor {
                label: Some("rcp-render"),
                required_features: wanted_features(&adapter),
                required_limits: wgpu::Limits::default(),
                memory_hints: wgpu::MemoryHints::Performance,
            },
            None,
        ))?;
        let flow_format = flow_format_for(&device, Some(&adapter));
        let pipeline_cache = load_or_create_pipeline_cache(&device);
        log::info!("optical flow format: {flow_format:?}");
        Ok(GpuContext { device: Arc::new(device), queue: Arc::new(queue), info, flow_format, pipeline_cache })
    }

    /// Persist the GPU pipeline cache to `%LOCALAPPDATA%/Argon/shader_cache.bin`.
    pub fn save_pipeline_cache(&self) {
        let path = nle_media::paths::shader_cache_path();
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Some(cache) = &self.pipeline_cache {
            if let Some(data) = cache.get_data() {
                let _ = std::fs::write(&path, data);
                return;
            }
        }
        // Fallback cache stamp when the backend does not expose raw PSO blobs.
        let stamp = format!("ARGON-WGPU-{}-{:?}", self.info.name, self.info.backend);
        let _ = std::fs::write(&path, stamp.as_bytes());
    }

    pub fn is_intel_arc(&self) -> bool {
        self.info.vendor == INTEL_VENDOR_ID && self.info.name.to_ascii_lowercase().contains("arc")
    }
}
