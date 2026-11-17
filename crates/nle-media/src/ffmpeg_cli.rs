//! Locating and invoking the FFmpeg command-line tools.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::OnceLock;

/// `ffmpeg` executable (override with `RCP_FFMPEG`). Otherwise it is looked up
/// next to the editor, on `PATH`, and in common install locations (see
/// [`locate_ffmpeg`]).
pub fn ffmpeg_path() -> PathBuf {
    if let Some(p) = std::env::var_os("RCP_FFMPEG") {
        return PathBuf::from(p);
    }
    static FOUND: OnceLock<PathBuf> = OnceLock::new();
    FOUND.get_or_init(locate_ffmpeg).clone()
}

fn exe(name: &str) -> String {
    if cfg!(windows) {
        format!("{name}.exe")
    } else {
        name.to_string()
    }
}

/// Find a directory holding both `ffmpeg` and `ffprobe`.
///
/// `PATH` alone is not enough on Windows: programs started from Explorer keep
/// the `PATH` Explorer had at login, so FFmpeg installed later (e.g. with
/// winget) is invisible until the user signs out. The editor therefore also
/// checks where winget, Scoop and Chocolatey put FFmpeg.
pub fn locate_ffmpeg() -> PathBuf {
    let (ff, fp) = (exe("ffmpeg"), exe("ffprobe"));
    let has_both = |dir: &Path| dir.join(&ff).is_file() && dir.join(&fp).is_file();
    let mut dirs: Vec<PathBuf> = Vec::new();
    if let Some(d) = std::env::current_exe().ok().and_then(|e| e.parent().map(Path::to_path_buf)) {
        dirs.push(d);
    }
    if let Some(path) = std::env::var_os("PATH") {
        dirs.extend(std::env::split_paths(&path));
    }
    dirs.extend(known_install_dirs());
    match dirs.into_iter().find(|d| has_both(d)) {
        Some(d) => {
            log::info!("using FFmpeg in {}", d.display());
            d.join(ff)
        }
        None => PathBuf::from("ffmpeg"),
    }
}

#[cfg(windows)]
fn known_install_dirs() -> Vec<PathBuf> {
    let env = |k: &str| std::env::var_os(k).map(PathBuf::from);
    let mut dirs = Vec::new();
    if let Some(local) = env("LOCALAPPDATA") {
        let winget = local.join("Microsoft").join("WinGet");
        dirs.push(winget.join("Links"));
        // Packages/Gyan.FFmpeg_*/ffmpeg-<ver>-full_build/bin (also .Essentials,
        // .Shared and BtbN builds); the static builds are preferred.
        let mut found: Vec<PathBuf> = Vec::new();
        if let Ok(pkgs) = std::fs::read_dir(winget.join("Packages")) {
            for pkg in pkgs.flatten() {
                let name = pkg.file_name().to_string_lossy().to_ascii_lowercase();
                if !name.contains("ffmpeg") {
                    continue;
                }
                if let Ok(subs) = std::fs::read_dir(pkg.path()) {
                    found.extend(subs.flatten().map(|s| s.path().join("bin")));
                }
            }
        }
        found.sort_by_key(|p| (p.to_string_lossy().to_ascii_lowercase().contains("shared"), std::cmp::Reverse(p.clone())));
        dirs.extend(found);
    }
    if let Some(home) = env("USERPROFILE") {
        dirs.push(home.join("scoop").join("shims"));
    }
    if let Some(pd) = env("ProgramData") {
        dirs.push(pd.join("chocolatey").join("bin"));
    }
    if let Some(pf) = env("ProgramFiles") {
        dirs.push(pf.join("ffmpeg").join("bin"));
    }
    dirs.push(PathBuf::from("C:/ffmpeg/bin"));
    dirs
}

#[cfg(not(windows))]
fn known_install_dirs() -> Vec<PathBuf> {
    ["/usr/local/bin", "/usr/bin", "/snap/bin", "/opt/homebrew/bin"].iter().map(PathBuf::from).collect()
}

/// `ffprobe` executable (override with `RCP_FFPROBE`).
pub fn ffprobe_path() -> PathBuf {
    if let Some(p) = std::env::var_os("RCP_FFPROBE") {
        return PathBuf::from(p);
    }
    let ff = ffmpeg_path();
    match ff.parent().filter(|p| !p.as_os_str().is_empty()) {
        Some(dir) => dir.join(if cfg!(windows) { "ffprobe.exe" } else { "ffprobe" }),
        None => PathBuf::from("ffprobe"),
    }
}

/// A `Command` that never pops up a console window on Windows.
pub fn command(program: &std::path::Path) -> Command {
    #[allow(unused_mut)]
    let mut c = Command::new(program);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        c.creation_flags(CREATE_NO_WINDOW);
    }
    c
}

/// Run a tool and capture stdout (stderr discarded).
pub fn run_capture(program: &std::path::Path, args: &[&str]) -> Option<String> {
    let out = command(program).args(args).stdin(Stdio::null()).stderr(Stdio::null()).output().ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

static AVAILABLE: OnceLock<bool> = OnceLock::new();

/// Whether `ffmpeg` and `ffprobe` can be executed (cached).
pub fn available() -> bool {
    *AVAILABLE.get_or_init(|| {
        let ok = run_capture(&ffmpeg_path(), &["-hide_banner", "-version"]).is_some() && run_capture(&ffprobe_path(), &["-hide_banner", "-version"]).is_some();
        if !ok {
            log::warn!("ffmpeg/ffprobe not found (next to the editor, on PATH or in winget/Scoop/Chocolatey locations) — using synthetic media and disabling export");
        }
        ok
    })
}
