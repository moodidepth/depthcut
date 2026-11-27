//! Compare Whisper word recall across backends on one recording with a known
//! script: CPU vs GPU, with and without DTW word timestamps. Ignored by
//! default (needs a model, a recording and its script):
//!
//! ```text
//! ffmpeg -i speech.wav -f f32le -ac 1 -ar 16000 speech.f32
//! RCP_WHISPER_PCM=speech.f32 RCP_WHISPER_SCRIPT=script.txt cargo test -p nle-media \
//!     --features whisper-vulkan --test whisper_backends -- --ignored --nocapture
//! ```
#![cfg(feature = "whisper")]

use nle_media::transcribe::{self, TranscribeOptions};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

fn bare(w: &str) -> String {
    w.chars().filter(|c| c.is_alphanumeric()).flat_map(char::to_lowercase).collect()
}

/// Longest common subsequence length: how many script words were heard in order.
fn lcs(a: &[String], b: &[String]) -> usize {
    let mut prev = vec![0usize; b.len() + 1];
    for x in a {
        let mut cur = vec![0usize; b.len() + 1];
        for (j, y) in b.iter().enumerate() {
            cur[j + 1] = if x == y { prev[j] + 1 } else { prev[j + 1].max(cur[j]) };
        }
        prev = cur;
    }
    prev[b.len()]
}

#[test]
#[ignore = "needs RCP_WHISPER_PCM, RCP_WHISPER_SCRIPT and a downloaded model"]
fn word_recall_per_backend() {
    let (Some(pcm_path), Some(script_path)) = (std::env::var_os("RCP_WHISPER_PCM"), std::env::var_os("RCP_WHISPER_SCRIPT")) else {
        eprintln!("RCP_WHISPER_PCM / RCP_WHISPER_SCRIPT not set; skipping");
        return;
    };
    let model = std::env::var_os("RCP_WHISPER_MODEL").map(std::path::PathBuf::from).unwrap_or_else(|| nle_media::paths::models_dir().join("ggml-base-q5_1.bin"));
    let bytes = std::fs::read(&pcm_path).expect("read PCM");
    let pcm: Vec<f32> = bytes.as_chunks::<4>().0.iter().map(|b| f32::from_le_bytes(*b)).collect();
    let script: Vec<String> = std::fs::read_to_string(&script_path).expect("read script").split_whitespace().map(bare).filter(|w| !w.is_empty()).collect();
    let configs: Vec<(&str, bool, &str)> = match std::env::var("RCP_BACKENDS").ok().as_deref() {
        Some("gpu") => vec![("gpu plain", true, "0"), ("gpu dtw", true, "1")],
        Some("cpu") => vec![("cpu plain", false, "0"), ("cpu dtw", false, "1")],
        _ => vec![("cpu plain", false, "0"), ("cpu dtw", false, "1"), ("gpu plain", true, "0"), ("gpu dtw", true, "1")],
    };
    for (name, gpu, dtw) in configs {
        // SAFETY: this test is the only thread reading these variables
        unsafe {
            std::env::set_var("RCP_WHISPER_DTW", dtw);
            std::env::set_var("RCP_WHISPER_DTW_GPU", dtw);
        }
        let mut opts = TranscribeOptions::new(model.clone());
        opts.use_gpu = gpu;
        let t0 = std::time::Instant::now();
        let tr = transcribe::transcribe(&pcm, &opts, Arc::new(|_| {}), Arc::new(|_| {}), Arc::new(AtomicBool::new(false))).expect("transcription");
        let heard: Vec<String> = tr.words.iter().map(|w| bare(&w.text)).filter(|w| !w.is_empty()).collect();
        let hit = lcs(&script, &heard);
        let missing: Vec<&String> = {
            let set: std::collections::HashSet<&String> = heard.iter().collect();
            script.iter().filter(|w| !set.contains(w)).collect()
        };
        eprintln!(
            "{name:10} | {} | {:.1?} | {} words heard | recall {hit}/{} ({:.0}%) | fallback {:?}\n    missing: {missing:?}",
            tr.backend,
            t0.elapsed(),
            heard.len(),
            script.len(),
            100.0 * hit as f64 / script.len() as f64,
            tr.fallback_reason
        );
        if std::env::var_os("RCP_WHISPER_TIMES").is_some() {
            for w in &tr.words {
                eprintln!("      {:>7.3}-{:<7.3} {}", w.start.as_secs_f64(), w.end.as_secs_f64(), w.text);
            }
        }
    }
}
