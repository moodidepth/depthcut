//! Per-user data folders: `%LOCALAPPDATA%\Depthcut\...` on Windows,
//! `$XDG_DATA_HOME/depthcut/...` (or `~/.local/share`) elsewhere.
//!
//! The app was called Rust Cut Pro before; [`app_dir`] moves an old
//! `RustCutPro` / `rust-cut-pro` folder to the new name the first time it is
//! asked for, so models, autosaves and settings carry over.

use std::path::{Path, PathBuf};

/// Folder name under `%LOCALAPPDATA%` / `%APPDATA%` on Windows.
pub const APP_DIR_WINDOWS: &str = "Depthcut";
/// Folder name under the XDG data dir elsewhere.
pub const APP_DIR_UNIX: &str = "depthcut";
const LEGACY_DIR_WINDOWS: &str = "RustCutPro";
const LEGACY_DIR_UNIX: &str = "rust-cut-pro";

fn names(windows_style: bool) -> (&'static str, &'static str) {
    if windows_style {
        (APP_DIR_WINDOWS, LEGACY_DIR_WINDOWS)
    } else {
        (APP_DIR_UNIX, LEGACY_DIR_UNIX)
    }
}

/// `base/<app folder>`: the new name, unless only a pre-rename folder exists
/// (not migrated yet, or the move failed), which is then used as it is.
/// Never moves anything, so tests and tools can call it freely.
pub fn app_dir(base: &Path, windows_style: bool) -> PathBuf {
    let (name, legacy) = names(windows_style);
    let new = base.join(name);
    let old = base.join(legacy);
    if !new.exists() && old.is_dir() {
        old
    } else {
        new
    }
}

/// Move a pre-rename `base/RustCutPro` (or `rust-cut-pro`) folder to the new
/// name. Called once by the app at startup, before anything opens files in
/// it. On failure (files in use, permissions) the old folder keeps being used.
pub fn migrate_legacy_dir(base: &Path, windows_style: bool) {
    let (name, legacy) = names(windows_style);
    let (new, old) = (base.join(name), base.join(legacy));
    if new.exists() || !old.is_dir() {
        return;
    }
    match std::fs::rename(&old, &new) {
        Ok(()) => log::info!("moved {} to {}", old.display(), new.display()),
        Err(e) => log::warn!("could not move {} to {} ({e}); still using the old folder", old.display(), new.display()),
    }
}

/// Base folder (and naming style) [`local_data_dir`] lives in.
fn local_base() -> Option<(PathBuf, bool)> {
    if cfg!(windows) {
        if let Some(local) = std::env::var_os("LOCALAPPDATA") {
            return Some((PathBuf::from(local), true));
        }
    }
    if let Some(x) = std::env::var_os("XDG_DATA_HOME") {
        return Some((PathBuf::from(x), false));
    }
    std::env::var_os("HOME").map(|home| (PathBuf::from(home).join(".local").join("share"), false))
}

/// App startup: rename the machine-local data folder from the old app name.
pub fn migrate_local_data_dir() {
    if std::env::var_os("RCP_DATA_DIR").is_none() {
        if let Some((base, win)) = local_base() {
            migrate_legacy_dir(&base, win);
        }
    }
}

/// Machine-local data root (large caches: models, downloads).
pub fn local_data_dir() -> PathBuf {
    if let Some(p) = std::env::var_os("RCP_DATA_DIR") {
        return PathBuf::from(p);
    }
    match local_base() {
        Some((base, win)) => app_dir(&base, win),
        None => std::env::temp_dir().join(APP_DIR_UNIX),
    }
}

/// Whisper GGML models.
pub fn models_dir() -> PathBuf {
    local_data_dir().join("models")
}

/// Media downloaded from online libraries / yt-dlp.
pub fn downloads_dir() -> PathBuf {
    local_data_dir().join("downloads")
}

/// Argon configuration & GPU cache folder (`%LOCALAPPDATA%\Argon` on Windows).
pub fn argon_data_dir() -> PathBuf {
    let dir = if let Some(p) = std::env::var_os("ARGON_DATA_DIR") {
        PathBuf::from(p)
    } else if cfg!(windows) {
        std::env::var_os("LOCALAPPDATA").map(|local| PathBuf::from(local).join("Argon")).unwrap_or_else(|| local_data_dir().join("Argon"))
    } else if let Some(x) = std::env::var_os("XDG_DATA_HOME") {
        PathBuf::from(x).join("argon")
    } else if let Some(home) = std::env::var_os("HOME") {
        PathBuf::from(home).join(".local").join("share").join("argon")
    } else {
        std::env::temp_dir().join("argon")
    };
    let _ = std::fs::create_dir_all(&dir);
    dir
}

/// Persistent wgpu pipeline/shader cache (`%LOCALAPPDATA%\Argon\shader_cache.bin`).
pub fn shader_cache_path() -> PathBuf {
    argon_data_dir().join("shader_cache.bin")
}

/// Cloud/Local AI Copilot configuration (`%LOCALAPPDATA%\Argon\ai_config.toml`).
pub fn ai_config_path() -> PathBuf {
    argon_data_dir().join("ai_config.toml")
}

/// A file-system safe version of `name` (keeps letters, digits, `-_. ()`).
pub fn sanitize_file_name(name: &str, max_len: usize) -> String {
    let mut s: String = name.chars().map(|c| if c.is_alphanumeric() || " -_.()[]".contains(c) { c } else { '_' }).collect::<String>().trim().trim_matches('.').to_string();
    if s.chars().count() > max_len {
        s = s.chars().take(max_len).collect();
    }
    if s.is_empty() {
        "download".into()
    } else {
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitizes_names() {
        assert_eq!(sanitize_file_name("Rain: on/a \"roof\"?", 64), "Rain_ on_a _roof__");
        assert_eq!(sanitize_file_name("...", 64), "download");
        assert_eq!(sanitize_file_name("abcdef", 3), "abc");
        assert!(models_dir().ends_with("models"));
    }

    #[test]
    fn legacy_folder_moves_to_new_name() {
        let base = std::env::temp_dir().join(format!("dc-paths-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("RustCutPro").join("models")).unwrap();
        std::fs::write(base.join("RustCutPro").join("models").join("m.bin"), b"x").unwrap();
        // before the move the old folder is used as it is
        assert_eq!(app_dir(&base, true), base.join("RustCutPro"));
        migrate_legacy_dir(&base, true);
        let dir = app_dir(&base, true);
        assert_eq!(dir, base.join("Depthcut"));
        assert!(dir.join("models").join("m.bin").is_file());
        assert!(!base.join("RustCutPro").exists());
        // nothing to do the second time
        migrate_legacy_dir(&base, true);
        assert_eq!(app_dir(&base, true), base.join("Depthcut"));
        let _ = std::fs::remove_dir_all(&base);
    }
}
