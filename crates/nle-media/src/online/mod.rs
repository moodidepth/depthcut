//! Free, key-less online media: Openverse (CC0 / CC-BY audio from Freesound,
//! Jamendo, Wikimedia Commons…) and the Internet Archive (public-domain and
//! Creative Commons audio), streamed downloads with progress, the Whisper
//! model manager and optional `yt-dlp` URL import.
//!
//! Network I/O is async (`reqwest` + `tokio`, rustls) on a small shared
//! runtime; callers use the blocking helpers from background [`crate::jobs`].

pub mod archive;
pub mod download;
pub mod ffmpeg_install;
pub mod models;
pub mod openverse;
pub mod ytdlp;

use std::sync::OnceLock;
use std::time::Duration;

/// Generic client name (no personal data is ever sent).
pub const USER_AGENT: &str = concat!("RustCutPro/", env!("CARGO_PKG_VERSION"), " (open-source video editor)");

static RUNTIME: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();

/// Shared 2-thread tokio runtime for network I/O.
pub fn runtime() -> &'static tokio::runtime::Runtime {
    RUNTIME.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread().worker_threads(2).thread_name("rcp-net").enable_all().build().expect("tokio runtime")
    })
}

pub fn client() -> &'static reqwest::Client {
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .user_agent(USER_AGENT)
            .connect_timeout(Duration::from_secs(15))
            .timeout(Duration::from_secs(60 * 60))
            .build()
            .expect("http client")
    })
}

/// Run a future to completion on the shared runtime (from a non-async thread).
pub fn block_on<F: std::future::Future>(f: F) -> F::Output {
    runtime().block_on(f)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Provider {
    Openverse,
    InternetArchive,
}

impl Provider {
    pub const ALL: [Provider; 2] = [Provider::Openverse, Provider::InternetArchive];
    pub fn label(self) -> &'static str {
        match self {
            Provider::Openverse => "Openverse",
            Provider::InternetArchive => "Internet Archive",
        }
    }
}

/// One search hit, normalised across providers.
#[derive(Clone, Debug, PartialEq)]
pub struct SearchResult {
    pub provider: Provider,
    /// Provider-specific id (Openverse UUID / Archive identifier).
    pub id: String,
    pub title: String,
    pub creator: String,
    pub duration_secs: Option<f64>,
    /// Short badge: "CC0", "CC-BY", "CC-BY-SA", "PD", …
    pub license: String,
    /// Full licence name for credits, e.g. "CC BY 4.0".
    pub license_name: String,
    pub license_url: String,
    /// Page of the original work.
    pub landing_url: String,
    /// Direct audio URL (Openverse). Archive items are resolved on demand.
    pub audio_url: Option<String>,
    pub source: String,
    pub tags: Vec<String>,
    pub filetype: Option<String>,
}

impl SearchResult {
    pub fn attribution(&self) -> nle_core::Attribution {
        nle_core::Attribution {
            title: self.title.clone(),
            creator: self.creator.clone(),
            license: self.license_name.clone(),
            license_url: self.license_url.clone(),
            source_url: self.landing_url.clone(),
            provider: self.source.clone(),
        }
    }

    pub fn requires_credit(&self) -> bool {
        self.attribution().requires_credit()
    }

    /// Resolve the downloadable audio URL (fetches Archive metadata if needed).
    pub async fn resolve_audio_url(&self) -> Result<String, String> {
        if let Some(u) = &self.audio_url {
            return Ok(u.clone());
        }
        match self.provider {
            Provider::InternetArchive => archive::best_audio_file(&self.id).await.map(|f| f.url),
            Provider::Openverse => Err("result has no audio URL".into()),
        }
    }
}

/// Search the selected providers concurrently. Returns results (Openverse
/// first) plus one error message per provider that failed.
pub async fn search(query: &str, providers: &[Provider]) -> (Vec<SearchResult>, Vec<String>) {
    let q = query.trim();
    if q.is_empty() {
        return (Vec::new(), Vec::new());
    }
    let ov = async {
        if providers.contains(&Provider::Openverse) {
            Some(openverse::search(q).await)
        } else {
            None
        }
    };
    let ia = async {
        if providers.contains(&Provider::InternetArchive) {
            Some(archive::search(q).await)
        } else {
            None
        }
    };
    let (a, b) = tokio::join!(ov, ia);
    let mut out = Vec::new();
    let mut errors = Vec::new();
    for (p, r) in [(Provider::Openverse, a), (Provider::InternetArchive, b)] {
        match r {
            Some(Ok(v)) => out.extend(v),
            Some(Err(e)) => errors.push(format!("{}: {e}", p.label())),
            None => {}
        }
    }
    (out, errors)
}

/// Blocking wrapper around [`search`].
pub fn search_blocking(query: &str, providers: &[Provider]) -> (Vec<SearchResult>, Vec<String>) {
    block_on(search(query, providers))
}

/// Short badge for a Creative Commons licence code / URL.
pub fn license_badge(code_or_url: &str) -> String {
    let l = code_or_url.to_ascii_lowercase();
    if l.is_empty() {
        return "Unknown".into();
    }
    if l == "cc0" || l.contains("publicdomain/zero") {
        return "CC0".into();
    }
    if l == "pdm" || l.contains("publicdomain") || l.contains("public domain") {
        return "PD".into();
    }
    let code = if l.contains("/licenses/") { l.split("/licenses/").nth(1).and_then(|r| r.split('/').next()).unwrap_or("").to_string() } else { l };
    match code.as_str() {
        "by" => "CC-BY".into(),
        "by-sa" => "CC-BY-SA".into(),
        "by-nd" => "CC-BY-ND".into(),
        "by-nc" => "CC-BY-NC".into(),
        "by-nc-sa" => "CC-BY-NC-SA".into(),
        "by-nc-nd" => "CC-BY-NC-ND".into(),
        "sampling+" => "Sampling+".into(),
        other if !other.is_empty() => other.to_ascii_uppercase(),
        _ => "Unknown".into(),
    }
}

/// Human licence name ("CC BY 4.0") from a code + version.
pub fn license_name(code: &str, version: &str) -> String {
    let badge = license_badge(code);
    let name = match badge.as_str() {
        "CC0" => "CC0".to_string(),
        "PD" => "Public Domain".to_string(),
        b if b.starts_with("CC-") => format!("CC {}", b[3..].replace('-', "-")),
        b => b.to_string(),
    };
    if version.is_empty() || badge == "PD" {
        name
    } else {
        format!("{name} {version}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn license_badges_and_names() {
        assert_eq!(license_badge("cc0"), "CC0");
        assert_eq!(license_badge("by"), "CC-BY");
        assert_eq!(license_badge("https://creativecommons.org/licenses/by/4.0/"), "CC-BY");
        assert_eq!(license_badge("http://creativecommons.org/licenses/by-nc-sa/2.5/"), "CC-BY-NC-SA");
        assert_eq!(license_badge("http://creativecommons.org/publicdomain/zero/1.0/"), "CC0");
        assert_eq!(license_badge("http://creativecommons.org/licenses/publicdomain/"), "PD");
        assert_eq!(license_badge("pdm"), "PD");
        assert_eq!(license_badge(""), "Unknown");
        assert_eq!(license_name("by", "4.0"), "CC BY 4.0");
        assert_eq!(license_name("cc0", "1.0"), "CC0 1.0");
        assert_eq!(license_name("pdm", "1.0"), "Public Domain");
    }
}
