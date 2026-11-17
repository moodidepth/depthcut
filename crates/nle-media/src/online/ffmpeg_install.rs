//! One-click FFmpeg install for Windows.
//!
//! Rust Cut Pro does not ship FFmpeg (the common Windows builds are GPL). When
//! no FFmpeg is found, the user can let the editor fetch the gyan.dev
//! "release essentials" build: the published SHA-256 is downloaded first, the
//! zip is streamed to disk and checked against it, and only `ffmpeg.exe`,
//! `ffprobe.exe` and the license file are extracted into
//! [`crate::ffmpeg_cli::managed_bin_dir`].

use crate::ffmpeg_cli;
use sha2::{Digest, Sha256};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

/// gyan.dev "release essentials" build (redirects to the current version).
pub const FFMPEG_ZIP_URL: &str = "https://www.gyan.dev/ffmpeg/builds/ffmpeg-release-essentials.zip";
/// Hex SHA-256 of [`FFMPEG_ZIP_URL`], published next to it.
pub const FFMPEG_SHA256_URL: &str = "https://www.gyan.dev/ffmpeg/builds/ffmpeg-release-essentials.zip.sha256";

/// Install progress reported to the UI.
#[derive(Clone, Debug, PartialEq)]
pub enum InstallStep {
    Checksum,
    Downloading { done: u64, total: Option<u64> },
    Verifying,
    Extracting,
}

impl InstallStep {
    /// 0..1 overall progress (download dominates).
    pub fn fraction(&self) -> f32 {
        match self {
            InstallStep::Checksum => 0.0,
            InstallStep::Downloading { done, total: Some(t) } if *t > 0 => 0.9 * (*done as f32 / *t as f32),
            InstallStep::Downloading { .. } => 0.0,
            InstallStep::Verifying => 0.92,
            InstallStep::Extracting => 0.96,
        }
    }

    pub fn label(&self) -> String {
        match self {
            InstallStep::Checksum => "Fetching checksum…".into(),
            InstallStep::Downloading { done, total } => match total {
                Some(t) => format!("Downloading FFmpeg… {:.0} / {:.0} MB", *done as f64 / 1e6, *t as f64 / 1e6),
                None => format!("Downloading FFmpeg… {:.0} MB", *done as f64 / 1e6),
            },
            InstallStep::Verifying => "Verifying SHA-256…".into(),
            InstallStep::Extracting => "Extracting ffmpeg.exe and ffprobe.exe…".into(),
        }
    }
}

/// Parse a `.sha256` file: the first 64-hex-digit token.
pub fn parse_sha256(text: &str) -> Option<String> {
    text.split_whitespace()
        .find(|t| t.len() == 64 && t.chars().all(|c| c.is_ascii_hexdigit()))
        .map(|t| t.to_ascii_lowercase())
}

/// Hex SHA-256 of a file.
pub fn sha256_file(path: &Path) -> std::io::Result<String> {
    let mut f = std::fs::File::open(path)?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(h.finalize().iter().map(|b| format!("{b:02x}")).collect())
}

/// Pull `bin/ffmpeg(.exe)`, `bin/ffprobe(.exe)` and `LICENSE` out of the
/// archive into `dest`. Each file is written to `<name>.part` and renamed, so
/// a failed extraction never leaves a half-written executable behind.
pub fn extract_tools(zip_path: &Path, dest: &Path) -> Result<Vec<PathBuf>, String> {
    let file = std::fs::File::open(zip_path).map_err(|e| e.to_string())?;
    let mut zip = zip::ZipArchive::new(file).map_err(|e| format!("not a valid zip: {e}"))?;
    std::fs::create_dir_all(dest).map_err(|e| format!("create {}: {e}", dest.display()))?;
    let wanted = ["ffmpeg.exe", "ffprobe.exe", "ffmpeg", "ffprobe"];
    let mut written = Vec::new();
    for i in 0..zip.len() {
        let mut entry = zip.by_index(i).map_err(|e| e.to_string())?;
        if entry.is_dir() {
            continue;
        }
        let Some(rel) = entry.enclosed_name() else { continue };
        let parent = rel.parent().and_then(|p| p.file_name()).map(|n| n.to_string_lossy().to_ascii_lowercase());
        let name = rel.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let lower = name.to_ascii_lowercase();
        let out_name = if parent.as_deref() == Some("bin") && wanted.contains(&lower.as_str()) {
            lower
        } else if rel.components().count() <= 2 && lower == "license" {
            "FFMPEG-LICENSE.txt".to_string()
        } else {
            continue;
        };
        let target = dest.join(&out_name);
        let part = dest.join(format!("{out_name}.part"));
        let res = (|| -> std::io::Result<()> {
            let mut out = std::fs::File::create(&part)?;
            std::io::copy(&mut entry, &mut out)?;
            out.flush()?;
            Ok(())
        })();
        if let Err(e) = res {
            let _ = std::fs::remove_file(&part);
            return Err(format!("extract {name}: {e}"));
        }
        let _ = std::fs::remove_file(&target);
        std::fs::rename(&part, &target).map_err(|e| format!("install {}: {e}", target.display()))?;
        written.push(target);
    }
    let have = |n: &str| written.iter().any(|p| p.file_name().is_some_and(|f| f.to_string_lossy().to_ascii_lowercase().starts_with(n)));
    if !have("ffmpeg") || !have("ffprobe") {
        return Err("the archive did not contain bin/ffmpeg and bin/ffprobe".into());
    }
    Ok(written)
}

/// Download, verify and install FFmpeg into the editor's own `bin` folder.
/// Blocking; run it on a background job. Returns the installed `ffmpeg` path.
pub fn install_ffmpeg(mut progress: impl FnMut(InstallStep), cancelled: impl Fn() -> bool) -> Result<PathBuf, String> {
    progress(InstallStep::Checksum);
    let sha_text = super::block_on(async {
        let resp = super::client().get(FFMPEG_SHA256_URL).send().await.map_err(|e| e.to_string())?;
        if !resp.status().is_success() {
            return Err(format!("checksum: HTTP {}", resp.status().as_u16()));
        }
        resp.text().await.map_err(|e| e.to_string())
    })?;
    let expected = parse_sha256(&sha_text).ok_or("the published checksum file was not a SHA-256")?;

    let bin = ffmpeg_cli::managed_bin_dir();
    let tmp_dir = crate::paths::local_data_dir().join("downloads");
    let zip_path = tmp_dir.join("ffmpeg-release-essentials.zip");
    let result = (|| {
        super::download::download_blocking(FFMPEG_ZIP_URL, &zip_path, |done, total| progress(InstallStep::Downloading { done, total }), &cancelled)?;
        if cancelled() {
            return Err("Cancelled".to_string());
        }
        progress(InstallStep::Verifying);
        let got = sha256_file(&zip_path).map_err(|e| e.to_string())?;
        if got != expected {
            return Err(format!(
                "checksum mismatch (expected {expected}, got {got}). The build may have just been updated — try again."
            ));
        }
        progress(InstallStep::Extracting);
        extract_tools(&zip_path, &bin)
    })();
    let _ = std::fs::remove_file(&zip_path);
    result?;
    ffmpeg_cli::refresh();
    let ff = bin.join(if cfg!(windows) { "ffmpeg.exe" } else { "ffmpeg" });
    if ffmpeg_cli::run_capture(&ff, &["-hide_banner", "-version"]).is_none() {
        return Err(format!("installed {} but it does not run", ff.display()));
    }
    log::info!("FFmpeg installed to {}", bin.display());
    Ok(ff)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_bare_and_sha256sum_style_checksums() {
        let h = "60f467265b1e312373dbcd92200c2618a74850f98d3d078e94296bb3fa2047ba";
        assert_eq!(parse_sha256(h).as_deref(), Some(h));
        assert_eq!(parse_sha256(&format!("{}  ffmpeg.zip\n", h.to_uppercase())).as_deref(), Some(h));
        assert_eq!(parse_sha256("<html>not found</html>"), None);
    }

    /// Real network run against gyan.dev (~115 MB) into a temp folder:
    /// `cargo test -p nle-media --lib real_gyan_download -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn real_gyan_download_verifies_and_extracts() {
        let dir = std::env::temp_dir().join(format!("rcp-ffreal-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let sha = crate::online::block_on(async { crate::online::client().get(FFMPEG_SHA256_URL).send().await.unwrap().text().await.map_err(|e| e.to_string()) }).unwrap();
        let expected = parse_sha256(&sha).expect("checksum file");
        let zip = dir.join("ff.zip");
        crate::online::download::download_blocking(FFMPEG_ZIP_URL, &zip, |_, _| {}, || false).unwrap();
        assert_eq!(sha256_file(&zip).unwrap(), expected);
        let files = extract_tools(&zip, &dir.join("bin")).unwrap();
        println!("extracted: {files:?}");
        let ff = dir.join("bin").join(if cfg!(windows) { "ffmpeg.exe" } else { "ffmpeg" });
        let v = ffmpeg_cli::run_capture(&ff, &["-hide_banner", "-version"]).expect("extracted ffmpeg runs");
        println!("{}", v.lines().next().unwrap_or(""));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn extracts_only_the_tools_and_license() {
        let dir = std::env::temp_dir().join(format!("rcp-ffzip-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let zp = dir.join("t.zip");
        {
            let mut w = zip::ZipWriter::new(std::fs::File::create(&zp).unwrap());
            let opt = zip::write::SimpleFileOptions::default();
            for (n, body) in [
                ("ffmpeg-9.0-essentials_build/bin/ffmpeg.exe", "FF"),
                ("ffmpeg-9.0-essentials_build/bin/ffprobe.exe", "FP"),
                ("ffmpeg-9.0-essentials_build/bin/ffplay.exe", "PLAY"),
                ("ffmpeg-9.0-essentials_build/LICENSE", "GPL"),
                ("ffmpeg-9.0-essentials_build/doc/ffmpeg.html", "doc"),
                ("../evil/bin/ffmpeg.exe", "EVIL"),
            ] {
                w.start_file(n, opt).unwrap();
                w.write_all(body.as_bytes()).unwrap();
            }
            w.finish().unwrap();
        }
        let out = dir.join("bin");
        let files = extract_tools(&zp, &out).unwrap();
        let mut names: Vec<_> = files.iter().map(|p| p.file_name().unwrap().to_string_lossy().into_owned()).collect();
        names.sort();
        assert_eq!(names, vec!["FFMPEG-LICENSE.txt", "ffmpeg.exe", "ffprobe.exe"]);
        assert_eq!(std::fs::read_to_string(out.join("ffmpeg.exe")).unwrap(), "FF");
        assert!(!dir.join("evil").exists());
        assert_eq!(sha256_file(&out.join("ffprobe.exe")).unwrap().len(), 64);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
