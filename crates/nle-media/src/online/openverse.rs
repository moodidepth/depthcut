//! Openverse audio search — <https://api.openverse.org/v1/audio/> — no key.

use super::{client, license_badge, license_name, Provider, SearchResult};
use serde_json::Value;

pub const ENDPOINT: &str = "https://api.openverse.org/v1/audio/";
/// Requested page size; anonymous clients are capped at 20 and are retried.
pub const PAGE_SIZE: u32 = 25;
const ANON_PAGE_SIZE: u32 = 20;
/// Licences that allow commercial re-use with (at most) a credit.
pub const LICENSES: &str = "cc0,pdm,by";

fn str_of(v: &Value, k: &str) -> String {
    v.get(k).and_then(Value::as_str).unwrap_or_default().trim().to_string()
}

/// Parse an Openverse `/v1/audio/` response.
pub fn parse(body: &Value) -> Vec<SearchResult> {
    let Some(results) = body.get("results").and_then(Value::as_array) else { return Vec::new() };
    results
        .iter()
        .filter_map(|r| {
            let url = str_of(r, "url");
            if url.is_empty() {
                return None;
            }
            let code = str_of(r, "license");
            let version = str_of(r, "license_version");
            let provider = {
                let s = str_of(r, "source");
                if s.is_empty() {
                    str_of(r, "provider")
                } else {
                    s
                }
            };
            Some(SearchResult {
                provider: Provider::Openverse,
                id: str_of(r, "id"),
                title: {
                    let t = str_of(r, "title");
                    if t.is_empty() {
                        "Untitled".into()
                    } else {
                        t
                    }
                },
                creator: str_of(r, "creator"),
                duration_secs: r.get("duration").and_then(Value::as_f64).map(|ms| ms / 1000.0),
                license: license_badge(&code),
                license_name: license_name(&code, &version),
                license_url: str_of(r, "license_url"),
                landing_url: str_of(r, "foreign_landing_url"),
                audio_url: Some(url),
                source: pretty_source(&provider),
                tags: r
                    .get("tags")
                    .and_then(Value::as_array)
                    .map(|t| t.iter().filter_map(|x| x.get("name").and_then(Value::as_str)).take(8).map(str::to_string).collect())
                    .unwrap_or_default(),
                filetype: r.get("filetype").and_then(Value::as_str).map(str::to_string),
            })
        })
        .collect()
}

fn pretty_source(s: &str) -> String {
    match s {
        "freesound" => "Freesound".into(),
        "jamendo" => "Jamendo".into(),
        "wikimedia_audio" | "wikimedia" => "Wikimedia Commons".into(),
        "ccmixter" => "ccMixter".into(),
        other => other.to_string(),
    }
}

async fn fetch(query: &str, page_size: u32) -> Result<(u16, Value), String> {
    let resp = client()
        .get(ENDPOINT)
        .query(&[("q", query), ("page_size", &page_size.to_string()), ("license", LICENSES)])
        .send()
        .await
        .map_err(|e| e.to_string())?;
    let status = resp.status().as_u16();
    let v: Value = resp.json().await.map_err(|e| e.to_string())?;
    Ok((status, v))
}

pub async fn search(query: &str) -> Result<Vec<SearchResult>, String> {
    let (mut status, mut body) = fetch(query, PAGE_SIZE).await?;
    if status == 400 || status == 401 {
        (status, body) = fetch(query, ANON_PAGE_SIZE).await?;
    }
    if !(200..300).contains(&status) {
        let detail = body.get("detail").and_then(Value::as_str).unwrap_or("request failed");
        return Err(format!("HTTP {status}: {detail}"));
    }
    Ok(parse(&body))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_api_results() {
        let body: Value = serde_json::from_str(
            r#"{"result_count":2,"results":[
              {"id":"6b07","title":"Rain, Moderate","url":"https://cdn.freesound.org/x.mp3","creator":"InspectorJ",
               "license":"by","license_version":"4.0","license_url":"https://creativecommons.org/licenses/by/4.0/",
               "provider":"freesound","source":"freesound","filetype":"mp3","duration":60116,
               "foreign_landing_url":"https://freesound.org/people/InspectorJ/sounds/401275","tags":[{"name":"Rain"},{"name":"Water"}]},
              {"id":"no-url","title":"Broken","url":null}
            ]}"#,
        )
        .unwrap();
        let r = parse(&body);
        assert_eq!(r.len(), 1);
        let a = &r[0];
        assert_eq!(a.license, "CC-BY");
        assert_eq!(a.license_name, "CC BY 4.0");
        assert_eq!(a.source, "Freesound");
        assert!((a.duration_secs.unwrap() - 60.116).abs() < 1e-9);
        assert_eq!(a.tags, ["Rain", "Water"]);
        assert!(a.requires_credit());
        assert!(a.attribution().credit_line().contains("InspectorJ"));
    }
}
