//! Per-user data folders: `%LOCALAPPDATA%\RustCutPro\...` on Windows,
//! `$XDG_DATA_HOME/rust-cut-pro/...` (or `~/.local/share`) elsewhere.

use std::path::PathBuf;

/// Machine-local data root (large caches: models, downloads).
pub fn local_data_dir() -> PathBuf {
    if let Some(p) = std::env::var_os("RCP_DATA_DIR") {
        return PathBuf::from(p);
    }
    if cfg!(windows) {
        if let Some(local) = std::env::var_os("LOCALAPPDATA") {
            return PathBuf::from(local).join("RustCutPro");
        }
    }
    if let Some(x) = std::env::var_os("XDG_DATA_HOME") {
        return PathBuf::from(x).join("rust-cut-pro");
    }
    if let Some(home) = std::env::var_os("HOME") {
        return PathBuf::from(home).join(".local").join("share").join("rust-cut-pro");
    }
    std::env::temp_dir().join("rust-cut-pro")
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
        std::env::var_os("LOCALAPPDATA")
            .map(|local| PathBuf::from(local).join("Argon"))
            .unwrap_or_else(|| local_data_dir().join("Argon"))
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
    let mut s: String = name
        .chars()
        .map(|c| if c.is_alphanumeric() || " -_.()[]".contains(c) { c } else { '_' })
        .collect::<String>()
        .trim()
        .trim_matches('.')
        .to_string();
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
}
