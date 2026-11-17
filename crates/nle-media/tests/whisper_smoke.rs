//! Manual end-to-end check of the Whisper pipeline against a real model and
//! recording (ignored by default; needs a downloaded model):
//!
//! ```text
//! ffmpeg -i speech.wav -f f32le -ac 1 -ar 16000 speech.f32
//! RCP_WHISPER_PCM=speech.f32 cargo test -p nle-media --features whisper-vulkan \
//!     --test whisper_smoke -- --ignored --nocapture
//! ```
//! `RCP_WHISPER_MODEL` overrides the model (default: models dir, base-q5_1).
#![cfg(feature = "whisper")]

use nle_media::transcribe::{self, TranscribeOptions};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Instant;

fn run(pcm: &[f32], model: &std::path::Path, use_gpu: bool) -> transcribe::Transcript {
    let mut opts = TranscribeOptions::new(model.to_path_buf());
    opts.use_gpu = use_gpu;
    let started = Instant::now();
    let tr = transcribe::transcribe(pcm, &opts, Arc::new(|_| {}), Arc::new(|m| eprintln!("  {m}")), Arc::new(AtomicBool::new(false)))
        .unwrap_or_else(|e| panic!("transcription failed (gpu={use_gpu}): {e}"));
    let text: Vec<&str> = tr.words.iter().take(12).map(|w| w.text.as_str()).collect();
    eprintln!(
        "  {} | {} words | lang {} | {:.1?} | fallback: {:?}\n  \"{}…\"",
        tr.backend,
        tr.words.len(),
        tr.language,
        started.elapsed(),
        tr.fallback_reason,
        text.join(" ")
    );
    if std::env::var_os("RCP_WHISPER_TIMES").is_some() {
        for w in &tr.words {
            eprintln!("    {:>7.3}–{:<7.3} {}", w.start.as_secs_f64(), w.end.as_secs_f64(), w.text);
        }
    }
    tr
}

#[test]
#[ignore = "needs RCP_WHISPER_PCM and a downloaded model"]
fn transcribes_real_speech_on_gpu_and_cpu() {
    let Some(pcm_path) = std::env::var_os("RCP_WHISPER_PCM") else {
        eprintln!("RCP_WHISPER_PCM not set; skipping");
        return;
    };
    let model = std::env::var_os("RCP_WHISPER_MODEL")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| nle_media::paths::models_dir().join("ggml-base-q5_1.bin"));
    let bytes = std::fs::read(&pcm_path).expect("read PCM");
    let pcm: Vec<f32> = bytes.chunks_exact(4).map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]])).collect();
    eprintln!("{:.1} s of audio, {} window(s)", pcm.len() as f64 / 16_000.0, transcribe::chunk_windows(pcm.len()).len());

    let gpu = run(&pcm, &model, true);
    assert!(!gpu.words.is_empty());
    if transcribe::GPU_BUILD {
        assert!(gpu.backend.starts_with("GPU: Vulkan"), "GPU not used: {} ({:?})", gpu.backend, gpu.fallback_reason);
    }
    let cpu = run(&pcm, &model, false);
    assert!(!cpu.words.is_empty() && cpu.backend.starts_with("CPU ("));
    // Timestamps are monotonic across chunk boundaries.
    assert!(gpu.words.windows(2).all(|w| w[0].end <= w[1].start));

    // Sub-second clip: zero-padded, no encoder abort.
    let short = &pcm[..pcm.len().min(9_600)];
    run(short, &model, true);
}
