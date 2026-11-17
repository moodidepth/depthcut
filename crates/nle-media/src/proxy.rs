//! Background proxy-media transcoder.
//!
//! Proxies are half-resolution (max 1080p, typically 960×540 for HD and
//! 1920×1080 for 4K) H.264 files encoded on the Arc's media engine
//! (`h264_qsv` / `h264_vaapi`) with a `libx264` fallback. Jobs run on the
//! 12-thread media pool and report progress via `-progress pipe:1`.

use crate::ffmpeg_cli;
use crate::hwaccel::{self, Platform};
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

/// Encoder choices for proxies, best first.
pub fn proxy_encoder_args(platform: Platform, caps: &hwaccel::HwCaps) -> Vec<(String, Vec<String>, Vec<String>)> {
    let mut v = Vec::new();
    if caps.has_encoder("h264_qsv") {
        let mut pre = hwaccel::qsv_device_args(platform);
        pre.extend(["-filter_hw_device".into(), "hw".into()]);
        v.push(("h264_qsv".into(), pre, ["-c:v", "h264_qsv", "-preset", "veryfast", "-global_quality", "26", "-look_ahead", "0"].map(String::from).to_vec()));
    }
    if platform == Platform::Linux && caps.has_encoder("h264_vaapi") {
        v.push(("h264_vaapi".into(), ["-vaapi_device", "/dev/dri/renderD128"].map(String::from).to_vec(), ["-c:v", "h264_vaapi", "-qp", "26"].map(String::from).to_vec()));
    }
    v.push(("libx264".into(), Vec::new(), ["-c:v", "libx264", "-preset", "veryfast", "-crf", "23", "-threads", "8"].map(String::from).to_vec()));
    v
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
    dir.join(format!("{stem}_proxy.mp4"))
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
    for (enc, pre, args) in proxy_encoder_args(platform, hwaccel::probe_caps()) {
        let vf = match enc.as_str() {
            "h264_qsv" => format!("scale={pw}:{ph},format=nv12,{}", hwaccel::qsv_upload_filter(platform)),
            "h264_vaapi" => format!("scale={pw}:{ph},format=nv12,hwupload"),
            _ => format!("scale={pw}:{ph}"),
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
        let mut child = match cmd.spawn() {
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
        let caps = hwaccel::HwCaps::assume_full(Platform::Linux);
        let encs: Vec<String> = proxy_encoder_args(Platform::Linux, &caps).into_iter().map(|e| e.0).collect();
        assert_eq!(encs, vec!["h264_qsv", "h264_vaapi", "libx264"]);
        let encs: Vec<String> = proxy_encoder_args(Platform::Windows, &hwaccel::HwCaps::default()).into_iter().map(|e| e.0).collect();
        assert_eq!(encs, vec!["libx264"]);
    }
}
