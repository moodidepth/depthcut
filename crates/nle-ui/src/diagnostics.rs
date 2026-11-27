//! Crash diagnostics.
//!
//! * Every session logs to `<data>/logs/depthcut.log` as well as stderr,
//!   because release builds have no console. The previous session's log is
//!   kept as `depthcut.prev.log`.
//! * A panic hook writes the message, location and backtrace to the log.
//! * A `session.running` marker exists while the app runs and is removed on
//!   a clean exit. If the marker is still there at the next launch, that
//!   session died (panic, native crash, system crash or power loss), and the
//!   app offers the old log. The marker also records the panic message, if
//!   there was one.
//!
//! Only the first instance owns the main log and the marker. While it holds
//! the log open, a second instance can't rotate it, so that instance logs to
//! its own `depthcut-<pid>.log` and skips the crash check.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

const LOG_NAME: &str = "depthcut.log";
const PREV_LOG_NAME: &str = "depthcut.prev.log";
const MARKER_NAME: &str = "session.running";
const PANIC_PREFIX: &str = "PANIC: ";
/// Stop writing to the file past this size, so a log storm can't fill the disk.
const MAX_LOG_BYTES: u64 = 64 * 1024 * 1024;

/// What the previous, abnormally ended session left behind.
#[derive(Clone, Debug, PartialEq)]
pub struct PreviousCrash {
    /// Recorded panic(s), if the app itself panicked. `None` means it was
    /// stopped from outside: a native library, the OS, or a power or hardware fault.
    pub panic: Option<String>,
    /// That session's log.
    pub log: PathBuf,
}

struct Session {
    /// Owned crash marker (primary instance only).
    marker: Option<PathBuf>,
    log: Option<PathBuf>,
}

static SESSION: OnceLock<Session> = OnceLock::new();
static PREVIOUS: Mutex<Option<PreviousCrash>> = Mutex::new(None);

pub fn logs_dir() -> PathBuf {
    crate::media::data_dir().join("logs")
}

/// Path of this session's log file (if one could be created).
pub fn log_path() -> Option<PathBuf> {
    SESSION.get().and_then(|s| s.log.clone())
}

/// Previous-session crash info, handed out once (to the UI).
pub fn take_previous_crash() -> Option<PreviousCrash> {
    PREVIOUS.lock().ok().and_then(|mut p| p.take())
}

/// Set up file logging, the panic hook and the crash marker. Call first thing in `main`.
pub fn init(default_filter: &str) {
    let dir = logs_dir();
    let _ = fs::create_dir_all(&dir);
    let opened = open_log(&dir);
    let (file, log, primary) = match opened {
        Some((f, p, primary)) => (Some(f), Some(p), primary),
        None => (None, None, false),
    };

    let mut builder = env_logger::Builder::from_env(env_logger::Env::default().default_filter_or(default_filter));
    builder.target(env_logger::Target::Pipe(Box::new(Tee { file, written: 0 })));
    builder.init();

    let marker = primary.then(|| dir.join(MARKER_NAME));
    if let Some(m) = &marker {
        if let Some(prev) = read_marker(m, &dir.join(PREV_LOG_NAME)) {
            log::warn!("previous session ended unexpectedly{}", prev.panic.as_deref().map(|p| format!(": {p}")).unwrap_or_default());
            if let Ok(mut slot) = PREVIOUS.lock() {
                *slot = Some(prev);
            }
        }
        let _ = fs::write(m, marker_header());
    }
    log::info!(
        "Depthcut {} ({} {}) · log: {}",
        env!("CARGO_PKG_VERSION"),
        std::env::consts::OS,
        std::env::consts::ARCH,
        log.as_deref().map(|p| p.display().to_string()).unwrap_or_else(|| "stderr only".into())
    );
    let _ = SESSION.set(Session { marker, log });
    install_panic_hook();
}

/// Clean shutdown: the next launch won't report a crash.
pub fn shutdown() {
    log::info!("clean exit");
    if let Some(m) = SESSION.get().and_then(|s| s.marker.as_ref()) {
        let _ = fs::remove_file(m);
    }
}

fn marker_header() -> String {
    let started = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    format!("pid {} · v{} · started {started}\n", std::process::id(), env!("CARGO_PKG_VERSION"))
}

/// Parse a leftover marker: `Some` means the session it belongs to never exited cleanly.
fn read_marker(marker: &Path, prev_log: &Path) -> Option<PreviousCrash> {
    let body = fs::read_to_string(marker).ok()?;
    let panics: Vec<&str> = body.lines().filter_map(|l| l.strip_prefix(PANIC_PREFIX)).collect();
    Some(PreviousCrash { panic: (!panics.is_empty()).then(|| panics.join("\n")), log: prev_log.to_path_buf() })
}

/// Rotate `depthcut.log` to `.prev.log` and open a fresh one, held so a
/// second instance can't rotate it. Returns (file, path, is_primary).
fn open_log(dir: &Path) -> Option<(File, PathBuf, bool)> {
    let cur = dir.join(LOG_NAME);
    let rotated = match fs::rename(&cur, dir.join(PREV_LOG_NAME)) {
        Ok(()) => true,
        Err(e) => e.kind() == io::ErrorKind::NotFound,
    };
    if rotated {
        if let Ok(f) = exclusive_create(&cur) {
            return Some((f, cur, true));
        }
    }
    // another instance owns the main log
    let own = dir.join(format!("depthcut-{}.log", std::process::id()));
    File::create(&own).ok().map(|f| (f, own, false))
}

fn exclusive_create(path: &Path) -> io::Result<File> {
    let mut o = OpenOptions::new();
    o.write(true).create(true).truncate(true);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        const FILE_SHARE_READ: u32 = 0x1; // readers OK, no rename/delete while we run
        o.share_mode(FILE_SHARE_READ);
    }
    o.open(path)
}

/// Log sink: stderr (debug builds and terminals) plus the session file.
struct Tee {
    file: Option<File>,
    written: u64,
}

impl Write for Tee {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let _ = io::stderr().write_all(buf);
        if let Some(f) = &mut self.file {
            if self.written + buf.len() as u64 <= MAX_LOG_BYTES {
                if f.write_all(buf).is_ok() {
                    self.written += buf.len() as u64;
                }
            } else {
                let _ = f.write_all(b"[log size limit reached; further output goes to stderr only]\n");
                self.file = None;
            }
        }
        Ok(buf.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        if let Some(f) = &mut self.file {
            let _ = f.flush();
        }
        io::stderr().flush()
    }
}

fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    payload.downcast_ref::<&str>().map(|s| s.to_string()).or_else(|| payload.downcast_ref::<String>().cloned()).unwrap_or_else(|| "(non-string panic payload)".into())
}

fn install_panic_hook() {
    let default = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let thread = std::thread::current();
        let name = thread.name().unwrap_or("<unnamed>").to_string();
        let msg = panic_message(info.payload());
        let at = info.location().map(|l| format!("{}:{}", l.file(), l.line())).unwrap_or_else(|| "?".into());
        let summary = format!("thread '{name}' at {at}: {msg}");
        log::error!("panic in {summary}\n{}", std::backtrace::Backtrace::force_capture());
        // Record it in the marker too. If the panic is caught (background
        // jobs catch theirs) and the app later exits cleanly, the marker is
        // removed and nothing is reported.
        if let Some(m) = SESSION.get().and_then(|s| s.marker.as_ref()) {
            if let Ok(mut f) = OpenOptions::new().append(true).open(m) {
                let _ = writeln!(f, "{PANIC_PREFIX}{}", summary.replace(['\r', '\n'], " "));
            }
        }
        default(info);
    }));
}

/// Short text for "Copy Details".
pub fn crash_details(c: &PreviousCrash) -> String {
    format!(
        "Depthcut {} ({} {})\nPrevious session ended unexpectedly.\n{}\nLog: {}",
        env!("CARGO_PKG_VERSION"),
        std::env::consts::OS,
        std::env::consts::ARCH,
        c.panic.as_deref().map(|p| format!("Panic: {p}")).unwrap_or_else(|| "No panic recorded (native crash, system crash or power loss).".into()),
        c.log.display()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("rcp-diag-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn rotates_the_previous_log_and_detects_a_second_instance() {
        let dir = temp_dir("rotate");
        fs::write(dir.join(LOG_NAME), "old session").unwrap();
        let (mut f, path, primary) = open_log(&dir).unwrap();
        assert!(primary);
        assert_eq!(path, dir.join(LOG_NAME));
        assert_eq!(fs::read_to_string(dir.join(PREV_LOG_NAME)).unwrap(), "old session");
        writeln!(f, "new session").unwrap();
        if cfg!(windows) {
            // the first instance still holds the log: a second one gets its own file
            let (_, other, primary2) = open_log(&dir).unwrap();
            assert!(!primary2);
            assert_ne!(other, path);
            assert_eq!(fs::read_to_string(dir.join(PREV_LOG_NAME)).unwrap(), "old session", "held log was not rotated away");
        }
        drop(f);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn leftover_marker_means_the_session_died() {
        let dir = temp_dir("marker");
        let marker = dir.join(MARKER_NAME);
        let prev = dir.join(PREV_LOG_NAME);
        assert_eq!(read_marker(&marker, &prev), None, "no marker: clean exit");
        fs::write(&marker, marker_header()).unwrap();
        assert_eq!(read_marker(&marker, &prev), Some(PreviousCrash { panic: None, log: prev.clone() }), "hard crash: no panic text");
        fs::write(&marker, format!("{}{PANIC_PREFIX}thread 'main' at a.rs:1: boom\n", marker_header())).unwrap();
        let c = read_marker(&marker, &prev).unwrap();
        assert_eq!(c.panic.as_deref(), Some("thread 'main' at a.rs:1: boom"));
        assert!(crash_details(&c).contains("boom"));
        let _ = fs::remove_dir_all(&dir);
    }
}
