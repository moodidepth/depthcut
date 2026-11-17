//! AI Copilot configuration persisted at `%LOCALAPPDATA%\Argon\ai_config.toml`.

use serde::{Deserialize, Serialize};
use std::path::Path;

pub const GEMINI_4_MODELS: [(&str, &str); 3] = [
    ("gemini-4-hydrogen", "Gemini 4 Hydrogen — Ultra-fast & low latency"),
    ("gemini-4-argon", "Gemini 4 Argon — Balanced multimodal flagship"),
    ("gemini-4-carbon", "Gemini 4 Carbon — Deepest reasoning & complex edits"),
];

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AiProviderKind {
    #[default]
    Gemini4,
    OpenAiCompatible,
    Ollama,
}

impl AiProviderKind {
    pub const ALL: [AiProviderKind; 3] = [
        AiProviderKind::Gemini4,
        AiProviderKind::OpenAiCompatible,
        AiProviderKind::Ollama,
    ];

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
    pub gemini_api_key: String,
    pub gemini_model: String,
    pub thinking_level: ThinkingLevel,
    pub openai_base_url: String,
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
            gemini_model: "gemini-4-argon".to_string(),
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

impl AiConfig {
    /// Load configuration from `%LOCALAPPDATA%\Argon\ai_config.toml` (falling back to defaults).
    pub fn load() -> Self {
        let path = nle_media::paths::ai_config_path();
        Self::load_from_path(&path)
    }

    pub fn load_from_path(path: &Path) -> Self {
        let mut cfg = match std::fs::read_to_string(path) {
            Ok(text) => toml::from_str::<AiConfig>(&text).unwrap_or_default(),
            Err(_) => AiConfig::default(),
        };
        if cfg.gemini_model.trim().is_empty() {
            cfg.gemini_model = "gemini-4-argon".to_string();
        }
        cfg
    }

    pub fn save(&self) -> std::io::Result<()> {
        let path = nle_media::paths::ai_config_path();
        self.save_to_path(&path)
    }

    pub fn save_to_path(&self, path: &Path) -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let body = toml::to_string_pretty(self).map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        std::fs::write(path, body)
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
