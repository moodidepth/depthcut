//! Background proxy-media transcoder.
//!
//! Proxies are half-resolution (max 1080p, typically 960×540 for HD and
//! 1920×1080 for 4K) H.264 files encoded on the GPU's media engine: the
//! render GPU's encoder first (`h264_nvenc`, `h264_amf` or `h264_qsv`), then
//! the other vendors', `h264_vaapi` on Linux, and `libx264` last. On Windows
//! the source is decoded through D3D11VA. Jobs run on the media pool (worker
//! cores only) and report progress via `-progress pipe:1`.

use crate::encoder_probe::{self, EncoderBackend};
use crate::ffmpeg_cli;
use crate::hwaccel::{self, GpuVendor, Platform};
use crate::MediaError;
use nle_core::{AssetId, MediaAsset};
use parking_lot::Mutex;
use std::collections::HashMap;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;

#[derive(Clone, Debug, PartialEq)]
pub enum ProxyStatus {
    Queued,
    Running { progress: f32, encoder: String },
    Done(PathBuf),
    Failed(String),
}

/// Encoder choices for proxies, best first: (encoder, input-side args, output args).
/// `vendor` is the render GPU's; its encoder goes first.
pub fn proxy_encoder_args(platform: Platform, caps: &hwaccel::HwCaps, vendor: GpuVendor) -> Vec<(String, Vec<String>, Vec<String>)> {
    // Windows: decode the source on the GPU too (FFmpeg falls back to
    // software when it can't); frames come back to system memory for scaling
    let d3d = |extra: &[&str]| -> Vec<String> {
        if platform == Platform::Windows && caps.has_hwaccel("d3d11va") {
            let mut a: Vec<String> = ["-hwaccel", "d3d11va"].map(String::from).to_vec();
            a.extend(extra.iter().map(|s| s.to_string()));
            a
        } else {
            Vec::new()
        }
    };
    let mut hw: Vec<(EncoderBackend, String, Vec<String>, Vec<String>)> = Vec::new();
    if caps.has_encoder("h264_qsv") {
        let mut pre = hwaccel::qsv_device_args(platform);
        pre.extend(["-filter_hw_device".into(), "hw".into()]);
        // decode on the same Intel device the encoder uses
        pre.extend(d3d(&["-hwaccel_device", "dx"]));
        hw.push((EncoderBackend::Qsv, "h264_qsv".into(), pre, ["-c:v", "h264_qsv", "-preset", "veryfast", "-global_quality", "26", "-look_ahead", "0"].map(String::from).to_vec()));
    }
    if caps.has_encoder("h264_nvenc") {
        hw.push((EncoderBackend::Nvenc, "h264_nvenc".into(), d3d(&[]), ["-c:v", "h264_nvenc", "-preset", "p2", "-rc", "vbr", "-cq", "26", "-b:v", "0"].map(String::from).to_vec()));
    }
    if caps.has_encoder("h264_amf") {
        hw.push((
            EncoderBackend::Amf,
            "h264_amf".into(),
            d3d(&[]),
            ["-c:v", "h264_amf", "-quality", "speed", "-rc", "cqp", "-qp_i", "24", "-qp_p", "26", "-qp_b", "28"].map(String::from).to_vec(),
        ));
    }
    let prefer = encoder_probe::backend_for(vendor);
    hw.sort_by_key(|(b, ..)| Some(*b) != prefer);
    let mut v: Vec<(String, Vec<String>, Vec<String>)> = hw.into_iter().map(|(_, n, p, a)| (n, p, a)).collect();
    if platform == Platform::Linux && vendor != GpuVendor::Nvidia && caps.has_encoder("h264_vaapi") {
        v.push(("h264_vaapi".into(), ["-vaapi_device", "/dev/dri/renderD128"].map(String::from).to_vec(), ["-c:v", "h264_vaapi", "-qp", "26"].map(String::from).to_vec()));
    }
    let threads = crate::threads::budget().ffmpeg_threads().to_string();
    v.push(("libx264".into(), d3d(&[]), ["-c:v", "libx264", "-preset", "veryfast", "-crf", "23", "-threads", &threads].map(String::from).to_vec()));
    v
}

/// Whether the start-up encoder probe ruled `encoder` out (unknown = keep).
fn probe_rejects(encoder: &str) -> bool {
    let probed = encoder_probe::CANDIDATES.iter().any(|(n, _)| *n == encoder);
    probed && encoder_probe::result().is_some_and(|p| !p.works(encoder))
}

/// Target proxy dimensions (half size, max 1920×1080, even).
pub fn proxy_size(w: u32, h: u32) -> (u32, u32) {
    let (mut pw, mut ph) = (w / 2, h / 2);
    if ph > 1080 {
        pw = pw * 1080 / ph;
        ph = 1080;
    }
    (pw.max(2) & !1, ph.max(2) & !1)
}

pub fn proxy_path_for(asset: &MediaAsset, dir: &Path) -> PathBuf {
    let stem = asset.path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| asset.name.clone());
    let ext = if asset.video.as_ref().is_some_and(|v| v.alpha()) { "mov" } else { "mp4" };
    dir.join(format!("{stem}_proxy.{ext}"))
}

/// A proxy at `path` carries alpha: alpha media get ProRes 4444 `.mov`
/// proxies, everything else (and every proxy made before alpha support) is
/// 4:2:0 H.264 `.mp4`.
pub fn proxy_keeps_alpha(path: &Path) -> bool {
    path.extension().is_some_and(|e| e.eq_ignore_ascii_case("mov"))
}

/// The one encoder for the proxy of an alpha source: ProRes 4444 with alpha
/// (no H.264 encoder keeps it), decoded on the CPU — through libvpx for WebM
/// VP8 / VP9, whose native decoders drop the alpha.
pub fn alpha_proxy_args(asset: &MediaAsset) -> Option<(String, Vec<String>, Vec<String>)> {
    let v = asset.video.as_ref().filter(|v| v.alpha())?;
    let pre = crate::decode::alpha_decoder(&v.codec).map(|d| vec!["-c:v".to_string(), d.to_string()]).unwrap_or_default();
    let args = ["-c:v", "prores_ks", "-profile:v", "4", "-pix_fmt", "yuva444p10le", "-vendor", "apl0"].map(String::from).to_vec();
    Some(("prores_ks".to_string(), pre, args))
}

#[derive(Clone, Default)]
pub struct ProxyManager {
    status: Arc<Mutex<HashMap<AssetId, ProxyStatus>>>,
}

impl ProxyManager {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn status(&self, id: AssetId) -> Option<ProxyStatus> {
        self.status.lock().get(&id).cloned()
    }

    pub fn all(&self) -> HashMap<AssetId, ProxyStatus> {
        self.status.lock().clone()
    }

    /// Queue a proxy transcode on the media pool.
    pub fn enqueue(&self, asset: MediaAsset, out_dir: PathBuf) {
        if !ffmpeg_cli::available() || asset.video.is_none() || crate::probe::is_synthetic(&asset) {
            self.status.lock().insert(asset.id, ProxyStatus::Failed("proxy generation requires ffmpeg and a video file".into()));
            return;
        }
        {
            let mut st = self.status.lock();
            if matches!(st.get(&asset.id), Some(ProxyStatus::Queued | ProxyStatus::Running { .. })) {
                return;
            }
            st.insert(asset.id, ProxyStatus::Queued);
        }
        let status = self.status.clone();
        rayon::spawn(move || {
            let r = transcode(&asset, &out_dir, |p, enc| {
                status.lock().insert(asset.id, ProxyStatus::Running { progress: p, encoder: enc.to_string() });
            });
            let s = match r {
                Ok(p) => ProxyStatus::Done(p),
                Err(e) => ProxyStatus::Failed(e.to_string()),
            };
            status.lock().insert(asset.id, s);
        });
    }
}

/// Blocking transcode (runs on a pool worker).
pub fn transcode(asset: &MediaAsset, out_dir: &Path, mut progress: impl FnMut(f32, &str)) -> Result<PathBuf, MediaError> {
    let v = asset.video.as_ref().ok_or_else(|| MediaError::Unsupported("no video".into()))?;
    std::fs::create_dir_all(out_dir).map_err(MediaError::Io)?;
    let out = proxy_path_for(asset, out_dir);
    let (pw, ph) = proxy_size(v.width, v.height);
    let total = asset.duration.as_secs_f64().max(0.001);
    let platform = Platform::current();
    let mut last_err = String::new();
    let encoders = match alpha_proxy_args(asset) {
        Some(alpha) => vec![alpha],
        None => proxy_encoder_args(platform, hwaccel::probe_caps(), hwaccel::render_vendor()).into_iter().filter(|(e, ..)| !probe_rejects(e)).collect(),
    };
    for (enc, pre, args) in encoders {
        // proxies are progressive: interlaced sources are deinterlaced first
        let di = asset.deinterlace_fields().map(|f| format!("{},", crate::decode::deinterlace_filter(f))).unwrap_or_default();
        let vf = match enc.as_str() {
            "h264_qsv" => format!("{di}scale={pw}:{ph},format=nv12,{}", hwaccel::qsv_upload_filter(platform)),
            "h264_vaapi" => format!("{di}scale={pw}:{ph},format=nv12,hwupload"),
            "h264_nvenc" | "h264_amf" => format!("{di}scale={pw}:{ph},format=nv12"),
            _ => format!("{di}scale={pw}:{ph}"),
        };
        let mut cmd = ffmpeg_cli::command(&ffmpeg_cli::ffmpeg_path());
        cmd.args(["-hide_banner", "-loglevel", "error", "-nostdin", "-y"]);
        cmd.args(&pre);
        cmd.arg("-i").arg(&asset.path);
        cmd.args(["-map", "0:v:0", "-map", "0:a?", "-vf", &vf]);
        cmd.args(&args);
        cmd.args(["-c:a", "aac", "-b:a", "160k", "-movflags", "+faststart", "-progress", "pipe:1"]);
        cmd.arg(&out);
        cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
        let mut child = match ffmpeg_cli::spawn_confined(&mut cmd) {
            Ok(c) => c,
            Err(e) => {
                last_err = e.to_string();
                continue;
            }
        };
        progress(0.0, &enc);
        if let Some(stdout) = child.stdout.take() {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if let Some(us) = line.strip_prefix("out_time_us=").or_else(|| line.strip_prefix("out_time_ms=")) {
                    if let Ok(us) = us.trim().parse::<f64>() {
                        progress(((us / 1e6) / total).clamp(0.0, 1.0) as f32, &enc);
                    }
                }
            }
        }
        let output = child.wait_with_output().map_err(MediaError::Io)?;
        if output.status.success() {
            progress(1.0, &enc);
            return Ok(out);
        }
        last_err = String::from_utf8_lossy(&output.stderr).lines().last().unwrap_or("encoder failed").to_string();
        log::warn!("proxy encoder {enc} failed: {last_err}; trying next");
    }
    Err(MediaError::Decode(format!("proxy transcode failed: {last_err}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn proxy_sizes() {
        assert_eq!(proxy_size(1920, 1080), (960, 540));
        assert_eq!(proxy_size(3840, 2160), (1920, 1080));
        assert_eq!(proxy_size(7680, 4320), (1920, 1080));
    }

    #[test]
    fn encoder_preference() {
        let names = |platform, caps: &hwaccel::HwCaps, vendor| proxy_encoder_args(platform, caps, vendor).into_iter().map(|e| e.0).collect::<Vec<_>>();
        let linux = hwaccel::HwCaps::assume_full(Platform::Linux);
        assert_eq!(names(Platform::Linux, &linux, GpuVendor::Intel), ["h264_qsv", "h264_nvenc", "h264_amf", "h264_vaapi", "libx264"]);
        assert_eq!(names(Platform::Linux, &linux, GpuVendor::Nvidia), ["h264_nvenc", "h264_qsv", "h264_amf", "libx264"]);
        let windows = hwaccel::HwCaps::assume_full(Platform::Windows);
        assert_eq!(names(Platform::Windows, &windows, GpuVendor::Amd), ["h264_amf", "h264_qsv", "h264_nvenc", "libx264"]);
        assert_eq!(names(Platform::Windows, &hwaccel::HwCaps::default(), GpuVendor::Other), ["libx264"]);
        // Windows decodes the source with D3D11VA; QSV reuses its Intel device
        let args = proxy_encoder_args(Platform::Windows, &windows, GpuVendor::Nvidia);
        assert_eq!(args[0].1, ["-hwaccel", "d3d11va"]);
        assert!(args.iter().find(|a| a.0 == "h264_qsv").is_some_and(|a| a.1.join(" ").ends_with("-hwaccel d3d11va -hwaccel_device dx")));
    }

    #[test]
    fn alpha_sources_get_prores_4444_mov_proxies() {
        let mut a = crate::probe::synthetic_asset("logo", 2, 0.0);
        a.path = PathBuf::from("C:/media/logo.webm");
        let dir = Path::new("C:/proxies");
        assert_eq!(proxy_path_for(&a, dir), dir.join("logo_proxy.mp4"));
        assert!(alpha_proxy_args(&a).is_none());
        assert!(!proxy_keeps_alpha(&proxy_path_for(&a, dir)));
        let v = a.video.as_mut().unwrap();
        v.codec = "vp9".into();
        v.has_alpha = true;
        let p = proxy_path_for(&a, dir);
        assert_eq!(p, dir.join("logo_proxy.mov"));
        assert!(proxy_keeps_alpha(&p));
        let (enc, pre, args) = alpha_proxy_args(&a).unwrap();
        assert_eq!(enc, "prores_ks");
        assert_eq!(pre, ["-c:v", "libvpx-vp9"], "WebM alpha needs libvpx to decode");
        assert!(args.join(" ").contains("-profile:v 4 -pix_fmt yuva444p10le"), "{args:?}");
        // ProRes 4444 sources decode natively
        let v = a.video.as_mut().unwrap();
        v.codec = "prores".into();
        v.has_alpha = false;
        v.pix_fmt = "yuva444p12le".into();
        assert!(alpha_proxy_args(&a).unwrap().1.is_empty());
    }
}
