//! Streamed HTTP downloads to `<dest>.part` with progress and cancellation,
//! renamed into place only when complete.

use super::client;
use futures_util::StreamExt;
use std::path::{Path, PathBuf};
use tokio::io::AsyncWriteExt;

/// Download `url` to `dest`. `progress(done_bytes, total_bytes)` is called as
/// chunks arrive; `cancelled()` aborts (the partial file is removed).
pub async fn download_to(url: &str, dest: &Path, mut progress: impl FnMut(u64, Option<u64>), cancelled: impl Fn() -> bool) -> Result<PathBuf, String> {
    if let Some(dir) = dest.parent() {
        tokio::fs::create_dir_all(dir).await.map_err(|e| format!("create {}: {e}", dir.display()))?;
    }
    let resp = client().get(url).send().await.map_err(|e| e.to_string())?;
    let status = resp.status();
    if !status.is_success() {
        return Err(format!("HTTP {} for {url}", status.as_u16()));
    }
    let total = resp.content_length();
    let mut part = dest.as_os_str().to_owned();
    part.push(".part");
    let part = PathBuf::from(part);
    let mut file = tokio::fs::File::create(&part).await.map_err(|e| format!("create {}: {e}", part.display()))?;
    let mut stream = resp.bytes_stream();
    let mut done = 0u64;
    progress(0, total);
    let result: Result<(), String> = async {
        while let Some(chunk) = stream.next().await {
            if cancelled() {
                return Err("Cancelled".into());
            }
            let chunk = chunk.map_err(|e| e.to_string())?;
            file.write_all(&chunk).await.map_err(|e| e.to_string())?;
            done += chunk.len() as u64;
            progress(done, total);
        }
        file.flush().await.map_err(|e| e.to_string())?;
        Ok(())
    }
    .await;
    drop(file);
    if let Err(e) = result {
        let _ = tokio::fs::remove_file(&part).await;
        return Err(e);
    }
    if let Some(t) = total {
        if done != t {
            let _ = tokio::fs::remove_file(&part).await;
            return Err(format!("incomplete download ({done} of {t} bytes)"));
        }
    }
    let _ = tokio::fs::remove_file(dest).await;
    tokio::fs::rename(&part, dest).await.map_err(|e| format!("rename to {}: {e}", dest.display()))?;
    Ok(dest.to_path_buf())
}

/// Blocking wrapper around [`download_to`].
pub fn download_blocking(url: &str, dest: &Path, progress: impl FnMut(u64, Option<u64>), cancelled: impl Fn() -> bool) -> Result<PathBuf, String> {
    super::block_on(download_to(url, dest, progress, cancelled))
}

/// File extension for a result's audio (from the URL path or the declared type).
pub fn extension_for(url: &str, filetype: Option<&str>) -> String {
    let path = url.split(['?', '#']).next().unwrap_or(url);
    let last = path.rsplit('/').next().unwrap_or("");
    if let Some((_, ext)) = last.rsplit_once('.') {
        let ext = ext.to_ascii_lowercase();
        if (2..=4).contains(&ext.len()) && ext.chars().all(|c| c.is_ascii_alphanumeric()) {
            return ext;
        }
    }
    filetype.filter(|f| !f.is_empty()).map(|f| f.to_ascii_lowercase()).unwrap_or_else(|| "mp3".into())
}

/// Download a search result into the downloads cache (re-using a previous
/// download of the same item) and return the local path.
pub fn fetch_result(result: &super::SearchResult, progress: impl FnMut(u64, Option<u64>), cancelled: impl Fn() -> bool) -> Result<PathBuf, String> {
    super::block_on(async {
        let url = result.resolve_audio_url().await?;
        let ext = extension_for(&url, result.filetype.as_deref());
        let stem = crate::paths::sanitize_file_name(&result.title, 60);
        let id = crate::paths::sanitize_file_name(&result.id, 40);
        let dest = crate::paths::downloads_dir().join(format!("{stem} [{id}].{ext}"));
        if dest.metadata().map(|m| m.len() > 0).unwrap_or(false) {
            return Ok(dest);
        }
        download_to(&url, &dest, progress, cancelled).await
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extensions() {
        assert_eq!(extension_for("https://cdn.freesound.org/previews/1/1-hq.mp3", None), "mp3");
        assert_eq!(extension_for("https://x/a.OGG?dl=1", None), "ogg");
        assert_eq!(extension_for("https://x/download/123", Some("wav")), "wav");
        assert_eq!(extension_for("https://x/download/123", None), "mp3");
    }
}
