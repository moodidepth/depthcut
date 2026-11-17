//! Discord Rich Presence over Discord's local IPC socket / named pipe.
//!
//! Connects asynchronously on a background thread so UI rendering never blocks,
//! automatically reconnects if Discord starts after Rust Cut Pro, and persists
//! configuration in `%APPDATA%\RustCutPro\discord.json`.

use crate::media::data_dir;
use crate::theme;
use eframe::egui;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const DEFAULT_ASSET_KEY: &str = "rust_cut_pro";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct DiscordConfig {
    pub enabled: bool,
    pub app_id: String,
    pub asset_key: String,
    pub show_project_name: bool,
    pub show_timeline_stats: bool,
}

impl Default for DiscordConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            app_id: String::new(),
            asset_key: DEFAULT_ASSET_KEY.to_string(),
            show_project_name: true,
            show_timeline_stats: true,
        }
    }
}

impl DiscordConfig {
    fn path() -> PathBuf {
        data_dir().join("discord.json")
    }

    pub fn load() -> Self {
        let mut cfg = std::fs::read_to_string(Self::path())
            .ok()
            .and_then(|s| serde_json::from_str::<DiscordConfig>(&s).ok())
            .unwrap_or_default();
        if cfg.app_id.trim().is_empty() {
            if let Ok(env_id) = std::env::var("RCP_DISCORD_APP_ID") {
                cfg.app_id = env_id.trim().to_string();
            }
        }
        if cfg.asset_key.trim().is_empty() {
            cfg.asset_key = DEFAULT_ASSET_KEY.to_string();
        }
        cfg
    }

    pub fn save(&self) -> std::io::Result<()> {
        let path = Self::path();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let json = serde_json::to_string_pretty(self).map_err(std::io::Error::other)?;
        std::fs::write(path, json)
    }

    pub fn valid_app_id(&self) -> Option<&str> {
        let trimmed = self.app_id.trim();
        if !trimmed.is_empty() && trimmed.chars().all(|c| c.is_ascii_digit()) {
            Some(trimmed)
        } else {
            None
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PresenceSnapshot {
    pub project_name: String,
    pub clip_count: usize,
    pub duration_secs: u64,
    pub resolution: String,
    pub playing: bool,
    pub exporting: bool,
}

#[derive(Clone, Debug)]
pub enum ConnectionStatus {
    Disabled,
    MissingAppId,
    WaitingForDiscord,
    Connected,
    Error(String),
}

enum WorkerMsg {
    Update(PresenceSnapshot),
    ConfigChanged(DiscordConfig),
    Shutdown,
}

pub struct DiscordDialog {
    pub open: bool,
    draft: DiscordConfig,
    feedback: Option<String>,
}

impl DiscordDialog {
    pub fn from_config(cfg: &DiscordConfig) -> Self {
        Self { open: false, draft: cfg.clone(), feedback: None }
    }
}

pub struct DiscordPresence {
    tx: Sender<WorkerMsg>,
    status: Arc<Mutex<ConnectionStatus>>,
    config: DiscordConfig,
    last_snapshot: Option<PresenceSnapshot>,
    last_sent: Instant,
    worker: Option<JoinHandle<()>>,
    pub dialog: DiscordDialog,
}

impl DiscordPresence {
    pub fn start() -> Self {
        let config = DiscordConfig::load();
        let initial_status = if !config.enabled {
            ConnectionStatus::Disabled
        } else if config.valid_app_id().is_none() {
            ConnectionStatus::MissingAppId
        } else {
            ConnectionStatus::WaitingForDiscord
        };
        let status = Arc::new(Mutex::new(initial_status));
        let (tx, rx) = mpsc::channel();
        let worker_cfg = config.clone();
        let worker_status = status.clone();
        let worker = thread::Builder::new()
            .name("discord-rpc".into())
            .spawn(move || run_worker(rx, worker_cfg, worker_status))
            .ok();

        Self {
            tx,
            status,
            dialog: DiscordDialog::from_config(&config),
            config,
            last_snapshot: None,
            last_sent: Instant::now() - Duration::from_secs(10),
            worker,
        }
    }

    pub fn update(&mut self, snapshot: PresenceSnapshot) {
        let changed = self.last_snapshot.as_ref() != Some(&snapshot);
        if changed && self.last_sent.elapsed() >= Duration::from_secs(2) {
            self.last_snapshot = Some(snapshot.clone());
            self.last_sent = Instant::now();
            let _ = self.tx.send(WorkerMsg::Update(snapshot));
        }
    }

    pub fn status(&self) -> ConnectionStatus {
        self.status.lock().clone()
    }

    pub fn open_dialog(&mut self) {
        self.dialog.draft = self.config.clone();
        self.dialog.feedback = None;
        self.dialog.open = true;
    }

    pub fn show_dialog(&mut self, ctx: &egui::Context) {
        if !self.dialog.open {
            return;
        }
        let mut open = self.dialog.open;
        let status = self.status();
        let mut apply = false;

        egui::Window::new("Discord Rich Presence")
            .open(&mut open)
            .resizable(false)
            .default_width(470.0)
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.label("Status:");
                    let (label, color) = match &status {
                        ConnectionStatus::Connected => ("Connected to Discord", theme::AUDIO),
                        ConnectionStatus::WaitingForDiscord => ("Waiting for Discord desktop client…", theme::SNAP),
                        ConnectionStatus::MissingAppId => ("Setup needed — add a Discord Application ID below", theme::SNAP),
                        ConnectionStatus::Disabled => ("Disabled", theme::TEXT_DIM),
                        ConnectionStatus::Error(err) => (err.as_str(), theme::PLAYHEAD),
                    };
                    ui.label(egui::RichText::new(label).strong().color(color));
                });

                ui.add_space(6.0);
                ui.checkbox(&mut self.dialog.draft.enabled, "Show Rust Cut Pro activity on Discord");
                ui.checkbox(&mut self.dialog.draft.show_project_name, "Display current project name");
                ui.checkbox(&mut self.dialog.draft.show_timeline_stats, "Display clip count, duration & playback state");

                ui.add_space(6.0);
                egui::Grid::new("discord_cfg").num_columns(2).spacing([10.0, 6.0]).show(ui, |ui| {
                    ui.label("Discord Application ID:");
                    ui.add(
                        egui::TextEdit::singleline(&mut self.dialog.draft.app_id)
                            .hint_text("e.g. 1234567890123456789")
                            .desired_width(240.0),
                    );
                    ui.end_row();

                    ui.label("Icon Asset Key:");
                    ui.add(
                        egui::TextEdit::singleline(&mut self.dialog.draft.asset_key)
                            .hint_text(DEFAULT_ASSET_KEY)
                            .desired_width(240.0),
                    );
                    ui.end_row();
                });

                ui.add_space(8.0);
                egui::CollapsingHeader::new("One-time Discord setup (takes ~30 seconds)")
                    .default_open(self.config.valid_app_id().is_none())
                    .show(ui, |ui| {
                        ui.label("1. Open the Discord Developer Portal and click New Application → name it \"Rust Cut Pro\".");
                        ui.label("2. Upload the Rust Cut Pro icon as the App Icon (and under Rich Presence → Art Assets as \"rust_cut_pro\").");
                        ui.label("3. Copy the Application ID from General Information and paste it above.");
                        ui.add_space(4.0);
                        ui.horizontal(|ui| {
                            if ui.button("Open Developer Portal").clicked() {
                                ctx.open_url(egui::OpenUrl::new_tab("https://discord.com/developers/applications"));
                            }
                            if let Some(icon_dir) = discord_icon_dir() {
                                if ui.button("Open Icon Folder").clicked() {
                                    open_folder(&icon_dir);
                                }
                            }
                        });
                    });

                if let Some(msg) = &self.dialog.feedback {
                    ui.add_space(4.0);
                    ui.label(egui::RichText::new(msg).small().color(theme::VIDEO));
                }

                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    if ui.button("Save & Connect").clicked() {
                        apply = true;
                    }
                    if ui.button("Close").clicked() {
                        self.dialog.open = false;
                    }
                });
            });

        if apply {
            let trimmed_id = self.dialog.draft.app_id.trim().to_string();
            if self.dialog.draft.enabled && !trimmed_id.is_empty() && !trimmed_id.chars().all(|c| c.is_ascii_digit()) {
                self.dialog.feedback = Some("Application ID must contain only digits.".to_string());
            } else {
                self.dialog.draft.app_id = trimmed_id;
                if self.dialog.draft.asset_key.trim().is_empty() {
                    self.dialog.draft.asset_key = DEFAULT_ASSET_KEY.to_string();
                } else {
                    self.dialog.draft.asset_key = self.dialog.draft.asset_key.trim().to_string();
                }
                self.config = self.dialog.draft.clone();
                match self.config.save() {
                    Ok(()) => self.dialog.feedback = Some("Saved Discord Rich Presence settings.".to_string()),
                    Err(e) => self.dialog.feedback = Some(format!("Failed to save config: {e}")),
                }
                let _ = self.tx.send(WorkerMsg::ConfigChanged(self.config.clone()));
                self.last_sent = Instant::now() - Duration::from_secs(10);
            }
        }
        if !open {
            self.dialog.open = false;
        }
    }
}

impl Drop for DiscordPresence {
    fn drop(&mut self) {
        let _ = self.tx.send(WorkerMsg::Shutdown);
        if let Some(handle) = self.worker.take() {
            let _ = handle.join();
        }
    }
}

fn discord_icon_dir() -> Option<PathBuf> {
    if let Ok(exe) = std::env::current_exe() {
        for anc in exe.ancestors() {
            let candidate = anc.join("assets").join("rust-cut-pro-discord.png");
            if candidate.is_file() {
                return candidate.parent().map(Path::to_path_buf);
            }
        }
    }
    let cwd_assets = PathBuf::from("assets").join("rust-cut-pro-discord.png");
    if cwd_assets.is_file() {
        return cwd_assets.parent().map(Path::to_path_buf);
    }
    None
}

fn open_folder(dir: &Path) {
    #[cfg(windows)]
    {
        let _ = std::process::Command::new("explorer").arg(dir).spawn();
    }
    #[cfg(target_os = "macos")]
    {
        let _ = std::process::Command::new("open").arg(dir).spawn();
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let _ = std::process::Command::new("xdg-open").arg(dir).spawn();
    }
}

// ----- background IPC worker -------------------------------------------------

const OP_HANDSHAKE: u32 = 0;
const OP_FRAME: u32 = 1;
const OP_CLOSE: u32 = 2;

struct IpcConn {
    stream: IpcStream,
    nonce: u64,
}

#[cfg(windows)]
type IpcStream = std::fs::File;
#[cfg(unix)]
type IpcStream = std::os::unix::net::UnixStream;

fn open_discord_pipe() -> std::io::Result<IpcStream> {
    #[cfg(windows)]
    {
        let mut last_err = std::io::Error::new(std::io::ErrorKind::NotFound, "Discord pipe not found");
        for i in 0..10 {
            let path = format!(r"\\.\pipe\discord-ipc-{i}");
            match std::fs::OpenOptions::new().read(true).write(true).open(&path) {
                Ok(f) => return Ok(f),
                Err(e) => last_err = e,
            }
        }
        Err(last_err)
    }
    #[cfg(unix)]
    {
        use std::os::unix::net::UnixStream;
        let mut dirs = Vec::new();
        for var in ["XDG_RUNTIME_DIR", "TMPDIR", "TMP", "TEMP"] {
            if let Ok(v) = std::env::var(var) {
                dirs.push(PathBuf::from(v));
            }
        }
        dirs.push(PathBuf::from("/tmp"));
        let mut last_err = std::io::Error::new(std::io::ErrorKind::NotFound, "Discord IPC socket not found");
        for dir in dirs {
            for sub in ["", "app/com.discordapp.Discord", "snap.discord"] {
                let base = if sub.is_empty() { dir.clone() } else { dir.join(sub) };
                for i in 0..10 {
                    let sock = base.join(format!("discord-ipc-{i}"));
                    match UnixStream::connect(&sock) {
                        Ok(s) => {
                            let _ = s.set_read_timeout(Some(Duration::from_secs(2)));
                            let _ = s.set_write_timeout(Some(Duration::from_secs(2)));
                            return Ok(s);
                        }
                        Err(e) => last_err = e,
                    }
                }
            }
        }
        Err(last_err)
    }
}

impl IpcConn {
    fn connect(client_id: &str) -> Result<Self, String> {
        let mut stream = open_discord_pipe().map_err(|e| format!("pipe: {e}"))?;
        let handshake = serde_json::json!({ "v": 1, "client_id": client_id }).to_string();
        write_packet(&mut stream, OP_HANDSHAKE, handshake.as_bytes()).map_err(|e| format!("handshake write: {e}"))?;
        let (op, reply) = read_packet(&mut stream).map_err(|e| format!("handshake read: {e}"))?;
        if op == OP_CLOSE {
            return Err(format!("Discord rejected handshake: {reply}"));
        }
        Ok(Self { stream, nonce: 1 })
    }

    fn set_activity(&mut self, cfg: &DiscordConfig, snap: &PresenceSnapshot, start_unix: u64) -> Result<(), String> {
        self.nonce += 1;
        let details = if snap.exporting {
            if cfg.show_project_name {
                format!("Exporting: {}", snap.project_name)
            } else {
                "Exporting movie".to_string()
            }
        } else if cfg.show_project_name {
            format!("Editing: {}", snap.project_name)
        } else {
            "Editing video".to_string()
        };

        let state = if cfg.show_timeline_stats {
            let dur = format_mm_ss(snap.duration_secs);
            let clips = if snap.clip_count == 1 { "1 clip".to_string() } else { format!("{} clips", snap.clip_count) };
            let mode = if snap.exporting {
                "Rendering"
            } else if snap.playing {
                "Playing"
            } else {
                "Timeline"
            };
            format!("{mode} · {clips} · {dur} · {}", snap.resolution)
        } else if snap.playing {
            "Playing timeline".to_string()
        } else {
            "In editor".to_string()
        };

        let asset_key = if cfg.asset_key.trim().is_empty() { DEFAULT_ASSET_KEY } else { cfg.asset_key.trim() };

        let payload = serde_json::json!({
            "cmd": "SET_ACTIVITY",
            "args": {
                "pid": std::process::id(),
                "activity": {
                    "details": details,
                    "state": state,
                    "timestamps": {
                        "start": start_unix
                    },
                    "assets": {
                        "large_image": asset_key,
                        "large_text": "Rust Cut Pro"
                    }
                }
            },
            "nonce": self.nonce.to_string()
        })
        .to_string();

        write_packet(&mut self.stream, OP_FRAME, payload.as_bytes()).map_err(|e| format!("write: {e}"))?;
        let (op, _) = read_packet(&mut self.stream).map_err(|e| format!("read: {e}"))?;
        if op == OP_CLOSE {
            return Err("Discord closed IPC connection".to_string());
        }
        Ok(())
    }

    fn clear_and_close(mut self) {
        self.nonce += 1;
        let clear = serde_json::json!({
            "cmd": "SET_ACTIVITY",
            "args": { "pid": std::process::id() },
            "nonce": self.nonce.to_string()
        })
        .to_string();
        let _ = write_packet(&mut self.stream, OP_FRAME, clear.as_bytes());
        let _ = write_packet(&mut self.stream, OP_CLOSE, b"{}");
    }
}

fn write_packet<W: Write>(w: &mut W, op: u32, payload: &[u8]) -> std::io::Result<()> {
    let mut header = [0u8; 8];
    header[..4].copy_from_slice(&op.to_le_bytes());
    header[4..].copy_from_slice(&(payload.len() as u32).to_le_bytes());
    w.write_all(&header)?;
    w.write_all(payload)?;
    w.flush()
}

fn read_packet<R: Read>(r: &mut R) -> std::io::Result<(u32, String)> {
    let mut header = [0u8; 8];
    r.read_exact(&mut header)?;
    let op = u32::from_le_bytes(header[..4].try_into().unwrap());
    let len = u32::from_le_bytes(header[4..].try_into().unwrap()).min(65536) as usize;
    let mut buf = vec![0u8; len];
    if len > 0 {
        r.read_exact(&mut buf)?;
    }
    Ok((op, String::from_utf8_lossy(&buf).into_owned()))
}

fn format_mm_ss(total_secs: u64) -> String {
    let h = total_secs / 3600;
    let m = (total_secs % 3600) / 60;
    let s = total_secs % 60;
    if h > 0 {
        format!("{h:02}:{m:02}:{s:02}")
    } else {
        format!("{m:02}:{s:02}")
    }
}

fn run_worker(rx: Receiver<WorkerMsg>, mut cfg: DiscordConfig, status: Arc<Mutex<ConnectionStatus>>) {
    let start_unix = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let mut conn: Option<IpcConn> = None;
    let mut latest: Option<PresenceSnapshot> = None;
    let mut last_attempt = Instant::now() - Duration::from_secs(60);
    let mut dirty = false;

    loop {
        if !cfg.enabled {
            if let Some(c) = conn.take() {
                c.clear_and_close();
            }
            *status.lock() = ConnectionStatus::Disabled;
        } else if let Some(app_id) = cfg.valid_app_id().map(str::to_string) {
            if conn.is_none() && last_attempt.elapsed() >= Duration::from_secs(5) {
                last_attempt = Instant::now();
                match IpcConn::connect(&app_id) {
                    Ok(c) => {
                        conn = Some(c);
                        dirty = true;
                        *status.lock() = ConnectionStatus::Connected;
                    }
                    Err(e) => {
                        log::debug!("Discord IPC not connected: {e}");
                        *status.lock() = if e.starts_with("Discord rejected handshake") {
                            ConnectionStatus::Error("Invalid Discord Application ID (handshake rejected)".to_string())
                        } else {
                            ConnectionStatus::WaitingForDiscord
                        };
                    }
                }
            }
            if dirty {
                if let (Some(c), Some(snap)) = (conn.as_mut(), latest.as_ref()) {
                    match c.set_activity(&cfg, snap, start_unix) {
                        Ok(()) => {
                            dirty = false;
                            *status.lock() = ConnectionStatus::Connected;
                        }
                        Err(e) => {
                            log::debug!("Discord SET_ACTIVITY failed: {e}");
                            conn = None;
                            *status.lock() = ConnectionStatus::WaitingForDiscord;
                        }
                    }
                }
            }
        } else {
            if let Some(c) = conn.take() {
                c.clear_and_close();
            }
            *status.lock() = ConnectionStatus::MissingAppId;
        }

        match rx.recv_timeout(Duration::from_secs(2)) {
            Ok(WorkerMsg::Update(snap)) => {
                latest = Some(snap);
                dirty = true;
            }
            Ok(WorkerMsg::ConfigChanged(new_cfg)) => {
                let id_changed = cfg.app_id != new_cfg.app_id || cfg.enabled != new_cfg.enabled;
                cfg = new_cfg;
                if id_changed {
                    if let Some(c) = conn.take() {
                        c.clear_and_close();
                    }
                    last_attempt = Instant::now() - Duration::from_secs(60);
                }
                dirty = true;
            }
            Ok(WorkerMsg::Shutdown) | Err(RecvTimeoutError::Disconnected) => {
                if let Some(c) = conn.take() {
                    c.clear_and_close();
                }
                break;
            }
            Err(RecvTimeoutError::Timeout) => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn app_id_validation() {
        let mut cfg = DiscordConfig::default();
        assert_eq!(cfg.valid_app_id(), None);
        cfg.app_id = "  123456789012345678 ".into();
        assert_eq!(cfg.valid_app_id(), Some("123456789012345678"));
        cfg.app_id = "abc123".into();
        assert_eq!(cfg.valid_app_id(), None);
    }

    #[test]
    fn packet_round_trip() {
        let mut buf = Vec::new();
        write_packet(&mut buf, OP_HANDSHAKE, br#"{"v":1}"#).unwrap();
        let mut cursor = Cursor::new(buf);
        let (op, body) = read_packet(&mut cursor).unwrap();
        assert_eq!(op, OP_HANDSHAKE);
        assert_eq!(body, r#"{"v":1}"#);
    }

    #[test]
    fn duration_formatting() {
        assert_eq!(format_mm_ss(68), "01:08");
        assert_eq!(format_mm_ss(3665), "01:01:05");
    }
}
