//! Whisper GGML model manager: downloads quantised whisper.cpp models from
//! Hugging Face into `%LOCALAPPDATA%\Depthcut\models`.

use crate::transcribe::{validate_model_file, ModelFileError};
use std::path::PathBuf;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WhisperModel {
    pub id: &'static str,
    pub label: &'static str,
    pub file: &'static str,
    /// Approximate size in bytes (for the UI before the download starts).
    pub approx_bytes: u64,
    pub note: &'static str,
}

pub const BASE_Q5_1: WhisperModel =
    WhisperModel { id: "base-q5_1", label: "Base (fast)", file: "ggml-base-q5_1.bin", approx_bytes: 59_707_625, note: "~60 MB · fast, good for clear speech" };

pub const LARGE_V3_TURBO_Q5_0: WhisperModel = WhisperModel {
    id: "large-v3-turbo-q5_0",
    label: "Large v3 Turbo (accurate)",
    file: "ggml-large-v3-turbo-q5_0.bin",
    approx_bytes: 574_041_195,
    note: "~547 MB · best accuracy, multilingual",
};

pub const MODELS: [WhisperModel; 2] = [BASE_Q5_1, LARGE_V3_TURBO_Q5_0];

pub const HF_BASE: &str = "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/";

impl WhisperModel {
    pub fn by_id(id: &str) -> Option<WhisperModel> {
        MODELS.iter().copied().find(|m| m.id == id)
    }
    pub fn url(&self) -> String {
        format!("{HF_BASE}{}", self.file)
    }
    pub fn path(&self) -> PathBuf {
        crate::paths::models_dir().join(self.file)
    }
    /// Smallest size a complete download can have (5% below the published
    /// size, so a re-quantised upload of the same model still passes).
    pub fn min_valid_bytes(&self) -> u64 {
        self.approx_bytes / 100 * 95
    }
    /// Present, full-size and starting with the GGML magic.
    pub fn verify(&self) -> Result<(), ModelFileError> {
        validate_model_file(&self.path(), self.min_valid_bytes())
    }
    /// Installed = present and intact (see [`WhisperModel::verify`]).
    pub fn is_installed(&self) -> bool {
        self.verify().is_ok()
    }
    /// Delete the file if it is truncated or not a GGML model; returns what
    /// was wrong with it (`None` = intact or simply missing).
    pub fn remove_if_corrupt(&self) -> Option<ModelFileError> {
        match self.verify() {
            Err(e) if e.is_corrupt() => {
                if let Err(io) = self.delete() {
                    log::warn!("could not delete corrupt model {}: {io}", self.path().display());
                }
                Some(e)
            }
            _ => None,
        }
    }
    pub fn size_label(&self) -> String {
        format!("{:.0} MB", self.approx_bytes as f64 / 1_048_576.0)
    }
    /// Download with `progress(fraction)`; `cancelled()` aborts.
    pub fn download(&self, mut progress: impl FnMut(f32), cancelled: impl Fn() -> bool) -> Result<PathBuf, String> {
        let approx = self.approx_bytes;
        super::download::download_blocking(
            &self.url(),
            &self.path(),
            |done, total| {
                let t = total.unwrap_or(approx).max(1);
                progress((done as f64 / t as f64) as f32)
            },
            cancelled,
        )?;
        if let Some(e) = self.remove_if_corrupt() {
            return Err(format!("downloaded {} is damaged ({e}); it was deleted, please retry", self.file));
        }
        Ok(self.path())
    }
    pub fn delete(&self) -> std::io::Result<()> {
        std::fs::remove_file(self.path())
    }
}

/// The first installed model, preferring the most accurate.
pub fn best_installed() -> Option<WhisperModel> {
    MODELS.iter().rev().copied().find(WhisperModel::is_installed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_urls() {
        assert_eq!(BASE_Q5_1.url(), "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-base-q5_1.bin");
        assert_eq!(WhisperModel::by_id("large-v3-turbo-q5_0"), Some(LARGE_V3_TURBO_Q5_0));
        assert!(LARGE_V3_TURBO_Q5_0.path().ends_with("ggml-large-v3-turbo-q5_0.bin"));
        assert_eq!(BASE_Q5_1.size_label(), "57 MB");
        assert!(BASE_Q5_1.min_valid_bytes() < BASE_Q5_1.approx_bytes);
        assert!(BASE_Q5_1.min_valid_bytes() > BASE_Q5_1.approx_bytes * 9 / 10);
    }
}
