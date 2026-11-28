//! Game events the Medal recorder (medal.tv) logged for its clips.
//!
//! Medal keeps a local SQLite library (`%APPDATA%\Medal\medal-<user>.db`)
//! whose `contents` rows hold each clip's path and a JSON(B) `metadata`
//! blob. For games it hooks (Valorant, ...) that blob has `bookmarks`:
//! `{"time": 39.633, "name": "2K", ...}` in seconds from the clip start,
//! taken from the game itself, so they land on the kill exactly where an
//! audio scan can only guess. Trimmed Medal edits keep no bookmarks of
//! their own; they point at their source (`parent_id`) and a `trimStart`.
//!
//! The library is opened read-only and cached until a database file changes.

use nle_core::time::Time;
use parking_lot::Mutex;
use serde_json::Value;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// One game event (a kill, a multi-kill, a clutch) at a time in the clip.
#[derive(Clone, Debug, PartialEq)]
pub struct GameEvent {
    /// Seconds from the start of the file.
    pub time: Time,
    /// As Medal names it: `Kill`, `2K` .. `5K`, `Clutch`, `Knife Kill`, ...
    pub name: String,
}

impl GameEvent {
    /// Whether the event marks a kill (everything Medal bookmarks for
    /// Valorant is one: multi-kills, clutches and aces end on a kill).
    pub fn is_kill(&self) -> bool {
        let n = self.name.to_ascii_lowercase();
        n.contains("kill") || n.contains("clutch") || n.contains("ace") || n.contains("thrifty") || multi_kill(&n).is_some()
    }

    /// Short label for a timeline badge: `2K`, `Kill`, `Clutch`.
    pub fn label(&self) -> &str {
        self.name.trim()
    }
}

/// `2K` -> 2.
fn multi_kill(n: &str) -> Option<u32> {
    let n = n.trim().trim_end_matches(['k', 'K']);
    n.parse().ok().filter(|k| (2..=9).contains(k))
}

/// One parsed `contents` row.
#[derive(Debug, Default, Clone, PartialEq)]
struct Row {
    events: Vec<(f64, String)>,
    duration: Option<f64>,
    /// `(parent local_content_id, trim start seconds)` for a trimmed edit.
    trim_of: Option<(String, f64)>,
}

/// Parse a row's `json(metadata)`.
fn parse_row(json: &str, parent_id: Option<String>) -> Row {
    let Ok(v) = serde_json::from_str::<Value>(json) else { return Row::default() };
    let num = |k: &str| v.get(k).and_then(Value::as_f64).filter(|x| x.is_finite());
    let events = v
        .get("bookmarks")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|b| {
                    let t = b.get("time").and_then(Value::as_f64).filter(|t| t.is_finite())?;
                    let name = b.get("name").and_then(Value::as_str).unwrap_or("Kill").trim();
                    Some((t, if name.is_empty() { "Kill".to_string() } else { name.to_string() }))
                })
                .collect()
        })
        .unwrap_or_default();
    let trimmed = v.get("wasTrimmed").and_then(Value::as_bool).unwrap_or(false) || num("trimStart").is_some();
    let trim_of = match (parent_id.filter(|p| !p.is_empty()), trimmed) {
        (Some(p), true) => Some((p, num("trimStart").or_else(|| num("trimStartTime")).unwrap_or(0.0))),
        _ => None,
    };
    Row { events, duration: num("clipDuration").or_else(|| num("trimDuration")), trim_of }
}

/// Events inside `[0, duration]`, sorted (Medal sometimes logs times from
/// before the clip, e.g. -1336 s).
fn clamp_events(events: &[(f64, String)], offset: f64, duration: Option<f64>) -> Vec<GameEvent> {
    let mut out: Vec<GameEvent> = events
        .iter()
        .map(|(t, n)| (t - offset, n))
        .filter(|(t, _)| *t >= 0.0 && duration.is_none_or(|d| d <= 0.0 || *t <= d + 0.05))
        .map(|(t, n)| GameEvent { time: Time::from_secs_f64(t), name: n.clone() })
        .collect();
    out.sort_by_key(|e| e.time);
    out.dedup_by(|a, b| a.time == b.time && a.name == b.name);
    out
}

fn path_key(p: &str) -> String {
    p.replace('/', "\\").trim_end_matches('\\').to_lowercase()
}

fn name_key(p: &str) -> Option<String> {
    Path::new(&p.replace('\\', "/")).file_name().map(|n| n.to_string_lossy().to_lowercase())
}

#[derive(Default)]
struct Library {
    stamp: Vec<(PathBuf, Option<SystemTime>)>,
    by_path: HashMap<String, Vec<GameEvent>>,
    /// File name -> events, for clips moved out of Medal's folder. `None`
    /// when two clips share the name (ambiguous).
    by_name: HashMap<String, Option<Vec<GameEvent>>>,
}

impl Library {
    /// Index `(video_path, row, local_content_id)` triples.
    fn build(rows: Vec<(String, Row, Option<String>)>) -> Self {
        let by_id: HashMap<&str, &Row> = rows.iter().filter_map(|(_, r, id)| Some((id.as_deref()?, r))).collect();
        let mut lib = Library::default();
        for (path, row, _) in &rows {
            let mut events = clamp_events(&row.events, 0.0, row.duration);
            if events.is_empty() {
                if let Some(parent) = row.trim_of.as_ref().and_then(|(p, start)| Some((by_id.get(p.as_str())?, *start))) {
                    events = clamp_events(&parent.0.events, parent.1, row.duration);
                }
            }
            if events.is_empty() {
                continue;
            }
            if let Some(n) = name_key(path) {
                lib.by_name.entry(n).and_modify(|e| *e = None).or_insert_with(|| Some(events.clone()));
            }
            lib.by_path.entry(path_key(path)).or_insert(events);
        }
        lib
    }

    fn lookup(&self, path: &Path) -> Vec<GameEvent> {
        let p = path.to_string_lossy();
        if let Some(e) = self.by_path.get(&path_key(&p)) {
            return e.clone();
        }
        name_key(&p).and_then(|n| self.by_name.get(&n).cloned().flatten()).unwrap_or_default()
    }
}

/// Medal's library databases (`medal-*.db`, guest last).
fn databases() -> Vec<PathBuf> {
    let Some(dir) = std::env::var_os("APPDATA").map(|a| PathBuf::from(a).join("Medal")) else { return Vec::new() };
    let mut dbs: Vec<PathBuf> = std::fs::read_dir(&dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            let n = p.file_name().map(|n| n.to_string_lossy().to_lowercase()).unwrap_or_default();
            n.starts_with("medal-") && n.ends_with(".db")
        })
        .collect();
    dbs.sort_by_key(|p| (p.to_string_lossy().to_lowercase().contains("guest"), p.clone()));
    dbs
}

fn stamp(dbs: &[PathBuf]) -> Vec<(PathBuf, Option<SystemTime>)> {
    let mtime = |p: &Path| std::fs::metadata(p).and_then(|m| m.modified()).ok();
    dbs.iter()
        .flat_map(|d| {
            let wal = PathBuf::from(format!("{}-wal", d.display()));
            [(d.clone(), mtime(d)), (wal.clone(), mtime(&wal))]
        })
        .collect()
}

fn read_db(db: &Path) -> rusqlite::Result<Vec<(String, Row, Option<String>)>> {
    use rusqlite::{Connection, OpenFlags};
    let conn = Connection::open_with_flags(db, OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX)?;
    conn.busy_timeout(std::time::Duration::from_millis(500))?;
    let mut st = conn.prepare("SELECT video_path, json(metadata), parent_id, local_content_id FROM contents WHERE video_path IS NOT NULL AND metadata IS NOT NULL")?;
    let rows = st.query_map([], |r| {
        let path: String = r.get(0)?;
        let json: Option<String> = r.get(1).ok();
        let parent: Option<String> = r.get(2).ok().flatten();
        let id: Option<String> = r.get(3).ok().flatten();
        Ok((path, json, parent, id))
    })?;
    Ok(rows.flatten().map(|(path, json, parent, id)| (path, json.map(|j| parse_row(&j, parent)).unwrap_or_default(), id)).collect())
}

static LIBRARY: Mutex<Option<Library>> = Mutex::new(None);

/// Medal's game events for a video file, or empty when Medal is not
/// installed or did not record this clip. Blocking (reads the database the
/// first time and after Medal writes to it): call it off the UI thread.
pub fn events_for(path: &Path) -> Vec<GameEvent> {
    let dbs = databases();
    if dbs.is_empty() {
        return Vec::new();
    }
    let now = stamp(&dbs);
    let mut lib = LIBRARY.lock();
    if lib.as_ref().is_none_or(|l| l.stamp != now) {
        let mut rows = Vec::new();
        for db in &dbs {
            match read_db(db) {
                Ok(r) => rows.extend(r),
                Err(e) => log::warn!("Medal library {}: {e}", db.display()),
            }
        }
        let mut built = Library::build(rows);
        built.stamp = now;
        log::info!("Medal library: {} clip(s) with game events", built.by_path.len());
        *lib = Some(built);
    }
    lib.as_ref().map(|l| l.lookup(path)).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    const CLIP: &str = r#"{"clipDuration": 70.0, "recorder": {"triggerType": "ICYMI"},
        "bookmarks": [
            {"time": 9.875, "name": "Kill", "data": {"Event ID": "1", "round": "9"}, "priority": 300},
            {"time": 40.671, "name": "2K", "data": {}},
            {"time": -1336.2, "name": "Kill"},
            {"time": 62.969, "name": "3K"},
            {"time": 95.0, "name": "Kill"}
        ]}"#;

    #[test]
    fn parses_bookmarks_inside_the_clip() {
        let row = parse_row(CLIP, None);
        assert_eq!(row.duration, Some(70.0));
        let ev = clamp_events(&row.events, 0.0, row.duration);
        let got: Vec<(f64, &str)> = ev.iter().map(|e| (e.time.as_secs_f64(), e.name.as_str())).collect();
        assert_eq!(got.len(), 3, "{got:?}");
        assert!((got[0].0 - 9.875).abs() < 1e-3 && got[0].1 == "Kill");
        assert_eq!((got[1].1, got[2].1), ("2K", "3K"));
        assert!(ev.iter().all(GameEvent::is_kill));
        assert!(!GameEvent { time: Time::ZERO, name: "Plant".into() }.is_kill());
        assert!(parse_row("not json", None).events.is_empty());
    }

    #[test]
    fn trimmed_edit_borrows_parent_events() {
        let trim = r#"{"wasTrimmed": true, "trimStart": 8.5, "trimDuration": 40.0, "bookmarks": []}"#;
        let rows = vec![
            ("C:\\Medal\\Clips\\Valorant\\A.mp4".to_string(), parse_row(CLIP, None), Some("pid".to_string())),
            ("C:\\Medal\\Edits\\A-trim-1.mp4".to_string(), parse_row(trim, Some("pid".into())), Some("tid".to_string())),
        ];
        let lib = Library::build(rows);
        let e = lib.lookup(Path::new("c:/medal/edits/a-trim-1.mp4"));
        assert_eq!(e.len(), 2, "kills at 9.875 and 40.671 fall in [8.5, 48.5]: {e:?}");
        assert!((e[0].time.as_secs_f64() - 1.375).abs() < 1e-3);
        // a copy moved elsewhere is still found by its file name
        assert_eq!(lib.lookup(Path::new("D:\\montage\\A.mp4")).len(), 3);
        assert!(lib.lookup(Path::new("D:\\other.mp4")).is_empty());
    }
}
