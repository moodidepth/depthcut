//! Cloud & Local AI providers: Google Gemini 4 (`thinkingLevel` + multimodal File API),
//! OpenAI / OpenRouter Chat Completions, Local Ollama, and the offline planner
//! (used only when no cloud provider is configured).
//!
//! Requests run on a worker thread, retry transient failures (timeouts,
//! connection errors, 408/429/5xx) with backoff, honour cancellation and never
//! panic; failures come back as a [`CopilotError`] the UI can offer to retry.

use crate::config::{AiConfig, AiProviderKind};
use crate::multimodal;
use crate::tools::{self, ActionOutcome, CopilotToolCall};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::path::PathBuf;
use std::time::Duration;

const SYSTEM_PROMPT: &str = "\
You are Argon AI Copilot, an expert gaming montage & fragmovie editor assistant (Valorant, CS2, Overwatch 2).
You receive a compact JSON snapshot of the user's timeline (clips, markers, playhead, selection, bounds and the allowed preset / transition / marker names) and optionally a 480p 15fps proxy video.
When editing the timeline, invoke one or more structured tools and explain concisely what you changed.
Editing: `jump_to_timecode`, `apply_velocity_preset`, `add_effect_preset`, `cut_and_ripple`, `sync_to_beat_markers`, `drop_marker`, `add_transition`.
Finishing: `color_grade` (looks / primaries), `add_mask` (vignette, spotlight, masked correction), `generate_captions`, `style_captions` (only when `captions.segments` > 0), `polish_audio`, `adjust_audio`, `set_export_preset` (opens the Render dialog with a delivery preset and, when the user names one, a `codec`; it never renders by itself).
Rules: use only clip ids that appear in `clips`; keep every time within `bounds_secs`; use only names listed in `allowed`; at most 32 actions per reply. Invalid or out-of-bounds actions are skipped and reported to the user.";

/// Attempts per request (first try + retries) for transient failures.
const MAX_ATTEMPTS: u32 = 3;
/// Generous per-request limit: HIGH thinking levels and cold local models can take minutes.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(300);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: String,
    pub text: String,
    pub tool_calls: Vec<CopilotToolCall>,
    /// Per-action results (`Applied` / `Skipped: reason`) shown as badges.
    #[serde(default)]
    pub outcomes: Vec<ActionOutcome>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct CopilotResponse {
    pub reply: String,
    pub tool_calls: Vec<CopilotToolCall>,
    /// Tool calls the model emitted that failed schema validation.
    pub rejected: Vec<ActionOutcome>,
    pub provider_label: String,
    pub used_offline_fallback: bool,
}

/// A failed Copilot request. Nothing was changed on the timeline.
#[derive(Clone, Debug, PartialEq)]
pub struct CopilotError {
    pub message: String,
    /// Transient (network, rate limit, server, malformed model output): retrying may succeed.
    pub retryable: bool,
    /// HTTP status of the provider's error response, when there was one.
    pub http_status: Option<u16>,
}

impl CopilotError {
    fn transient(message: impl Into<String>) -> Self {
        Self { message: message.into(), retryable: true, http_status: None }
    }
    fn fatal(message: impl Into<String>) -> Self {
        Self { message: message.into(), retryable: false, http_status: None }
    }
    fn with_status(mut self, code: u16) -> Self {
        self.http_status = Some(code);
        self
    }
    /// The model is overloaded or rate limited (another model may still answer).
    pub fn is_overload(&self) -> bool {
        matches!(self.http_status, Some(429 | 503 | 529))
    }
}

impl std::fmt::Display for CopilotError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

/// Text reply, valid tool calls, and rejected (schema-invalid) tool calls.
pub type ParsedReply = (String, Vec<CopilotToolCall>, Vec<ActionOutcome>);

/// Build a Gemini 4 `generateContent` JSON request body with `thinkingConfig.thinkingLevel`,
/// timeline JSON context, optional `fileData` video URI, and all 7 tool declarations.
pub fn build_gemini_request_body(config: &AiConfig, user_prompt: &str, timeline_json: &Value, file_uri: Option<&str>) -> Value {
    let mut parts = Vec::new();
    if let Some(uri) = file_uri {
        parts.push(json!({
            "fileData": {
                "mimeType": "video/mp4",
                "fileUri": uri,
            }
        }));
    }
    parts.push(json!({ "text": user_content(timeline_json, user_prompt) }));

    json!({
        "systemInstruction": {
            "parts": [{ "text": SYSTEM_PROMPT }]
        },
        "contents": [{
            "role": "user",
            "parts": parts,
        }],
        "tools": [tools::gemini_tool_declarations()],
        "generationConfig": {
            "temperature": 0.2,
            "thinkingConfig": {
                "thinkingLevel": config.thinking_level.as_gemini_str(),
            }
        }
    })
}

fn user_content(timeline_json: &Value, prompt: &str) -> String {
    format!("TIMELINE_STATE_JSON:\n{}\n\nUSER_REQUEST:\n{}", serde_json::to_string(timeline_json).unwrap_or_else(|_| "{}".into()), prompt)
}

/// At most `max` chars of `s`, for error messages (never dump whole bodies into the UI).
fn snippet(s: &str, max: usize) -> String {
    let s = s.trim();
    match s.char_indices().nth(max) {
        Some((i, _)) => format!("{}…", &s[..i]),
        None => s.to_string(),
    }
}

fn push_call(name: &str, args: &Value, calls: &mut Vec<CopilotToolCall>, rejected: &mut Vec<ActionOutcome>) {
    if calls.len() >= tools::MAX_ACTIONS_PER_REPLY {
        rejected.push(ActionOutcome::skipped("too_many_actions", format!("only the first {} actions of a reply are used", tools::MAX_ACTIONS_PER_REPLY)));
        return;
    }
    match CopilotToolCall::from_name_and_args(name, args) {
        Ok(call) => calls.push(call),
        Err(e) => rejected.push(ActionOutcome::skipped(if name.is_empty() { "unknown_tool" } else { name }, format!("invalid call: {e}"))),
    }
}

fn reply_text(texts: Vec<String>, n_calls: usize) -> String {
    if texts.is_empty() && n_calls > 0 {
        format!("Prepared {n_calls} timeline action(s).")
    } else {
        texts.join("\n\n")
    }
}

/// Parse Gemini `generateContent` JSON into text, typed tool calls and rejected calls.
pub fn parse_gemini_response(body: &Value) -> Result<ParsedReply, String> {
    if let Some(err) = body.get("error") {
        let msg = err.get("message").and_then(Value::as_str).map(str::to_string).unwrap_or_else(|| err.to_string());
        return Err(format!("Gemini API error: {}", snippet(&msg, 300)));
    }
    if let Some(reason) = body.get("promptFeedback").and_then(|p| p.get("blockReason")).and_then(Value::as_str) {
        return Err(format!("Gemini blocked the request ({reason})"));
    }
    let cand = body.get("candidates").and_then(|c| c.get(0)).ok_or_else(|| format!("Gemini returned no candidates: {}", snippet(&body.to_string(), 200)))?;
    let parts = cand.get("content").and_then(|c| c.get("parts")).and_then(Value::as_array);
    let Some(parts) = parts else {
        let reason = cand.get("finishReason").and_then(Value::as_str).unwrap_or("unknown");
        return Err(format!("Gemini returned an empty answer (finishReason {reason})"));
    };

    let mut texts = Vec::new();
    let mut calls = Vec::new();
    let mut rejected = Vec::new();
    for part in parts {
        if part.get("thought").and_then(Value::as_bool) == Some(true) {
            continue;
        }
        if let Some(t) = part.get("text").and_then(Value::as_str) {
            if !t.trim().is_empty() {
                texts.push(t.trim().to_string());
            }
        }
        if let Some(fc) = part.get("functionCall") {
            let name = fc.get("name").and_then(Value::as_str).unwrap_or("");
            push_call(name, fc.get("args").unwrap_or(&Value::Null), &mut calls, &mut rejected);
        }
    }
    let n = calls.len();
    Ok((reply_text(texts, n), calls, rejected))
}

/// Parse an OpenAI-compatible / Ollama `tool_calls` array (arguments as a JSON string or object).
fn parse_tool_call_array(arr: Option<&Vec<Value>>, calls: &mut Vec<CopilotToolCall>, rejected: &mut Vec<ActionOutcome>) {
    for tc in arr.into_iter().flatten() {
        let func = tc.get("function");
        let name = func.and_then(|f| f.get("name")).and_then(Value::as_str).unwrap_or("");
        match func.and_then(|f| f.get("arguments")) {
            Some(Value::String(raw)) => match serde_json::from_str::<Value>(if raw.trim().is_empty() { "{}" } else { raw }) {
                Ok(args) => push_call(name, &args, calls, rejected),
                Err(e) => rejected.push(ActionOutcome::skipped(if name.is_empty() { "unknown_tool" } else { name }, format!("arguments are not valid JSON: {e}"))),
            },
            Some(args) => push_call(name, args, calls, rejected),
            None => push_call(name, &Value::Null, calls, rejected),
        }
    }
}

/// Parse an OpenAI / OpenRouter Chat Completions response.
pub fn parse_openai_response(body: &Value) -> Result<ParsedReply, String> {
    if let Some(err) = body.get("error") {
        let msg = err.get("message").and_then(Value::as_str).map(str::to_string).unwrap_or_else(|| err.to_string());
        return Err(format!("OpenAI API error: {}", snippet(&msg, 300)));
    }
    let msg = body
        .get("choices")
        .and_then(|c| c.get(0))
        .and_then(|c| c.get("message"))
        .ok_or_else(|| format!("Unexpected OpenAI response shape: {}", snippet(&body.to_string(), 200)))?;
    let content = msg.get("content").and_then(Value::as_str).unwrap_or("").trim().to_string();
    let mut calls = Vec::new();
    let mut rejected = Vec::new();
    parse_tool_call_array(msg.get("tool_calls").and_then(Value::as_array), &mut calls, &mut rejected);
    let n = calls.len();
    Ok((reply_text(if content.is_empty() { Vec::new() } else { vec![content] }, n), calls, rejected))
}

/// Parse an Ollama `/api/chat` (non-streaming) response.
pub fn parse_ollama_response(body: &Value) -> Result<ParsedReply, String> {
    if let Some(err) = body.get("error") {
        return Err(format!("Ollama error: {}", snippet(&err.to_string(), 300)));
    }
    let msg = body.get("message").ok_or_else(|| format!("Unexpected Ollama response shape: {}", snippet(&body.to_string(), 200)))?;
    let content = msg.get("content").and_then(Value::as_str).unwrap_or("").trim().to_string();
    let mut calls = Vec::new();
    let mut rejected = Vec::new();
    parse_tool_call_array(msg.get("tool_calls").and_then(Value::as_array), &mut calls, &mut rejected);
    let n = calls.len();
    Ok((reply_text(if content.is_empty() { Vec::new() } else { vec![content] }, n), calls, rejected))
}

/// Returns `true` once the user cancelled the request.
pub type CancelFn<'a> = &'a (dyn Fn() -> bool + Sync);
/// Progress line for the UI ("Retrying…").
pub type StatusFn<'a> = &'a (dyn Fn(String) + Sync);

async fn sleep_unless_cancelled(d: Duration, cancelled: CancelFn<'_>) -> Result<(), CopilotError> {
    let step = Duration::from_millis(100);
    let mut left = d;
    while !left.is_zero() {
        if cancelled() {
            return Err(CopilotError::fatal("Cancelled"));
        }
        let s = left.min(step);
        tokio::time::sleep(s).await;
        left -= s;
    }
    Ok(())
}

/// Send a JSON request with retries for transient failures. `build` makes a
/// fresh request per attempt.
async fn send_json(label: &str, build: impl Fn() -> reqwest::RequestBuilder, cancelled: CancelFn<'_>, status: StatusFn<'_>) -> Result<Value, CopilotError> {
    let mut delay = Duration::from_secs(1);
    for attempt in 1..=MAX_ATTEMPTS {
        if cancelled() {
            return Err(CopilotError::fatal("Cancelled"));
        }
        let mut wait = delay;
        // never include the URL in messages: some providers carry keys in it
        let err = match build().send().await.map_err(reqwest::Error::without_url) {
            Ok(resp) => {
                let code = resp.status();
                let retry_after = resp.headers().get(reqwest::header::RETRY_AFTER).and_then(|v| v.to_str().ok()).and_then(|s| s.trim().parse::<u64>().ok());
                match resp.text().await {
                    Ok(text) if code.is_success() => match serde_json::from_str::<Value>(&text) {
                        Ok(v) => return Ok(v),
                        Err(e) => CopilotError::transient(format!("{label} returned malformed JSON ({e}): {}", snippet(&text, 160))),
                    },
                    Ok(text) => {
                        let detail = serde_json::from_str::<Value>(&text)
                            .ok()
                            .and_then(|v| v.pointer("/error/message").and_then(Value::as_str).map(str::to_string))
                            .unwrap_or_else(|| snippet(&text, 200));
                        let transient = code.as_u16() == 408 || code.as_u16() == 429 || code.is_server_error();
                        if let Some(s) = retry_after {
                            wait = Duration::from_secs(s.clamp(1, 30));
                        }
                        let msg = format!("{label} HTTP {code}: {detail}");
                        if transient {
                            CopilotError::transient(msg).with_status(code.as_u16())
                        } else {
                            CopilotError::fatal(msg).with_status(code.as_u16())
                        }
                    }
                    Err(e) => CopilotError::transient(format!("{label} response interrupted: {}", e.without_url())),
                }
            }
            Err(e) => {
                let what = if e.is_timeout() {
                    "timed out"
                } else if e.is_connect() {
                    "could not connect"
                } else {
                    "request failed"
                };
                CopilotError::transient(format!("{label} {what}: {e}"))
            }
        };
        if !err.retryable || attempt == MAX_ATTEMPTS {
            return Err(err);
        }
        log::warn!("copilot: {} (attempt {attempt}/{MAX_ATTEMPTS})", err.message);
        status(format!("{} — retrying ({}/{MAX_ATTEMPTS})…", snippet(&err.message, 80), attempt + 1));
        sleep_unless_cancelled(wait, cancelled).await?;
        delay *= 2;
    }
    Err(CopilotError::transient(format!("{label} failed")))
}

/// Run one Copilot request (call on a worker thread). Without a configured
/// cloud key the deterministic offline planner answers instead; any other
/// failure is returned as an error and nothing is applied.
#[allow(clippy::too_many_arguments)]
pub fn run_copilot_blocking(
    config: AiConfig,
    prompt: String,
    tl: nle_core::Timeline,
    assets: Vec<nle_core::MediaAsset>,
    playhead: nle_core::Time,
    selection: Vec<nle_core::ClipId>,
    active_video_path: Option<PathBuf>,
    cancelled: CancelFn<'_>,
    status: StatusFn<'_>,
) -> Result<CopilotResponse, CopilotError> {
    let offline = |why: &str| {
        let (plan_text, calls) = tools::offline_copilot_plan(&prompt, &tl, playhead, &selection);
        Ok(CopilotResponse {
            reply: format!("[Offline planner — {why}] {plan_text}"),
            tool_calls: calls,
            rejected: Vec::new(),
            provider_label: "Local Offline Planner".into(),
            used_offline_fallback: true,
        })
    };
    let key = match config.provider {
        AiProviderKind::Gemini4 => match config.effective_gemini_key() {
            Some(k) => k,
            None => return offline("no Gemini API key configured"),
        },
        AiProviderKind::OpenAiCompatible => match config.effective_openai_key() {
            Some(k) => k,
            None => return offline("no OpenAI/OpenRouter API key configured"),
        },
        AiProviderKind::Ollama => String::new(),
    };

    let timeline_json = multimodal::build_timeline_json(&tl, &assets, playhead, &selection);
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().map_err(|e| CopilotError::fatal(format!("could not start the async runtime: {e}")))?;
    let client =
        reqwest::Client::builder().connect_timeout(CONNECT_TIMEOUT).timeout(REQUEST_TIMEOUT).build().map_err(|e| CopilotError::fatal(format!("HTTP client init failed: {e}")))?;

    let (parsed, label) = rt.block_on(async {
        match config.provider {
            AiProviderKind::Gemini4 => {
                let mut uploaded = None;
                if config.attach_video_proxy {
                    if let Some(src) = active_video_path {
                        status("Encoding 480p proxy for Gemini…".into());
                        let proxy_out = nle_media::paths::argon_data_dir().join("copilot_proxy_480p15.mp4");
                        match multimodal::encode_proxy_480p15(&src, &proxy_out) {
                            Ok(proxy_file) => {
                                status("Uploading proxy video…".into());
                                match multimodal::upload_gemini_file(&client, &key, &proxy_file, "video/mp4").await {
                                    Ok(f) => uploaded = Some(f),
                                    Err(e) => {
                                        log::warn!("copilot: continuing without video: {e}");
                                        status("Video upload failed — asking without the video…".into());
                                    }
                                }
                            }
                            Err(e) => log::warn!("copilot: skipping 480p proxy attachment: {e}"),
                        }
                    }
                }
                let body = build_gemini_request_body(&config, &prompt, &timeline_json, uploaded.as_ref().map(|f| f.uri.as_str()));
                let model = config.effective_gemini_model();
                let ask = |model: String| {
                    let (client, key, body) = (&client, &key, &body);
                    async move {
                        let url = format!("https://generativelanguage.googleapis.com/v1beta/models/{model}:generateContent");
                        status(format!("Waiting for Gemini ({model})…"));
                        send_json("Gemini", || client.post(&url).header("x-goog-api-key", key).json(body), cancelled, status).await.map(|j| (j, model))
                    }
                };
                let mut result = ask(model.clone()).await;
                // the chosen model is overloaded: one try on the fallback model
                if let (Err(e), Some(fallback)) = (&result, config.gemini_fallback()) {
                    if e.is_overload() {
                        log::warn!("copilot: {model} overloaded ({}); trying {fallback}", e.message);
                        status(format!("{model} is busy — trying {fallback}…"));
                        result = ask(fallback).await;
                    }
                }
                if let Some(f) = &uploaded {
                    multimodal::delete_gemini_file(&client, &key, &f.name).await;
                }
                let (json, used) = result?;
                let label = if used == model { format!("Gemini ({used})") } else { format!("Gemini ({used}, fallback from {model})") };
                Ok::<_, CopilotError>((parse_gemini_response(&json).map_err(CopilotError::transient)?, label))
            }
            AiProviderKind::OpenAiCompatible => {
                let url = format!("{}/chat/completions", config.openai_base_url.trim_end_matches('/'));
                let body = json!({
                    "model": config.openai_model,
                    "messages": [
                        { "role": "system", "content": SYSTEM_PROMPT },
                        { "role": "user", "content": user_content(&timeline_json, &prompt) }
                    ],
                    "tools": tools::openai_tool_declarations(),
                    "tool_choice": "auto",
                    "temperature": 0.2
                });
                status(format!("Waiting for {}…", config.openai_model));
                let json = send_json("OpenAI", || client.post(&url).bearer_auth(&key).json(&body), cancelled, status).await?;
                Ok((parse_openai_response(&json).map_err(CopilotError::transient)?, format!("OpenAI ({})", config.openai_model)))
            }
            AiProviderKind::Ollama => {
                let url = format!("{}/api/chat", config.ollama_url.trim_end_matches('/'));
                let body = json!({
                    "model": config.ollama_model,
                    "stream": false,
                    "messages": [
                        { "role": "system", "content": SYSTEM_PROMPT },
                        { "role": "user", "content": user_content(&timeline_json, &prompt) }
                    ],
                    "tools": tools::openai_tool_declarations(),
                });
                status(format!("Waiting for Ollama ({})…", config.ollama_model));
                let json = send_json("Ollama", || client.post(&url).json(&body), cancelled, status).await?;
                Ok((parse_ollama_response(&json).map_err(CopilotError::transient)?, format!("Ollama ({})", config.ollama_model)))
            }
        }
    })?;
    let (reply, tool_calls, rejected) = parsed;
    Ok(CopilotResponse { reply, tool_calls, rejected, provider_label: label, used_offline_fallback: false })
}
