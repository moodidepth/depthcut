//! Internet Archive audio search (`advancedsearch.php`) and file resolution
//! (`/metadata/{id}`) — public-domain and Creative Commons items, no key.

use super::{client, license_badge, license_name, Provider, SearchResult};
use serde_json::Value;

pub const SEARCH: &str = "https://archive.org/advancedsearch.php";
pub const ROWS: u32 = 25;

/// Solr query: the user's terms, audio only, with a known PD / CC licence,
/// excluding podcasts and LibriVox audiobooks.
pub fn build_query(terms: &str) -> String {
    let t = terms.replace(['(', ')', ':', '"'], " ");
    format!(
        "({}) AND mediatype:(audio) AND (licenseurl:*publicdomain* OR licenseurl:*creativecommons*) AND -collection:(podcasts) AND -collection:(librivoxaudio)",
        t.trim()
    )
}

fn first_str(v: Option<&Value>) -> String {
    match v {
        Some(Value::String(s)) => s.trim().to_string(),
        Some(Value::Array(a)) => a.iter().filter_map(Value::as_str).next().unwrap_or_default().trim().to_string(),
        _ => String::new(),
    }
}

fn str_list(v: Option<&Value>) -> Vec<String> {
    match v {
        Some(Value::String(s)) => s.split(';').map(|x| x.trim().to_string()).filter(|x| !x.is_empty()).collect(),
        Some(Value::Array(a)) => a.iter().filter_map(Value::as_str).map(str::to_string).collect(),
        _ => Vec::new(),
    }
}

/// Parse an `advancedsearch.php?output=json` response.
pub fn parse(body: &Value) -> Vec<SearchResult> {
    let Some(docs) = body.pointer("/response/docs").and_then(Value::as_array) else { return Vec::new() };
    docs.iter()
        .filter_map(|d| {
            let id = first_str(d.get("identifier"));
            if id.is_empty() {
                return None;
            }
            let lic_url = first_str(d.get("licenseurl"));
            let badge = license_badge(&lic_url);
            let version = lic_url.trim_end_matches('/').rsplit('/').next().filter(|v| v.chars().next().is_some_and(|c| c.is_ascii_digit())).unwrap_or("");
            let code = match badge.as_str() {
                "CC0" => "cc0".to_string(),
                "PD" => "pdm".to_string(),
                b if b.starts_with("CC-") => b[3..].to_ascii_lowercase(),
                _ => lic_url.clone(),
            };
            let title = first_str(d.get("title"));
            Some(SearchResult {
                provider: Provider::InternetArchive,
                landing_url: format!("https://archive.org/details/{id}"),
                title: if title.is_empty() { id.clone() } else { title },
                creator: first_str(d.get("creator")),
                duration_secs: None,
                license: badge,
                license_name: license_name(&code, version),
                license_url: lic_url,
                audio_url: None,
                source: "Internet Archive".into(),
                tags: str_list(d.get("subject")).into_iter().take(8).collect(),
                filetype: None,
                id,
            })
        })
        .collect()
}

pub async fn search(terms: &str) -> Result<Vec<SearchResult>, String> {
    let q = build_query(terms);
    let resp = client()
        .get(SEARCH)
        .query(&[
            ("q", q.as_str()),
            ("fl[]", "identifier"),
            ("fl[]", "title"),
            ("fl[]", "creator"),
            ("fl[]", "licenseurl"),
            ("fl[]", "subject"),
            ("rows", &ROWS.to_string()),
            ("page", "1"),
            ("output", "json"),
        ])
        .send()
        .await
        .map_err(|e| e.to_string())?;
    let status = resp.status();
    if !status.is_success() {
        return Err(format!("HTTP {}", status.as_u16()));
    }
    let v: Value = resp.json().await.map_err(|e| e.to_string())?;
    Ok(parse(&v))
}

#[derive(Clone, Debug, PartialEq)]
pub struct ArchiveFile {
    pub name: String,
    pub url: String,
    pub duration_secs: Option<f64>,
}

/// "59:54", "3591.69" or "1:02:03" → seconds.
pub fn parse_length(s: &str) -> Option<f64> {
    let s = s.trim();
    if s.contains(':') {
        let mut secs = 0.0;
        for part in s.split(':') {
            secs = secs * 60.0 + part.trim().parse::<f64>().ok()?;
        }
        Some(secs)
    } else {
        s.parse().ok()
    }
}

fn format_rank(format: &str, name: &str) -> Option<u8> {
    let f = format.to_ascii_lowercase();
    let n = name.to_ascii_lowercase();
    if f.contains("mp3") || n.ends_with(".mp3") {
        Some(0)
    } else if f.contains("ogg") || n.ends_with(".ogg") {
        Some(1)
    } else if f.contains("flac") || n.ends_with(".flac") {
        Some(2)
    } else if f.contains("wave") || n.ends_with(".wav") {
        Some(3)
    } else if f.contains("aac") || n.ends_with(".m4a") || n.ends_with(".aac") {
        Some(4)
    } else {
        None
    }
}

/// Choose the best playable audio file from an item's metadata.
pub fn pick_audio_file(id: &str, meta: &Value) -> Option<ArchiveFile> {
    let files = meta.get("files")?.as_array()?;
    let mut best: Option<(u8, &Value)> = None;
    for f in files {
        let name = f.get("name").and_then(Value::as_str).unwrap_or_default();
        let format = f.get("format").and_then(Value::as_str).unwrap_or_default();
        if let Some(rank) = format_rank(format, name) {
            if best.is_none_or(|(r, _)| rank < r) {
                best = Some((rank, f));
            }
        }
    }
    let (_, f) = best?;
    let name = f.get("name")?.as_str()?.to_string();
    let duration_secs = match f.get("length") {
        Some(Value::String(s)) => parse_length(s),
        Some(Value::Number(n)) => n.as_f64(),
        _ => None,
    };
    let enc: String = name
        .split('/')
        .map(|seg| seg.bytes().map(|b| if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) { (b as char).to_string() } else { format!("%{b:02X}") }).collect::<String>())
        .collect::<Vec<_>>()
        .join("/");
    Some(ArchiveFile { url: format!("https://archive.org/download/{id}/{enc}"), name, duration_secs })
}

pub async fn best_audio_file(id: &str) -> Result<ArchiveFile, String> {
    let url = format!("https://archive.org/metadata/{id}");
    let v: Value = client().get(&url).send().await.map_err(|e| e.to_string())?.json().await.map_err(|e| e.to_string())?;
    pick_audio_file(id, &v).ok_or_else(|| format!("no playable audio file in {id}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_search_docs() {
        let v: Value = serde_json::from_str(
            r#"{"response":{"numFound":2,"docs":[
              {"identifier":"cjbeards-fire-and-thunder","title":"Fire And Thunder","creator":["CJ Beard","x"],
               "licenseurl":"https://creativecommons.org/licenses/by/4.0/","subject":"thunder; storm"},
              {"identifier":"noise","title":"Noise","licenseurl":"http://creativecommons.org/licenses/publicdomain/"}
            ]}}"#,
        )
        .unwrap();
        let r = parse(&v);
        assert_eq!(r.len(), 2);
        assert_eq!(r[0].creator, "CJ Beard");
        assert_eq!(r[0].license, "CC-BY");
        assert_eq!(r[0].license_name, "CC BY 4.0");
        assert_eq!(r[0].tags, ["thunder", "storm"]);
        assert_eq!(r[0].landing_url, "https://archive.org/details/cjbeards-fire-and-thunder");
        assert_eq!(r[1].license, "PD");
        assert!(!r[1].requires_credit());
        assert!(build_query("rain (heavy)").starts_with("(rain  heavy) AND mediatype:(audio)"));
    }

    #[test]
    fn picks_mp3_and_parses_lengths() {
        let meta: Value = serde_json::from_str(
            r#"{"files":[
              {"name":"a.aac","format":"Advanced Audio Coding","length":"3591.69"},
              {"name":"a.png","format":"PNG"},
              {"name":"sub dir/a b.mp3","format":"VBR MP3","length":"59:54"},
              {"name":"a.ogg","format":"Ogg Vorbis","length":"3593.98"}
            ]}"#,
        )
        .unwrap();
        let f = pick_audio_file("item", &meta).unwrap();
        assert_eq!(f.name, "sub dir/a b.mp3");
        assert_eq!(f.url, "https://archive.org/download/item/sub%20dir/a%20b.mp3");
        assert_eq!(f.duration_secs, Some(3594.0));
        assert_eq!(parse_length("1:02:03"), Some(3723.0));
        assert!(pick_audio_file("x", &serde_json::json!({"files":[{"name":"a.txt","format":"Text"}]})).is_none());
    }
}
