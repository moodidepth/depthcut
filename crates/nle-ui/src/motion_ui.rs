//! Motion tab: point / region / planar trackers, attaching masks, censor
//! boxes, titles and smart reframe to them, camera stabilization and
//! scene-cut detection — plus the viewer overlay that draws (and lets you
//! place) track regions. Analysis runs on a background job with its own
//! renderer; results land as undoable `UpdateClip` / `Batch` commands.

use crate::app::{drag_key, App};
use crate::theme;
use eframe::egui::{self, Color32, Pos2, Rect, Sense, Stroke, Vec2};
use nle_core::color::{MaskShape, ShapeMask};
use nle_core::motion::{self, Follow, MotionModel, Stabilization, Track, TrackSample};
use nle_core::{Clip, ClipId, ClipKind, Command, Effect, EffectKind, MatteKind};
use nle_media::jobs::Job;
use nle_render::motion::{analyze, detect, stabilization_from, track, TrackEnd};

const OVERLAY_TAG: u64 = 0x2d;

/// What a finished motion job produced.
pub enum MotionResult {
    Track(Track, TrackEnd),
    Stabilize(Stabilization),
    Cuts {
        cuts: Vec<f64>,
        fps: f64,
    },
    /// AI matte frames written to the cache, plus (person mattes) a subject
    /// track built from the matte centroid.
    Matte {
        kind: MatteKind,
        frames: usize,
        /// Set when not every frame of the clip was done, with the reason.
        stopped: Option<&'static str>,
        track: Option<Track>,
    },
}

/// Name of the track generated from the person matte (replaced on re-run).
pub const SUBJECT_TRACK: &str = "Subject (AI)";

pub struct MotionJob {
    pub clip: ClipId,
    pub label: &'static str,
    pub job: Job<MotionResult>,
}

pub struct MotionUi {
    pub job: Option<MotionJob>,
    /// Track whose region is being placed in the viewer.
    pub edit_track: Option<u64>,
    pub cut_sensitivity: f32,
}

impl Default for MotionUi {
    fn default() -> Self {
        MotionUi { job: None, edit_track: None, cut_sensitivity: 0.5 }
    }
}

/// Requests from the Motion tab, run after the clip edit is committed.
pub enum MotionAction {
    Track { track: u64, forward: bool },
    Stabilize,
    DetectCuts,
    GenerateMatte(MatteKind),
    Cancel,
}

/// AI matte status for the Motion tab.
pub struct MatteStatus {
    pub kind: MatteKind,
    pub model_found: bool,
    /// The matte of the frame at the playhead is cached.
    pub frame_cached: bool,
}

/// A track another clip can follow: (clip, clip name, track id, track name).
pub type FollowTarget = (ClipId, String, u64, String);

/// Source seconds shown at clip-local `local`.
fn src_secs(c: &Clip, local: nle_core::Time) -> f64 {
    motion::source_secs(c, local)
}

fn frame_aspect(app: &App, c: &Clip) -> f32 {
    match &c.kind {
        ClipKind::Asset { asset } => app.project.asset(*asset).and_then(|a| a.video.as_ref()).map(|v| v.width as f32 / v.height.max(1) as f32),
        _ => None,
    }
    .unwrap_or_else(|| {
        let f = app.project.timeline.format;
        f.width as f32 / f.height.max(1) as f32
    })
}

/// Everything the tab needs from the app besides the edited clip.
pub struct TabCtx {
    pub local: nle_core::Time,
    pub aspect: f32,
    pub busy: Option<(&'static str, f32, String)>,
    pub targets: Vec<FollowTarget>,
    pub mattes: Vec<MatteStatus>,
}

pub fn tab_ctx(app: &App, id: ClipId, local: nle_core::Time) -> TabCtx {
    let busy = app.motion.job.as_ref().map(|j| {
        let (f, m) = j.job.progress();
        (j.label, f, m)
    });
    let targets = app
        .project
        .timeline
        .placed_clips()
        .iter()
        .filter(|p| p.clip.id != id)
        .flat_map(|p| p.clip.tracks.iter().map(|t| (p.clip.id, p.clip.name.clone(), t.id, t.name.clone())).collect::<Vec<_>>())
        .collect();
    let clip = app.project.timeline.clip(id);
    let aspect = clip.map(|c| frame_aspect(app, c)).unwrap_or(16.0 / 9.0);
    let mut mattes = Vec::new();
    if let Some((c, a)) = clip.and_then(|c| match c.kind {
        ClipKind::Asset { asset } => app.project.asset(asset).map(|a| (c, a)),
        _ => None,
    }) {
        let idx = nle_core::Time::from_secs_f64(src_secs(c, local)).frames_floor(a.frame_rate());
        for kind in MatteKind::ALL {
            let frame_cached = nle_media::matte::frame_path_in(&nle_media::matte::cache_root(), a, kind, idx).is_some_and(|p| p.is_file());
            mattes.push(MatteStatus { kind, model_found: nle_ai::segment::model_path(kind).is_file(), frame_cached });
        }
    }
    TabCtx { local, aspect, busy, targets, mattes }
}

fn model_combo(ui: &mut egui::Ui, salt: impl std::hash::Hash, m: &mut MotionModel) -> bool {
    let mut changed = false;
    egui::ComboBox::from_id_salt(salt).selected_text(m.label()).show_ui(ui, |ui| {
        for v in MotionModel::ALL {
            changed |= ui.selectable_value(m, v, v.label()).changed();
        }
    });
    changed
}

fn section(ui: &mut egui::Ui, title: &str, body: impl FnOnce(&mut egui::Ui)) {
    egui::CollapsingHeader::new(egui::RichText::new(title).strong()).default_open(true).show(ui, body);
}

/// A censor (blur / pixelate) effect masked to the track region.
fn censor(t: &Track, kind: EffectKind) -> Effect {
    let mut e = Effect::new(kind);
    let r = t.region;
    e.mask = Some(ShapeMask {
        center: [r[0] + r[2] / 2.0, r[1] + r[3] / 2.0],
        size: [r[2] * t.aspect / 2.0 * 1.15, r[3] / 2.0 * 1.15],
        feather: 0.02,
        track: Some(t.id),
        ..ShapeMask::new(MaskShape::Rectangle)
    });
    // strong defaults: a censor box should hide, not soften
    for (name, v) in [("Radius", 48.0), ("Block Size", 36.0)] {
        if let Some(i) = kind.params().iter().position(|d| d.name == name) {
            e.params[i].value = v;
        }
    }
    e
}

/// The Motion inspector tab. Edits `c` in place (committed by the caller as
/// one undoable `UpdateClip`) and returns a background request.
pub fn tab(ui: &mut egui::Ui, c: &mut Clip, cx: &TabCtx, motion_ui: &mut MotionUi) -> Option<MotionAction> {
    let mut action = None;
    if let Some((label, f, msg)) = &cx.busy {
        ui.horizontal(|ui| {
            ui.add(egui::ProgressBar::new(*f).desired_width(ui.available_width() - 70.0).text(format!("{label} {msg}")));
            if ui.button("Cancel").clicked() {
                action = Some(MotionAction::Cancel);
            }
        });
        ui.add_space(4.0);
    }
    let busy = cx.busy.is_some();
    let is_media = matches!(c.kind, ClipKind::Asset { .. });
    let now = src_secs(c, cx.local);

    if is_media {
        section(ui, "Tracking", |ui| {
            ui.label(egui::RichText::new("Place a region in the viewer, then track. Masks, censor boxes, titles and reframe follow it.").small().color(theme::TEXT_DIM));
            ui.horizontal_wrapped(|ui| {
                for (label, model, region) in [
                    ("+ Point", MotionModel::Translation, [0.47, 0.46, 0.06, 0.08]),
                    ("+ Region", MotionModel::Similarity, [0.4, 0.38, 0.2, 0.24]),
                    ("+ Planar", MotionModel::Perspective, [0.35, 0.33, 0.3, 0.34]),
                ] {
                    if ui.button(label).on_hover_text(model.label()).clicked() {
                        let n = c.tracks.len() + 1;
                        let t = Track::new(format!("Track {n}"), model, region, now, cx.aspect);
                        motion_ui.edit_track = Some(t.id);
                        c.tracks.push(t);
                    }
                }
            });
            let mut remove = None;
            let mut add_effects = Vec::new();
            for (i, t) in c.tracks.iter_mut().enumerate() {
                ui.separator();
                ui.horizontal(|ui| {
                    ui.add(egui::TextEdit::singleline(&mut t.name).desired_width(110.0));
                    if model_combo(ui, ("track-model", t.id), &mut t.model) {
                        // a different model invalidates the solved path
                        t.samples.retain(|s| s.t == t.reference);
                    }
                    if ui.small_button("🗑").on_hover_text("Delete track").clicked() {
                        remove = Some(i);
                    }
                });
                let range = t.range();
                let tracked = t.samples.len() > 1;
                ui.label(
                    egui::RichText::new(match range {
                        Some((a, b)) if tracked => format!("{} samples · {:.2}s – {:.2}s (source)", t.samples.len(), a, b),
                        _ => "Not tracked yet".into(),
                    })
                    .small()
                    .color(theme::TEXT_DIM),
                );
                ui.horizontal(|ui| {
                    if ui.add_enabled(!busy, egui::Button::new("◀ Track")).on_hover_text("Track backward to the clip start").clicked() {
                        action = Some(MotionAction::Track { track: t.id, forward: false });
                    }
                    if ui.add_enabled(!busy, egui::Button::new("Track ▶")).on_hover_text("Track forward to the clip end").clicked() {
                        action = Some(MotionAction::Track { track: t.id, forward: true });
                    }
                    let editing = motion_ui.edit_track == Some(t.id);
                    if ui.selectable_label(editing, "Place region").on_hover_text("Drag the region in the viewer (re-references the track here)").clicked() {
                        motion_ui.edit_track = if editing { None } else { Some(t.id) };
                    }
                });
                ui.horizontal_wrapped(|ui| {
                    if ui.button("Censor: Pixelate").clicked() {
                        add_effects.push(censor(t, EffectKind::Pixelate));
                    }
                    if ui.button("Censor: Blur").clicked() {
                        add_effects.push(censor(t, EffectKind::GaussianBlur));
                    }
                    if ui.button("Smart 9:16 Reframe").on_hover_text("Full 9:16 crop that pans with this track").clicked() {
                        let mut e = Effect::new(EffectKind::GamingReframe916);
                        if let Some(li) = EffectKind::GamingReframe916.params().iter().position(|d| d.name == "Layout") {
                            e.params[li].value = 2.0;
                        }
                        e.track = Some(t.id);
                        add_effects.push(e);
                    }
                });
            }
            if let Some(i) = remove {
                let id = c.tracks.remove(i).id;
                for e in &mut c.effects {
                    if e.track == Some(id) {
                        e.track = None;
                    }
                    if let Some(m) = e.mask.as_mut().filter(|m| m.track == Some(id)) {
                        m.track = None;
                    }
                }
                for n in &mut c.color.corrections {
                    if let Some(m) = n.mask.as_mut().filter(|m| m.track == Some(id)) {
                        m.track = None;
                    }
                }
            }
            c.effects.extend(add_effects);

            // existing masks: pick a track to follow
            if !c.tracks.is_empty() {
                let tracks: Vec<(u64, String)> = c.tracks.iter().map(|t| (t.id, t.name.clone())).collect();
                let mut rows: Vec<(String, &mut Option<u64>)> = Vec::new();
                for e in &mut c.effects {
                    let name = e.kind.label().to_string();
                    if let Some(m) = e.mask.as_mut() {
                        rows.push((format!("{name} mask"), &mut m.track));
                    }
                }
                for n in &mut c.color.corrections {
                    let name = n.name.clone();
                    if let Some(m) = n.mask.as_mut() {
                        rows.push((format!("Color “{name}” mask"), &mut m.track));
                    }
                }
                if !rows.is_empty() {
                    ui.separator();
                    ui.label(egui::RichText::new("Masks follow").small().strong());
                    for (i, (label, slot)) in rows.into_iter().enumerate() {
                        ui.horizontal(|ui| {
                            ui.label(label);
                            let sel = slot.and_then(|id| tracks.iter().find(|t| t.0 == id)).map_or("None", |t| t.1.as_str()).to_string();
                            egui::ComboBox::from_id_salt(("mask-follow", i)).selected_text(sel).show_ui(ui, |ui| {
                                ui.selectable_value(slot, None, "None");
                                for (id, n) in &tracks {
                                    ui.selectable_value(slot, Some(*id), n);
                                }
                            });
                        });
                    }
                }
            }
        });

        section(ui, "Stabilization", |ui| {
            let mut on = c.stabilization.is_some();
            if ui.checkbox(&mut on, "Stabilize").changed() {
                c.stabilization = on.then(|| Stabilization { aspect: cx.aspect, ..Default::default() });
            }
            if let Some(st) = c.stabilization.as_mut() {
                let mut changed = false;
                ui.horizontal(|ui| {
                    ui.label("Mode");
                    changed |= model_combo(ui, "stab-model", &mut st.model);
                });
                changed |= ui.checkbox(&mut st.tripod, "Camera lock (tripod)").changed();
                ui.add_enabled_ui(!st.tripod, |ui| {
                    changed |= ui.add(egui::Slider::new(&mut st.smoothing, 0.05..=3.0).text("Smoothing").suffix(" s").logarithmic(true)).changed();
                });
                changed |= ui.checkbox(&mut st.auto_crop, "Auto crop").changed();
                changed |= ui.add(egui::Slider::new(&mut st.zoom, 1.0..=2.0).text("Extra zoom").suffix("×")).changed();
                ui.checkbox(&mut st.enabled, "Enabled");
                if changed {
                    st.refresh();
                }
                ui.label(
                    egui::RichText::new(if st.path.is_empty() {
                        "Not analysed yet".to_string()
                    } else {
                        format!("{} frames analysed · crop {:.2}×", st.path.len(), st.crop_scale)
                    })
                    .small()
                    .color(theme::TEXT_DIM),
                );
                if ui.add_enabled(!busy, egui::Button::new("Analyze camera motion")).clicked() {
                    action = Some(MotionAction::Stabilize);
                }
            }
        });

        section(ui, "Scene Cuts", |ui| {
            ui.add(egui::Slider::new(&mut motion_ui.cut_sensitivity, 0.0..=1.0).text("Sensitivity"));
            if ui.add_enabled(!busy, egui::Button::new("Detect & Blade Scene Cuts")).on_hover_text("Splits compiled videos at every hard cut (one undo step)").clicked() {
                action = Some(MotionAction::DetectCuts);
            }
        });

        section(ui, "AI Mattes", |ui| {
            let runtime_found = nle_ai::segment::runtime_found();
            let dim = |ui: &mut egui::Ui, s: String| ui.label(egui::RichText::new(s).small().color(theme::TEXT_DIM));
            if !nle_ai::segment::onnx_supported() {
                dim(ui, "This build has no ONNX Runtime support (build with `--features onnx`). Cached mattes still render.".into());
            } else if !runtime_found {
                dim(ui, format!("ONNX Runtime missing: put version 1.22 (onnxruntime.dll on Windows) in {}", nle_media::paths::models_dir().display()));
            }
            for st in &cx.mattes {
                let effect = match st.kind {
                    MatteKind::Person => EffectKind::PersonMatte,
                    MatteKind::Depth => EffectKind::DepthMap,
                };
                ui.horizontal(|ui| {
                    ui.label(st.kind.label());
                    let gen = egui::Button::new(format!("Generate {}", st.kind.label()));
                    let hover = match st.kind {
                        MatteKind::Person => "Runs models/person.onnx on every frame of this clip and adds a 'Subject (AI)' track for Smart 9:16 Reframe",
                        MatteKind::Depth => "Runs models/depth.onnx on every frame of this clip",
                    };
                    if ui.add_enabled(!busy && st.model_found && runtime_found, gen).on_hover_text(hover).clicked() {
                        action = Some(MotionAction::GenerateMatte(st.kind));
                    }
                    let has = c.effects.iter().any(|e| e.kind == effect);
                    if ui.add_enabled(!has, egui::Button::new("Add Effect")).clicked() {
                        c.effects.push(Effect::new(effect));
                    }
                });
                if !st.model_found {
                    dim(ui, format!("Model missing: {}", nle_ai::segment::model_path(st.kind).display()));
                }
                dim(
                    ui,
                    if st.frame_cached {
                        "Matte cached at the playhead".into()
                    } else {
                        format!("No matte at the playhead: the effect does nothing until you click Generate {}", st.kind.label())
                    },
                );
            }
        });
    }

    section(ui, "Follow Track", |ui| {
        if cx.targets.is_empty() {
            ui.label(egui::RichText::new("Track something on another clip to attach this one to it.").small().color(theme::TEXT_DIM));
            return;
        }
        let cur = c.video.follow.as_ref().and_then(|f| cx.targets.iter().find(|t| t.2 == f.track)).map_or("None".to_string(), |t| format!("{} › {}", t.1, t.3));
        egui::ComboBox::from_id_salt("follow-target").selected_text(cur).show_ui(ui, |ui| {
            if ui.selectable_label(c.video.follow.is_none(), "None").clicked() {
                c.video.follow = None;
            }
            for (clip, cname, tid, tname) in &cx.targets {
                let on = c.video.follow.as_ref().is_some_and(|f| f.track == *tid);
                if ui.selectable_label(on, format!("{cname} › {tname}")).clicked() {
                    let transform = c.video.follow.as_ref().is_some_and(|f| f.transform);
                    c.video.follow = Some(Follow { clip: *clip, track: *tid, transform });
                }
            }
        });
        if let Some(f) = c.video.follow.as_mut() {
            ui.checkbox(&mut f.transform, "Also follow scale & rotation");
        }
    });
    action
}

impl App {
    /// Start / cancel the background work a Motion tab action asks for.
    pub fn run_motion_action(&mut self, id: ClipId, a: MotionAction) {
        if let MotionAction::Cancel = a {
            if let Some(j) = &self.motion.job {
                j.job.cancel();
            }
            return;
        }
        if self.motion.job.is_some() {
            return self.status("A motion analysis is already running");
        }
        if let MotionAction::GenerateMatte(kind) = a {
            return self.generate_matte(id, kind);
        }
        let Some(gpu) = self.gpu.clone() else { return self.status("Motion analysis needs a GPU") };
        let Some(clip) = self.project.timeline.clip(id).cloned() else { return };
        let ClipKind::Asset { asset } = clip.kind else { return self.status("Tracking needs a media clip") };
        let Some(asset) = self.project.asset(asset).cloned() else { return };
        let (s0, s1) = (src_secs(&clip, nle_core::Time::ZERO), src_secs(&clip, clip.duration));
        let (lo, hi) = (s0.min(s1), s0.max(s1));
        let now = src_secs(&clip, self.local_time(id));
        let open = move || {
            nle_media::decode::open(&asset, nle_media::decode::DecodeOptions { use_proxy: true, max_height: Some(nle_render::motion::ANALYSIS_MAX.1) }).map_err(|e| e.to_string())
        };
        let (label, job): (&'static str, Job<MotionResult>) = match a {
            MotionAction::Track { track: tid, forward } => {
                let Some(mut tr) = clip.tracks.iter().find(|t| t.id == tid).cloned() else { return };
                // a track started away from its reference begins where the playhead is
                let from = now.clamp(lo, hi);
                if tr.samples.len() <= 1 {
                    tr.reference = from;
                    tr.samples = vec![TrackSample { t: from, h: motion::IDENTITY }];
                }
                let to = if forward { hi } else { lo };
                (
                    "Tracking",
                    Job::spawn("track", move |ctx| {
                        let mut dec = open()?;
                        let mut r = nle_render::Renderer::new(gpu, 64, 64);
                        let end = track(&mut r, dec.as_mut(), &mut tr, from, to, |i, n| {
                            ctx.set_progress(i as f32 / n.max(1) as f32, format!("{i}/{n}"));
                            !ctx.cancelled()
                        });
                        Ok(MotionResult::Track(tr, end))
                    }),
                )
            }
            MotionAction::Stabilize => {
                let settings = clip.stabilization.clone().unwrap_or_default();
                (
                    "Analyzing",
                    Job::spawn("stabilize", move |ctx| {
                        let mut dec = open()?;
                        let mut r = nle_render::Renderer::new(gpu, 64, 64);
                        let an = analyze(&mut r, dec.as_mut(), lo, hi, settings.model, |i, n| {
                            ctx.set_progress(i as f32 / n.max(1) as f32, format!("{i}/{n}"));
                            !ctx.cancelled()
                        })
                        .ok_or("Cancelled or no frames")?;
                        Ok(MotionResult::Stabilize(stabilization_from(&an, &settings)))
                    }),
                )
            }
            MotionAction::DetectCuts => {
                let sens = self.motion.cut_sensitivity;
                (
                    "Scanning",
                    Job::spawn("scene-cuts", move |ctx| {
                        let mut dec = open()?;
                        let fps = dec.frame_rate().fps();
                        let mut r = nle_render::Renderer::new(gpu, 64, 64);
                        let an = analyze(&mut r, dec.as_mut(), lo, hi, MotionModel::Translation, |i, n| {
                            ctx.set_progress(i as f32 / n.max(1) as f32, format!("{i}/{n}"));
                            !ctx.cancelled()
                        })
                        .ok_or("Cancelled or no frames")?;
                        Ok(MotionResult::Cuts { cuts: detect(&an, sens), fps })
                    }),
                )
            }
            MotionAction::Cancel | MotionAction::GenerateMatte(_) => unreachable!(),
        };
        self.motion.job = Some(MotionJob { clip: id, label, job });
    }

    /// Run the local ONNX model over every source frame the clip uses and
    /// fill the matte cache (frames already cached are skipped, so a
    /// cancelled run resumes).
    fn generate_matte(&mut self, id: ClipId, kind: MatteKind) {
        let Some(clip) = self.project.timeline.clip(id).cloned() else { return };
        let ClipKind::Asset { asset } = clip.kind else { return self.status("AI mattes need a media clip") };
        let Some(asset) = self.project.asset(asset).cloned() else { return };
        let (s0, s1) = (src_secs(&clip, nle_core::Time::ZERO), src_secs(&clip, clip.duration));
        let (lo, hi) = (s0.min(s1), s0.max(s1));
        // preview and export look mattes up by the asset's frame rate, whatever a proxy's is
        let fps = asset.frame_rate().fps().max(1.0);
        let frames = matte_frames(lo, hi, asset.duration.as_secs_f64(), fps);
        let aspect = frame_aspect(self, &clip);
        let job = Job::spawn("ai-matte", move |ctx| {
            ctx.set_progress(0.0, "loading model");
            let mut model = nle_ai::segment::MatteModel::load(kind)?;
            let mut dec = nle_media::decode::open(&asset, nle_media::decode::DecodeOptions { use_proxy: true, max_height: Some(nle_render::motion::ANALYSIS_MAX.1) })
                .map_err(|e| e.to_string())?;
            let total = (frames.end() - frames.start() + 1).max(1);
            let root = nle_media::matte::cache_root();
            let (mut written, mut stopped) = (0usize, None);
            let mut subject: Vec<(f64, [f32; 2])> = Vec::new();
            for (i, idx) in frames.enumerate() {
                if ctx.cancelled() {
                    stopped = Some("cancelled");
                    break;
                }
                ctx.set_progress(i as f32 / total as f32, format!("{i}/{total}"));
                let t = (idx as f64 + 0.25) / fps;
                let m = match nle_media::matte::load_in(&root, &asset, kind, idx) {
                    Some(m) => m,
                    None => {
                        let Some(f) = nle_render::motion::decode(dec.as_mut(), t) else {
                            if i == 0 {
                                return Err("could not decode this clip's video".into());
                            }
                            stopped = Some("the video stopped decoding");
                            break;
                        };
                        let m = model.infer(&f.rgba, f.width, f.height)?;
                        // <= 480 px wide keeps the cache ~130 KB per frame
                        let m = if m.width > 480 { resize_matte(&m, 480, (m.height * 480 / m.width).max(1)) } else { m };
                        nle_media::matte::store_in(&root, &asset, kind, idx, &m).map_err(|e| format!("matte cache: {e}"))?;
                        written += 1;
                        m
                    }
                };
                if kind == MatteKind::Person {
                    if let Some((c, _)) = nle_ai::segment::subject_centroid(&m) {
                        subject.push((t, c));
                    }
                }
            }
            Ok(MotionResult::Matte { kind, frames: written, stopped, track: subject_track(&subject, aspect) })
        });
        self.motion.job = Some(MotionJob { clip: id, label: "AI matte", job });
    }

    /// Land a finished motion job as an undoable edit.
    pub fn poll_motion(&mut self, ctx: &egui::Context) {
        let Some(j) = self.motion.job.as_ref() else { return };
        let Some(res) = j.job.take() else {
            ctx.request_repaint_after(std::time::Duration::from_millis(60));
            return;
        };
        let id = j.clip;
        self.motion.job = None;
        // new mattes are not an edit, so nothing else redraws a paused viewer
        self.refresh_preview();
        let Some(mut c) = self.project.timeline.clip(id).cloned() else { return };
        match res {
            Ok(MotionResult::Track(t, end)) => {
                let n = t.samples.len();
                match c.tracks.iter_mut().find(|x| x.id == t.id) {
                    Some(slot) => *slot = t,
                    None => return,
                }
                if self.exec(Command::UpdateClip { clip: Box::new(c) }) {
                    self.status(match end {
                        TrackEnd::Done => format!("Tracked {n} frames"),
                        TrackEnd::Lost => format!("Track lost after {n} frames — re-place the region and continue"),
                        TrackEnd::NoFrames => format!("Tracked {n} frames (no more frames could be decoded)"),
                        TrackEnd::Cancelled => format!("Tracking stopped at {n} frames"),
                    });
                }
            }
            Ok(MotionResult::Stabilize(st)) => {
                let crop = st.crop_scale;
                c.stabilization = Some(st);
                if self.exec(Command::UpdateClip { clip: Box::new(c) }) {
                    self.status(format!("Stabilized (crop {crop:.2}×)"));
                }
            }
            Ok(MotionResult::Cuts { cuts, fps }) => match motion::scene_cut_command(&self.project.timeline, id, &cuts, fps) {
                Some(cmd) => {
                    let n = match &cmd {
                        Command::Batch { commands, .. } => commands.len(),
                        _ => 1,
                    };
                    if self.exec(cmd) {
                        self.status(format!("Bladed at {n} scene cuts"));
                    }
                }
                None => self.status("No scene cuts found in this clip"),
            },
            Ok(MotionResult::Matte { kind, frames, stopped, track }) => {
                let mut msg = format!("{}: {frames} new frames cached", kind.label());
                if let Some(why) = stopped {
                    msg += &format!(" ({why} before the end of the clip)");
                }
                if !c.effects.iter().any(|e| e.kind.matte() == Some(kind)) {
                    msg += " - click Add Effect to see it";
                }
                if let Some(t) = track {
                    msg += " - subject track updated (use Smart 9:16 Reframe on it)";
                    match c.tracks.iter_mut().find(|x| x.name == SUBJECT_TRACK) {
                        Some(slot) => *slot = Track { id: slot.id, ..t },
                        None => c.tracks.push(t),
                    }
                    if !self.exec(Command::UpdateClip { clip: Box::new(c) }) {
                        return;
                    }
                }
                self.status(msg);
            }
            Err(e) => self.status(format!("Motion analysis: {e}")),
        }
    }
}

/// Viewer overlay for the Motion tab: every track of the selected clip at
/// the playhead, and drag handles for the region being placed. Returns
/// `true` when it consumed the pointer.
pub fn overlay(app: &mut App, ui: &mut egui::Ui, img: Rect) -> bool {
    if app.inspector_tab != crate::app::InspectorTab::Motion {
        return false;
    }
    let Some(id) = app.selection.first().copied() else { return false };
    let Some(clip) = app.project.timeline.clip(id) else { return false };
    if clip.tracks.is_empty() {
        return false;
    }
    let now = src_secs(clip, app.local_time(id));
    let to_s = |p: [f32; 2]| Pos2::new(img.left() + p[0] * img.width(), img.top() + p[1] * img.height());
    let painter = ui.painter().with_clip_rect(img);
    for t in &clip.tracks {
        let editing = app.motion.edit_track == Some(t.id);
        let col = if editing { theme::SNAP } else { Color32::from_rgb(80, 220, 140) };
        let quad: Vec<Pos2> = t.corners_at(now).iter().map(|&p| to_s(p)).collect();
        painter.add(egui::Shape::closed_line(quad.clone(), Stroke::new(1.5_f32, col)));
        painter.circle_filled(to_s(t.point_at(now)), 3.0, col);
        painter.text(quad[0] + Vec2::new(2.0, -2.0), egui::Align2::LEFT_BOTTOM, &t.name, egui::FontId::proportional(10.0), col);
        // path of the centre across the tracked range
        if t.samples.len() > 1 {
            let path: Vec<Pos2> = t.samples.iter().map(|s| to_s(motion::apply(&s.h, [t.region[0] + t.region[2] / 2.0, t.region[1] + t.region[3] / 2.0]))).collect();
            painter.add(egui::Shape::line(path, Stroke::new(1.0_f32, col.gamma_multiply(0.45))));
        }
    }
    let Some(tid) = app.motion.edit_track else { return false };
    let Some(t) = clip.tracks.iter().find(|t| t.id == tid) else {
        app.motion.edit_track = None;
        return false;
    };
    let r = t.region;
    let body = Rect::from_min_max(to_s([r[0], r[1]]), to_s([r[0] + r[2], r[1] + r[3]]));
    let corner = body.right_bottom();
    painter.rect_filled(Rect::from_center_size(corner, Vec2::splat(9.0)), 2.0, Color32::WHITE);
    let rb = ui.interact(body, ui.id().with(("track-body", tid)), Sense::drag());
    let rc = ui.interact(Rect::from_center_size(corner, Vec2::splat(16.0)), ui.id().with(("track-corner", tid)), Sense::drag());
    let d = if rc.dragged() {
        Some((rc.drag_delta(), true))
    } else if rb.dragged() {
        Some((rb.drag_delta(), false))
    } else {
        None
    };
    if let Some((d, resize)) = d {
        let (dx, dy) = (d.x / img.width(), d.y / img.height());
        let mut c = clip.clone();
        if let Some(t) = c.tracks.iter_mut().find(|t| t.id == tid) {
            if resize {
                t.region[2] = (t.region[2] + dx).clamp(0.01, 1.0 - t.region[0]);
                t.region[3] = (t.region[3] + dy).clamp(0.01, 1.0 - t.region[1]);
            } else {
                t.region[0] = (t.region[0] + dx).clamp(0.0, 1.0 - t.region[2]);
                t.region[1] = (t.region[1] + dy).clamp(0.0, 1.0 - t.region[3]);
            }
            // a placed region starts a fresh track at this frame
            t.reference = now;
            t.samples = vec![TrackSample { t: now, h: motion::IDENTITY }];
        }
        app.exec_key(Command::UpdateClip { clip: Box::new(c) }, Some(drag_key(OVERLAY_TAG, id)));
    }
    if !ui.ctx().input(|i| i.pointer.any_down()) {
        app.history.seal();
    }
    rb.dragged() || rc.dragged() || rb.hovered() || rc.hovered()
}

/// Every source frame a clip spanning `lo..hi` seconds can show: from the
/// one it starts inside to the one at `hi` (where reversed playback starts),
/// but none after the last frame of the media.
fn matte_frames(lo: f64, hi: f64, media_secs: f64, fps: f64) -> std::ops::RangeInclusive<i64> {
    let first = (lo * fps + 1e-3).floor().max(0.0) as i64;
    let mut last = (hi * fps + 1e-3).floor() as i64;
    if media_secs > 0.0 && (hi - media_secs).abs() * fps < 1.0 {
        last = last.min((media_secs * fps - 1e-3).ceil() as i64 - 1);
    }
    first..=last.max(first)
}

/// Bilinear downscale of a matte.
fn resize_matte(m: &nle_media::matte::Matte, w: u32, h: u32) -> nle_media::matte::Matte {
    let data = (0..h).flat_map(|y| (0..w).map(move |x| (x, y))).map(|(x, y)| (m.sample((x as f32 + 0.5) / w as f32, (y as f32 + 0.5) / h as f32) * 255.0).round() as u8).collect();
    nle_media::matte::Matte::new(w, h, data).unwrap_or_else(|| m.clone())
}

/// A translation track from per-frame subject centroids (lightly smoothed),
/// for Smart 9:16 Reframe on non-gaming footage.
fn subject_track(pts: &[(f64, [f32; 2])], aspect: f32) -> Option<Track> {
    let &(t0, c0) = pts.first()?;
    let mut t = Track::new(SUBJECT_TRACK, MotionModel::Translation, [(c0[0] - 0.1).clamp(0.0, 0.8), (c0[1] - 0.15).clamp(0.0, 0.7), 0.2, 0.3], t0, aspect);
    let mut s = c0;
    for &(time, c) in &pts[1..] {
        s = [s[0] + (c[0] - s[0]) * 0.35, s[1] + (c[1] - s[1]) * 0.35];
        t.insert(TrackSample { t: time, h: motion::translation(s[0] - c0[0], s[1] - c0[1]) });
    }
    Some(t)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mattes_cover_every_frame_the_clip_shows() {
        // starts late inside frame 10: that frame is on screen first
        assert_eq!(matte_frames(10.8 / 30.0, 20.2 / 30.0, 10.0, 30.0), 10..=20);
        // whole media: nothing past its last frame
        assert_eq!(matte_frames(0.0, 2.0, 2.0, 30.0), 0..=59);
        // 123/30*30 is 122.99999 in floats
        assert_eq!(*matte_frames(123.0 / 30.0, 5.0, 10.0, 30.0).start(), 123);
        // a still has no length but still gets a matte
        assert_eq!(matte_frames(0.0, 0.0, 0.0, 30.0), 0..=0);
    }

    #[test]
    fn subject_track_follows_matte_centroids() {
        let pts: Vec<(f64, [f32; 2])> = (0..30).map(|i| (i as f64 / 30.0, [0.3 + 0.01 * i as f32, 0.5])).collect();
        let t = subject_track(&pts, 16.0 / 9.0).unwrap();
        assert_eq!(t.name, SUBJECT_TRACK);
        let c = t.corners_at(29.0 / 30.0);
        let cx = (c[0][0] + c[2][0]) / 2.0;
        // smoothed: lags the true centre (0.59) slightly, well past the start (0.3)
        assert!(cx > 0.5 && cx < 0.6, "{cx}");
        assert!(subject_track(&[], 1.0).is_none());
        let m = nle_media::matte::Matte::new(4, 2, vec![0, 255, 0, 255, 0, 255, 0, 255]).unwrap();
        let r = resize_matte(&m, 2, 1);
        assert_eq!((r.width, r.height), (2, 1));
    }
}
