//! AI Copilot configuration persisted at `%LOCALAPPDATA%\Argon\ai_config.toml`.
//!
//! API keys are **not** stored in that file on Windows: they live in the
//! Windows Credential Manager (service [`KEYRING_SERVICE`]). A key found in an
//! older plaintext config is moved there on load and scrubbed from the file.

use serde::{Deserialize, Serialize};
use std::path::Path;

/// Credential Manager service name for stored API keys.
pub const KEYRING_SERVICE: &str = "RustCutPro";
/// Model tried when the chosen Gemini model is overloaded (HTTP 503/429).
pub const DEFAULT_GEMINI_FALLBACK: &str = "gemini-flash-latest";

/// Gemini model used until the user picks another one. Which models a key can
/// actually call comes from the live list (`connection::list_gemini_models`),
/// not from a hardcoded catalogue.
pub const DEFAULT_GEMINI_MODEL: &str = "gemini-4-argon";

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AiProviderKind {
    #[default]
    Gemini4,
    OpenAiCompatible,
    Ollama,
}

impl AiProviderKind {
    pub const ALL: [AiProviderKind; 3] = [AiProviderKind::Gemini4, AiProviderKind::OpenAiCompatible, AiProviderKind::Ollama];

    pub fn label(self) -> &'static str {
        match self {
            AiProviderKind::Gemini4 => "Google Gemini 4",
            AiProviderKind::OpenAiCompatible => "OpenAI / OpenRouter",
            AiProviderKind::Ollama => "Local Ollama",
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ThinkingLevel {
    Low,
    #[default]
    Medium,
    High,
}

impl ThinkingLevel {
    pub const ALL: [ThinkingLevel; 3] = [ThinkingLevel::Low, ThinkingLevel::Medium, ThinkingLevel::High];

    pub fn as_gemini_str(self) -> &'static str {
        match self {
            ThinkingLevel::Low => "LOW",
            ThinkingLevel::Medium => "MEDIUM",
            ThinkingLevel::High => "HIGH",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            ThinkingLevel::Low => "LOW (Fast)",
            ThinkingLevel::Medium => "MEDIUM (Balanced)",
            ThinkingLevel::High => "HIGH (Deep Reasoning)",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct AiConfig {
    pub provider: AiProviderKind,
    /// Held in memory only; persisted in the Credential Manager (Windows).
    #[serde(skip_serializing_if = "skip_secret")]
    pub gemini_api_key: String,
    pub gemini_model: String,
    /// Free-text model id that overrides `gemini_model` when non-empty
    /// (for models newer than the built-in list).
    pub gemini_custom_model: String,
    /// Model to retry with when the main one is overloaded; empty disables it.
    pub gemini_fallback_model: String,
    pub thinking_level: ThinkingLevel,
    pub openai_base_url: String,
    /// Held in memory only; persisted in the Credential Manager (Windows).
    #[serde(skip_serializing_if = "skip_secret")]
    pub openai_api_key: String,
    pub openai_model: String,
    pub ollama_url: String,
    pub ollama_model: String,
    pub attach_video_proxy: bool,
}

impl Default for AiConfig {
    fn default() -> Self {
        Self {
            provider: AiProviderKind::Gemini4,
            gemini_api_key: String::new(),
            gemini_model: DEFAULT_GEMINI_MODEL.to_string(),
            gemini_custom_model: String::new(),
            gemini_fallback_model: DEFAULT_GEMINI_FALLBACK.to_string(),
            thinking_level: ThinkingLevel::Medium,
            openai_base_url: "https://api.openai.com/v1".to_string(),
            openai_api_key: String::new(),
            openai_model: "gpt-4o-mini".to_string(),
            ollama_url: "http://localhost:11434".to_string(),
            ollama_model: "llama3.2".to_string(),
            attach_video_proxy: false,
        }
    }
}

/// Unit tests must never touch the user's real stored keys.
fn default_service() -> &'static str {
    if cfg!(test) {
        "RustCutPro-unit-test"
    } else {
        KEYRING_SERVICE
    }
}

/// Keys are written to the TOML file only where no OS credential store is used.
fn skip_secret(_: &String) -> bool {
    secrets::SECURE
}

/// API-key storage in the OS credential store.
pub mod secrets {
    /// Whether keys go to the Credential Manager (Windows) rather than the file.
    pub const SECURE: bool = cfg!(windows);

    pub const GEMINI: &str = "gemini_api_key";
    pub const OPENAI: &str = "openai_api_key";

    /// Read a key; `Ok(None)` when none is stored.
    #[cfg(windows)]
    pub fn get(service: &str, account: &str) -> Result<Option<String>, String> {
        match keyring::Entry::new(service, account).and_then(|e| e.get_password()) {
            Ok(v) => Ok(Some(v)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(e) => Err(e.to_string()),
        }
    }

    /// Store a key (an empty value deletes it).
    #[cfg(windows)]
    pub fn set(service: &str, account: &str, value: &str) -> Result<(), String> {
        let entry = keyring::Entry::new(service, account).map_err(|e| e.to_string())?;
        if value.is_empty() {
            return match entry.delete_credential() {
                Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
                Err(e) => Err(e.to_string()),
            };
        }
        entry.set_password(value).map_err(|e| e.to_string())
    }

    #[cfg(not(windows))]
    pub fn get(_: &str, _: &str) -> Result<Option<String>, String> {
        Ok(None)
    }

    #[cfg(not(windows))]
    pub fn set(_: &str, _: &str, _: &str) -> Result<(), String> {
        Ok(())
    }
}

impl AiConfig {
    /// Load configuration from `%LOCALAPPDATA%\Argon\ai_config.toml` (falling back to defaults).
    pub fn load() -> Self {
        let path = nle_media::paths::ai_config_path();
        Self::load_from_path(&path)
    }

    pub fn load_from_path(path: &Path) -> Self {
        Self::load_with_store(path, default_service())
    }

    /// [`load_from_path`](Self::load_from_path) against a specific
    /// credential-store service (tests use a throwaway one).
    pub fn load_with_store(path: &Path, service: &str) -> Self {
        let mut cfg = match std::fs::read_to_string(path) {
            Ok(text) => toml::from_str::<AiConfig>(&text).unwrap_or_default(),
            Err(_) => AiConfig::default(),
        };
        if cfg.gemini_model.trim().is_empty() {
            cfg.gemini_model = DEFAULT_GEMINI_MODEL.to_string();
        }
        if secrets::SECURE {
            cfg.migrate_plaintext_keys(path, service);
            for (account, field) in [(secrets::GEMINI, &mut cfg.gemini_api_key), (secrets::OPENAI, &mut cfg.openai_api_key)] {
                if field.is_empty() {
                    match secrets::get(service, account) {
                        Ok(Some(v)) => *field = v,
                        Ok(None) => {}
                        Err(e) => log::warn!("could not read {account} from the Credential Manager: {e}"),
                    }
                }
            }
        }
        cfg
    }

    /// Move keys still sitting in the plaintext file into the credential
    /// store, then rewrite the file without them. The file is only scrubbed
    /// once every key was stored, so a failure never loses a key.
    fn migrate_plaintext_keys(&self, path: &Path, service: &str) {
        let plain = [(secrets::GEMINI, self.gemini_api_key.trim()), (secrets::OPENAI, self.openai_api_key.trim())];
        if plain.iter().all(|(_, v)| v.is_empty()) {
            return;
        }
        for (account, value) in plain.iter().filter(|(_, v)| !v.is_empty()) {
            if let Err(e) = secrets::set(service, account, value) {
                log::warn!("could not move {account} into the Credential Manager ({e}); leaving it in {}", path.display());
                return;
            }
        }
        match self.write_file(path) {
            Ok(()) => log::info!("moved API keys from {} into the Windows Credential Manager", path.display()),
            Err(e) => log::warn!("keys stored securely but {} could not be rewritten: {e}", path.display()),
        }
    }

    pub fn save(&self) -> std::io::Result<()> {
        let path = nle_media::paths::ai_config_path();
        self.save_to_path(&path)
    }

    pub fn save_to_path(&self, path: &Path) -> std::io::Result<()> {
        self.save_with_store(path, default_service())
    }

    /// Save settings to `path` and keys to the credential store `service`.
    pub fn save_with_store(&self, path: &Path, service: &str) -> std::io::Result<()> {
        if secrets::SECURE {
            for (account, value) in [(secrets::GEMINI, self.gemini_api_key.trim()), (secrets::OPENAI, self.openai_api_key.trim())] {
                secrets::set(service, account, value).map_err(|e| std::io::Error::other(format!("Credential Manager: {e}")))?;
            }
        }
        self.write_file(path)
    }

    fn write_file(&self, path: &Path) -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let body = toml::to_string_pretty(self).map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        std::fs::write(path, body)
    }

    /// Model id actually sent to Gemini (custom override, else the picker).
    pub fn effective_gemini_model(&self) -> String {
        let custom = self.gemini_custom_model.trim();
        if custom.is_empty() {
            self.gemini_model.trim().to_string()
        } else {
            custom.trim_start_matches("models/").to_string()
        }
    }

    /// Fallback model for overload errors, unless it is the same model.
    pub fn gemini_fallback(&self) -> Option<String> {
        let f = self.gemini_fallback_model.trim().trim_start_matches("models/");
        (!f.is_empty() && f != self.effective_gemini_model()).then(|| f.to_string())
    }

    /// Returns explicit Gemini key or falls back to `GEMINI_API_KEY` / `GOOGLE_API_KEY`.
    pub fn effective_gemini_key(&self) -> Option<String> {
        let trimmed = self.gemini_api_key.trim();
        if !trimmed.is_empty() {
            return Some(trimmed.to_string());
        }
        for var in ["GEMINI_API_KEY", "GOOGLE_API_KEY"] {
            if let Ok(v) = std::env::var(var) {
                let t = v.trim();
                if !t.is_empty() {
                    return Some(t.to_string());
                }
            }
        }
        None
    }

    pub fn effective_openai_key(&self) -> Option<String> {
        let trimmed = self.openai_api_key.trim();
        if !trimmed.is_empty() {
            return Some(trimmed.to_string());
        }
        for var in ["OPENAI_API_KEY", "OPENROUTER_API_KEY"] {
            if let Ok(v) = std::env::var(var) {
                let t = v.trim();
                if !t.is_empty() {
                    return Some(t.to_string());
                }
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("rcp-aicfg-{}-{name}", std::process::id())).join("ai_config.toml")
    }

    #[test]
    fn custom_model_overrides_picker_and_fallback_skips_same_model() {
        let mut c = AiConfig::default();
        assert_eq!(c.effective_gemini_model(), DEFAULT_GEMINI_MODEL);
        assert_eq!(c.gemini_fallback().as_deref(), Some(DEFAULT_GEMINI_FALLBACK));
        c.gemini_custom_model = " models/gemini-flash-latest ".into();
        assert_eq!(c.effective_gemini_model(), "gemini-flash-latest");
        assert_eq!(c.gemini_fallback(), None);
    }

    #[test]
    fn old_config_without_new_fields_still_loads() {
        let p = scratch("old");
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, "provider = \"gemini4\"\ngemini_model = \"gemini-4-hydrogen\"\n").unwrap();
        let c = AiConfig::load_with_store(&p, &format!("RustCutPro-test-{}-old", std::process::id()));
        assert_eq!(c.gemini_model, "gemini-4-hydrogen");
        assert_eq!(c.gemini_fallback_model, DEFAULT_GEMINI_FALLBACK);
        let _ = std::fs::remove_dir_all(p.parent().unwrap());
    }

    /// Uses a throwaway Credential Manager entry (never the real one).
    #[cfg(windows)]
    #[test]
    fn plaintext_key_moves_to_credential_manager_and_is_scrubbed() {
        let p = scratch("migrate");
        let service = format!("RustCutPro-test-{}", std::process::id());
        // removes the throwaway credentials even if an assert below fails
        struct Cleanup(String);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                let _ = secrets::set(&self.0, secrets::GEMINI, "");
                let _ = secrets::set(&self.0, secrets::OPENAI, "");
            }
        }
        let _cleanup = Cleanup(service.clone());
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, "gemini_api_key = \"test-key-123\"\ngemini_model = \"gemini-4-argon\"\n").unwrap();

        let c = AiConfig::load_with_store(&p, &service);
        assert_eq!(c.gemini_api_key, "test-key-123");
        let text = std::fs::read_to_string(&p).unwrap();
        assert!(!text.contains("test-key-123"), "key still in file:\n{text}");
        assert_eq!(secrets::get(&service, secrets::GEMINI).unwrap().as_deref(), Some("test-key-123"));

        // a fresh load reads it back from the store
        let again = AiConfig::load_with_store(&p, &service);
        assert_eq!(again.gemini_api_key, "test-key-123");

        // clearing the key and saving deletes the credential
        let mut cleared = again.clone();
        cleared.gemini_api_key.clear();
        cleared.save_with_store(&p, &service).unwrap();
        assert_eq!(secrets::get(&service, secrets::GEMINI).unwrap(), None);
        let _ = std::fs::remove_dir_all(p.parent().unwrap());
    }
}
