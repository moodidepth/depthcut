use nle_media::transcribe::*;
use nle_core::Time;
use std::fs;

#[test]
fn test_pcm_sanitization() {
    let input = vec![f32::NAN, f32::INFINITY, f32::NEG_INFINITY, 0.5, -0.5, 3.5, -7.0];
    let prepared = prepare_pcm(&input).unwrap();
    // Padded to MIN_SAMPLES (16000) because input is short
    assert_eq!(prepared.len(), 16000);
    assert_eq!(prepared[0], 0.0);
    assert_eq!(prepared[1], 0.0);
    assert_eq!(prepared[2], 0.0);
    assert_eq!(prepared[3], 0.5);
    assert_eq!(prepared[4], -0.5);
    assert_eq!(prepared[5], 1.0);
    assert_eq!(prepared[6], -1.0);
    
    let long_input = vec![0.1; 20000];
    let prepared_long = prepare_pcm(&long_input).unwrap();
    assert_eq!(prepared_long.len(), 20000);
}

#[test]
fn test_silence_detection() {
    assert!(prepare_pcm(&[]).is_none());
    assert!(prepare_pcm(&[0.0; 16000]).is_none());
    assert!(prepare_pcm(&[1e-8; 16000]).is_none());
}

#[test]
fn test_sine_padding() {
    let mut input = Vec::with_capacity(4000);
    for i in 0..4000 {
        input.push(0.5 * (2.0 * std::f32::consts::PI * 440.0 * i as f32 / 16000.0).sin());
    }
    let prepared = prepare_pcm(&input).expect("Should not be silent");
    assert_eq!(prepared.len(), 16000);
    for i in 0..4000 {
        assert!((prepared[i] - input[i]).abs() < 1e-6);
    }
    for i in 4000..16000 {
        assert_eq!(prepared[i], 0.0);
    }
}

#[test]
fn test_chunk_windows() {
    assert!(chunk_windows(0).is_empty());
    
    let w100 = chunk_windows(100);
    assert_eq!(w100, vec![0..100]);
    
    let w480k = chunk_windows(480000);
    assert_eq!(w480k, vec![0..480000]);
    
    let w480001 = chunk_windows(480001);
    assert_eq!(w480001, vec![0..480000, 472000..480001]);
    
    let len_20m = 20 * 60 * 16000;
    let w20m = chunk_windows(len_20m);
    assert!(!w20m.is_empty());
    for i in 0..w20m.len() {
        assert!(w20m[i].len() <= 480000);
        if i > 0 {
            assert_eq!(w20m[i].start, w20m[i-1].end - 8000);
        }
    }
    assert_eq!(w20m.last().unwrap().end, len_20m);
}

#[test]
fn test_merge_chunk_words() {
    let len = 16000 * 50;
    let windows = chunk_windows(len);
    let offset = Time::from_secs(7);
    
    // Window 0: 0..480000 (0s..30s)
    // Window 1: 472000..800000 (29.5s..50s)
    // Overlap: 472000..480000 (29.5s..30s), Midpoint: 476000 (29.75s)
    
    let chunk0 = vec![
        (100, 150, " hello".to_string()),     // 1.0s (rel: 1.0s). Kept (1.0 < 29.75)
        (2970, 2980, " boundary".to_string()), // 29.7s (rel: 29.7s). Kept (29.7 < 29.75)
    ];
    let chunk1 = vec![
        (20, 30, " boundary".to_string()),    // 29.5s + 0.2s = 29.7s (rel: 29.7s). Dropped (29.7 < 29.75)
        (500, 560, " world".to_string()),     // 29.5s + 5.0s = 34.5s (rel: 34.5s). Kept (34.5 >= 29.75)
    ];
    
    let chunks = vec![chunk0, chunk1];
    let words = merge_chunk_words(&windows, &chunks, offset);
    
    assert_eq!(words.len(), 3);
    assert_eq!(words[0].text, "hello");
    assert_eq!(words[0].start, offset + Time::from_secs(1));
    
    assert_eq!(words[1].text, "boundary");
    assert_eq!(words[1].start, offset + Time::from_millis(29700));
    
    assert_eq!(words[2].text, "world");
    assert_eq!(words[2].start, offset + Time::from_samples(472000, 16000) + Time::from_secs(5));
}

#[test]
fn merge_keeps_later_copy_past_midpoint_and_drops_padding_hallucinations() {
    let windows = chunk_windows(16000 * 50);
    let offset = Time::ZERO;
    // 29.8 s absolute is past the 29.75 s cut: the second window owns it.
    let chunk0 = vec![(2980, 2990, " late".to_string())];
    // 29.5 s + 0.3 s = 29.8 s; and a phantom word at 29.5 s + 21 s = 50.5 s (past the audio end).
    let chunk1 = vec![(30, 40, " late".to_string()), (2100, 2150, " phantom".to_string())];
    let words = merge_chunk_words(&windows, &[chunk0, chunk1], offset);
    let texts: Vec<&str> = words.iter().map(|w| w.text.as_str()).collect();
    assert_eq!(texts, ["late"]);
    assert_eq!(words[0].start, Time::from_samples(472_000, 16_000) + Time::from_millis(300));
}

#[test]
fn test_normalize_language() {
    assert_eq!(normalize_language(None), Ok(None));
    assert_eq!(normalize_language(Some("")), Ok(None));
    assert_eq!(normalize_language(Some("  ")), Ok(None));
    assert_eq!(normalize_language(Some("auto")), Ok(None));
    assert_eq!(normalize_language(Some("EN")), Ok(Some("en".to_string())));
    assert_eq!(normalize_language(Some("spa")), Ok(Some("spa".to_string())));
    assert!(normalize_language(Some("e")).is_err());
    assert!(normalize_language(Some("english")).is_err());
    assert!(normalize_language(Some("12")).is_err());
}

#[test]
fn test_validate_model_file() {
    let temp_dir = std::env::temp_dir();
    let pid = std::process::id();
    
    let path_missing = temp_dir.join(format!("missing_{}.bin", pid));
    assert!(matches!(validate_model_file(&path_missing, 1024), Err(ModelFileError::Missing)));
    
    let path_truncated = temp_dir.join(format!("truncated_{}.bin", pid));
    fs::write(&path_truncated, b"lmgg padded content").unwrap();
    let err = validate_model_file(&path_truncated, 1024).unwrap_err();
    assert!(matches!(err, ModelFileError::Truncated { .. }));
    assert!(err.is_corrupt());
    
    let path_bad_magic = temp_dir.join(format!("bad_magic_{}.bin", pid));
    fs::write(&path_bad_magic, b"GGUF more padding for length").unwrap();
    let err2 = validate_model_file(&path_bad_magic, 16).unwrap_err();
    assert!(matches!(err2, ModelFileError::BadMagic));
    assert!(err2.is_corrupt());
    
    let path_ok = temp_dir.join(format!("ok_{}.bin", pid));
    let mut content = b"lmgg".to_vec();
    content.resize(2048, 0);
    fs::write(&path_ok, &content).unwrap();
    assert!(validate_model_file(&path_ok, 1024).is_ok());
    
    let _ = fs::remove_file(path_truncated);
    let _ = fs::remove_file(path_bad_magic);
    let _ = fs::remove_file(path_ok);
}
