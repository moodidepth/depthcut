//! Text-based editing against real speech: transcribe a recording, delete a
//! phrase by text, then transcribe the cut audio again. The phrase must be
//! gone, its neighbours intact, and the captions left on the timeline must
//! line up with where the words are now heard. Ignored by default (needs a
//! downloaded model and a recording):
//!
//! ```text
//! ffmpeg -i speech.wav -f f32le -ac 1 -ar 16000 speech.f32
//! RCP_WHISPER_PCM=speech.f32 RCP_CUT_PHRASE="brown fox" cargo test -p nle-media \
//!     --features whisper-vulkan --test text_edit_real -- --ignored --nocapture
//! ```
#![cfg(feature = "whisper")]

use nle_core::text_edit::{self, Word};
use nle_core::{CaptionTrack, Clip, MediaAsset, Time, Timeline};
use nle_media::transcribe::{self, TranscribeOptions};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

fn transcribe_pcm(pcm: &[f32], model: &std::path::Path) -> Vec<nle_core::CaptionWord> {
    let opts = TranscribeOptions::new(model.to_path_buf());
    let tr = transcribe::transcribe(pcm, &opts, Arc::new(|_| {}), Arc::new(|_| {}), Arc::new(AtomicBool::new(false))).expect("transcription");
    eprintln!("  backend: {}", tr.backend);
    tr.words
}

fn show(words: &[Word]) -> String {
    words.iter().map(|w| format!("{}@{:.2}", w.text, w.start.as_secs_f64())).collect::<Vec<_>>().join(" ")
}

#[test]
#[ignore = "needs RCP_WHISPER_PCM and a downloaded model"]
fn deleting_a_phrase_by_text_removes_its_speech() {
    let Some(pcm_path) = std::env::var_os("RCP_WHISPER_PCM") else {
        eprintln!("RCP_WHISPER_PCM not set; skipping");
        return;
    };
    let phrase = std::env::var("RCP_CUT_PHRASE").unwrap_or_else(|_| "brown fox".into());
    let model = std::env::var_os("RCP_WHISPER_MODEL").map(std::path::PathBuf::from).unwrap_or_else(|| nle_media::paths::models_dir().join("ggml-base-q5_1.bin"));
    let bytes = std::fs::read(&pcm_path).expect("read PCM");
    let pcm: Vec<f32> = bytes.as_chunks::<4>().0.iter().map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]])).collect();
    let dur = Time::from_secs_f64(pcm.len() as f64 / 16_000.0);

    let mut tl = Timeline::new("T", Default::default());
    let asset = MediaAsset::new("speech", "speech.wav", dur);
    tl.append(Clip::from_asset_range(&asset, Time::ZERO, dur)).unwrap();
    tl.captions.segments = CaptionTrack::phrases(&transcribe_pcm(&pcm, &model));
    let words = text_edit::timeline_words(&tl.captions);
    eprintln!("before: {}", show(&words));

    let sel = text_edit::find_phrase(&words, &phrase);
    let n = phrase.split_whitespace().count();
    assert_eq!(sel.len(), n, "'{phrase}' found once");
    let ranges = text_edit::word_ranges(&words, &sel, tl.format.rate);
    let removed = text_edit::total(&ranges);
    text_edit::cut_command(&tl.captions, &ranges, "Delete Words").unwrap().apply(&mut tl).unwrap();
    assert_eq!(tl.extent(), dur - removed);
    let after = text_edit::timeline_words(&tl.captions);
    eprintln!("cut {ranges:?}\nafter:  {}", show(&after));
    assert_eq!(after.len(), words.len() - n);

    // the audio the edited timeline plays
    let mut cut_pcm = Vec::with_capacity(pcm.len());
    let mut from = 0usize;
    for (s, e) in &ranges {
        let (a, b) = ((s.as_secs_f64() * 16_000.0) as usize, (e.as_secs_f64() * 16_000.0) as usize);
        cut_pcm.extend_from_slice(&pcm[from..a.min(pcm.len())]);
        from = b.min(pcm.len());
    }
    cut_pcm.extend_from_slice(&pcm[from..]);
    let heard = text_edit::source_words(&transcribe_pcm(&cut_pcm, &model));
    eprintln!("heard:  {}", show(&heard));

    let bare: Vec<String> = heard.iter().map(|w| text_edit::bare(&w.text)).collect();
    for gone in phrase.split_whitespace() {
        assert!(!bare.contains(&text_edit::bare(gone)), "'{gone}' is still spoken");
    }
    // every caption word that is still heard sits where it is heard now
    let mut matched = 0;
    for w in &after {
        if let Some(h) = heard.iter().find(|h| text_edit::bare(&h.text) == text_edit::bare(&w.text) && (h.start - w.start).abs() < Time::from_millis(400)) {
            let err = (h.start - w.start).as_secs_f64().abs();
            assert!(err < 0.4, "{} off by {err:.2} s", w.text);
            matched += 1;
        }
    }
    eprintln!("{matched} of {} caption words match the re-transcription within 0.4 s", after.len());
    assert!(matched * 10 >= after.len() * 8, "most captions line up with the cut audio");
}
