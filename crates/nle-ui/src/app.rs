//! Application state, frame loop, keyboard shortcuts and menus.

use crate::diagnostics::{self, PreviousCrash};
use crate::discord::{DiscordPresence, PresenceSnapshot};
use crate::export_ui::ExportDialog;
use crate::media::{data_dir, MediaEvent, MediaLibrary};
use crate::timeline_ui::TimelineState;
use crate::{browser, captions_ui, copilot_ui, demo, inspector, online_ui, presets_ui, shortcuts_ui, theme, timeline_ui, viewer};
use eframe::egui;
use eframe::egui_wgpu;
use nle_core::autosave::Autosaver;
use nle_core::eval::{self, Composition, Layer, LayerSource, RenderNode};
use nle_core::focus::Focus;
use nle_core::keymap::{Action, ActionMap, KeyBinding, KeyCode};
use nle_core::{
    AssetId, Clip, ClipId, ClipKind, Command, Effect, EffectKind, FrameBlendMode, History, Marker, MarkerKind, Project, Retime, RetimeSegment, Time, TitleParams, Transition,
    TransitionKind,
};
use nle_media::analysis::{self, BeatBand};
use nle_media::audio::output::AudioEngine;
use nle_media::jobs::Job;
use nle_media::FrameServer;
use nle_render::{FrameKey, FrameSource, GpuContext, RenderOptions, Renderer};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tool {
    Select,
    Trim,
    /// Non-magnetic placement: moving leaves a gap and overwrites at the target.
    Position,
    Blade,
    Range,
}

impl Tool {
    pub const ALL: [Tool; 5] = [Tool::Select, Tool::Trim, Tool::Position, Tool::Blade, Tool::Range];
    pub fn label(self) -> &'static str {
        match self {
            Tool::Select => "Select (A)",
            Tool::Trim => "Trim (T)",
            Tool::Position => "Position (P): move without ripple, leaving gaps",
            Tool::Blade => "Blade (B)",
            Tool::Range => "Range (R)",
        }
    }
    pub fn icon(self) -> &'static str {
        match self {
            Tool::Select => "⬉",
            Tool::Trim => "⇹",
            Tool::Position => "⇄",
            Tool::Blade => "✂",
            Tool::Range => "⬚",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InspectorTab {
    Video,
    Color,
    Audio,
    TitlesTransitions,
}

/// Where a browser clip goes on the timeline (FCP E / W / Q / D).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EditMode {
    Append,
    Insert,
    Connect,
    Overwrite,
}

/// Viewer-side state.
pub struct ViewerState {
    /// `None` = fit to window, else 1.0 = 100 %.
    pub zoom: Option<f32>,
    pub safe_zones: bool,
    pub wipe: Option<f32>,
    pub gizmo: viewer::Gizmo,
    pub half_res: bool,
    pub editing_tc: Option<String>,
    /// Preview monitoring volume, 0..150 % (never affects export).
    pub volume_pct: f32,
    pub muted: bool,
}

/// Zoom-punch settings shared by the inspector button and the Z / Shift+Z hotkeys.
pub struct PunchState {
    /// Peak zoom of a regular punch (1.05..1.50).
    pub intensity: f64,
    pub anchor: nle_core::zoom_punch::PunchAnchor,
    /// Next click on the preview picks a custom anchor.
    pub picking: bool,
}

impl Default for PunchState {
    fn default() -> Self {
        PunchState { intensity: nle_core::zoom_punch::DEFAULT_INTENSITY, anchor: Default::default(), picking: false }
    }
}

pub struct BrowserState {
    pub search: String,
    pub tag: Option<String>,
    pub selected: Option<AssetId>,
    pub favorites_only: bool,
    pub thumb_size: f32,
    pub new_tag: String,
    /// Asset + source time being hover-scrubbed (shown in the viewer).
    pub skim: Option<(AssetId, Time)>,
}

/// What the last preview frame was rendered from:
/// (history revision, playhead, wipe, proxy, skim position).
type PreviewKey = (u64, Time, Option<f32>, bool, Option<(AssetId, Time)>);

/// GPU preview: the renderer shares eframe's wgpu device and its display
/// texture is registered with egui, so the viewer shows it without copies.
pub struct Preview {
    pub renderer: Renderer,
    pub texture: egui::TextureId,
    render_state: egui_wgpu::RenderState,
    last: Option<PreviewKey>,
    pending_uploads: bool,
}

/// FrameSource used by the interactive preview: frames are uploaded from the
/// decode workers as they arrive, so the renderer never blocks here.
struct NoBlock<'a>(&'a Project);

impl FrameSource for NoBlock<'_> {
    fn frame_rate(&self, asset: AssetId) -> nle_core::FrameRate {
        self.0.asset(asset).map(|a| a.frame_rate()).unwrap_or_default()
    }
    fn frame(&mut self, _asset: AssetId, _index: i64) -> Option<nle_media::VideoFrame> {
        None
    }
}

pub struct Playback {
    pub playing: bool,
    pub speed: f64,
}

pub struct App {
    pub project: Project,
    pub history: History,
    pub project_path: Option<PathBuf>,
    pub autosaver: Autosaver,
    pub recovery: Option<(Project, std::time::SystemTime)>,
    /// The previous session ended abnormally (shown once at startup).
    pub previous_crash: Option<PreviousCrash>,
    pub selection: Vec<ClipId>,
    pub playhead: Time,
    /// Compound clips / storylines stepped into (double-click). While any
    /// are open, `project.timeline` shows the innermost one's contents and
    /// [`App::exec`] routes every edit into it on the real timeline.
    pub focus: Focus,
    /// ` (grave) is held: edits leave connected clips where they are.
    pub hold_connections: bool,
    pub skimmer: Option<Time>,
    pub skimming: bool,
    pub playback: Playback,
    pub tool: Tool,
    pub tl: TimelineState,
    pub library: MediaLibrary,
    pub preview: Option<Preview>,
    pub gpu: Option<GpuContext>,
    pub frames: FrameServer,
    pub audio: AudioEngine,
    pub use_proxy: bool,
    pub viewer: ViewerState,
    pub punch: PunchState,
    pub inspector_tab: InspectorTab,
    pub browser: BrowserState,
    pub export: ExportDialog,
    pub beat_job: Option<Job<(Vec<Marker>, f64)>>,
    pub kill_job: Option<Job<Vec<Marker>>>,
    pub multiband_job: Option<Job<Vec<Marker>>>,
    pub presets_ui: presets_ui::PresetsUiState,
    pub copilot: copilot_ui::CopilotState,
    pub marker_dialog: Option<(usize, String, [u8; 3])>,
    pub captions: captions_ui::CaptionsState,
    pub online: online_ui::OnlineState,
    pub discord: DiscordPresence,
    pub show_shortcuts: bool,
    pub show_captions: bool,
    pub keymap: ActionMap,
    pub shortcuts_ui: shortcuts_ui::ShortcutsState,
    pub ffmpeg_setup: crate::ffmpeg_setup::FfmpegSetup,
    pub about: crate::about_ui::AboutState,
    status: Option<(String, Instant)>,
    audio_revision: u64,
    audio_sources: HashSet<AssetId>,
    /// Look-ahead ducking: background envelope job, the inputs it was built
    /// from, and the installed envelope (also read by the timeline overlay).
    duck_job: Option<Job<nle_media::audio::DuckEnvelope>>,
    duck_key: Option<DuckKey>,
    pub duck_env: Option<Arc<nle_media::audio::DuckEnvelope>>,
    pub duck_overlay: std::cell::RefCell<std::collections::HashMap<nle_core::ClipId, (nle_core::DuckingSettings, Arc<nle_media::audio::DuckCurve>)>>,
    pub gpu_label: String,
    /// History state when the project was loaded or last saved; autosave and
    /// the "Save changes?" prompt only kick in once the project differs from it.
    saved_state: u64,
    pub assets_changed: bool,
    /// A project switch (or quit) waiting on the "Save changes?" answer.
    pending_switch: Option<ProjectSwitch>,
    /// Quit was confirmed; let the next close request through.
    allow_close: bool,
    #[cfg(windows)]
    ctrl_v_was_down: bool,
}

static COALESCE: AtomicU64 = AtomicU64::new(1);

/// Something that replaces the open project (or closes the app).
#[derive(Clone, Debug)]
pub enum ProjectSwitch {
    New,
    Open,
    Demo,
    ImportFcpxml,
    DroppedFcpxml(PathBuf),
    Quit(egui::Context),
}

/// A fresh key for grouping several commands into one undo step.
pub fn coalesce_key() -> u64 {
    COALESCE.fetch_add(1, Ordering::Relaxed) | (1 << 62)
}

/// Stable coalescing key for continuous edits (drags) of one clip.
pub fn drag_key(tag: u64, id: ClipId) -> u64 {
    (tag << 40) ^ id.0
}

impl App {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        // Detect working hardware encoders off the UI thread; the export
        // dialog reads the cached result.
        nle_media::encoder_probe::start_background_probe();
        theme::apply(&cc.egui_ctx);
        theme::install_fonts(&cc.egui_ctx);
        let autosaver = Autosaver::new(data_dir().join("autosave"), "session");
        let recovery = autosaver.recover();
        let project = demo::demo_project();

        let (preview, gpu, gpu_label) = match cc.wgpu_render_state.as_ref() {
            Some(rs) => {
                let info = rs.adapter.get_info();
                let label = format!("{} ({:?})", info.name, info.backend);
                log::info!("UI + render device: {label}");
                let gpu = GpuContext::from_existing(Arc::new(rs.device.clone()), Arc::new(rs.queue.clone()), info).with_adapter(&rs.adapter);
                let (w, h) = preview_size(&project, false);
                let renderer = Renderer::new(gpu.clone(), w, h);
                let texture = rs.renderer.write().register_native_texture(&rs.device, renderer.display_view(), nle_render::wgpu::FilterMode::Linear);
                (Some(Preview { renderer, texture, render_state: rs.clone(), last: None, pending_uploads: true }), Some(gpu), label)
            }
            None => (None, None, "no wgpu device".to_string()),
        };

        let mut app = App {
            project,
            history: History::new(500),
            project_path: None,
            autosaver,
            recovery,
            previous_crash: diagnostics::take_previous_crash(),
            selection: Vec::new(),
            playhead: Time::ZERO,
            focus: Focus::default(),
            hold_connections: false,
            skimmer: None,
            skimming: true,
            playback: Playback { playing: false, speed: 0.0 },
            tool: Tool::Select,
            tl: TimelineState::default(),
            library: MediaLibrary::new(),
            preview,
            gpu,
            frames: FrameServer::new(64, Some(1080)),
            audio: AudioEngine::new(),
            use_proxy: false,
            viewer: ViewerState { zoom: None, safe_zones: false, wipe: None, gizmo: viewer::Gizmo::Transform, half_res: false, editing_tc: None, volume_pct: 100.0, muted: false },
            punch: PunchState::default(),
            inspector_tab: InspectorTab::Video,
            browser: BrowserState { search: String::new(), tag: None, selected: None, favorites_only: false, thumb_size: 132.0, new_tag: String::new(), skim: None },
            export: ExportDialog::default(),
            beat_job: None,
            kill_job: None,
            multiband_job: None,
            presets_ui: presets_ui::PresetsUiState::default(),
            copilot: copilot_ui::CopilotState::default(),
            marker_dialog: None,
            captions: captions_ui::CaptionsState::default(),
            online: online_ui::OnlineState::default(),
            discord: DiscordPresence::start(),
            show_shortcuts: false,
            show_captions: false,
            keymap: shortcuts_ui::load_keymap(),
            shortcuts_ui: shortcuts_ui::ShortcutsState::default(),
            ffmpeg_setup: Default::default(),
            about: Default::default(),
            status: None,
            audio_revision: u64::MAX,
            audio_sources: HashSet::new(),
            duck_job: None,
            duck_key: None,
            duck_env: None,
            duck_overlay: Default::default(),
            gpu_label,
            saved_state: 0,
            assets_changed: false,
            pending_switch: None,
            allow_close: false,
            #[cfg(windows)]
            ctrl_v_was_down: false,
        };
        app.after_project_loaded();
        app
    }

    // ----- project lifecycle -------------------------------------------------

    pub fn after_project_loaded(&mut self) {
        self.focus.reset();
        self.library.reset();
        self.audio_sources.clear();
        self.audio_revision = u64::MAX;
        self.duck_job = None;
        self.duck_key = None;
        self.set_duck_env(None);
        self.selection.clear();
        self.playhead = Time::ZERO;
        self.history.clear();
        self.mark_saved();
        for a in self.project.assets.clone() {
            self.library.ensure(&a);
        }
        if let Some(p) = &mut self.preview {
            p.renderer.clear_cache();
            p.last = None;
        }
        self.resize_preview();
        self.tl.fit_pending = true;
    }

    pub fn set_project(&mut self, p: Project, path: Option<PathBuf>) {
        self.project = p;
        self.project_path = path;
        self.after_project_loaded();
    }

    /// True when the project differs from what was loaded or last saved.
    pub fn is_edited(&self) -> bool {
        self.history.state_id() != self.saved_state || self.assets_changed
    }

    /// The current state matches the file on disk (after load or save).
    fn mark_saved(&mut self) {
        self.saved_state = self.history.state_id();
        self.assets_changed = false;
    }

    /// Run `switch` now, or first ask to save when there are unsaved changes.
    pub fn request_switch(&mut self, switch: ProjectSwitch) {
        if self.is_edited() {
            self.pending_switch = Some(switch);
        } else {
            self.perform_switch(switch);
        }
    }

    fn perform_switch(&mut self, switch: ProjectSwitch) {
        match switch {
            ProjectSwitch::New => self.new_project(),
            ProjectSwitch::Open => self.open_dialog(),
            ProjectSwitch::Demo => self.set_project(demo::demo_project(), None),
            ProjectSwitch::ImportFcpxml => self.import_fcpxml(),
            ProjectSwitch::DroppedFcpxml(path) => self.load_fcpxml_file(&path),
            ProjectSwitch::Quit(ctx) => {
                self.allow_close = true;
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
        }
    }

    /// "Save changes?" modal shown before replacing an edited project.
    fn save_changes_window(&mut self, ctx: &egui::Context) {
        if self.pending_switch.is_none() {
            return;
        }
        let mut choice = None;
        let quitting = matches!(self.pending_switch, Some(ProjectSwitch::Quit(_)));
        egui::Modal::new(egui::Id::new("save_changes")).show(ctx, |ui| {
            ui.set_width(380.0);
            ui.heading("Save changes?");
            ui.label(format!("\u{201c}{}\u{201d} has unsaved changes. Save them before {}?", self.project.name, if quitting { "quitting" } else { "continuing" }));
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                if ui.button(egui::RichText::new("Save").strong()).clicked() {
                    choice = Some(0);
                }
                if ui.button("Don\u{2019}t Save").clicked() {
                    choice = Some(1);
                }
                if ui.button("Cancel").clicked() || ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                    choice = Some(2);
                }
            });
        });
        match choice {
            Some(0) => {
                self.save(false);
                // the save dialog was cancelled or the write failed: stay put
                if self.is_edited() {
                    self.pending_switch = None;
                    return;
                }
                if let Some(s) = self.pending_switch.take() {
                    self.perform_switch(s);
                }
            }
            Some(1) => {
                if let Some(s) = self.pending_switch.take() {
                    // the user threw these edits away: don't offer them back
                    // as "recovered work" (Drop would autosave them on quit)
                    self.mark_saved();
                    self.autosaver.discard();
                    self.perform_switch(s);
                }
            }
            Some(_) => self.pending_switch = None,
            None => {}
        }
    }

    pub fn status(&mut self, msg: impl Into<String>) {
        let m = msg.into();
        log::info!("{m}");
        self.status = Some((m, Instant::now()));
    }

    pub fn status_text(&self) -> Option<&str> {
        self.status.as_ref().filter(|(_, t)| t.elapsed() < Duration::from_secs(6)).map(|(m, _)| m.as_str())
    }

    // ----- editing -----------------------------------------------------------

    pub fn exec(&mut self, cmd: Command) -> bool {
        self.exec_key(cmd, None)
    }

    /// Execute, merging with the previous command when `key` matches
    /// (continuous drags become one undo step).
    pub fn exec_key(&mut self, cmd: Command, key: Option<u64>) -> bool {
        let label = cmd.label();
        if !self.focus.is_empty() && matches!(cmd, Command::SetMarkers { .. } | Command::SetCaptions { .. } | Command::SyncClipsToBeats { .. }) {
            self.status(format!("{label}: markers and captions belong to the project timeline \u{2014} step out of the compound first"));
            return false;
        }
        let result = self.focus.execute(&mut self.project.timeline, &mut self.history, cmd, key);
        self.after_focus_edit();
        match result {
            Ok(()) => {
                self.prune_selection();
                true
            }
            Err(e) => {
                self.status(format!("{label}: {e}"));
                false
            }
        }
    }

    pub fn undo(&mut self) {
        let label = self.history.undo_label();
        if self.focus.undo(&mut self.project.timeline, &mut self.history) {
            self.after_focus_edit();
            self.status(format!("Undo {}", label.unwrap_or("")));
            self.prune_selection();
        }
    }

    pub fn redo(&mut self) {
        let label = self.history.redo_label();
        if self.focus.redo(&mut self.project.timeline, &mut self.history) {
            self.after_focus_edit();
            self.status(format!("Redo {}", label.unwrap_or("")));
            self.prune_selection();
        }
    }

    // ----- step-in editing (compound clips and secondary storylines) ---------

    /// The project as saved and exported: with the real timeline even while
    /// stepped into a container.
    pub fn full_project(&self) -> std::borrow::Cow<'_, Project> {
        if self.focus.is_empty() {
            return std::borrow::Cow::Borrowed(&self.project);
        }
        let mut p = self.project.clone();
        p.timeline = self.focus.root(&self.project.timeline).clone();
        std::borrow::Cow::Owned(p)
    }

    /// Name of the project's real timeline (the breadcrumb root).
    pub fn root_timeline_name(&self) -> &str {
        &self.focus.root(&self.project.timeline).name
    }

    /// Open a compound clip or storyline in the timeline (double-click).
    pub fn step_into(&mut self, id: ClipId) {
        match self.focus.step_into(&mut self.project.timeline, id, self.playhead) {
            Ok(inner) => {
                self.after_focus_change();
                self.playhead = inner;
            }
            Err(_) => self.status("Only compound clips and storylines open in the timeline"),
        }
    }

    /// Step back out `levels` containers (breadcrumb, Ctrl+[).
    pub fn step_out(&mut self, levels: usize) {
        if let Some(playhead) = self.focus.step_out(&mut self.project.timeline, levels) {
            self.after_focus_change();
            self.playhead = playhead;
        }
    }

    /// An edit or undo removed a container that was open: back out of it.
    fn after_focus_edit(&mut self) {
        if let Some(playhead) = self.focus.take_restored_playhead() {
            self.after_focus_change();
            self.playhead = playhead;
            self.status("The opened compound clip no longer exists \u{2014} stepped out");
        }
    }

    fn after_focus_change(&mut self) {
        self.stop();
        self.selection.clear();
        self.tl.range = None;
        self.tl.selected_key = None;
        self.tl.fit_pending = true;
        // the shown timeline changed without a history revision
        self.audio_revision = u64::MAX;
        if let Some(p) = &mut self.preview {
            p.last = None;
        }
    }

    fn prune_selection(&mut self) {
        let tl = &self.project.timeline;
        self.selection.retain(|id| tl.clip(*id).is_some());
    }

    /// Selected clip, else the topmost clip under the playhead.
    pub fn target_clip(&self) -> Option<ClipId> {
        if let Some(id) = self.selection.first() {
            return Some(*id);
        }
        let t = self.playhead;
        let placed = self.project.timeline.placed_clips();
        placed.iter().filter(|p| t >= p.start && t < p.end() && !p.clip.is_gap()).max_by_key(|p| (p.lane.max(0), p.lane == 0)).map(|p| p.clip.id)
    }

    /// Clip-local time of the playhead for `id`.
    pub fn local_time(&self, id: ClipId) -> Time {
        let start = self.project.timeline.clip_start(id).unwrap_or(Time::ZERO);
        let dur = self.project.timeline.clip(id).map(|c| c.duration).unwrap_or(Time::ZERO);
        (self.playhead - start).max(Time::ZERO).min((dur - Time(1)).max(Time::ZERO))
    }

    pub fn add_asset_to_timeline(&mut self, asset: AssetId, mode: EditMode) {
        let Some(a) = self.project.asset(asset).cloned() else { return };
        let clip = Clip::from_asset(&a);
        let audio_only = !a.has_video();
        let at = self.playhead;
        let cmd = match mode {
            EditMode::Append => Command::Append { clip },
            EditMode::Insert => Command::Insert { at, clip },
            EditMode::Overwrite => Command::Overwrite { at, clip },
            EditMode::Connect => Command::Connect { clip, at, lane: if audio_only { -1 } else { 1 } },
        };
        if self.exec(cmd) && mode == EditMode::Append {
            self.playhead = self.project.timeline.duration();
        }
    }

    pub fn add_title(&mut self, lower_third: bool) {
        let params = if lower_third { TitleParams::lower_third("Name", "Title") } else { TitleParams::default() };
        let mut clip = Clip::title(params, Time::from_secs(4));
        clip.name = if lower_third { "Lower Third".into() } else { "Basic Title".into() };
        let lane = self.free_lane_above(self.playhead, Time::from_secs(4));
        if self.project.timeline.spine.is_empty() {
            self.exec(Command::Append { clip });
        } else {
            self.exec(Command::Connect { clip, at: self.playhead, lane });
        }
        self.inspector_tab = InspectorTab::TitlesTransitions;
    }

    fn free_lane_above(&self, at: Time, dur: Time) -> i32 {
        let placed = self.project.timeline.placed_clips();
        (1..32).find(|&lane| !placed.iter().any(|p| p.lane == lane && p.start < at + dur && p.end() > at)).unwrap_or(1)
    }

    pub fn add_adjustment_clip(&mut self) {
        let dur = Time::from_secs(5);
        let clip = Clip::adjustment(dur);
        let lane = self.free_lane_above(self.playhead, dur);
        if self.project.timeline.spine.is_empty() {
            self.exec(Command::Append { clip });
        } else {
            self.exec(Command::Connect { clip, at: self.playhead, lane });
        }
        if matches!(self.inspector_tab, InspectorTab::Audio | InspectorTab::TitlesTransitions) {
            self.inspector_tab = InspectorTab::Video;
        }
    }

    pub fn insert_gap(&mut self) {
        self.exec(Command::InsertGap { at: self.playhead, duration: Time::from_secs(2) });
    }

    pub fn add_marker(&mut self) {
        let mut markers = self.project.timeline.markers.clone();
        let idx = markers.iter().filter(|m| m.kind == MarkerKind::Standard).count() + 1;
        let name = format!("Marker {idx}");
        markers.push(Marker::new(self.playhead, &name));
        markers.sort_by_key(|m| m.time);
        if self.exec(Command::SetMarkers { markers }) {
            self.status(format!("Added {name}"));
        }
    }

    pub fn open_named_marker_dialog(&mut self) {
        let t = self.playhead;
        let existing = self.project.timeline.markers.iter().position(|m| (m.time - t).abs() <= self.project.timeline.frame());
        let idx = if let Some(i) = existing {
            i
        } else {
            let mut markers = self.project.timeline.markers.clone();
            let count = markers.len() + 1;
            let new_m = Marker::with_color(t, format!("Highlight {count}"), [255, 214, 10]);
            markers.push(new_m);
            markers.sort_by_key(|m| m.time);
            let pos = markers.iter().position(|m| m.time == t).unwrap_or(markers.len() - 1);
            self.exec(Command::SetMarkers { markers });
            pos
        };
        if let Some(m) = self.project.timeline.markers.get(idx) {
            let col = m.color.unwrap_or([255, 214, 10]);
            self.marker_dialog = Some((idx, m.name.clone(), col));
        }
    }

    fn marker_edit_window(&mut self, ctx: &egui::Context) {
        let Some((idx, mut name, mut color)) = self.marker_dialog.clone() else { return };
        let mut keep_open = true;
        let mut save = false;
        egui::Window::new("Edit Marker (Alt+M)").collapsible(false).resizable(false).open(&mut keep_open).show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.label("Name");
                ui.text_edit_singleline(&mut name);
            });
            ui.horizontal_wrapped(|ui| {
                ui.label("Color");
                for (label, rgb) in
                    [("Kill Red", [255, 69, 58]), ("Kick Orange", [255, 110, 48]), ("Snare Cyan", [64, 210, 255]), ("Highlight Gold", [255, 214, 10]), ("Blue", [50, 173, 230])]
                {
                    let c = egui::Color32::from_rgb(rgb[0], rgb[1], rgb[2]);
                    let txt = egui::RichText::new(format!("■ {label}")).color(c);
                    if ui.selectable_label(color == rgb, txt).clicked() {
                        color = rgb;
                    }
                }
            });
            ui.horizontal(|ui| {
                if ui.button("Save Marker").clicked() || ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                    save = true;
                }
                if ui.button("Cancel").clicked() {
                    self.marker_dialog = None;
                }
            });
        });
        if save {
            let mut markers = self.project.timeline.markers.clone();
            if let Some(m) = markers.get_mut(idx) {
                m.name = name;
                m.color = Some(color);
            }
            self.exec(Command::SetMarkers { markers });
            self.marker_dialog = None;
        } else if !keep_open {
            self.marker_dialog = None;
        } else if self.marker_dialog.is_some() {
            self.marker_dialog = Some((idx, name, color));
        }
    }

    pub fn add_keyframe_at_playhead(&mut self) {
        let Some(id) = self.target_clip() else { return };
        let Some(mut c) = self.project.timeline.clip(id).cloned() else { return };
        let t = self.local_time(id);
        let mut any_anim = false;
        c.for_each_param_mut(|p| {
            if p.is_animated() {
                p.add_keyframe(t);
                any_anim = true;
            }
        });
        if !any_anim {
            c.video.opacity.add_keyframe(t);
            c.video.position_x.add_keyframe(t);
            c.video.position_y.add_keyframe(t);
            c.video.scale_x.add_keyframe(t);
            c.video.scale_y.add_keyframe(t);
            c.video.rotation.add_keyframe(t);
        }
        self.exec(Command::UpdateClip { clip: Box::new(c) });
        self.tl.show_animation = true;
        self.status("Added keyframe");
    }

    /// Fairlight-style one-click audio polish over the whole timeline (one undo step).
    pub fn polish_audio(&mut self) {
        let tl = self.project.timeline.clone();
        match nle_core::audio_polish::polish_audio_command(&tl, &mut |a| self.asset_loudness_lufs(a)) {
            Some(cmd) => {
                let n = match &cmd {
                    Command::Batch { commands, .. } => commands.len(),
                    _ => 1,
                };
                self.exec(cmd);
                self.status(format!("Polished audio on {n} clip(s) — Ctrl+Z to undo"));
            }
            None => self.status("Audio is already polished (or there are no audible clips)"),
        }
    }

    pub fn asset_loudness_lufs(&mut self, id: AssetId) -> Option<f32> {
        if let Some(l) = self.project.asset(id).and_then(|a| a.loudness_lufs) {
            return Some(l);
        }
        let buf = self.library.audio.get(&id)?.clone();
        if buf.samples.iter().all(|&s| s.abs() < 1e-6) {
            return None;
        }
        let lufs = analysis::integrated_lufs(&buf.samples, buf.channels as usize, buf.sample_rate);
        if let Some(a) = self.project.asset_mut(id) {
            a.loudness_lufs = lufs;
        }
        lufs
    }

    pub fn analyze_selected_beats(&mut self) {
        if self.beat_job.is_some() {
            return;
        }
        let Some(id) = self.target_clip() else {
            self.status("Select an audio clip first");
            return;
        };
        let Some(clip) = self.project.timeline.clip(id).cloned() else {
            return;
        };
        let ClipKind::Asset { asset } = clip.kind else {
            self.status("Beat detection requires a media clip");
            return;
        };
        let Some(buf) = self.library.audio.get(&asset).cloned() else {
            self.status("Audio is still decoding — try again shortly");
            return;
        };
        let start = self.project.timeline.clip_start(id).unwrap_or(Time::ZERO);
        let dur = clip.duration;
        let speed = clip.retime.as_ref().and_then(|r| (r.segments.len() == 1 && r.segments[0].speed > 0.0).then_some(r.segments[0].speed)).unwrap_or(1.0);
        let src_in = clip.source_in + clip.retime.as_ref().map_or(Time::ZERO, |r| r.start_offset);
        self.beat_job = Some(Job::spawn("beats", move |ctx| {
            if ctx.cancelled() {
                return Err("Cancelled".into());
            }
            ctx.set_progress(0.2, "Analyzing beat grid…");
            let ch = buf.channels.max(1) as usize;
            let rate = buf.sample_rate.max(1);
            let total_frames = buf.samples.len() / ch;
            let in_frame = (src_in.as_secs_f64().max(0.0) * rate as f64).round() as usize;
            let span_frames = ((dur.as_secs_f64() * speed).max(0.0) * rate as f64).round() as usize;
            let from = in_frame.min(total_frames);
            let to = from.saturating_add(span_frames).min(total_frames);
            let slice = &buf.samples[from * ch..to * ch];
            let res = analysis::analyze_beats(slice, ch, rate);
            if ctx.cancelled() {
                return Err("Cancelled".into());
            }
            let end = start + dur;
            let markers = res.beats.into_iter().map(|t| start + Time::from_secs_f64(t / speed)).filter(|&t| t >= start && t <= end).map(Marker::beat).collect();
            Ok((markers, res.bpm))
        }));
        self.status("Analyzing beats…");
    }

    pub fn analyze_multiband_beats(&mut self) {
        if self.multiband_job.is_some() {
            return;
        }
        let Some(id) = self.target_clip() else {
            return self.status("Select a music or gameplay clip first");
        };
        let Some(clip) = self.project.timeline.clip(id).cloned() else { return };
        let ClipKind::Asset { asset } = clip.kind else {
            return self.status("Multi-band beat detection requires a media clip");
        };
        let Some(buf) = self.library.audio.get(&asset).cloned() else {
            return self.status("Audio is still decoding — try again shortly");
        };
        let start = self.project.timeline.clip_start(id).unwrap_or(Time::ZERO);
        let dur = clip.duration;
        let speed = clip.retime.as_ref().and_then(|r| (r.segments.len() == 1 && r.segments[0].speed > 0.0).then_some(r.segments[0].speed)).unwrap_or(1.0);
        let src_in = clip.source_in + clip.retime.as_ref().map_or(Time::ZERO, |r| r.start_offset);
        self.multiband_job = Some(Job::spawn("multiband-beats", move |ctx| {
            if ctx.cancelled() {
                return Err("Cancelled".into());
            }
            let ch = buf.channels.max(1) as usize;
            let rate = buf.sample_rate.max(1);
            let total_frames = buf.samples.len() / ch;
            let in_frame = (src_in.as_secs_f64().max(0.0) * rate as f64).round() as usize;
            let span_frames = ((dur.as_secs_f64() * speed).max(0.0) * rate as f64).round() as usize;
            let from = in_frame.min(total_frames);
            let to = from.saturating_add(span_frames).min(total_frames);
            let slice = &buf.samples[from * ch..to * ch];
            let res = analysis::analyze_multiband(slice, ch, rate, 1.0);
            let end = start + dur;
            let markers = res
                .into_iter()
                .filter_map(|b| {
                    let t = start + Time::from_secs_f64(b.secs / speed);
                    (t >= start && t <= end).then(|| match b.band {
                        BeatBand::SubBassKick => Marker::sub_bass(t),
                        BeatBand::SnareTransient => Marker::snare(t),
                    })
                })
                .collect();
            Ok(markers)
        }));
        self.status("Scanning Sub-Bass (20–120 Hz) & Snare (1.5–6 kHz) beats…");
    }

    pub fn scan_selected_kills(&mut self) {
        if self.kill_job.is_some() {
            return;
        }
        let Some(id) = self.target_clip() else {
            return self.status("Select a gameplay VOD clip first");
        };
        let Some(clip) = self.project.timeline.clip(id).cloned() else { return };
        let ClipKind::Asset { asset } = clip.kind else {
            return self.status("Kill scanner requires a media clip");
        };
        let Some(buf) = self.library.audio.get(&asset).cloned() else {
            return self.status("Audio is still decoding — try again shortly");
        };
        let start = self.project.timeline.clip_start(id).unwrap_or(Time::ZERO);
        let dur = clip.duration;
        let speed = clip.retime.as_ref().and_then(|r| (r.segments.len() == 1 && r.segments[0].speed > 0.0).then_some(r.segments[0].speed)).unwrap_or(1.0);
        let src_in = clip.source_in + clip.retime.as_ref().map_or(Time::ZERO, |r| r.start_offset);
        self.kill_job = Some(Job::spawn("kill-scan", move |ctx| {
            if ctx.cancelled() {
                return Err("Cancelled".into());
            }
            let ch = buf.channels.max(1) as usize;
            let rate = buf.sample_rate.max(1);
            let total_frames = buf.samples.len() / ch;
            let in_frame = (src_in.as_secs_f64().max(0.0) * rate as f64).round() as usize;
            let span_frames = ((dur.as_secs_f64() * speed).max(0.0) * rate as f64).round() as usize;
            let from = in_frame.min(total_frames);
            let to = from.saturating_add(span_frames).min(total_frames);
            let slice = &buf.samples[from * ch..to * ch];
            let peaks = analysis::analyze_kills_from_samples(slice, ch, rate, 1.0);
            let end = start + dur;
            let markers = peaks
                .into_iter()
                .filter_map(|p| {
                    let t = start + Time::from_secs_f64(p.time.as_secs_f64() / speed);
                    (t >= start && t <= end).then(|| Marker::kill(t, p.label))
                })
                .collect();
            Ok(markers)
        }));
        self.status("Scanning gameplay audio for kills & multi-kill spikes…");
    }

    pub fn sync_clips_to_beats(&mut self) {
        let ids: Vec<ClipId> = if self.selection.is_empty() { self.target_clip().into_iter().collect() } else { self.selection.clone() };
        if ids.is_empty() {
            return self.status("Select clip(s) to sync to beat markers");
        }
        if self.exec(Command::SyncClipsToBeats { ids }) {
            self.status("Synced selected clip(s) & velocity peaks to beat markers");
        } else {
            self.status("Add Beat / Kick / Snare markers first to sync clips");
        }
    }

    /// Zoom punch (`hard` = Shift+Z hold punch) on the target clip at the playhead.
    pub fn zoom_punch(&mut self, hard: bool) {
        use nle_core::zoom_punch::{apply_punch, PunchAnchor};
        let Some(id) = self.target_clip() else {
            return self.status("Put the playhead over a clip (or select one) to zoom punch");
        };
        let Some(mut c) = self.project.timeline.clip(id).cloned() else { return };
        if (!c.has_video() && !matches!(c.kind, ClipKind::Title(_))) || matches!(c.kind, ClipKind::Adjustment | ClipKind::Gap) {
            return self.status("Zoom punch needs a video or title clip");
        }
        let local = self.local_time(id);
        let anchor = match self.punch.anchor {
            PunchAnchor::Center => {
                let ratio = viewer::frame_per_layer(self, &c, local);
                self.punch.anchor.resolve([c.video.position_x.at(local), c.video.position_y.at(local)], ratio)
            }
            a => a.resolve([0.0, 0.0], [1.0, 1.0]),
        };
        if !apply_punch(&mut c, local, self.punch.intensity, hard, anchor) {
            return;
        }
        if self.exec(Command::UpdateClip { clip: Box::new(c) }) {
            self.history.seal();
            self.tl.show_animation = true;
            if hard {
                self.status(format!("Hard punch {:.2}× (hold) · {}", nle_core::zoom_punch::HARD_INTENSITY, self.punch.anchor.label()));
            } else {
                self.status(format!("Zoom punch {:.2}× · {}", self.punch.intensity, self.punch.anchor.label()));
            }
        }
    }

    pub fn toggle_gaming_reframe_916(&mut self) {
        let Some(id) = self.target_clip() else {
            return self.status("Select a gameplay clip first");
        };
        let Some(mut c) = self.project.timeline.clip(id).cloned() else { return };
        if let Some(pos) = c.effects.iter().position(|e| e.kind == EffectKind::GamingReframe916) {
            c.effects.remove(pos);
            if self.exec(Command::UpdateClip { clip: Box::new(c) }) {
                self.status("Disabled Gaming 9:16 Reframe");
            }
        } else {
            c.effects.push(Effect::new(EffectKind::GamingReframe916));
            if self.exec(Command::UpdateClip { clip: Box::new(c) }) {
                // the effect renders a 9:16 layer; a landscape sequence would pillarbox it
                let f = self.project.timeline.format;
                if f.width > f.height {
                    self.project.timeline.format.width = 1080;
                    self.project.timeline.format.height = 1920;
                    self.resize_preview();
                    self.status(format!("Enabled Gaming 9:16 Reframe — sequence switched {}×{} → 1080×1920", f.width, f.height));
                } else {
                    self.status("Enabled Gaming 9:16 Reframe (Split Stack · aspect-locked, no stretching)");
                }
            }
        }
    }

    pub fn apply_velocity_preset(&mut self, name: &str) {
        let Some(id) = self.retime_target() else {
            return self.status("Select a video clip for velocity remapping");
        };
        self.set_retime(id, |len, _| Retime::velocity_preset(name, len));
        self.tl.show_retime = true;
        self.status(format!("Applied velocity preset: {name}"));
    }

    pub fn apply_montage_preset_by_name(&mut self, id: ClipId, name: &str) {
        let Some(preset) = presets_ui::find_preset_with_user(name, &self.presets_ui.user_presets) else {
            return self.status(format!("Preset '{name}' not found"));
        };
        let Some(mut clip) = self.project.timeline.clip(id).cloned() else { return };
        let local = self.local_time(id);
        preset.apply_to_clip(&mut clip, local);
        let retimed = clip.retime.is_some();
        // one atomic undo step (Ctrl+Z restores the clip exactly)
        if !self.exec(Command::ReplaceClip { label: "Apply Preset", clip: Box::new(clip) }) {
            return;
        }
        if retimed {
            self.tl.show_retime = true;
        }
        self.status(format!("Applied preset: {}", preset.name));
    }

    /// Add the default transition (Cross Dissolve, 1 s) to selected clips.
    pub fn add_transition(&mut self, kind: TransitionKind) {
        let ids = if self.selection.is_empty() { self.target_clip().into_iter().collect() } else { self.selection.clone() };
        let key = coalesce_key();
        for id in ids {
            if !nle_core::command::is_spine(&self.project.timeline, id) {
                continue;
            }
            let t = Transition::new(kind, Time::from_secs(1));
            self.exec_key(Command::SetTransition { id, at_end: true, transition: Some(t) }, Some(key));
            let first = self.project.timeline.spine.first().map(|c| c.id) == Some(id);
            if first {
                self.exec_key(Command::SetTransition { id, at_end: false, transition: Some(Transition::new(kind, Time::from_secs(1))) }, Some(key));
            }
        }
        self.history.seal();
    }

    /// Remove the keyframes (all parameters) at clip-local `t` as one undo step.
    pub fn delete_selected_keyframe(&mut self, id: ClipId, t: Time) {
        let Some(mut c) = self.project.timeline.clip(id).cloned() else { return };
        if nle_core::zoom_punch::remove_keys_near(&mut c, t, Time(1)) && self.exec(Command::UpdateClip { clip: Box::new(c) }) {
            self.status("Deleted keyframe");
        }
        self.tl.selected_key = None;
    }

    pub fn delete_selection(&mut self, leave_gap: bool) {
        // a selected keyframe diamond takes priority over its clip
        if let Some((id, t)) = self.tl.selected_key {
            if self.tl.show_animation {
                return self.delete_selected_keyframe(id, t);
            }
        }
        if let Some((s, e)) = self.tl.range.take() {
            self.exec(Command::DeleteRange { start: s, end: e });
            return;
        }
        let ids = std::mem::take(&mut self.selection);
        let key = coalesce_key();
        for id in ids {
            let spine = nle_core::command::is_spine(&self.project.timeline, id);
            let cmd = if leave_gap && spine {
                Command::DeleteWithGap { id }
            } else if self.hold_connections && spine {
                // ` held: the deleted clip's connected clips stay on the timeline
                Command::HoldConnections { ids: vec![id], cmd: Box::new(Command::DeleteRipple { id }) }
            } else {
                Command::DeleteRipple { id }
            };
            self.exec_key(cmd, Some(key));
        }
        self.history.seal();
    }

    /// Caption block that caption edits (nudge) act on: the selected block,
    /// else the one under the playhead.
    pub fn caption_target(&self) -> Option<usize> {
        let tr = &self.project.timeline.captions;
        self.captions.selected_seg.filter(|&i| i < tr.segments.len()).or_else(|| tr.index_at(tr.caption_time(self.playhead)))
    }

    /// Split the caption block under the playhead, partitioning its words.
    /// False when no caption block is under the playhead.
    pub fn split_caption_at_playhead(&mut self) -> bool {
        let mut tr = self.project.timeline.captions.clone();
        let at = tr.caption_time(self.playhead);
        let Some(i) = tr.index_at(at) else { return false };
        if tr.split_segment(i, at) {
            self.captions.selected_seg = Some(i + 1);
            self.exec(Command::SetCaptions { captions: Box::new(tr) });
        } else {
            self.status("Caption block needs 2+ words and the playhead inside it to split");
        }
        true
    }

    /// Alt+Left / Alt+Right: move the targeted caption block by `frames`.
    pub fn nudge_caption(&mut self, frames: i64) -> bool {
        let Some(i) = self.caption_target() else { return false };
        let mut tr = self.project.timeline.captions.clone();
        let d = tr.slide_segment(i, Time::from_frames(frames, self.project.timeline.format.rate));
        self.captions.selected_seg = Some(i);
        if !d.is_zero() {
            self.exec(Command::SetCaptions { captions: Box::new(tr) });
        }
        true
    }

    pub fn blade_at_playhead(&mut self, all: bool) {
        let at = self.playhead;
        // C / Ctrl+B splits captions when the captions row is hovered or a
        // caption block (and no clip) is selected
        let caption_focus = self.tl.hover_captions || (self.selection.is_empty() && self.captions.selected_seg.is_some());
        if !all && caption_focus && self.split_caption_at_playhead() {
            return;
        }
        if all {
            self.exec(Command::BladeAll { at });
        } else if let Some(id) = self.selection.first().copied().into_iter().chain(self.tl.hovered_clip).find(|id| {
            // active (selected) clip first, then the clip under the pointer
            let tl = &self.project.timeline;
            let s = tl.clip_start(*id).unwrap_or(Time::ZERO);
            tl.clip(*id).is_some_and(|c| at > s && at < s + c.duration)
        }) {
            self.exec(Command::BladeClip { id, at });
        } else {
            self.exec(Command::Blade { at });
        }
    }

    pub fn ripple_trim_playhead(&mut self, start_edge: bool) {
        let at = self.view_time();
        let target = self
            .selection
            .first()
            .copied()
            .filter(|id| {
                let s = self.project.timeline.clip_start(*id).unwrap_or(Time::ZERO);
                self.project.timeline.clip(*id).is_some_and(|c| at > s && at < s + c.duration)
            })
            .or_else(|| self.target_clip());
        let Some(id) = target else { return };
        let cmd = if start_edge { Command::RippleTrimStartTo { id, at } } else { Command::RippleTrimEndTo { id, at } };
        if self.exec(cmd) && start_edge {
            if let Some(s) = self.project.timeline.clip_start(id) {
                self.seek(s);
            }
        }
    }

    pub fn make_compound(&mut self) {
        if self.selection.is_empty() {
            self.status("Select clips to make a compound clip");
            return;
        }
        let ids = self.selection.clone();
        if self.exec(Command::MakeCompound { ids, name: "Compound Clip".into() }) {
            self.selection.clear();
        }
    }

    pub fn break_apart(&mut self) {
        if let Some(id) = self.selection.first().copied() {
            self.exec(Command::BreakApart { id });
            self.selection.clear();
        }
    }

    /// Group the selected connected clips into a secondary storyline.
    pub fn make_storyline(&mut self) {
        let tl = &self.project.timeline;
        let ids: Vec<ClipId> = self.selection.iter().copied().filter(|id| tl.clip(*id).is_some() && !nle_core::command::is_spine(tl, *id)).collect();
        if ids.is_empty() {
            self.status("Select connected clips to create a storyline");
            return;
        }
        if self.exec(Command::MakeStoryline { ids }) {
            self.selection.clear();
        }
    }

    /// Add a copy of the selected clip as an alternate audition take
    /// (e.g. to try different effects), keeping the current pick.
    pub fn duplicate_as_audition(&mut self) {
        let Some(id) = self.target_clip() else {
            self.status("Select a clip to duplicate as an audition");
            return;
        };
        let Some(c) = self.project.timeline.clip(id) else { return };
        if c.is_gap() || c.is_storyline() {
            self.status("Gaps and storylines cannot be auditioned");
            return;
        }
        let mut take = c.clone();
        take.id = ClipId::new();
        if self.exec(Command::AddAuditionTake { id, take: Box::new(take) }) {
            self.status("Added to audition \u{2014} Ctrl+\u{2192} / Ctrl+\u{2190} cycles the takes");
        }
    }

    /// Add the browser's selected clip as an alternate take of `id`.
    pub fn add_browser_clip_to_audition(&mut self, id: ClipId) {
        let Some(a) = self.browser.selected.and_then(|a| self.project.asset(a)).cloned() else {
            self.status("Select a clip in the browser first");
            return;
        };
        if self.exec(Command::AddAuditionTake { id, take: Box::new(Clip::from_asset(&a)) }) {
            self.status(format!("Added {} to the audition", a.name));
        }
    }

    pub fn cycle_audition(&mut self, step: i32) {
        let tl = &self.project.timeline;
        let target = self.selection.iter().copied().find(|id| tl.clip(*id).is_some_and(|c| !c.audition.is_empty())).or_else(|| self.target_clip());
        match target {
            Some(id) if tl.clip(id).is_some_and(|c| !c.audition.is_empty()) => {
                if self.exec(Command::CycleAudition { id, step }) {
                    if let Some(c) = self.project.timeline.clip(id) {
                        let n = c.audition.len() + 1;
                        self.status(format!("Audition pick: {} ({n} takes)", c.name));
                    }
                }
            }
            _ => self.status("No audition selected (Ctrl+Shift+Y duplicates a clip as one)"),
        }
    }

    pub fn finalize_audition(&mut self) {
        if let Some(id) = self.target_clip() {
            self.exec(Command::FinalizeAudition { id });
        }
    }

    // ----- retiming ----------------------------------------------------------

    fn retime_target(&self) -> Option<ClipId> {
        self.target_clip().filter(|id| self.project.timeline.clip(*id).is_some_and(|c| matches!(c.kind, ClipKind::Asset { .. } | ClipKind::Compound(_))))
    }

    /// Replace a clip's retime as one undo step.
    pub fn set_retime(&mut self, id: ClipId, make: impl FnOnce(Time, Option<&Retime>) -> Option<Retime>) {
        let Some(c) = self.project.timeline.clip(id).cloned() else { return };
        let key = coalesce_key();
        if c.retime.is_some() {
            self.exec_key(Command::SetRetime { id, retime: None }, Some(key));
        }
        // after clearing, the clip plays its source span at 100 %
        let len = self.project.timeline.clip(id).map(|c| c.duration).unwrap_or(c.duration);
        if let Some(r) = make(len, c.retime.as_ref()) {
            self.exec_key(Command::SetRetime { id, retime: Some(r) }, Some(key));
        }
        self.history.seal();
    }

    pub fn retime_preset(&mut self, speed: f64) {
        let Some(id) = self.retime_target() else { return self.status("Select a clip to retime") };
        let blend = self.project.timeline.clip(id).and_then(|c| c.retime.as_ref().map(|r| r.blend)).unwrap_or(FrameBlendMode::FrameBlend);
        self.set_retime(id, |len, _| {
            if (speed - 1.0).abs() < 1e-9 {
                return None;
            }
            let mut r = Retime::uniform(len, speed);
            r.blend = blend;
            Some(r)
        });
        self.status(format!("Speed {:.0}%", speed * 100.0));
    }

    pub fn retime_reverse(&mut self) {
        let Some(id) = self.retime_target() else { return };
        let Some(c) = self.project.timeline.clip(id).cloned() else { return };
        let r = c.retime.clone().unwrap_or_else(|| Retime::uniform(c.duration, 1.0)).reversed();
        self.exec(Command::SetRetime { id, retime: Some(r) });
    }

    /// Smooth speed ramp over four segments (`up`: 100 → 400 %, else 100 → 25 %).
    pub fn retime_ramp(&mut self, up: bool) {
        let Some(id) = self.retime_target() else { return };
        self.set_retime(id, |len, _| {
            let speeds: [f64; 4] = if up { [1.0, 1.5, 2.5, 4.0] } else { [1.0, 0.7, 0.45, 0.25] };
            let q = len.div_i64(4);
            let mut segs: Vec<RetimeSegment> = speeds.iter().map(|s| RetimeSegment::new(Time::from_secs_f64(q.as_secs_f64() / s).max(Time(1)), *s)).collect();
            for s in segs.iter_mut().skip(1) {
                s.ramp = s.duration.div_i64(2);
            }
            Some(Retime { segments: segs, start_offset: Time::ZERO, lead_speed: None, blend: FrameBlendMode::FrameBlend, velocity_curve: None })
        });
    }

    pub fn freeze_frame(&mut self) {
        let Some(id) = self.retime_target() else { return self.status("No clip under the playhead") };
        let at = self.local_time(id);
        self.exec(Command::FreezeFrame { id, at, hold: Time::from_secs(2) });
    }

    pub fn set_retime_blend(&mut self, mode: FrameBlendMode) {
        let Some(id) = self.retime_target() else { return };
        let Some(mut r) = self.project.timeline.clip(id).and_then(|c| c.retime.clone()) else { return };
        r.blend = mode;
        self.exec(Command::SetRetime { id, retime: Some(r) });
    }

    // ----- transport ---------------------------------------------------------

    pub fn toggle_play(&mut self) {
        if self.playback.playing {
            self.stop();
        } else {
            if self.playhead >= self.project.timeline.extent() {
                self.playhead = Time::ZERO;
            }
            self.play(1.0);
        }
    }

    pub fn play(&mut self, speed: f64) {
        self.playback = Playback { playing: true, speed };
        if let Some(p) = &mut self.preview {
            p.renderer.shuttle_proxy_scale = if speed.abs() > 4.0 {
                0.25
            } else if speed.abs() > 1.0 {
                0.5
            } else {
                1.0
            };
        }
        self.audio.play(self.playhead, speed);
    }

    pub fn stop(&mut self) {
        if self.playback.playing {
            self.playhead = self.audio.pause().max(Time::ZERO);
            self.playhead = self.playhead.snap_to_frame(self.project.timeline.format.rate);
        }
        if let Some(p) = &mut self.preview {
            p.renderer.shuttle_proxy_scale = 1.0;
        }
        self.playback = Playback { playing: false, speed: 0.0 };
    }

    /// J / L shuttle: 1× → 2× → 4× in that direction; the opposite key
    /// restarts at 1× the other way.
    pub fn shuttle(&mut self, dir: f64) {
        let cur = if self.playback.playing { self.playback.speed } else { 0.0 };
        let next = if cur * dir > 0.0 { (cur * 2.0).clamp(-MAX_SHUTTLE, MAX_SHUTTLE) } else { dir.signum() };
        if self.playback.playing {
            self.playhead = self.audio.pause();
        }
        self.play(next);
    }

    pub fn step(&mut self, frames: i64) {
        self.stop();
        // Step on the exact frame grid so repeated steps never drift off it.
        let rate = self.project.timeline.format.rate;
        self.playhead = Time::from_frames((self.playhead.frames_round(rate) + frames).max(0), rate);
    }

    pub fn seek(&mut self, t: Time) {
        let t = t.max(Time::ZERO);
        self.playhead = t;
        if self.playback.playing {
            self.audio.play(t, self.playback.speed);
        }
    }

    /// Jump to the previous/next edit point.
    pub fn jump_edit(&mut self, forward: bool) {
        let mut pts = self.project.timeline.snap_points(None);
        pts.push(Time::ZERO);
        pts.sort();
        pts.dedup();
        let t = self.playhead;
        let target = if forward { pts.into_iter().find(|p| *p > t) } else { pts.into_iter().rev().find(|p| *p < t) };
        if let Some(p) = target {
            self.seek(p);
        }
    }

    /// Time shown in the viewer: the skimmer wins over the playhead.
    pub fn view_time(&self) -> Time {
        if self.playback.playing {
            return self.playhead;
        }
        if self.skimming {
            if let Some(s) = self.skimmer {
                return s;
            }
        }
        self.playhead
    }

    // ----- preview -----------------------------------------------------------

    pub fn resize_preview(&mut self) {
        let (w, h) = preview_size(&self.project, self.viewer.half_res);
        if let Some(p) = &mut self.preview {
            if p.renderer.size() != (w, h) {
                p.renderer.resize(w, h);
                let rs = &p.render_state;
                rs.renderer.write().update_egui_texture_from_wgpu_texture(&rs.device, p.renderer.display_view(), nle_render::wgpu::FilterMode::Linear, p.texture);
                p.last = None;
            }
        }
    }

    pub fn toggle_proxy(&mut self, proxy: bool) {
        if self.use_proxy == proxy {
            return;
        }
        self.use_proxy = proxy;
        if let Some(p) = &mut self.preview {
            p.renderer.clear_cache();
            p.last = None;
        }
        self.status(if proxy { "Viewing proxy media" } else { "Viewing original media" });
    }

    fn browser_composition(&self, asset: AssetId, t: Time) -> Composition {
        let layer = Layer::new(ClipId(0), 0, LayerSource::Media { asset, source_time: t, speed: 1.0, blend: FrameBlendMode::FloorFrame });
        Composition::new(t, vec![RenderNode::Layer(layer)])
    }

    /// Request decodes, upload arrived frames and re-render if anything changed.
    fn update_preview(&mut self) {
        let t = self.view_time();
        let comp = match self.browser.skim {
            Some((a, st)) => self.browser_composition(a, st),
            None => eval::evaluate(&self.project.timeline, t),
        };
        let proxy = self.use_proxy;
        let playing = self.playback.playing;
        let Some(p) = &mut self.preview else { return };

        // upload whatever the decoders produced
        for d in self.frames.drain() {
            if d.proxy != proxy {
                continue;
            }
            p.renderer.upload_frame(FrameKey { asset: d.asset, index: d.index }, &d.frame);
            p.pending_uploads = true;
        }
        // request missing frames
        for (asset, st) in comp.media_requests() {
            let Some(a) = self.project.asset(asset) else { continue };
            let idx = st.frames_floor(a.frame_rate());
            if playing || !p.renderer.is_cached(FrameKey { asset, index: idx }) {
                self.frames.request(a, st, proxy && a.proxy_path.is_some(), playing);
            }
        }
        for (asset, st) in comp.next_frame_requests() {
            let Some(a) = self.project.asset(asset) else { continue };
            let next_idx = st.frames_floor(a.frame_rate()) + 1;
            let next_st = Time::from_frames(next_idx, a.frame_rate());
            if next_st < a.duration && !p.renderer.is_cached(FrameKey { asset, index: next_idx }) {
                self.frames.request(a, next_st, proxy && a.proxy_path.is_some(), playing);
            }
        }
        let key = (self.history.revision(), t, self.viewer.wipe, proxy, self.browser.skim);
        if p.last != Some(key) || p.pending_uploads {
            let opts = RenderOptions { wipe: self.viewer.wipe, color: true };
            p.renderer.render(&comp, &mut NoBlock(&self.project), opts);
            p.last = Some(key);
            p.pending_uploads = false;
        }
    }

    fn sync_audio(&mut self) {
        let rev = self.history.revision();
        if rev != self.audio_revision {
            let spans = eval::audio_spans(&self.project.timeline);
            self.refresh_duck_envelope(&spans);
            self.audio.set_spans(spans);
            self.audio_revision = rev;
        }
        if let Some(r) = self.duck_job.as_ref().and_then(|j| j.take()) {
            self.duck_job = None;
            match r {
                Ok(env) => self.set_duck_env(Some(Arc::new(env))),
                Err(e) => log::warn!("duck envelope: {e}"),
            }
        }
    }

    /// Recompute the dialogue envelope in the background when the voice
    /// spans (or their decoded sources) change. Until it lands, the mixer
    /// falls back to its causal ducker, so playback never waits on it.
    fn refresh_duck_envelope(&mut self, spans: &[eval::AudioSpan]) {
        use nle_media::audio::{dialogue_envelope, dialogue_spans, needs_duck_envelope};
        if !needs_duck_envelope(spans) {
            self.duck_key = None;
            self.duck_job = None;
            self.set_duck_env(None);
            return;
        }
        let voice = dialogue_spans(spans);
        let end = voice.iter().map(|s| s.end()).max().unwrap_or(Time::ZERO);
        let mut loaded: Vec<AssetId> = voice.iter().map(|s| s.asset).filter(|a| self.library.audio.contains_key(a)).collect();
        loaded.sort_unstable_by_key(|a| a.0);
        loaded.dedup();
        let key = DuckKey { voice: voice.clone(), loaded, end };
        if self.duck_key.as_ref() == Some(&key) {
            return;
        }
        if let Some(j) = self.duck_job.take() {
            j.cancel();
        }
        let sources: std::collections::HashMap<_, _> = key.loaded.iter().filter_map(|a| self.library.audio.get(a).map(|b| (*a, b.clone()))).collect();
        self.duck_key = Some(key);
        self.duck_job = Some(Job::spawn("Voice envelope", move |ctx| dialogue_envelope(&voice, &sources, end, &|| ctx.cancelled()).ok_or_else(|| "cancelled".to_string())));
    }

    fn set_duck_env(&mut self, env: Option<Arc<nle_media::audio::DuckEnvelope>>) {
        self.audio.set_duck_envelope(env.clone());
        self.duck_env = env;
        self.duck_overlay.borrow_mut().clear();
    }

    /// Duck gain curve for a ducked clip (cached per settings), for drawing.
    pub fn duck_curve_for(&self, c: &Clip) -> Option<Arc<nle_media::audio::DuckCurve>> {
        let env = self.duck_env.as_ref()?;
        if !c.audio.ducking.enabled || c.audio.role == nle_core::AudioRole::Dialogue {
            return None;
        }
        let mut cache = self.duck_overlay.borrow_mut();
        if let Some((s, curve)) = cache.get(&c.id) {
            if *s == c.audio.ducking {
                return Some(curve.clone());
            }
        }
        let curve = Arc::new(env.gain_curve(&c.audio.ducking));
        cache.insert(c.id, (c.audio.ducking, curve.clone()));
        Some(curve)
    }

    // ----- files -------------------------------------------------------------

    pub fn import_dialog(&mut self) {
        if let Some(files) = rfd::FileDialog::new()
            .set_title("Import Media")
            .add_filter("Media", &["mp4", "mov", "mkv", "webm", "avi", "mxf", "m4v", "mts", "wav", "mp3", "m4a", "aac", "flac", "ogg", "png", "jpg", "jpeg", "tif", "tiff"])
            .add_filter("All files", &["*"])
            .pick_files()
        {
            self.library.import(files);
        }
    }

    pub fn open_dialog(&mut self) {
        if let Some(path) = rfd::FileDialog::new().add_filter("Rust Cut Pro project", &["rcp", "json"]).pick_file() {
            self.open_path(&path);
        }
    }

    /// Open a project file (File > Open, or a `.rcp` passed on the command line).
    pub fn open_path(&mut self, path: &Path) {
        match Project::load(path) {
            Ok(p) => {
                self.set_project(p, Some(path.to_path_buf()));
                self.status(format!("Opened {}", path.display()));
            }
            Err(e) => {
                log::error!("open {}: {e}", path.display());
                self.status(format!("Open failed: {e}"));
            }
        }
    }

    pub fn save(&mut self, save_as: bool) {
        let path = match (&self.project_path, save_as) {
            (Some(p), false) => Some(p.clone()),
            _ => rfd::FileDialog::new().add_filter("Rust Cut Pro project", &["rcp"]).set_file_name(format!("{}.rcp", self.project.name)).save_file(),
        };
        let Some(path) = path else { return };
        let result = self.full_project().save(&path);
        match result {
            Ok(()) => {
                self.project_path = Some(path.clone());
                self.mark_saved();
                // the saved file now holds this work; a stale autosave would
                // otherwise be offered as "recovery" on the next launch
                self.autosaver.discard();
                self.status(format!("Saved {}", path.display()));
            }
            Err(e) => self.status(format!("Save failed: {e}")),
        }
    }

    pub fn import_fcpxml(&mut self) {
        let Some(path) = rfd::FileDialog::new().add_filter("Final Cut Pro XML", &["fcpxml", "xml"]).pick_file() else { return };
        self.load_fcpxml_file(&path);
    }

    fn load_fcpxml_file(&mut self, path: &std::path::Path) {
        let res = std::fs::read_to_string(path).map_err(|e| e.to_string()).and_then(|s| nle_core::fcpxml::import(&s).map_err(|e| e.to_string()));
        match res {
            Ok(p) => {
                self.set_project(p, None);
                self.status(format!("Imported {}", path.display()));
            }
            Err(e) => self.status(format!("FCPXML import failed: {e}")),
        }
    }

    pub fn export_fcpxml(&mut self) {
        let Some(path) = rfd::FileDialog::new().add_filter("Final Cut Pro XML", &["fcpxml"]).set_file_name(format!("{}.fcpxml", self.project.name)).save_file() else {
            return;
        };
        let xml = nle_core::fcpxml::export(&self.full_project());
        match nle_core::autosave::write_atomic(&path, xml.as_bytes()) {
            Ok(()) => self.status(format!("Exported {}", path.display())),
            Err(e) => self.status(format!("FCPXML export failed: {e}")),
        }
    }

    pub fn new_project(&mut self) {
        let mut p = Project::new("Untitled Project");
        p.timeline.format = self.project.timeline.format;
        self.set_project(p, None);
    }

    pub fn create_proxies(&mut self, ids: &[AssetId]) {
        let dir = data_dir().join("proxies");
        for id in ids {
            if let Some(a) = self.project.asset(*id) {
                if nle_media::probe::is_synthetic(a) || !a.has_video() {
                    continue;
                }
                self.library.proxies.enqueue(a.clone(), dir.clone());
            }
        }
    }

    // ----- shortcuts ---------------------------------------------------------

    /// Ctrl+V edge. egui-winit swallows the key and only emits a Paste event
    /// when the clipboard holds text, so on Windows the key state is read
    /// directly; elsewhere the Paste event is used.
    #[cfg(windows)]
    fn ctrl_v_pressed(&mut self, ctx: &egui::Context) -> bool {
        use windows_sys::Win32::UI::Input::KeyboardAndMouse::{GetAsyncKeyState, VK_CONTROL};
        let down = |vk: i32| unsafe { (GetAsyncKeyState(vk) as u16 & 0x8000) != 0 };
        let now = ctx.input(|i| i.focused) && down(VK_CONTROL as i32) && down(b'V' as i32);
        let edge = now && !self.ctrl_v_was_down;
        self.ctrl_v_was_down = now;
        edge
    }

    #[cfg(not(windows))]
    fn ctrl_v_pressed(&mut self, ctx: &egui::Context) -> bool {
        ctx.input(|i| i.events.iter().any(|e| matches!(e, egui::Event::Paste(_))))
    }

    fn dispatch_action(&mut self, action: Action) {
        match action {
            Action::PlayPause => self.toggle_play(),
            Action::ShuttleLeft => self.shuttle(-1.0),
            Action::ShuttleStop => self.stop(),
            Action::ShuttleRight => self.shuttle(1.0),
            Action::StepBack => self.step(-1),
            Action::StepForward => self.step(1),
            Action::StepBack10 => self.step(-10),
            Action::StepForward10 => self.step(10),
            Action::PrevEdit => self.jump_edit(false),
            Action::NextEdit => self.jump_edit(true),
            Action::GoToStart => self.seek(Time::ZERO),
            Action::GoToEnd => {
                let end = self.project.timeline.extent();
                self.seek(end);
            }
            Action::SelectTool => self.tool = Tool::Select,
            Action::TrimTool => self.tool = Tool::Trim,
            Action::BladeTool => self.tool = Tool::Blade,
            Action::RangeTool => self.tool = Tool::Range,
            Action::PositionTool => self.tool = Tool::Position,
            Action::BladeSplit => self.blade_at_playhead(false),
            Action::BladeAll => self.blade_at_playhead(true),
            Action::RippleTrimStart => self.ripple_trim_playhead(true),
            Action::RippleTrimEnd => self.ripple_trim_playhead(false),
            Action::RippleDelete => self.delete_selection(false),
            Action::DeleteWithGap => self.delete_selection(true),
            Action::InsertGap => self.insert_gap(),
            Action::Undo => self.undo(),
            Action::Redo => self.redo(),
            Action::SelectAll => self.selection = self.project.timeline.all_ids(),
            Action::ClearSelection => {
                self.selection.clear();
                self.tl.range = None;
            }
            Action::ToggleClipEnabled => {
                let key = coalesce_key();
                for id in self.selection.clone() {
                    self.exec_key(Command::ToggleEnabled { id }, Some(key));
                }
                self.history.seal();
            }
            Action::AddCrossDissolve => self.add_transition(TransitionKind::CrossDissolve),
            Action::AddAdjustmentClip => self.add_adjustment_clip(),
            Action::NewCompound => self.make_compound(),
            Action::BreakApart => self.break_apart(),
            Action::NewStoryline => self.make_storyline(),
            Action::StepOut => self.step_out(1),
            Action::DuplicateAsAudition => self.duplicate_as_audition(),
            Action::AuditionNext => self.cycle_audition(1),
            Action::AuditionPrev => self.cycle_audition(-1),
            Action::FinalizeAudition => self.finalize_audition(),
            Action::ToggleAudioLanes => self.tl.audio_lanes = !self.tl.audio_lanes,
            Action::HoldFrame => self.freeze_frame(),
            Action::AppendClip => {
                if let Some(asset) = self.browser.selected {
                    self.add_asset_to_timeline(asset, EditMode::Append);
                }
            }
            Action::InsertClip => {
                if let Some(asset) = self.browser.selected {
                    self.add_asset_to_timeline(asset, EditMode::Insert);
                }
            }
            Action::ConnectClip => {
                if let Some(asset) = self.browser.selected {
                    self.add_asset_to_timeline(asset, EditMode::Connect);
                }
            }
            Action::OverwriteClip => {
                if let Some(asset) = self.browser.selected {
                    self.add_asset_to_timeline(asset, EditMode::Overwrite);
                }
            }
            Action::AddKeyframe => self.add_keyframe_at_playhead(),
            Action::ToggleAnimation => self.tl.show_animation = !self.tl.show_animation,
            Action::ToggleRetime => self.tl.show_retime = !self.tl.show_retime,
            Action::AddMarker => self.add_marker(),
            Action::AddNamedMarker => self.open_named_marker_dialog(),
            Action::AnalyzeBeats => self.analyze_selected_beats(),
            Action::ScanKills => self.scan_selected_kills(),
            Action::SyncToBeats => self.sync_clips_to_beats(),
            Action::GamingReframe916 => self.toggle_gaming_reframe_916(),
            Action::ZoomPunch => self.zoom_punch(false),
            Action::HardZoomPunch => self.zoom_punch(true),
            Action::ToggleCopilot => self.copilot.open = !self.copilot.open,
            Action::ToggleSnapping => {
                self.project.timeline.snapping = !self.project.timeline.snapping;
                self.status(if self.project.timeline.snapping { "Snapping on" } else { "Snapping off" });
            }
            Action::ToggleSkimming => {
                self.skimming = !self.skimming;
                self.status(if self.skimming { "Skimming on" } else { "Skimming off" });
            }
            Action::ZoomIn => self.tl.zoom_by(1.5),
            Action::ZoomOut => self.tl.zoom_by(1.0 / 1.5),
            Action::ZoomToFit => self.tl.fit_pending = true,
            Action::AutoCaptions => self.show_captions = !self.show_captions,
            Action::Import => self.import_dialog(),
            Action::ImportFcpxml => self.request_switch(ProjectSwitch::ImportFcpxml),
            Action::Open => self.request_switch(ProjectSwitch::Open),
            Action::Save => self.save(false),
            Action::SaveAs => self.save(true),
            Action::Export => self.export.open = true,
            Action::ShowShortcuts => self.show_shortcuts = !self.show_shortcuts,
        }
    }

    fn handle_keys(&mut self, ctx: &egui::Context) {
        self.hold_connections = !ctx.wants_keyboard_input() && ctx.input(|i| i.key_down(egui::Key::Backtick));
        let ctrl_v = self.ctrl_v_pressed(ctx);
        if self.shortcuts_ui.recording.is_some() || ctx.wants_keyboard_input() {
            return;
        }

        // Alt+Left / Alt+Right nudge a caption block by one frame (unless rebound)
        for (code, key, dir) in [(KeyCode::ArrowLeft, egui::Key::ArrowLeft, -1), (KeyCode::ArrowRight, egui::Key::ArrowRight, 1)] {
            if self.keymap.lookup(&KeyBinding::alt(code)).is_none()
                && self.caption_target().is_some()
                && ctx.input_mut(|i| i.modifiers.alt && !i.modifiers.ctrl && !i.modifiers.shift && i.consume_key(egui::Modifiers::ALT, key))
            {
                self.nudge_caption(dir);
            }
        }

        let ctrl_v_binding = KeyBinding::ctrl(KeyCode::V);
        let plain_ctrl_v = ctrl_v && ctx.input(|i| !i.modifiers.shift && !i.modifiers.alt);
        let mut fired_ctrl_v = false;

        let mut entries: Vec<(Action, KeyBinding)> = self.keymap.bindings.iter().flat_map(|(a, list)| list.iter().copied().map(move |b| (*a, b))).collect();
        entries.sort_by_key(|(a, b)| {
            let specificity = (b.mods.ctrl as u8) + (b.mods.shift as u8) + (b.mods.alt as u8);
            (std::cmp::Reverse(specificity), *a)
        });

        let mut to_run = Vec::new();
        for (action, kb) in entries {
            let matched = if let Some(kc) = kb.key {
                let ek = shortcuts_ui::to_egui_key(kc);
                let em = shortcuts_ui::to_egui_mods(kb.mods);
                let key_hit = ctx.input_mut(|i| {
                    let cur = shortcuts_ui::from_egui_mods(i.modifiers);
                    let mods_ok = cur == kb.mods || (kc == KeyCode::Plus && i.modifiers.shift && cur.ctrl == kb.mods.ctrl && cur.alt == kb.mods.alt);
                    mods_ok && i.consume_key(em, ek)
                });
                if key_hit {
                    if kb == ctrl_v_binding {
                        fired_ctrl_v = true;
                    }
                    true
                } else if plain_ctrl_v && !fired_ctrl_v && kb == ctrl_v_binding {
                    fired_ctrl_v = true;
                    true
                } else {
                    false
                }
            } else if let Some(mb) = kb.mouse {
                let ep = shortcuts_ui::to_egui_pointer(mb);
                ctx.input(|i| i.pointer.button_pressed(ep) && shortcuts_ui::from_egui_mods(i.modifiers) == kb.mods)
            } else {
                false
            };
            if matched {
                to_run.push(action);
            }
        }

        // K held + L / J steps one frame instead of shuttling.
        let k_held = self.keymap.get(Action::ShuttleStop).iter().filter_map(|b| b.key).any(|k| ctx.input(|i| i.key_down(shortcuts_ui::to_egui_key(k))));
        for action in to_run {
            let action = match action {
                Action::ShuttleRight if k_held => Action::StepForward,
                Action::ShuttleLeft if k_held => Action::StepBack,
                a => a,
            };
            self.dispatch_action(action);
        }
    }

    // ----- menus -------------------------------------------------------------

    fn menu_label(&self, text: &str, a: Action) -> String {
        let hint = self.keymap.hint(a);
        if hint.is_empty() {
            text.to_string()
        } else {
            format!("{text}   {hint}")
        }
    }

    fn menu_bar(&mut self, ui: &mut egui::Ui) {
        egui::menu::bar(ui, |ui| {
            ui.label(egui::RichText::new("◆ Rust Cut Pro").strong().color(theme::VIDEO));
            ui.add_space(8.0);
            ui.menu_button("File", |ui| {
                if ui.button("New Project").clicked() {
                    ui.close_menu();
                    self.request_switch(ProjectSwitch::New);
                }
                if ui.button(self.menu_label("Open Project…", Action::Open)).clicked() {
                    ui.close_menu();
                    self.request_switch(ProjectSwitch::Open);
                }
                if ui.button(self.menu_label("Save", Action::Save)).clicked() {
                    ui.close_menu();
                    self.save(false);
                }
                if ui.button(self.menu_label("Save As…", Action::SaveAs)).clicked() {
                    ui.close_menu();
                    self.save(true);
                }
                ui.separator();
                if ui.button(self.menu_label("Import Media…", Action::Import)).clicked() {
                    ui.close_menu();
                    self.import_dialog();
                }
                if ui.button(self.menu_label("Import FCPXML…", Action::ImportFcpxml)).clicked() {
                    ui.close_menu();
                    self.request_switch(ProjectSwitch::ImportFcpxml);
                }
                if ui.button("Export FCPXML…").clicked() {
                    ui.close_menu();
                    self.export_fcpxml();
                }
                ui.separator();
                if ui.button(self.menu_label("Share / Export Movie…", Action::Export)).clicked() {
                    self.export.open = true;
                    ui.close_menu();
                }
                ui.separator();
                if ui.button("Load Demo Project").clicked() {
                    ui.close_menu();
                    self.request_switch(ProjectSwitch::Demo);
                }
            });
            ui.menu_button("Edit", |ui| {
                let undo = self.history.undo_label().map(|l| self.menu_label(&format!("Undo {l}"), Action::Undo)).unwrap_or_else(|| self.menu_label("Undo", Action::Undo));
                if ui.add_enabled(self.history.can_undo(), egui::Button::new(undo)).clicked() {
                    self.undo();
                    ui.close_menu();
                }
                let redo = self.history.redo_label().map(|l| self.menu_label(&format!("Redo {l}"), Action::Redo)).unwrap_or_else(|| self.menu_label("Redo", Action::Redo));
                if ui.add_enabled(self.history.can_redo(), egui::Button::new(redo)).clicked() {
                    self.redo();
                    ui.close_menu();
                }
                ui.separator();
                if ui.button(self.menu_label("Delete", Action::RippleDelete)).clicked() {
                    self.delete_selection(false);
                    ui.close_menu();
                }
                if ui.button(self.menu_label("Replace with Gap", Action::DeleteWithGap)).clicked() {
                    self.delete_selection(true);
                    ui.close_menu();
                }
                if ui.button(self.menu_label("Insert Gap (2 s)", Action::InsertGap)).clicked() {
                    self.insert_gap();
                    ui.close_menu();
                }
                if ui.button(self.menu_label("Select All", Action::SelectAll)).clicked() {
                    self.selection = self.project.timeline.all_ids();
                    ui.close_menu();
                }
                if ui.button(self.menu_label("Clear Selection", Action::ClearSelection)).clicked() {
                    self.selection.clear();
                    self.tl.range = None;
                    ui.close_menu();
                }
                ui.separator();
                if ui.button(self.menu_label("Add Cross Dissolve", Action::AddCrossDissolve)).clicked() {
                    self.add_transition(TransitionKind::CrossDissolve);
                    ui.close_menu();
                }
                if ui.button("Add Basic Title").clicked() {
                    self.add_title(false);
                    ui.close_menu();
                }
                if ui.button("Add Lower Third").clicked() {
                    self.add_title(true);
                    ui.close_menu();
                }
                if ui.button(self.menu_label("Add Adjustment Clip", Action::AddAdjustmentClip)).clicked() {
                    self.add_adjustment_clip();
                    ui.close_menu();
                }
            });
            ui.menu_button("Clip", |ui| {
                if ui.button(self.menu_label("Blade", Action::BladeSplit)).clicked() {
                    self.blade_at_playhead(false);
                    ui.close_menu();
                }
                if ui.button(self.menu_label("Blade All", Action::BladeAll)).clicked() {
                    self.blade_at_playhead(true);
                    ui.close_menu();
                }
                if ui.button(self.menu_label("New Compound Clip", Action::NewCompound)).clicked() {
                    self.make_compound();
                    ui.close_menu();
                }
                if ui.button(self.menu_label("Break Apart Clip Items", Action::BreakApart)).clicked() {
                    self.break_apart();
                    ui.close_menu();
                }
                if ui.button(self.menu_label("Create Storyline", Action::NewStoryline)).clicked() {
                    self.make_storyline();
                    ui.close_menu();
                }
                if ui.add_enabled(!self.focus.is_empty(), egui::Button::new(self.menu_label("Back Out of Compound", Action::StepOut))).clicked() {
                    self.step_out(1);
                    ui.close_menu();
                }
                ui.menu_button("Audition", |ui| {
                    if ui.button(self.menu_label("Duplicate as Audition", Action::DuplicateAsAudition)).clicked() {
                        self.duplicate_as_audition();
                        ui.close_menu();
                    }
                    if ui.button(self.menu_label("Next Pick", Action::AuditionNext)).clicked() {
                        self.cycle_audition(1);
                        ui.close_menu();
                    }
                    if ui.button(self.menu_label("Previous Pick", Action::AuditionPrev)).clicked() {
                        self.cycle_audition(-1);
                        ui.close_menu();
                    }
                    if ui.button(self.menu_label("Finalize Audition", Action::FinalizeAudition)).clicked() {
                        self.finalize_audition();
                        ui.close_menu();
                    }
                });
                if ui.button("Lift from Storyline").clicked() {
                    if let Some(id) = self.selection.first().copied() {
                        self.exec(Command::LiftToLane { id, lane: 1 });
                    }
                    ui.close_menu();
                }
                if ui.button(self.menu_label("Enable / Disable", Action::ToggleClipEnabled)).clicked() {
                    for id in self.selection.clone() {
                        self.exec(Command::ToggleEnabled { id });
                    }
                    ui.close_menu();
                }
                if ui.button(self.menu_label("Add Keyframe", Action::AddKeyframe)).clicked() {
                    self.add_keyframe_at_playhead();
                    ui.close_menu();
                }
                if ui
                    .button("🎚 Polish Audio")
                    .on_hover_text("One click for every audible clip: -14 LUFS normalize; dialogue gets vocal clarity, music sits 6 dB under and auto-ducks. One undo step.")
                    .clicked()
                {
                    self.polish_audio();
                    ui.close_menu();
                }
                ui.separator();
                ui.menu_button("Retime", |ui| self.retime_menu(ui));
            });
            ui.menu_button("Markers", |ui| {
                if ui.button(self.menu_label("Add Marker", Action::AddMarker)).clicked() {
                    self.add_marker();
                    ui.close_menu();
                }
                if ui.button(self.menu_label("Add Named / Colored Marker…", Action::AddNamedMarker)).clicked() {
                    self.open_named_marker_dialog();
                    ui.close_menu();
                }
                if ui.add_enabled(self.beat_job.is_none(), egui::Button::new(self.menu_label("Analyze Beats", Action::AnalyzeBeats))).clicked() {
                    self.analyze_selected_beats();
                    ui.close_menu();
                }
                if ui.add_enabled(self.multiband_job.is_none(), egui::Button::new("Multi-Band Beats (Kick + Snare)")).clicked() {
                    self.analyze_multiband_beats();
                    ui.close_menu();
                }
                if ui.add_enabled(self.kill_job.is_none(), egui::Button::new(self.menu_label("Scan VOD for Kills", Action::ScanKills))).clicked() {
                    self.scan_selected_kills();
                    ui.close_menu();
                }
                if ui.button(self.menu_label("Sync Selected Clips to Beats", Action::SyncToBeats)).clicked() {
                    self.sync_clips_to_beats();
                    ui.close_menu();
                }
                ui.separator();
                let has_beats = self.project.timeline.markers.iter().any(|m| m.kind == MarkerKind::Beat);
                if ui.add_enabled(has_beats, egui::Button::new("Clear Beat Markers")).clicked() {
                    let markers = self.project.timeline.markers.iter().filter(|m| m.kind != MarkerKind::Beat).cloned().collect();
                    self.exec(Command::SetMarkers { markers });
                    ui.close_menu();
                }
                let has_markers = !self.project.timeline.markers.is_empty();
                if ui.add_enabled(has_markers, egui::Button::new("Clear All Markers")).clicked() {
                    self.exec(Command::SetMarkers { markers: Vec::new() });
                    ui.close_menu();
                }
            });
            ui.menu_button("Captions", |ui| {
                if ui.button(self.menu_label("Auto Captions & Whisper…", Action::AutoCaptions)).clicked() {
                    self.show_captions = true;
                    ui.close_menu();
                }
                let has_caps = !self.project.timeline.captions.is_empty();
                let mut enabled = self.project.timeline.captions.enabled;
                if ui.add_enabled(has_caps, egui::Checkbox::new(&mut enabled, "Show Captions Track")).changed() {
                    let mut track = self.project.timeline.captions.clone();
                    track.enabled = enabled;
                    self.exec(Command::SetCaptions { captions: Box::new(track) });
                }
            });
            ui.menu_button("View", |ui| {
                let l_snap = self.menu_label("Snapping", Action::ToggleSnapping);
                let l_skim = self.menu_label("Skimming", Action::ToggleSkimming);
                let l_anim = self.menu_label("Video Animation", Action::ToggleAnimation);
                let l_ret = self.menu_label("Retime Editor", Action::ToggleRetime);
                ui.checkbox(&mut self.project.timeline.snapping, l_snap);
                ui.checkbox(&mut self.skimming, l_skim);
                ui.checkbox(&mut self.tl.show_animation, l_anim);
                ui.checkbox(&mut self.tl.show_retime, l_ret);
                ui.separator();
                ui.checkbox(&mut self.viewer.safe_zones, "Title/Action Safe Zones");
                let mut wipe = self.viewer.wipe.is_some();
                if ui.checkbox(&mut wipe, "Before/After Wipe").changed() {
                    self.viewer.wipe = if wipe { Some(0.5) } else { None };
                }
                let mut half = self.viewer.half_res;
                if ui.checkbox(&mut half, "Better Performance (half-res preview)").changed() {
                    self.viewer.half_res = half;
                    self.resize_preview();
                }
                ui.separator();
                let mut proxy = self.use_proxy;
                if ui.radio_value(&mut proxy, false, "Original Media").clicked() || ui.radio_value(&mut proxy, true, "Proxy Media").clicked() {
                    self.toggle_proxy(proxy);
                }
                ui.separator();
                if ui.button(self.menu_label("Zoom In", Action::ZoomIn)).clicked() {
                    self.tl.zoom_by(1.5);
                    ui.close_menu();
                }
                if ui.button(self.menu_label("Zoom Out", Action::ZoomOut)).clicked() {
                    self.tl.zoom_by(1.0 / 1.5);
                    ui.close_menu();
                }
                if ui.button(self.menu_label("Zoom to Fit", Action::ZoomToFit)).clicked() {
                    self.tl.fit_pending = true;
                    ui.close_menu();
                }
            });
            ui.menu_button("Help", |ui| {
                if ui.button("Discord Rich Presence…").clicked() {
                    self.discord.open_dialog();
                    ui.close_menu();
                }
                ui.separator();
                if ui.button(self.menu_label("Keyboard Shortcuts…", Action::ShowShortcuts)).clicked() {
                    self.show_shortcuts = true;
                    ui.close_menu();
                }
                ui.separator();
                if ui.button("Show Log File").on_hover_text("Logs for this and the previous session").clicked() {
                    match diagnostics::log_path() {
                        Some(p) => crate::export_ui::reveal_in_folder(&p),
                        None => self.status("Logging to a file is unavailable (logs folder not writable)"),
                    }
                    ui.close_menu();
                }
                ui.separator();
                if ui.button("Licenses & Notices…").clicked() {
                    self.about.open_licenses();
                    ui.close_menu();
                }
                if ui.button("About Rust Cut Pro…").clicked() {
                    self.about.open = true;
                    ui.close_menu();
                }
            });
            ui.separator();
            ui.toggle_value(&mut self.copilot.open, "🤖 AI Copilot").on_hover_text(self.menu_label("Toggle AI Copilot", Action::ToggleCopilot));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                // always-visible Render button (top-right), same as Ctrl+E
                let rendering = self.export.running();
                let label = if rendering { "⏳ Rendering…" } else { "⤴ Render" };
                let btn = egui::Button::new(egui::RichText::new(label).strong().color(egui::Color32::WHITE)).fill(theme::VIDEO).min_size(egui::vec2(96.0, 22.0));
                if ui.add(btn).on_hover_text(self.menu_label("Render / Export the timeline", Action::Export)).clicked() {
                    self.export.open = true;
                }
                ui.add_space(6.0);
                ui.label(egui::RichText::new(&self.gpu_label).small().color(theme::TEXT_DIM));
                if let Some(p) = &self.preview {
                    let st = p.renderer.stats();
                    ui.label(egui::RichText::new(format!("VRAM cache {} MB · {} frames", st.cache_bytes / (1 << 20), st.cache_frames)).small().color(theme::TEXT_DIM));
                }
                if self.library.importing > 0 {
                    ui.spinner();
                    ui.label(format!("Importing {}…", self.library.importing));
                }
                if let Some(s) = self.status_text() {
                    ui.label(egui::RichText::new(s).color(theme::SNAP));
                }
            });
        });
    }

    pub fn retime_menu(&mut self, ui: &mut egui::Ui) {
        for (label, s) in [("Slow 25%", 0.25), ("Slow 50%", 0.5), ("Normal 100%", 1.0), ("Fast 200%", 2.0), ("Fast 400%", 4.0)] {
            if ui.button(label).clicked() {
                self.retime_preset(s);
                ui.close_menu();
            }
        }
        ui.separator();
        if ui.button("Reverse Clip").clicked() {
            self.retime_reverse();
            ui.close_menu();
        }
        if ui.button("Speed Ramp → 25%").clicked() {
            self.retime_ramp(false);
            ui.close_menu();
        }
        if ui.button("Speed Ramp → 400%").clicked() {
            self.retime_ramp(true);
            ui.close_menu();
        }
        if ui.button(self.menu_label("Hold Frame", Action::HoldFrame)).clicked() {
            self.freeze_frame();
            ui.close_menu();
        }
        ui.separator();
        for m in FrameBlendMode::ALL {
            if ui.button(format!("Video Quality: {}", m.label())).clicked() {
                self.set_retime_blend(m);
                ui.close_menu();
            }
        }
    }

    /// Non-modal notice that the last session crashed, with its log.
    fn crash_notice(&mut self, ctx: &egui::Context) {
        let Some(crash) = &self.previous_crash else { return };
        let mut dismiss = false;
        egui::Window::new("Last Session Ended Unexpectedly")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::RIGHT_TOP, egui::vec2(-16.0, 48.0))
            .default_width(380.0)
            .show(ctx, |ui| {
                match &crash.panic {
                    Some(p) => {
                        ui.label("Rust Cut Pro hit an internal error:");
                        let short: String = p.chars().take(600).collect();
                        ui.label(egui::RichText::new(short).monospace().small().color(theme::TEXT_DIM));
                    }
                    None => {
                        ui.label(
                            "No error was recorded, so the app was stopped from outside it: a system crash, power loss, or a native component (GPU driver, FFmpeg, whisper.cpp).",
                        );
                    }
                }
                if self.recovery.is_some() {
                    ui.label(egui::RichText::new("Your autosaved work can be recovered in the other dialog.").color(theme::TEXT_DIM));
                }
                ui.label(egui::RichText::new("The log from that session was kept.").color(theme::TEXT_DIM));
                ui.horizontal(|ui| {
                    if ui.button("Show Log").clicked() {
                        crate::export_ui::reveal_in_folder(&crash.log);
                    }
                    if ui.button("Copy Details").clicked() {
                        ui.ctx().copy_text(diagnostics::crash_details(crash));
                    }
                    if ui.button("Dismiss").clicked() {
                        dismiss = true;
                    }
                });
            });
        if dismiss {
            self.previous_crash = None;
        }
    }

    fn recovery_window(&mut self, ctx: &egui::Context) {
        let Some((_, when)) = &self.recovery else { return };
        let ago = when.elapsed().map(|d| d.as_secs() / 60).unwrap_or(0);
        let mut choice = None;
        egui::Window::new("Recover Unsaved Work").collapsible(false).resizable(false).anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0)).show(ctx, |ui| {
            ui.label(format!("An autosaved project from {ago} min ago was found."));
            ui.label(egui::RichText::new("Autosave runs every 30 s with atomic writes.").color(theme::TEXT_DIM));
            ui.horizontal(|ui| {
                if ui.button("Recover").clicked() {
                    choice = Some(true);
                }
                if ui.button("Discard").clicked() {
                    choice = Some(false);
                }
            });
        });
        match choice {
            Some(true) => {
                if let Some((p, _)) = self.recovery.take() {
                    self.set_project(p, None);
                    // recovered work exists only in the autosave: keep it
                    // "unsaved" so autosave, the save prompt and Drop protect it
                    self.assets_changed = true;
                    self.status("Recovered autosaved project \u{2014} save it to keep it");
                }
            }
            Some(false) => {
                self.recovery = None;
                self.autosaver.discard();
            }
            None => {}
        }
    }

    fn handle_dropped_files(&mut self, ctx: &egui::Context) {
        let files: Vec<PathBuf> = ctx.input(|i| i.raw.dropped_files.iter().filter_map(|f| f.path.clone()).collect());
        if files.is_empty() {
            return;
        }
        let (xml, media): (Vec<_>, Vec<_>) = files.into_iter().partition(|p| p.extension().is_some_and(|e| e.eq_ignore_ascii_case("fcpxml")));
        if let Some(x) = xml.first() {
            self.request_switch(ProjectSwitch::DroppedFcpxml(x.clone()));
        }
        if !media.is_empty() {
            self.library.import(media);
        }
    }

    fn poll_media(&mut self, ctx: &egui::Context) {
        for ev in self.library.poll(ctx) {
            match ev {
                MediaEvent::Imported(a) => {
                    let name = a.name.clone();
                    let id = self.project.add_asset(a);
                    self.assets_changed = true;
                    if let Some(a) = self.project.asset(id).cloned() {
                        self.library.ensure(&a);
                    }
                    self.browser.selected = Some(id);
                    self.status(format!("Imported {name}"));
                }
                MediaEvent::AudioReady(id, buf, lufs) => {
                    self.audio.set_source(id, buf);
                    self.audio_sources.insert(id);
                    // a newly decoded voice source changes the ducking envelope
                    self.audio_revision = u64::MAX;
                    if let Some(a) = self.project.asset_mut(id) {
                        a.loudness_lufs = lufs;
                    }
                }
                MediaEvent::Message(m) => self.status(m),
            }
        }
        for (id, path) in self.library.take_finished_proxies() {
            if let Some(a) = self.project.asset_mut(id) {
                a.proxy_path = Some(path);
            }
            self.frames.forget(id);
            self.status("Proxy ready");
        }
    }

    fn poll_jobs(&mut self, ctx: &egui::Context) {
        if let Some(job) = self.beat_job.as_ref() {
            if let Some(res) = job.take() {
                self.beat_job = None;
                match res {
                    Ok((new_beats, bpm)) => {
                        let count = new_beats.len();
                        let mut markers: Vec<Marker> = self.project.timeline.markers.iter().filter(|m| m.kind != MarkerKind::Beat).cloned().collect();
                        markers.extend(new_beats);
                        markers.sort_by_key(|m| m.time);
                        self.exec(Command::SetMarkers { markers });
                        if count > 0 {
                            self.status(format!("Detected {count} beats (~{bpm:.0} BPM)"));
                        } else {
                            self.status("No clear beat grid detected");
                        }
                    }
                    Err(e) => self.status(format!("Beat analysis failed: {e}")),
                }
            } else {
                ctx.request_repaint_after(Duration::from_millis(50));
            }
        }
        if let Some(job) = self.kill_job.as_ref() {
            if let Some(res) = job.take() {
                self.kill_job = None;
                match res {
                    Ok(new_kills) => {
                        let count = new_kills.len();
                        let mut markers: Vec<Marker> = self.project.timeline.markers.iter().filter(|m| m.kind != MarkerKind::Kill).cloned().collect();
                        markers.extend(new_kills);
                        markers.sort_by_key(|m| m.time);
                        self.exec(Command::SetMarkers { markers });
                        self.status(format!("Kill Scanner dropped {count} kill/multi-kill markers"));
                    }
                    Err(e) => self.status(format!("Kill scan failed: {e}")),
                }
            } else {
                ctx.request_repaint_after(Duration::from_millis(50));
            }
        }
        if let Some(job) = self.multiband_job.as_ref() {
            if let Some(res) = job.take() {
                self.multiband_job = None;
                match res {
                    Ok(new_mb) => {
                        let count = new_mb.len();
                        let mut markers: Vec<Marker> =
                            self.project.timeline.markers.iter().filter(|m| !matches!(m.kind, MarkerKind::SubBass | MarkerKind::Snare)).cloned().collect();
                        markers.extend(new_mb);
                        markers.sort_by_key(|m| m.time);
                        self.exec(Command::SetMarkers { markers });
                        self.status(format!("Detected {count} Sub-Bass / Snare beat markers"));
                    }
                    Err(e) => self.status(format!("Multi-band beat scan failed: {e}")),
                }
            } else {
                ctx.request_repaint_after(Duration::from_millis(50));
            }
        }
        captions_ui::poll(self, ctx);
        online_ui::poll(self, ctx);
        copilot_ui::poll(self, ctx);
    }

    fn advance_playback(&mut self, ctx: &egui::Context) {
        if !self.playback.playing {
            return;
        }
        let t = self.audio.position();
        let end = self.project.timeline.extent();
        if (self.playback.speed > 0.0 && t >= end) || (self.playback.speed < 0.0 && t <= Time::ZERO) {
            self.playhead = t.clamp(Time::ZERO, end);
            self.audio.pause();
            self.playback = Playback { playing: false, speed: 0.0 };
        } else {
            self.playhead = t;
        }
        ctx.request_repaint();
    }

    fn sync_discord(&mut self) {
        let project_name = self.project_path.as_ref().and_then(|p| p.file_stem()).map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| self.project.name.clone());
        let snap = PresenceSnapshot {
            project_name,
            clip_count: self.project.timeline.placed_clips().len(),
            duration_secs: self.project.timeline.extent().as_secs_f64().max(0.0).round() as u64,
            resolution: self.project.timeline.format.label(),
            playing: self.playback.playing,
            exporting: self.export.running(),
        };
        self.discord.update(snap);
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.poll_media(ctx);
        self.poll_jobs(ctx);
        self.handle_dropped_files(ctx);
        self.handle_keys(ctx);
        self.advance_playback(ctx);
        self.sync_audio();

        egui::TopBottomPanel::top("menu").frame(egui::Frame::new().fill(theme::BG).inner_margin(egui::Margin::symmetric(8, 4))).show(ctx, |ui| self.menu_bar(ui));
        crate::ffmpeg_setup::show(self, ctx);
        egui::TopBottomPanel::bottom("timeline")
            .resizable(true)
            .default_height(390.0)
            .min_height(200.0)
            .frame(egui::Frame::new().fill(theme::BG).inner_margin(egui::Margin::same(0)))
            .show(ctx, |ui| timeline_ui::show(self, ui));
        egui::SidePanel::left("browser")
            .resizable(true)
            .default_width(380.0)
            .min_width(260.0)
            .frame(egui::Frame::new().fill(theme::PANEL).inner_margin(egui::Margin::same(8)).stroke(egui::Stroke::new(1.0_f32, theme::BORDER)))
            .show(ctx, |ui| browser::show(self, ui));
        copilot_ui::show(self, ctx);
        egui::SidePanel::right("inspector")
            .resizable(true)
            .default_width(330.0)
            .min_width(280.0)
            .frame(egui::Frame::new().fill(theme::PANEL).inner_margin(egui::Margin::same(8)).stroke(egui::Stroke::new(1.0_f32, theme::BORDER)))
            .show(ctx, |ui| inspector::show(self, ui));
        egui::CentralPanel::default().frame(egui::Frame::new().fill(theme::BG).inner_margin(egui::Margin::same(6))).show(ctx, |ui| viewer::show(self, ui));

        // the movie export always renders the whole project, also while stepped into a compound
        let full = (!self.focus.is_empty() && (self.export.open || self.export.running())).then(|| self.full_project().into_owned());
        self.export.show(ctx, full.as_ref().unwrap_or(&self.project), self.gpu.as_ref(), &self.library.audio);
        captions_ui::show(self, ctx);
        self.discord.show_dialog(ctx);
        shortcuts_ui::show(self, ctx);
        crate::about_ui::show(&mut self.about, ctx);
        self.marker_edit_window(ctx);
        self.recovery_window(ctx);
        self.crash_notice(ctx);
        // closing the window with unsaved changes asks first
        if ctx.input(|i| i.viewport().close_requested()) && !self.allow_close && self.is_edited() && self.recovery.is_none() {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            self.pending_switch = Some(ProjectSwitch::Quit(ctx.clone()));
        }
        self.save_changes_window(ctx);

        // A minimised window has a 0×0 surface: skip GPU preview work until
        // it is restored (transport, audio and autosave keep running).
        if !ctx.input(|i| i.viewport().minimized.unwrap_or(false)) {
            self.update_preview();
        }
        self.sync_discord();

        if self.recovery.is_none() && self.is_edited() && self.autosaver.is_due() {
            let full = self.full_project().into_owned();
            if let Err(e) = self.autosaver.tick(&full) {
                self.status(format!("Autosave failed: {e}"));
            }
        }
        // keep decoding/thumbnail jobs flowing
        if self.library.importing > 0 || self.export.running() || self.preview.as_ref().is_some_and(|p| p.renderer.stats().missing_frames > 0) {
            ctx.request_repaint_after(Duration::from_millis(30));
        } else {
            ctx.request_repaint_after(Duration::from_millis(250));
        }
    }
}

/// Preview resolution: the timeline format capped at 1080 lines (or half).
/// Fastest J / L shuttle rate.
pub const MAX_SHUTTLE: f64 = 4.0;

pub fn preview_size(p: &Project, half: bool) -> (u32, u32) {
    let f = p.timeline.format;
    let (mut w, mut h) = (f.width.max(16) as f32, f.height.max(16) as f32);
    let cap = if half { 540.0 } else { 1080.0 };
    if h > cap {
        w *= cap / h;
        h = cap;
    }
    ((w as u32).next_multiple_of(4), (h as u32).next_multiple_of(2))
}

impl Drop for App {
    fn drop(&mut self) {
        self.online.preview.stop();
        // Keep unsaved edits recoverable; an untouched session leaves nothing behind.
        if self.recovery.is_some() {
            return;
        }
        if self.is_edited() {
            let full = self.full_project().into_owned();
            let _ = self.autosaver.save_if_changed(&full, Instant::now());
        } else {
            self.autosaver.discard();
        }
    }
}

/// Inputs of the cached dialogue envelope; a change triggers a recompute.
#[derive(PartialEq)]
struct DuckKey {
    voice: Vec<eval::AudioSpan>,
    loaded: Vec<AssetId>,
    end: Time,
}
