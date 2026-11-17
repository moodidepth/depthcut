//! Locating and invoking the FFmpeg command-line tools.

use parking_lot::RwLock;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// Cached lookup results; [`refresh`] clears them after FFmpeg is installed
/// or the user points the editor at an `ffmpeg.exe`.
static FOUND: RwLock<Option<PathBuf>> = RwLock::new(None);
static AVAILABLE: RwLock<Option<bool>> = RwLock::new(None);

/// `ffmpeg` executable (override with `RCP_FFMPEG`). Otherwise it is looked up
/// in the folder the user picked, next to the editor, in the editor's own
/// `bin` folder (where the built-in downloader puts it), on `PATH`, and in
/// common install locations (see [`locate_ffmpeg`]).
pub fn ffmpeg_path() -> PathBuf {
    if let Some(p) = std::env::var_os("RCP_FFMPEG") {
        return PathBuf::from(p);
    }
    if let Some(p) = FOUND.read().clone() {
        return p;
    }
    let p = locate_ffmpeg();
    *FOUND.write() = Some(p.clone());
    p
}

/// Forget cached lookups so the next call searches again.
pub fn refresh() {
    *FOUND.write() = None;
    *AVAILABLE.write() = None;
}

/// Folder the built-in downloader installs FFmpeg into
/// (`%LOCALAPPDATA%\RustCutPro\bin` on Windows).
pub fn managed_bin_dir() -> PathBuf {
    crate::paths::local_data_dir().join("bin")
}

fn custom_dir_file() -> PathBuf {
    crate::paths::local_data_dir().join("ffmpeg_dir.txt")
}

/// Folder the user picked with "Locate ffmpeg.exe", if any.
pub fn custom_dir() -> Option<PathBuf> {
    let s = std::fs::read_to_string(custom_dir_file()).ok()?;
    let s = s.trim();
    (!s.is_empty()).then(|| PathBuf::from(s))
}

/// Remember the folder holding a user-chosen `ffmpeg` (with `ffprobe` next to
/// it). Fails when either tool is missing there or does not run.
pub fn set_custom_ffmpeg(ffmpeg_exe: &Path) -> Result<PathBuf, String> {
    let dir = ffmpeg_exe.parent().ok_or("not a file path")?.to_path_buf();
    for tool in ["ffmpeg", "ffprobe"] {
        let p = dir.join(exe(tool));
        if !p.is_file() {
            return Err(format!("{} is missing in {}", exe(tool), dir.display()));
        }
        if run_capture(&p, &["-hide_banner", "-version"]).is_none() {
            return Err(format!("{} did not run", p.display()));
        }
    }
    let f = custom_dir_file();
    if let Some(parent) = f.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    std::fs::write(&f, dir.to_string_lossy().as_bytes()).map_err(|e| e.to_string())?;
    refresh();
    Ok(dir)
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
    dirs.extend(custom_dir());
    if let Some(d) = std::env::current_exe().ok().and_then(|e| e.parent().map(Path::to_path_buf)) {
        dirs.push(d);
    }
    dirs.push(managed_bin_dir());
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

/// Whether `ffmpeg` and `ffprobe` can be executed (cached until [`refresh`]).
pub fn available() -> bool {
    if let Some(ok) = *AVAILABLE.read() {
        return ok;
    }
    let ok = run_capture(&ffmpeg_path(), &["-hide_banner", "-version"]).is_some() && run_capture(&ffprobe_path(), &["-hide_banner", "-version"]).is_some();
    if !ok {
        log::warn!("ffmpeg/ffprobe not found (next to the editor, in its bin folder, on PATH or in winget/Scoop/Chocolatey locations) — using synthetic media and disabling export");
    }
    *AVAILABLE.write() = Some(ok);
    ok
}
