//! Multimodal helpers: 480p 15fps H.264 proxy encoding, Gemini File API upload,
//! and timeline JSON serialization for the Cloud AI Copilot.

use nle_core::timeline::MarkerKind;
use nle_core::{ClipId, MediaAsset, Time, Timeline};
use nle_media::ffmpeg_cli;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

/// Transcode `source` to a lightweight 480p 15fps H.264 MP4 proxy (the render
/// GPU's H.264 encoder first, then the other vendors', then `libx264`) suitable
/// for fast cloud AI video understanding uploads.
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

    let threads = nle_media::threads::budget().ffmpeg_threads().to_string();
    let mut attempts: Vec<(&str, Vec<&str>)> = vec![
        ("h264_qsv", vec!["-c:v", "h264_qsv", "-b:v", "900k", "-maxrate", "1200k"]),
        ("h264_nvenc", vec!["-c:v", "h264_nvenc", "-preset", "p2", "-b:v", "900k", "-maxrate", "1200k", "-pix_fmt", "yuv420p"]),
        ("h264_amf", vec!["-c:v", "h264_amf", "-quality", "speed", "-b:v", "900k", "-maxrate", "1200k", "-pix_fmt", "yuv420p"]),
    ];
    let prefer = nle_media::encoder_probe::backend_for(nle_media::hwaccel::render_vendor());
    let backend = |n: &str| nle_media::encoder_probe::CANDIDATES.iter().find(|(c, _)| *c == n).map(|(_, b)| *b);
    attempts.sort_by_key(|(n, _)| backend(n) != prefer);
    // skip GPU encoders the start-up probe found not working
    attempts.retain(|(n, _)| nle_media::encoder_probe::result().is_none_or(|p| p.works(n)));
    attempts.push(("libx264", vec!["-c:v", "libx264", "-preset", "veryfast", "-crf", "28", "-threads", &threads]));

    let mut last_err = String::new();
    for (codec_name, v_args) in attempts {
        let mut args: Vec<String> =
            vec!["-hide_banner".into(), "-loglevel".into(), "error".into(), "-y".into(), "-i".into(), src_s.clone(), "-vf".into(), "scale=-2:480,fps=15".into()];
        args.extend(v_args.iter().map(|s| (*s).to_string()));
        args.extend(["-c:a".into(), "aac".into(), "-b:a".into(), "96k".into(), "-movflags".into(), "+faststart".into(), out_s.clone()]);
        let output = ffmpeg_cli::output_confined(ffmpeg_cli::command(&ffmpeg).args(&args)).map_err(|e| format!("spawn ffmpeg ({codec_name}): {e}"))?;
        if output.status.success() && out.is_file() {
            return Ok(out.to_path_buf());
        }
        last_err = String::from_utf8_lossy(&output.stderr).trim().to_string();
    }

    Err(format!("480p proxy encode failed: {last_err}"))
}

const GEMINI_API: &str = "https://generativelanguage.googleapis.com";

/// A file stored with the Gemini File API.
#[derive(Clone, Debug, PartialEq)]
pub struct UploadedFile {
    /// `files/<id>` (used to poll and delete).
    pub name: String,
    /// URI passed as `fileData.fileUri` in `generateContent`.
    pub uri: String,
    pub mime_type: String,
}

/// Upload a local media/proxy file with the Gemini File API resumable
/// protocol (start → `x-goog-upload-url` → upload+finalize), then wait until
/// the server has processed it (`state == ACTIVE`).
pub async fn upload_gemini_file(client: &reqwest::Client, api_key: &str, file_path: &Path, mime_type: &str) -> Result<UploadedFile, String> {
    let bytes = tokio::fs::read(file_path).await.map_err(|e| format!("read upload file {}: {e}", file_path.display()))?;
    let display_name = file_path.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| "depthcut_proxy.mp4".to_string());
    let len = bytes.len();

    // 1. start: returns the session URL in a response header.
    // The key goes in a header (never the URL) so it can't leak into error text.
    let start = client
        .post(format!("{GEMINI_API}/upload/v1beta/files"))
        .header("x-goog-api-key", api_key)
        .header("X-Goog-Upload-Protocol", "resumable")
        .header("X-Goog-Upload-Command", "start")
        .header("X-Goog-Upload-Header-Content-Length", len.to_string())
        .header("X-Goog-Upload-Header-Content-Type", mime_type)
        .json(&json!({ "file": { "display_name": display_name } }))
        .send()
        .await
        .map_err(|e| format!("Gemini upload start failed: {}", e.without_url()))?;
    let status = start.status();
    let session = start.headers().get("x-goog-upload-url").and_then(|v| v.to_str().ok()).map(str::to_string);
    if !status.is_success() {
        let text = start.text().await.unwrap_or_default();
        return Err(format!("Gemini upload start HTTP {status}: {}", api_error(&text)));
    }
    let session = session.ok_or("Gemini upload start returned no upload URL")?;

    // 2. upload all bytes and finalize
    let resp = client
        .post(&session)
        .header("x-goog-api-key", api_key)
        .header(reqwest::header::CONTENT_LENGTH, len.to_string())
        .header("X-Goog-Upload-Offset", "0")
        .header("X-Goog-Upload-Command", "upload, finalize")
        .body(bytes)
        .send()
        .await
        .map_err(|e| format!("Gemini file upload failed: {}", e.without_url()))?;
    let status = resp.status();
    let text = resp.text().await.map_err(|e| format!("Gemini file upload response: {}", e.without_url()))?;
    if !status.is_success() {
        return Err(format!("Gemini file upload HTTP {status}: {}", api_error(&text)));
    }
    let body: Value = serde_json::from_str(&text).map_err(|e| format!("Gemini file upload invalid JSON: {e}"))?;
    let mut file = parse_file(body.get("file").unwrap_or(&body)).ok_or_else(|| format!("Gemini file upload response missing name/uri: {}", clip_str(&text, 200)))?;
    if file.mime_type.is_empty() {
        file.mime_type = mime_type.to_string();
    }
    let mut state = body.pointer("/file/state").and_then(Value::as_str).unwrap_or("PROCESSING").to_string();

    // 3. video must finish server-side processing before generateContent can use it
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(120);
    while state != "ACTIVE" {
        if state == "FAILED" {
            delete_gemini_file(client, api_key, &file.name).await;
            return Err("Gemini could not process the uploaded proxy video".into());
        }
        if std::time::Instant::now() >= deadline {
            delete_gemini_file(client, api_key, &file.name).await;
            return Err("Gemini file processing did not finish within 120 s".into());
        }
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        let poll = client
            .get(format!("{GEMINI_API}/v1beta/{}", file.name))
            .header("x-goog-api-key", api_key)
            .send()
            .await
            .map_err(|e| format!("Gemini file status request failed: {}", e.without_url()))?;
        let code = poll.status();
        let text = poll.text().await.map_err(|e| format!("Gemini file status response: {}", e.without_url()))?;
        if code.is_server_error() || code.as_u16() == 429 {
            continue; // transient: keep polling until the deadline
        }
        if !code.is_success() {
            return Err(format!("Gemini file status HTTP {code}: {}", api_error(&text)));
        }
        let v: Value = serde_json::from_str(&text).map_err(|e| format!("Gemini file status invalid JSON: {e}"))?;
        state = v.get("state").and_then(Value::as_str).unwrap_or("PROCESSING").to_string();
    }
    Ok(file)
}

/// Best-effort delete of an uploaded file (they also expire after 48 h).
pub async fn delete_gemini_file(client: &reqwest::Client, api_key: &str, name: &str) {
    if !name.starts_with("files/") {
        return;
    }
    match client.delete(format!("{GEMINI_API}/v1beta/{name}")).header("x-goog-api-key", api_key).send().await {
        Ok(r) if r.status().is_success() => log::debug!("deleted Gemini upload {name}"),
        Ok(r) => log::debug!("could not delete Gemini upload {name}: HTTP {}", r.status()),
        Err(e) => log::debug!("could not delete Gemini upload {name}: {}", e.without_url()),
    }
}

fn parse_file(file: &Value) -> Option<UploadedFile> {
    Some(UploadedFile {
        name: file.get("name")?.as_str()?.to_string(),
        uri: file.get("uri")?.as_str()?.to_string(),
        mime_type: file.get("mimeType").and_then(Value::as_str).unwrap_or_default().to_string(),
    })
}

/// `error.message` from a Google API error body, else a short snippet.
fn api_error(text: &str) -> String {
    serde_json::from_str::<Value>(text).ok().and_then(|v| v.pointer("/error/message").and_then(Value::as_str).map(str::to_string)).unwrap_or_else(|| clip_str(text, 200))
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
                "has_video": p.clip.has_video(),
                "has_audio": p.clip.has_audio(),
                "audio_role": p.clip.has_audio().then(|| format!("{:?}", p.clip.audio.role).to_lowercase()),
                "volume_db": p.clip.has_audio().then(|| round3(p.clip.audio.volume_db.value)),
                "color_nodes": p.clip.color.corrections.iter().map(|n| n.name.as_str()).take(8).collect::<Vec<_>>(),
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
            "color_looks": crate::tools_post::COLOR_LOOKS,
            "mask_styles": crate::tools_post::MASK_STYLES,
            "caption_presets": crate::tools_post::CAPTION_PRESETS,
            "caption_highlight_colors": crate::tools_post::CAPTION_COLORS,
            "audio_roles": crate::tools_post::AUDIO_ROLES,
            "export_deliveries": crate::tools_post::EXPORT_DELIVERIES,
            "export_codecs": crate::tools_post::EXPORT_CODECS,
        },
        "captions": {
            "segments": tl.captions.segments.len(),
            "enabled": tl.captions.enabled,
            "font_size": tl.captions.style.font_size,
            "position_y": round3(tl.captions.style.position_y as f64),
            "uppercase": tl.captions.style.uppercase,
            "word_highlight": tl.captions.style.word_highlight,
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

#[cfg(test)]
mod upload_tests {
    use super::*;

    #[test]
    fn parses_file_resource_and_api_errors() {
        let v = json!({"file": {"name": "files/abc", "uri": "https://x/files/abc", "mimeType": "video/mp4", "state": "PROCESSING"}});
        let f = parse_file(&v["file"]).unwrap();
        assert_eq!((f.name.as_str(), f.mime_type.as_str()), ("files/abc", "video/mp4"));
        assert!(parse_file(&json!({"name": "files/abc"})).is_none());
        assert_eq!(api_error(r#"{"error":{"code":503,"message":"overloaded"}}"#), "overloaded");
        assert_eq!(api_error("plain"), "plain");
    }
}
