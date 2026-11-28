//! Batch kill scan: runs Depthcut's kill detector over recordings.
//!
//!     cargo run --release -p nle-media --example kill_scan -- [--track N] FILE...
//!
//! `--track N` picks the Nth audio stream (Medal: 0 = all audio, 1 = game).
//! Prints one line per file: kill times in seconds, `*` marks a multi-kill.

use std::process::Command;

fn main() {
    let mut track = 0usize;
    let mut files = Vec::new();
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        if a == "--track" {
            track = args.next().and_then(|v| v.parse().ok()).expect("--track N");
        } else {
            files.push(a);
        }
    }
    for f in files {
        let out = Command::new("ffmpeg")
            .args(["-v", "error", "-i", &f, "-map", &format!("0:a:{track}"), "-ac", "2", "-ar", "48000", "-f", "f32le", "-"])
            .output()
            .expect("run ffmpeg");
        if !out.status.success() {
            println!("{f}: ffmpeg failed: {}", String::from_utf8_lossy(&out.stderr).trim());
            continue;
        }
        let samples: Vec<f32> = out.stdout.chunks_exact(4).map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]])).collect();
        let peaks = nle_media::analysis::analyze_kills_from_samples(&samples, 2, 48_000, 1.0);
        let list: Vec<String> = peaks.iter().map(|p| format!("{:.2}{}", p.time.as_secs_f64(), if p.is_multikill { "*" } else { "" })).collect();
        let name = std::path::Path::new(&f).file_name().map_or(f.clone(), |n| n.to_string_lossy().into_owned());
        println!("{name}: {} kills [{}]", peaks.len(), list.join(" "));
    }
}
