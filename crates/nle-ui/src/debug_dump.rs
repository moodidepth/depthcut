//! Hidden state dump for automated UI testing.
//!
//! * `DEPTHCUT_STATE_DUMP=<file>` (or `--state-dump=<file>`): the app keeps
//!   `<file>` up to date with its state as JSON. It rewrites it when anything
//!   in it changes, at least once a second, and right away when `<file>.req`
//!   appears (the request file's text comes back as `request`, then the
//!   request file is removed). A driver writes a nonce to `.req` after an
//!   action and waits for that nonce in the dump.
//! * Ctrl+Alt+Shift+D writes a one-off dump to the logs folder.
//!
//! The dump holds the project format, every timeline clip (placement, source
//! in / out, speed, effects and its full serialized form), markers, assets,
//! selection, playhead, open dialogs / menus, background jobs, the last
//! status message, recent widget clicks and the on-screen rectangles of the
//! panels, clips and media tiles (egui points; multiply by
//! `window.pixels_per_point` and add the window's client origin for pixels).

use crate::app::App;
use eframe::egui;
use egui::Rect;
use nle_core::{ClipKind, Time};
use serde_json::{json, Map, Value};
use std::cell::RefCell;
use std::collections::VecDeque;
use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// How many recent widget events the dump keeps.
const EVENTS_KEPT: usize = 30;

#[derive(Default)]
pub struct DebugDump {
    /// Continuous dump target (from the environment / command line).
    path: Option<PathBuf>,
    /// Last written body without its volatile header, to skip identical writes.
    last_body: String,
    last_write: Option<Instant>,
    seq: u64,
    /// Rectangles drawn this frame, by name (`panel:timeline`, `clip:12`, `asset:3`).
    rects: RefCell<Vec<(String, Rect)>>,
    events: VecDeque<Value>,
    started: Option<Instant>,
    /// Ctrl+Alt+Shift+D was pressed this frame.
    snapshot: bool,
}

impl DebugDump {
    pub fn from_env() -> Self {
        let arg = std::env::args().find_map(|a| a.strip_prefix("--state-dump=").map(PathBuf::from));
        let path = arg.or_else(|| std::env::var_os("DEPTHCUT_STATE_DUMP").filter(|v| !v.is_empty()).map(PathBuf::from));
        if let Some(p) = &path {
            log::info!("State dump enabled: {}", p.display());
        }
        Self { path, started: Some(Instant::now()), ..Default::default() }
    }

    /// Start of a frame: forget last frame's rectangles and take the
    /// snapshot hotkey before the shortcut handler can see it.
    pub fn begin_frame(&mut self, ctx: &egui::Context) {
        self.rects.borrow_mut().clear();
        self.snapshot = ctx.input_mut(|i| i.consume_key(egui::Modifiers::CTRL | egui::Modifiers::ALT | egui::Modifiers::SHIFT, egui::Key::D));
    }

    /// Remember where something was drawn this frame.
    pub fn rect(&self, name: impl Into<String>, r: Rect) {
        self.rects.borrow_mut().push((name.into(), r));
    }

    fn record_events(&mut self, ctx: &egui::Context) {
        let events = ctx.output(|o| o.events.clone());
        for e in events {
            let (kind, info) = match e {
                egui::output::OutputEvent::Clicked(i) => ("clicked", i),
                egui::output::OutputEvent::DoubleClicked(i) => ("double_clicked", i),
                egui::output::OutputEvent::TripleClicked(i) => ("triple_clicked", i),
                egui::output::OutputEvent::FocusGained(i) => ("focus", i),
                egui::output::OutputEvent::TextSelectionChanged(i) => ("text_selection", i),
                egui::output::OutputEvent::ValueChanged(i) => ("changed", i),
            };
            let mut v = json!({ "event": kind, "widget": format!("{:?}", info.typ), "frame": ctx.cumulative_pass_nr() });
            if let Some(l) = info.label.filter(|l| !l.is_empty()) {
                v["label"] = json!(l);
            }
            if let Some(x) = info.current_text_value {
                v["text"] = json!(x);
            }
            if let Some(x) = info.value {
                v["value"] = json!(x);
            }
            if let Some(x) = info.selected {
                v["selected"] = json!(x);
            }
            self.events.push_back(v);
            while self.events.len() > EVENTS_KEPT {
                self.events.pop_front();
            }
        }
    }
}

fn secs(t: Time) -> f64 {
    (t.as_secs_f64() * 1e6).round() / 1e6
}

fn rect_json(r: Rect) -> Value {
    let f = |x: f32| (x as f64 * 10.0).round() / 10.0;
    json!([f(r.min.x), f(r.min.y), f(r.width()), f(r.height())])
}

/// Called once per frame after everything is drawn.
pub fn tick(app: &mut App, ctx: &egui::Context) {
    app.dbg.record_events(ctx);
    if std::mem::take(&mut app.dbg.snapshot) {
        let path = crate::diagnostics::logs_dir().join(format!("state-{}.json", unix_ms()));
        let body = body(app, ctx);
        match std::fs::write(&path, serde_json::to_string_pretty(&wrap(app, ctx, body, 0, None)).unwrap_or_default()) {
            Ok(()) => app.status(format!("State written to {}", path.display())),
            Err(e) => app.status(format!("Could not write the state dump: {e}")),
        }
    }
    let Some(path) = app.dbg.path.clone() else {
        return;
    };
    let req_path = PathBuf::from(format!("{}.req", path.display()));
    let request = std::fs::read_to_string(&req_path).ok();
    let body = body(app, ctx);
    let text = serde_json::to_string(&body).unwrap_or_default();
    let stale = app.dbg.last_write.is_none_or(|t| t.elapsed() > Duration::from_secs(1));
    if request.is_none() && !stale && text == app.dbg.last_body {
        return;
    }
    app.dbg.seq += 1;
    let out = serde_json::to_string_pretty(&wrap(app, ctx, body, app.dbg.seq, request.as_deref())).unwrap_or_default();
    // write then rename, so a reader never sees half a file
    let tmp = PathBuf::from(format!("{}.tmp", path.display()));
    if std::fs::write(&tmp, out).is_ok() && std::fs::rename(&tmp, &path).is_ok() {
        if request.is_some() {
            let _ = std::fs::remove_file(&req_path);
        }
        app.dbg.last_body = text;
        app.dbg.last_write = Some(Instant::now());
    }
}

fn unix_ms() -> u128 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0)
}

/// The dump with its header: fields that change every frame stay out of the
/// body, so an idle app does not rewrite the file.
fn wrap(app: &App, ctx: &egui::Context, body: Value, seq: u64, request: Option<&str>) -> Value {
    let mut m = Map::new();
    m.insert("seq".into(), json!(seq));
    m.insert("written_unix_ms".into(), json!(unix_ms() as u64));
    m.insert("request".into(), json!(request.map(str::trim)));
    m.insert("frame".into(), json!(ctx.cumulative_pass_nr()));
    m.insert("uptime_s".into(), json!(app.dbg.started.map(|s| s.elapsed().as_secs_f32())));
    m.insert("status_age_ms".into(), json!(app.last_status().map(|(_, age)| age.as_millis() as u64)));
    if let Value::Object(b) = body {
        m.extend(b);
    }
    Value::Object(m)
}

fn clip_kind(k: &ClipKind) -> Value {
    match k {
        ClipKind::Asset { asset } => json!({ "type": "asset", "asset": asset.0 }),
        ClipKind::Gap => json!({ "type": "gap" }),
        ClipKind::Title(t) => json!({ "type": "title", "text": t.text }),
        ClipKind::Adjustment => json!({ "type": "adjustment" }),
        ClipKind::Compound(_) => json!({ "type": "compound" }),
    }
}

fn body(app: &App, ctx: &egui::Context) -> Value {
    let tl = &app.project.timeline;
    let f = tl.format;
    let clips: Vec<Value> = tl
        .placed_clips()
        .iter()
        .map(|p| {
            let c = p.clip;
            let mut raw = serde_json::to_value(c).unwrap_or(Value::Null);
            if let Value::Object(m) = &mut raw {
                m.remove("connected"); // listed as their own entries
            }
            let speed = c.retime.as_ref().map(|r| serde_json::to_value(r).unwrap_or(Value::Null));
            json!({
                "id": c.id.0,
                "name": c.name,
                "kind": clip_kind(&c.kind),
                "lane": p.lane,
                "parent": p.parent.map(|id| id.0),
                "start_s": secs(p.start),
                "end_s": secs(p.end()),
                "duration_s": secs(c.duration),
                "source_in_s": secs(c.source_in),
                "source_out_s": secs(c.source_in + c.duration),
                "enabled": c.enabled,
                "video": c.av.video,
                "audio": c.av.audio,
                "retime": speed,
                "transition_in": c.transition_in.as_ref().map(|t| json!({ "kind": format!("{:?}", t.kind), "duration_s": secs(t.duration) })),
                "transition_out": c.transition_out.as_ref().map(|t| json!({ "kind": format!("{:?}", t.kind), "duration_s": secs(t.duration) })),
                "effects": c.effects.iter().map(|e| json!({ "kind": format!("{:?}", e.kind), "enabled": e.enabled, "params": serde_json::to_value(&e.params).unwrap_or(Value::Null) })).collect::<Vec<_>>(),
                "selected": app.selection.contains(&c.id),
                "raw": raw,
            })
        })
        .collect();
    let markers: Vec<Value> = tl.markers.iter().map(|m| json!({ "time_s": secs(m.time), "kind": format!("{:?}", m.kind), "name": m.name })).collect();
    let assets: Vec<Value> = app
        .project
        .assets
        .iter()
        .map(|a| {
            json!({
                "id": a.id.0,
                "name": a.name,
                "path": a.path,
                "kind": format!("{:?}", a.kind),
                "duration_s": secs(a.duration),
                "video": a.video.as_ref().map(|v| json!({ "width": v.width, "height": v.height, "fps": v.frame_rate.fps(), "vfr": v.is_vfr, "codec": v.codec })),
                "audio": a.audio.is_some(),
                "loudness_lufs": a.loudness_lufs,
                "failed": app.library.failed.get(&a.id),
                "audio_loaded": app.library.audio.contains_key(&a.id),
            })
        })
        .collect();

    let mut dialogs = Vec::new();
    let mut flag = |on: bool, name: &str| {
        if on {
            dialogs.push(name.to_string());
        }
    };
    flag(app.export.open, "export");
    flag(app.audio_export.open, "audio_export");
    flag(app.voiceover.open, "voiceover");
    flag(app.show_captions, "captions");
    flag(app.transcript.open, "transcript");
    flag(app.graph.open, "graph_editor");
    flag(app.show_shortcuts, "shortcuts");
    flag(app.about.open, "about");
    flag(app.welcome.open, "welcome");
    flag(app.copilot.open, "copilot");
    flag(app.discord.dialog.open, "discord");
    flag(app.marker_dialog.is_some(), "marker_edit");
    flag(app.multicam.dialog.is_some(), "new_multicam");
    flag(app.grade.paste.is_some(), "paste_attributes");
    flag(app.recovery.is_some(), "recovery");
    flag(app.previous_crash.is_some(), "crash_notice");
    flag(app.mixer_view.open, "audio_mixer");
    flag(app.has_pending_switch(), "save_changes");

    let windows: Vec<Value> = ctx.memory(|m| {
        let visible = m.areas().visible_layer_ids();
        let mut v: Vec<Value> = m
            .layer_ids()
            .filter(|l| visible.contains(l) && l.order != egui::Order::Background && l.order != egui::Order::Tooltip)
            .filter_map(|l| m.area_rect(l.id).map(|r| json!({ "order": format!("{:?}", l.order), "id": format!("{:?}", l.id), "rect": rect_json(r) })))
            .collect();
        v.dedup();
        v
    });

    let mut rects = Map::new();
    for (name, r) in app.dbg.rects.borrow().iter() {
        rects.insert(name.clone(), rect_json(*r));
    }

    let status = app.last_status().map(|(s, _)| s.to_string());
    let inner = ctx.input(|i| i.viewport().inner_rect);
    json!({
        "app": {
            "version": env!("CARGO_PKG_VERSION"),
            "pid": std::process::id(),
            "log": crate::diagnostics::log_path(),
            "project_name": app.project.name,
            "project_path": app.project_path,
            "edited": app.is_edited(),
            "gpu": app.gpu_label,
        },
        "window": {
            "pixels_per_point": ctx.pixels_per_point(),
            "inner_rect": inner.map(rect_json),
            "screen_rect": rect_json(ctx.screen_rect()),
            "focused": ctx.input(|i| i.viewport().focused),
            "minimized": ctx.input(|i| i.viewport().minimized),
        },
        "format": {
            "width": f.width,
            "height": f.height,
            "fps": f.rate.fps(),
            "rate": [f.rate.num, f.rate.den],
            "chosen": tl.format_chosen,
        },
        "playhead_s": secs(app.playhead),
        "skimmer_s": app.skimmer.map(secs),
        "playing": app.playback.playing,
        "speed": app.playback.speed,
        "tool": format!("{:?}", app.tool),
        "focus": app.focus.levels().iter().map(|l| l.name.clone()).collect::<Vec<_>>(),
        "selection": app.selection.iter().map(|c| c.0).collect::<Vec<_>>(),
        "browser": { "tab": format!("{:?}", app.online.tab), "selected_asset": app.browser.selected.map(|a| a.0) },
        "inspector_tab": format!("{:?}", app.inspector_tab),
        "timeline_view": {
            "pps": app.tl.pps,
            "scroll_s": app.tl.scroll,
            "vscroll": app.tl.vscroll,
            "range": app.tl.range.map(|(a, b)| [secs(a), secs(b)]),
            "hovered_clip": app.tl.hovered_clip.map(|c| c.0),
            "dragging": app.tl.drag.is_some(),
        },
        "timeline": {
            "name": tl.name,
            "count": app.project.timelines.len() + 1,
            "duration_s": secs(tl.duration()),
            "snapping": tl.snapping,
            "clips": clips,
            "markers": markers,
            "captions": tl.captions.segments.len(),
            "marks": [app.source.record.mark_in.map(secs), app.source.record.mark_out.map(secs)],
        },
        "assets": assets,
        "history": {
            "undo": app.history.undo_len(),
            "redo": app.history.redo_len(),
            "undo_label": app.history.undo_label(),
            "redo_label": app.history.redo_label(),
        },
        "ui": {
            "dialogs": dialogs,
            "menu_open": app.menu_open(),
            "popup_open": ctx.memory(|m| m.any_popup_open()),
            "text_input_focused": ctx.wants_keyboard_input(),
            "pointer_over_ui": ctx.is_pointer_over_area(),
            "windows": windows,
        },
        "jobs": {
            "importing": app.library.importing,
            "kill_scan": app.kill_job.is_some(),
            "beat_scan": app.beat_job.is_some(),
            "multiband_scan": app.multiband_job.is_some(),
            "media_job": app.media_job.is_some(),
            "export": app.export.debug_state(),
        },
        "status": status,
        "recent_events": app.dbg.events.iter().cloned().collect::<Vec<_>>(),
        "rects": rects,
    })
}
