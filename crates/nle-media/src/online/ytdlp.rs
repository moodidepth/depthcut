//! Optional `yt-dlp` integration for importing audio from a pasted URL.
//! Only used when the user has yt-dlp next to the editor (`tools/`), in the
//! working directory's `tools/`, or on PATH. Users are responsible for only
//! importing media they have the rights to use.

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::Stdio;

fn exe_name() -> &'static str {
    if cfg!(windows) {
        "yt-dlp.exe"
    } else {
        "yt-dlp"
    }
}

/// Locate yt-dlp (`RCP_YTDLP`, `./tools`, `<exe dir>/tools`, `<exe dir>`, PATH).
pub fn find() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("RCP_YTDLP").map(PathBuf::from) {
        if p.is_file() {
            return Some(p);
        }
    }
    let mut dirs: Vec<PathBuf> = Vec::new();
    if let Ok(cwd) = std::env::current_dir() {
        dirs.push(cwd.join("tools"));
    }
    if let Some(exe_dir) = std::env::current_exe().ok().and_then(|e| e.parent().map(Path::to_path_buf)) {
        dirs.push(exe_dir.join("tools"));
        dirs.push(exe_dir);
    }
    if let Some(path) = std::env::var_os("PATH") {
        dirs.extend(std::env::split_paths(&path));
    }
    dirs.into_iter().map(|d| d.join(exe_name())).find(|p| p.is_file())
}

/// Parse a `[download]  42.3% of ...` progress line.
pub fn parse_progress(line: &str) -> Option<f32> {
    let rest = line.trim().strip_prefix("[download]")?.trim_start();
    let pct = rest.split('%').next()?.trim();
    pct.parse::<f32>().ok().map(|p| (p / 100.0).clamp(0.0, 1.0))
}

/// Download the best audio of `url` into `dir` and return the file path.
pub fn download_audio(url: &str, dir: &Path, mut progress: impl FnMut(f32), cancelled: impl Fn() -> bool) -> Result<PathBuf, String> {
    let exe = find().ok_or("yt-dlp was not found (put yt-dlp.exe in a 'tools' folder next to the editor or on PATH)")?;
    let url = url.trim();
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        return Err("enter an http(s) URL".into());
    }
    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    let mut cmd = crate::ffmpeg_cli::command(&exe);
    cmd.arg("--no-playlist")
        .arg("--newline")
        .arg("-f")
        .arg("bestaudio/best")
        .arg("-x")
        .arg("--audio-format")
        .arg("m4a")
        .arg("--restrict-filenames")
        .arg("-P")
        .arg(dir)
        .arg("-o")
        .arg("%(title).80s [%(id)s].%(ext)s")
        .arg("--print")
        .arg("after_move:filepath");
    if crate::ffmpeg_cli::available() {
        cmd.arg("--ffmpeg-location").arg(crate::ffmpeg_cli::ffmpeg_path());
    }
    cmd.arg("--").arg(url);
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped()).stdin(Stdio::null());
    let mut child = cmd.spawn().map_err(|e| format!("could not start yt-dlp: {e}"))?;
    let stderr = child.stderr.take();
    let err_thread = std::thread::spawn(move || {
        let mut tail = Vec::new();
        if let Some(s) = stderr {
            for line in BufReader::new(s).lines().map_while(Result::ok) {
                tail.push(line);
                if tail.len() > 20 {
                    tail.remove(0);
                }
            }
        }
        tail
    });
    let mut out_path = None;
    if let Some(stdout) = child.stdout.take() {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            if cancelled() {
                let _ = child.kill();
                break;
            }
            if let Some(p) = parse_progress(&line) {
                progress(p);
            } else {
                let t = line.trim();
                if !t.is_empty() && !t.starts_with('[') && Path::new(t).is_file() {
                    out_path = Some(PathBuf::from(t));
                }
            }
        }
    }
    let status = child.wait().map_err(|e| e.to_string())?;
    let tail = err_thread.join().unwrap_or_default();
    if cancelled() {
        return Err("Cancelled".into());
    }
    if !status.success() {
        let msg = tail.iter().rev().find(|l| l.contains("ERROR")).cloned().unwrap_or_else(|| tail.last().cloned().unwrap_or_default());
        return Err(format!("yt-dlp failed: {msg}"));
    }
    progress(1.0);
    out_path.ok_or_else(|| "yt-dlp finished but reported no file".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn progress_lines() {
        assert_eq!(parse_progress("[download]  42.5% of 3.20MiB at 1.00MiB/s ETA 00:02"), Some(0.425));
        assert_eq!(parse_progress("[download] 100% of 3.20MiB"), Some(1.0));
        assert_eq!(parse_progress("[download] Destination: x.m4a"), None);
        assert_eq!(parse_progress("[youtube] abc: Downloading"), None);
    }
}
