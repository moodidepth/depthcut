//! "Test Live Connection" and the live Gemini model list for the Copilot
//! settings. Both are blocking; call them from a worker thread.

use crate::config::{AiConfig, AiProviderKind};
use serde_json::{json, Value};
use std::time::{Duration, Instant};

const GEMINI_API: &str = "https://generativelanguage.googleapis.com/v1beta";

/// One step of a connection test.
#[derive(Clone, Debug, PartialEq)]
pub struct CheckLine {
    pub ok: bool,
    pub text: String,
}

/// Result of [`test_connection`]: one line per check, in order.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ConnectionReport {
    pub lines: Vec<CheckLine>,
}

impl ConnectionReport {
    /// Every check passed.
    pub fn ok(&self) -> bool {
        !self.lines.is_empty() && self.lines.iter().all(|l| l.ok)
    }
    fn push(&mut self, ok: bool, text: impl Into<String>) {
        self.lines.push(CheckLine { ok, text: text.into() });
    }
}

/// A model the key can call `generateContent` on.
#[derive(Clone, Debug, PartialEq)]
pub struct ModelInfo {
    pub id: String,
    pub display_name: String,
}

fn client() -> Result<reqwest::Client, String> {
    reqwest::Client::builder().connect_timeout(Duration::from_secs(10)).timeout(Duration::from_secs(45)).build().map_err(|e| e.to_string())
}

fn runtime() -> Result<tokio::runtime::Runtime, String> {
    tokio::runtime::Builder::new_current_thread().enable_all().build().map_err(|e| e.to_string())
}

/// `error.message` of a Google/OpenAI style error body, else a short snippet.
fn error_detail(text: &str) -> String {
    serde_json::from_str::<Value>(text)
        .ok()
        .and_then(|v| v.pointer("/error/message").and_then(Value::as_str).map(str::to_string))
        .unwrap_or_else(|| text.chars().take(160).collect())
}

/// GET returning (status, body, elapsed). The URL never appears in errors.
async fn get(req: reqwest::RequestBuilder) -> Result<(u16, String, Duration), String> {
    let t = Instant::now();
    let resp = req.send().await.map_err(|e| describe(&e.without_url()))?;
    let code = resp.status().as_u16();
    let body = resp.text().await.map_err(|e| e.without_url().to_string())?;
    Ok((code, body, t.elapsed()))
}

fn describe(e: &reqwest::Error) -> String {
    if e.is_timeout() {
        "timed out".into()
    } else if e.is_connect() {
        "could not connect (offline or blocked?)".into()
    } else {
        e.to_string()
    }
}

fn ms(d: Duration) -> String {
    format!("{} ms", d.as_millis())
}

/// Parse a `models.list` page into generateContent-capable models.
pub fn parse_model_list(body: &Value) -> (Vec<ModelInfo>, Option<String>) {
    let models = body
        .get("models")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter(|m| m.get("supportedGenerationMethods").and_then(Value::as_array).is_some_and(|ms| ms.iter().any(|x| x.as_str() == Some("generateContent"))))
                .filter_map(|m| {
                    let id = m.get("name")?.as_str()?.trim_start_matches("models/").to_string();
                    let display_name = m.get("displayName").and_then(Value::as_str).unwrap_or(&id).to_string();
                    Some(ModelInfo { id, display_name })
                })
                .collect()
        })
        .unwrap_or_default();
    let next = body.get("nextPageToken").and_then(Value::as_str).filter(|s| !s.is_empty()).map(str::to_string);
    (models, next)
}

/// Every Gemini model this key can use for `generateContent`, sorted by id.
pub fn list_gemini_models(api_key: &str) -> Result<Vec<ModelInfo>, String> {
    let (rt, client) = (runtime()?, client()?);
    rt.block_on(async {
        let mut out = Vec::new();
        let mut page: Option<String> = None;
        for _ in 0..10 {
            let mut req = client.get(format!("{GEMINI_API}/models")).header("x-goog-api-key", api_key).query(&[("pageSize", "1000")]);
            if let Some(p) = &page {
                req = req.query(&[("pageToken", p.as_str())]);
            }
            let (code, body, _) = get(req).await?;
            if code != 200 {
                return Err(format!("HTTP {code}: {}", error_detail(&body)));
            }
            let v: Value = serde_json::from_str(&body).map_err(|e| format!("invalid JSON: {e}"))?;
            let (models, next) = parse_model_list(&v);
            out.extend(models);
            match next {
                Some(n) => page = Some(n),
                None => break,
            }
        }
        out.sort_by(|a, b| a.id.cmp(&b.id));
        out.dedup_by(|a, b| a.id == b.id);
        Ok(out)
    })
}

/// Check the configured provider end to end: key/endpoint reachability,
/// whether the chosen model exists, and a tiny real request with latency.
pub fn test_connection(config: &AiConfig) -> ConnectionReport {
    let mut r = ConnectionReport::default();
    let (rt, client) = match (runtime(), client()) {
        (Ok(rt), Ok(c)) => (rt, c),
        (Err(e), _) | (_, Err(e)) => {
            r.push(false, format!("could not start the network client: {e}"));
            return r;
        }
    };
    rt.block_on(async {
        match config.provider {
            AiProviderKind::Gemini4 => gemini_checks(&client, config, &mut r).await,
            AiProviderKind::OpenAiCompatible => openai_checks(&client, config, &mut r).await,
            AiProviderKind::Ollama => ollama_checks(&client, config, &mut r).await,
        }
    });
    r
}

async fn gemini_checks(client: &reqwest::Client, config: &AiConfig, r: &mut ConnectionReport) {
    let Some(key) = config.effective_gemini_key() else {
        r.push(false, "No Gemini API key set (paste one above, or set GEMINI_API_KEY)");
        return;
    };
    let model = config.effective_gemini_model();
    // 1. model metadata: proves the key works and the model id exists
    match get(client.get(format!("{GEMINI_API}/models/{model}")).header("x-goog-api-key", &key)).await {
        Ok((200, body, t)) => {
            let name = serde_json::from_str::<Value>(&body).ok().and_then(|v| v.get("displayName").and_then(Value::as_str).map(str::to_string)).unwrap_or_else(|| model.clone());
            r.push(true, format!("Key accepted · model “{model}” found ({name}) · {}", ms(t)));
        }
        Ok((404, _, _)) => {
            r.push(false, format!("Model “{model}” does not exist for this key — pick one from “Refresh model list”"));
            return;
        }
        Ok((code @ (400 | 401 | 403), body, _)) => {
            r.push(false, format!("Key rejected (HTTP {code}): {}", error_detail(&body)));
            return;
        }
        Ok((code, body, _)) => {
            r.push(false, format!("Model lookup HTTP {code}: {}", error_detail(&body)));
            return;
        }
        Err(e) => {
            r.push(false, format!("Google API unreachable: {e}"));
            return;
        }
    }
    // 2. a tiny generation: proves the model is currently serving
    let mut models = vec![model.clone()];
    models.extend(config.gemini_fallback());
    for (i, m) in models.iter().enumerate() {
        let body = json!({
            "contents": [{ "role": "user", "parts": [{ "text": "Reply with the single word OK." }] }],
            "generationConfig": { "maxOutputTokens": 16, "temperature": 0 }
        });
        let req = client.post(format!("{GEMINI_API}/models/{m}:generateContent")).header("x-goog-api-key", &key).json(&body);
        match get(req).await {
            Ok((200, _, t)) => {
                let note = if i > 0 { " (fallback model — the Copilot will use it while the main one is busy)" } else { "" };
                r.push(true, format!("“{m}” answered in {}{note}", ms(t)));
                return;
            }
            Ok((code @ (429 | 503 | 529), body, t)) => {
                r.push(false, format!("“{m}” is busy (HTTP {code}, {}): {}", ms(t), error_detail(&body)));
            }
            Ok((code, body, _)) => {
                r.push(false, format!("“{m}” failed (HTTP {code}): {}", error_detail(&body)));
                return;
            }
            Err(e) => {
                r.push(false, format!("“{m}” request {e}"));
                return;
            }
        }
    }
}

async fn openai_checks(client: &reqwest::Client, config: &AiConfig, r: &mut ConnectionReport) {
    let Some(key) = config.effective_openai_key() else {
        r.push(false, "No OpenAI/OpenRouter API key set");
        return;
    };
    let base = config.openai_base_url.trim_end_matches('/');
    match get(client.get(format!("{base}/models")).bearer_auth(&key)).await {
        Ok((200, body, t)) => {
            let listed = serde_json::from_str::<Value>(&body)
                .ok()
                .and_then(|v| v.get("data").and_then(Value::as_array).cloned())
                .is_some_and(|d| d.iter().any(|m| m.get("id").and_then(Value::as_str) == Some(config.openai_model.as_str())));
            r.push(true, format!("Key accepted · {}", ms(t)));
            r.push(
                listed,
                if listed {
                    format!("Model “{}” is available", config.openai_model)
                } else {
                    format!("Model “{}” is not in this endpoint's model list", config.openai_model)
                },
            );
        }
        Ok((code, body, _)) => r.push(false, format!("HTTP {code}: {}", error_detail(&body))),
        Err(e) => r.push(false, format!("Endpoint unreachable: {e}")),
    }
}

async fn ollama_checks(client: &reqwest::Client, config: &AiConfig, r: &mut ConnectionReport) {
    let base = config.ollama_url.trim_end_matches('/');
    match get(client.get(format!("{base}/api/tags"))).await {
        Ok((200, body, t)) => {
            let want = config.ollama_model.trim();
            let has = serde_json::from_str::<Value>(&body)
                .ok()
                .and_then(|v| v.get("models").and_then(Value::as_array).cloned())
                .is_some_and(|ms| ms.iter().filter_map(|m| m.get("name").and_then(Value::as_str)).any(|n| n == want || n.split(':').next() == Some(want)));
            r.push(true, format!("Ollama is running · {}", ms(t)));
            r.push(has, if has { format!("Model “{want}” is installed") } else { format!("Model “{want}” is not pulled — run: ollama pull {want}") });
        }
        Ok((code, body, _)) => r.push(false, format!("Ollama HTTP {code}: {}", error_detail(&body))),
        Err(e) => r.push(false, format!("Ollama not reachable at {base}: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_list_keeps_only_generate_content_models() {
        let v = json!({
            "models": [
                { "name": "models/gemini-4-argon", "displayName": "Gemini 4 Argon", "supportedGenerationMethods": ["generateContent", "countTokens"] },
                { "name": "models/text-embedding-004", "supportedGenerationMethods": ["embedContent"] },
                { "name": "models/gemini-flash-latest", "supportedGenerationMethods": ["generateContent"] }
            ],
            "nextPageToken": "abc"
        });
        let (m, next) = parse_model_list(&v);
        assert_eq!(m.iter().map(|x| x.id.as_str()).collect::<Vec<_>>(), ["gemini-4-argon", "gemini-flash-latest"]);
        assert_eq!(m[1].display_name, "gemini-flash-latest");
        assert_eq!(next.as_deref(), Some("abc"));
    }

    #[test]
    fn missing_key_is_reported_without_network() {
        let mut c = AiConfig::default();
        c.gemini_api_key.clear();
        if c.effective_gemini_key().is_some() {
            return; // GEMINI_API_KEY is set in this environment
        }
        let r = test_connection(&c);
        assert!(!r.ok());
        assert!(r.lines[0].text.contains("No Gemini API key"));
    }
}
