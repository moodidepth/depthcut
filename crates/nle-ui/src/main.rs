//! Rust Cut Pro — a Final Cut Pro-style non-linear editor.
//!
//! eframe/egui UI on the wgpu backend. The preview renderer shares eframe's
//! wgpu device, so rendered frames are shown in the viewer without copies.

// Release builds on Windows open without a console window.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod about_ui;
mod app;
mod browser;
mod captions_ui;
mod copilot_ui;
mod demo;
mod diagnostics;
mod discord;
mod export_ui;
mod ffmpeg_setup;
mod inspector;
mod media;
mod online_ui;
mod presets_ui;
mod shortcuts_ui;
mod theme;
mod timeline_ui;
mod viewer;
mod widgets;

use eframe::egui;
use eframe::egui_wgpu;
use nle_render::wgpu;
use std::sync::Arc;

fn main() -> eframe::Result<()> {
    // log file + panic hook + "previous session crashed" detection
    diagnostics::init("info,wgpu_core=warn,wgpu_hal=warn,naga=warn,eframe=warn,egui_wgpu=warn");
    let budget = nle_media::threads::init_media_pool();
    log::info!("media pool: {} workers, {} threads reserved for UI/audio ({} logical CPUs)", budget.workers, budget.reserved, budget.logical);

    let backends = nle_render::gpu::preferred_backends();
    let setup = egui_wgpu::WgpuSetupCreateNew {
        instance_descriptor: wgpu::InstanceDescriptor { backends, ..Default::default() },
        power_preference: wgpu::PowerPreference::HighPerformance,
        // prefer the Intel Arc A750 when several adapters are present
        native_adapter_selector: Some(Arc::new(|adapters: &[wgpu::Adapter], surface: Option<&wgpu::Surface<'_>>| {
            adapters
                .iter()
                .filter(|a| surface.map_or(true, |s| a.is_surface_supported(s)))
                .max_by_key(|a| nle_render::gpu::adapter_score(&a.get_info()))
                .cloned()
                .ok_or_else(|| "no compatible GPU adapter".to_string())
        })),
        device_descriptor: Arc::new(|adapter| {
            let base_limits = if adapter.get_info().backend == wgpu::Backend::Gl {
                wgpu::Limits::downlevel_webgl2_defaults()
            } else {
                wgpu::Limits::default()
            };
            wgpu::DeviceDescriptor {
                label: Some("egui wgpu device"),
                required_features: nle_render::gpu::wanted_features(adapter),
                required_limits: wgpu::Limits {
                    max_texture_dimension_2d: 8192,
                    ..base_limits
                },
                memory_hints: wgpu::MemoryHints::default(),
            }
        }),
        ..Default::default()
    };
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Rust Cut Pro")
            .with_icon(load_app_icon())
            .with_inner_size([1680.0, 1000.0])
            .with_min_inner_size([1100.0, 700.0])
            .with_drag_and_drop(true),
        renderer: eframe::Renderer::Wgpu,
        wgpu_options: egui_wgpu::WgpuConfiguration {
            wgpu_setup: setup.into(),
            present_mode: wgpu::PresentMode::AutoVsync,
            desired_maximum_frame_latency: Some(1),
            ..Default::default()
        },
        ..Default::default()
    };
    let result = eframe::run_native("Rust Cut Pro", options, Box::new(|cc| Ok(Box::new(app::App::new(cc)))));
    if let Err(e) = &result {
        log::error!("UI failed to start: {e}");
    }
    diagnostics::shutdown();
    result
}

fn load_app_icon() -> Arc<egui::IconData> {
    const ICON_RGBA: &[u8] = include_bytes!("../assets/icon-128.rgba");
    Arc::new(egui::IconData { rgba: ICON_RGBA.to_vec(), width: 128, height: 128 })
}
