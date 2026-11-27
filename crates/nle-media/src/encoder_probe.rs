//! Startup hardware-encoder probe.
//!
//! `ffmpeg -encoders` only lists what the build was *compiled* with — an
//! Intel machine still lists `h264_nvenc`. This module runs a tiny real
//! encode per candidate, once, on a background thread. The render GPU's own
//! encoder goes first (NVENC on an NVIDIA card, AMF on AMD, Quick Sync on
//! Intel), then the other vendors' (a laptop's integrated GPU), then
//! `libx264` / `libx265`, so the export dialog can
//! pre-select the fastest encoder that actually works and never blocks the
//! UI. Any failure (no FFmpeg, driver missing, timeout) degrades to the next
//! candidate and finally to an empty, CPU-only result — never a panic.

use crate::ffmpeg_cli;
use crate::hwaccel::{self, GpuVendor, HwCaps, Platform};
use std::process::Stdio;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum EncoderBackend {
    Qsv,
    Nvenc,
    Amf,
    Cpu,
}

impl EncoderBackend {
    pub fn label(self) -> &'static str {
        match self {
            EncoderBackend::Qsv => "Intel Quick Sync",
            EncoderBackend::Nvenc => "NVIDIA NVENC",
            EncoderBackend::Amf => "AMD AMF",
            EncoderBackend::Cpu => "CPU (software)",
        }
    }
}

/// Encoders to test, fastest first.
pub const CANDIDATES: [(&str, EncoderBackend); 11] = [
    ("av1_qsv", EncoderBackend::Qsv),
    ("hevc_qsv", EncoderBackend::Qsv),
    ("h264_qsv", EncoderBackend::Qsv),
    ("av1_nvenc", EncoderBackend::Nvenc),
    ("hevc_nvenc", EncoderBackend::Nvenc),
    ("h264_nvenc", EncoderBackend::Nvenc),
    ("av1_amf", EncoderBackend::Amf),
    ("hevc_amf", EncoderBackend::Amf),
    ("h264_amf", EncoderBackend::Amf),
    ("libx264", EncoderBackend::Cpu),
    ("libx265", EncoderBackend::Cpu),
];

/// Result of the probe: which backend won and every encoder that works.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EncoderProbe {
    /// Backend of the best working encoder (`Cpu` when no GPU encoder works).
    pub backend: EncoderBackend,
    /// Working encoder names in priority order.
    pub encoders: Vec<&'static str>,
}

impl Default for EncoderProbe {
    fn default() -> Self {
        EncoderProbe { backend: EncoderBackend::Cpu, encoders: Vec::new() }
    }
}

impl EncoderProbe {
    pub fn works(&self, encoder: &str) -> bool {
        self.encoders.contains(&encoder)
    }
    /// Fastest working encoder, if any.
    pub fn best(&self) -> Option<&'static str> {
        self.encoders.first().copied()
    }
    pub fn has_hardware(&self) -> bool {
        self.backend != EncoderBackend::Cpu
    }
}

/// Hardware encoder family of a GPU vendor.
pub fn backend_for(vendor: GpuVendor) -> Option<EncoderBackend> {
    match vendor {
        GpuVendor::Intel => Some(EncoderBackend::Qsv),
        GpuVendor::Nvidia => Some(EncoderBackend::Nvenc),
        GpuVendor::Amd => Some(EncoderBackend::Amf),
        GpuVendor::Other => None,
    }
}

/// Candidates with `prefer`'s encoders moved to the front (stable otherwise).
pub fn candidates(prefer: Option<EncoderBackend>) -> Vec<(&'static str, EncoderBackend)> {
    let mut list = CANDIDATES.to_vec();
    list.sort_by_key(|(_, b)| Some(*b) != prefer);
    list
}

/// Run `test` over every candidate the FFmpeg build lists (an empty
/// `listed` set means "unknown — try everything"), in the default order.
pub fn probe_with(listed: &HwCaps, test: impl Fn(&str, EncoderBackend) -> bool) -> EncoderProbe {
    probe_preferring(listed, None, test)
}

/// [`probe_with`], trying `prefer`'s encoders first.
pub fn probe_preferring(listed: &HwCaps, prefer: Option<EncoderBackend>, test: impl Fn(&str, EncoderBackend) -> bool) -> EncoderProbe {
    let encoders: Vec<(&'static str, EncoderBackend)> =
        candidates(prefer).into_iter().filter(|(name, backend)| (listed.encoders.is_empty() || listed.has_encoder(name)) && test(name, *backend)).collect();
    EncoderProbe { backend: encoders.first().map_or(EncoderBackend::Cpu, |(_, b)| *b), encoders: encoders.into_iter().map(|(n, _)| n).collect() }
}

/// FFmpeg arguments for a 3-frame test encode to the null muxer. QSV uses
/// the same device + upload chain as the real export so a pass here means
/// the export path works too.
pub fn test_encode_args(encoder: &str, backend: EncoderBackend, platform: Platform) -> Vec<String> {
    let mut a: Vec<String> = ["-hide_banner", "-loglevel", "error", "-nostdin"].map(String::from).to_vec();
    if backend == EncoderBackend::Qsv {
        a.extend(hwaccel::qsv_device_args(platform));
        a.extend(["-filter_hw_device", "hw"].map(String::from));
    }
    a.extend(["-f", "lavfi", "-i", "color=c=black:s=640x360:r=30", "-frames:v", "3"].map(String::from));
    let vf = match backend {
        EncoderBackend::Qsv => format!("format=nv12,{}", hwaccel::qsv_upload_filter(platform)),
        EncoderBackend::Cpu => "format=yuv420p".to_string(),
        _ => "format=nv12".to_string(),
    };
    a.extend(["-vf".to_string(), vf, "-c:v".to_string(), encoder.to_string()]);
    a.extend(["-f", "null", "-"].map(String::from));
    a
}

/// Whether a real encode with `encoder` succeeds. Kills FFmpeg after
/// `timeout` (a hung driver must not stall the probe thread forever).
pub fn test_encoder(encoder: &str, backend: EncoderBackend, timeout: Duration) -> bool {
    let args = test_encode_args(encoder, backend, Platform::current());
    let child = ffmpeg_cli::command(&ffmpeg_cli::ffmpeg_path()).args(&args).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).spawn();
    let Ok(mut child) = child else { return false };
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return status.success(),
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(20)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return false;
            }
        }
    }
}

static PROBE: OnceLock<EncoderProbe> = OnceLock::new();

/// Blocking probe (cached). Call from a worker thread, never the UI thread.
pub fn probe_blocking() -> &'static EncoderProbe {
    PROBE.get_or_init(|| {
        if !ffmpeg_cli::available() {
            log::warn!("encoder probe: FFmpeg not available — export disabled");
            return EncoderProbe::default();
        }
        let started = Instant::now();
        let prefer = backend_for(hwaccel::render_vendor());
        let probe = probe_preferring(hwaccel::probe_caps(), prefer, |name, backend| test_encoder(name, backend, Duration::from_secs(20)));
        log::info!("encoder probe: {} — {:?} ({} ms)", probe.backend.label(), probe.encoders, started.elapsed().as_millis());
        probe
    })
}

/// Start the probe on a background thread (idempotent). If the thread cannot
/// be spawned the probe simply runs lazily on first [`probe_blocking`] call.
pub fn start_background_probe() {
    if PROBE.get().is_some() {
        return;
    }
    if let Err(e) = std::thread::Builder::new().name("rcp-encoder-probe".into()).spawn(|| {
        probe_blocking();
    }) {
        log::warn!("encoder probe thread: {e}");
    }
}

/// The probe result, or `None` while it is still running.
pub fn result() -> Option<&'static EncoderProbe> {
    PROBE.get()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn caps(list: &[&str]) -> HwCaps {
        let mut c = HwCaps::default();
        c.encoders.extend(list.iter().map(|s| s.to_string()));
        c
    }

    #[test]
    fn priority_is_qsv_nvenc_amf_cpu() {
        let all = probe_with(&HwCaps::default(), |_, _| true);
        assert_eq!(all.backend, EncoderBackend::Qsv);
        assert_eq!(all.best(), Some("av1_qsv"));
        assert_eq!(all.encoders.len(), CANDIDATES.len());
        assert_eq!(all.encoders.last(), Some(&"libx265"));

        let nvidia = probe_with(&HwCaps::default(), |_, b| matches!(b, EncoderBackend::Nvenc | EncoderBackend::Cpu));
        assert_eq!(nvidia.backend, EncoderBackend::Nvenc);
        assert_eq!(nvidia.encoders, ["av1_nvenc", "hevc_nvenc", "h264_nvenc", "libx264", "libx265"]);

        let amd = probe_with(&HwCaps::default(), |n, _| n.ends_with("_amf") && n != "av1_amf");
        assert_eq!((amd.backend, amd.best()), (EncoderBackend::Amf, Some("hevc_amf")));
    }

    #[test]
    fn render_gpu_vendor_goes_first() {
        // NVIDIA card in a machine whose Intel iGPU also encodes
        let both = |_: &str, b| matches!(b, EncoderBackend::Qsv | EncoderBackend::Nvenc | EncoderBackend::Cpu);
        let p = probe_preferring(&HwCaps::default(), backend_for(GpuVendor::Nvidia), both);
        assert_eq!(p.backend, EncoderBackend::Nvenc);
        assert_eq!(p.encoders, ["av1_nvenc", "hevc_nvenc", "h264_nvenc", "av1_qsv", "hevc_qsv", "h264_qsv", "libx264", "libx265"]);
        let p = probe_preferring(&HwCaps::default(), backend_for(GpuVendor::Amd), |_, _| true);
        assert_eq!(p.encoders[..3], ["av1_amf", "hevc_amf", "h264_amf"]);
        assert_eq!(p.encoders[3], "av1_qsv");
        // unknown vendor: default order
        assert_eq!(probe_preferring(&HwCaps::default(), backend_for(GpuVendor::Other), |_, _| true).best(), Some("av1_qsv"));
    }

    #[test]
    fn degrades_to_cpu_then_to_nothing() {
        let cpu = probe_with(&HwCaps::default(), |_, b| b == EncoderBackend::Cpu);
        assert_eq!(cpu.backend, EncoderBackend::Cpu);
        assert_eq!(cpu.encoders, ["libx264", "libx265"]);
        assert!(!cpu.has_hardware() && cpu.works("libx264") && !cpu.works("h264_qsv"));

        let none = probe_with(&HwCaps::default(), |_, _| false);
        assert_eq!(none, EncoderProbe::default());
        assert_eq!(none.best(), None);
    }

    #[test]
    fn encoders_missing_from_the_build_are_never_run() {
        let listed = caps(&["h264_nvenc", "libx264"]);
        let tested = std::cell::RefCell::new(Vec::new());
        let p = probe_with(&listed, |n, _| {
            tested.borrow_mut().push(n.to_string());
            n == "libx264"
        });
        assert_eq!(*tested.borrow(), ["h264_nvenc", "libx264"]);
        assert_eq!(p.backend, EncoderBackend::Cpu);
    }

    #[test]
    fn qsv_test_uses_export_device_chain() {
        let a = test_encode_args("hevc_qsv", EncoderBackend::Qsv, Platform::Windows).join(" ");
        assert!(a.contains("-init_hw_device") && a.contains("hwupload") && a.contains("-c:v hevc_qsv") && a.ends_with("-f null -"));
        let n = test_encode_args("h264_nvenc", EncoderBackend::Nvenc, Platform::Windows).join(" ");
        assert!(!n.contains("init_hw_device") && n.contains("format=nv12 -c:v h264_nvenc"));
    }
}
