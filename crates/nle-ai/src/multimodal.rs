//! Multimodal helpers: 480p 15fps H.264 proxy encoding, Gemini File API upload,
//! and timeline JSON serialization for the Cloud AI Copilot.

use nle_core::timeline::MarkerKind;
use nle_core::{ClipId, MediaAsset, Time, Timeline};
use nle_media::ffmpeg_cli;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// Transcode `source` to a lightweight 480p 15fps H.264 MP4 proxy (`h264_qsv` first,
/// falling back to `libx264`) suitable for fast cloud AI video understanding uploads.
pub fn encode_proxy_480p15(source: &Path, out: &Path) -> Result<PathBuf, String> {
    if !ffmpeg_cli::available() {
        return Err("FFmpeg executable not found (cannot encode 480p 15fps proxy)".into());
    }
    if let Some(parent) = out.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("create proxy dir {}: {e}", parent.display()))?;
    }
    let ffmpeg = ffmpeg_cli::ffmpeg_path();
    let src_s = source.to_string_lossy().into_owned();
    let out_s = out.to_string_lossy().into_owned();

    let attempts: [(&str, &[&str]); 2] = [
        ("h264_qsv", &["-c:v", "h264_qsv", "-b:v", "900k", "-maxrate", "1200k"]),
        ("libx264", &["-c:v", "libx264", "-preset", "veryfast", "-crf", "28"]),
    ];

    let mut last_err = String::new();
    for (codec_name, v_args) in attempts {
        let mut args: Vec<String> = vec![
            "-hide_banner".into(),
            "-loglevel".into(),
            "error".into(),
            "-y".into(),
            "-i".into(),
            src_s.clone(),
            "-vf".into(),
            "scale=-2:480,fps=15".into(),
        ];
        args.extend(v_args.iter().map(|s| (*s).to_string()));
        args.extend([
            "-c:a".into(),
            "aac".into(),
            "-b:a".into(),
            "96k".into(),
            "-movflags".into(),
            "+faststart".into(),
            out_s.clone(),
        ]);
        let output = Command::new(&ffmpeg)
            .args(&args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .map_err(|e| format!("spawn ffmpeg ({codec_name}): {e}"))?;
        if output.status.success() && out.is_file() {
            return Ok(out.to_path_buf());
        }
        last_err = String::from_utf8_lossy(&output.stderr).trim().to_string();
    }

    Err(format!("480p proxy encode failed: {last_err}"))
}

/// Upload a local media/proxy file to the Gemini File API (`upload/v1beta/files`)
/// and return the resulting `file.uri`.
pub async fn upload_gemini_file(client: &reqwest::Client, api_key: &str, file_path: &Path, mime_type: &str) -> Result<String, String> {
    let bytes = tokio::fs::read(file_path)
        .await
        .map_err(|e| format!("read upload file {}: {e}", file_path.display()))?;
    let display_name = file_path
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "argon_proxy.mp4".to_string());
    let len = bytes.len();
    let url = "https://generativelanguage.googleapis.com/upload/v1beta/files";

    // key goes in a header (never the URL) so it can't leak into error text
    let resp = client
        .post(url)
        .header("x-goog-api-key", api_key)
        .header("X-Goog-Upload-Protocol", "raw")
        .header("X-Goog-Upload-Command", "start, upload, finalize")
        .header("X-Goog-Upload-Header-Content-Length", len.to_string())
        .header("X-Goog-Upload-Header-Content-Type", mime_type)
        .header("X-Goog-Upload-File-Name", display_name)
        .header(reqwest::header::CONTENT_TYPE, mime_type)
        .body(bytes)
        .send()
        .await
        .map_err(|e| format!("Gemini file upload request failed: {}", e.without_url()))?;

    let status = resp.status();
    let text = resp.text().await.map_err(|e| format!("Gemini file upload response: {}", e.without_url()))?;
    if !status.is_success() {
        return Err(format!("Gemini file upload HTTP {status}: {}", clip_str(&text, 200)));
    }
    let body: Value = serde_json::from_str(&text).map_err(|e| format!("Gemini file upload invalid JSON: {e}"))?;
    let file = body.get("file").unwrap_or(&body);
    let uri = file
        .get("uri")
        .and_then(Value::as_str)
        .map(ToString::to_string)
        .ok_or_else(|| format!("Gemini file upload response missing URI: {}", clip_str(&text, 200)))?;
    let name = file.get("name").and_then(Value::as_str).map(ToString::to_string);
    let mut state = file.get("state").and_then(Value::as_str).unwrap_or("ACTIVE").to_string();

    // Video files must finish server-side processing before generateContent can use them.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(90);
    while state != "ACTIVE" {
        if state == "FAILED" {
            return Err("Gemini could not process the uploaded proxy video".into());
        }
        let Some(name) = name.as_deref() else { break };
        if std::time::Instant::now() >= deadline {
            return Err("Gemini file processing did not finish within 90 s".into());
        }
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        let poll = client
            .get(format!("https://generativelanguage.googleapis.com/v1beta/{name}"))
            .header("x-goog-api-key", api_key)
            .send()
            .await
            .map_err(|e| format!("Gemini file status request failed: {}", e.without_url()))?;
        let v: Value = poll.json().await.map_err(|e| format!("Gemini file status invalid JSON: {}", e.without_url()))?;
        state = v.get("state").and_then(Value::as_str).unwrap_or("FAILED").to_string();
    }
    Ok(uri)
}

fn clip_str(s: &str, max: usize) -> String {
    match s.char_indices().nth(max) {
        Some((i, _)) => format!("{}…", &s[..i]),
        None => s.to_string(),
    }
}

/// Max markers sent to the model (nearest the playhead first); the rest are counted.
const MAX_CONTEXT_MARKERS: usize = 300;

/// Build a compact structured JSON snapshot of the timeline state for LLM context.
pub fn build_timeline_json(tl: &Timeline, assets: &[MediaAsset], playhead: Time, selection: &[ClipId]) -> Value {
    let placed = tl.placed_clips();
    let clips: Vec<Value> = placed
        .iter()
        .map(|p| {
            let effects: Vec<&'static str> = p.clip.effects.iter().filter(|e| e.enabled).map(|e| e.kind.label()).collect();
            json!({
                "id": p.clip.id.0,
                "name": p.clip.name,
                "lane": p.lane,
                "start_secs": round3(p.start.as_secs_f64()),
                "end_secs": round3(p.end().as_secs_f64()),
                "duration_secs": round3(p.clip.duration.as_secs_f64()),
                "is_gap": p.clip.is_gap(),
                "has_velocity_curve": p.clip.retime.as_ref().is_some_and(|r| r.has_velocity_curve()),
                "effects": effects,
                "transition_in": p.clip.transition_in.as_ref().map(|t| t.kind.label()),
                "transition_out": p.clip.transition_out.as_ref().map(|t| t.kind.label()),
            })
        })
        .collect();

    let mut nearest: Vec<&nle_core::timeline::Marker> = tl.markers.iter().collect();
    if nearest.len() > MAX_CONTEXT_MARKERS {
        nearest.sort_by_key(|m| (m.time.0 - playhead.0).abs());
        nearest.truncate(MAX_CONTEXT_MARKERS);
    }
    nearest.sort_by_key(|m| m.time.0);
    let markers: Vec<Value> = nearest
        .into_iter()
        .map(|m| {
            let kind = match m.kind {
                MarkerKind::Standard => "Standard",
                MarkerKind::Beat => "Beat",
                MarkerKind::SubBass => "SubBass",
                MarkerKind::Snare => "Snare",
                MarkerKind::Kill => "Kill",
            };
            json!({
                "time_secs": round3(m.time.as_secs_f64()),
                "name": m.name,
                "kind": kind,
            })
        })
        .collect();

    let asset_list: Vec<Value> = assets
        .iter()
        .map(|a| {
            json!({
                "id": a.id.0,
                "name": a.name,
                "duration_secs": round3(a.duration.as_secs_f64()),
                "fps": round3(a.frame_rate().fps()),
            })
        })
        .collect();

    let sel_ids: Vec<u64> = selection.iter().map(|id| id.0).collect();
    let mut effect_presets: Vec<String> = nle_core::presets::MontagePreset::built_in().into_iter().map(|p| p.name).collect();
    effect_presets.push("Gaming 9:16 Split-Crop".into());

    json!({
        "bounds_secs": [0.0, round3(tl.extent().as_secs_f64())],
        "frame_secs": round3(tl.frame().as_secs_f64() * 1000.0) / 1000.0,
        "markers_total": tl.markers.len(),
        "allowed": {
            "velocity_presets": nle_core::Retime::PRESET_NAMES,
            "effect_presets": effect_presets,
            "transition_kinds": crate::tools::TRANSITION_KINDS,
            "marker_kinds": crate::tools::MARKER_KINDS,
            "transition_duration_frames": [2, 120],
        },
        "timeline_name": tl.name,
        "format": {
            "width": tl.format.width,
            "height": tl.format.height,
            "fps": round3(tl.format.rate.fps()),
        },
        "playhead_secs": round3(playhead.as_secs_f64()),
        "duration_secs": round3(tl.extent().as_secs_f64()),
        "selected_clip_ids": sel_ids,
        "clips": clips,
        "markers": markers,
        "assets": asset_list,
    })
}

fn round3(v: f64) -> f64 {
    (v * 1000.0).round() / 1000.0
}
