//! Atomic crash-recovery autosave (every 30 s by default).
//!
//! Snapshots are written to `<file>.tmp`, flushed with `fsync`, then renamed
//! over the target, so a crash mid-write can never leave a truncated file.
//! The previous snapshot is rotated to `.prev` as a second line of defence.

use crate::project::Project;
use std::collections::hash_map::DefaultHasher;
use std::fs::{self, File};
use std::hash::{Hash, Hasher};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

pub const AUTOSAVE_INTERVAL: Duration = Duration::from_secs(30);

/// Write `bytes` to `path` atomically (temp file + fsync + rename).
pub fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    if let Some(dir) = path.parent() {
        if !dir.as_os_str().is_empty() {
            fs::create_dir_all(dir)?;
        }
    }
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    {
        let mut f = File::create(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
    }
    fs::rename(&tmp, path)?;
    // Persist the directory entry too (POSIX); harmless elsewhere.
    #[cfg(unix)]
    if let Some(dir) = path.parent() {
        if let Ok(d) = File::open(dir) {
            let _ = d.sync_all();
        }
    }
    Ok(())
}

#[derive(Debug)]
pub struct Autosaver {
    path: PathBuf,
    interval: Duration,
    last_save: Option<Instant>,
    last_hash: Option<u64>,
}

impl Autosaver {
    /// Autosave `project_name` into `dir`.
    pub fn new(dir: impl Into<PathBuf>, project_name: &str) -> Self {
        let safe: String = project_name.chars().map(|c| if c.is_alphanumeric() || c == '-' || c == '_' { c } else { '_' }).collect();
        let path = dir.into().join(format!("{safe}.autosave.json"));
        Autosaver { path, interval: AUTOSAVE_INTERVAL, last_save: None, last_hash: None }
    }

    pub fn with_interval(mut self, interval: Duration) -> Self {
        self.interval = interval;
        self
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn prev_path(&self) -> PathBuf {
        self.path.with_extension("prev.json")
    }

    /// Call frequently (e.g. once per UI frame). Saves when the interval
    /// has elapsed *and* the project changed. Returns `true` if it saved.
    pub fn tick(&mut self, project: &Project) -> io::Result<bool> {
        self.tick_at(project, Instant::now())
    }

    pub fn tick_at(&mut self, project: &Project, now: Instant) -> io::Result<bool> {
        if let Some(last) = self.last_save {
            if now.duration_since(last) < self.interval {
                return Ok(false);
            }
        }
        self.save_if_changed(project, now)
    }

    /// Save immediately if the content differs from the last snapshot.
    pub fn save_if_changed(&mut self, project: &Project, now: Instant) -> io::Result<bool> {
        let json = project.to_json().map_err(io::Error::other)?;
        let mut h = DefaultHasher::new();
        json.hash(&mut h);
        let hash = h.finish();
        self.last_save = Some(now);
        if self.last_hash == Some(hash) {
            return Ok(false);
        }
        if self.path.exists() {
            let _ = fs::copy(&self.path, self.prev_path());
        }
        write_atomic(&self.path, json.as_bytes())?;
        self.last_hash = Some(hash);
        Ok(true)
    }

    /// Load the newest readable snapshot (current, then `.prev`).
    pub fn recover(&self) -> Option<(Project, SystemTime)> {
        for p in [self.path.clone(), self.prev_path()] {
            if let Ok(s) = fs::read_to_string(&p) {
                if let Ok(project) = Project::from_json(&s) {
                    let mtime = fs::metadata(&p).and_then(|m| m.modified()).unwrap_or(SystemTime::UNIX_EPOCH);
                    return Some((project, mtime));
                }
            }
        }
        None
    }

    /// Remove snapshots after a clean save/exit.
    pub fn discard(&self) {
        let _ = fs::remove_file(&self.path);
        let _ = fs::remove_file(self.prev_path());
    }
}
