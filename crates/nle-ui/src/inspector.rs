//! Inspector: Video, Color, Audio, Titles & Transitions tabs for the
//! selected clip, or project settings when nothing is selected. Every change
//! is an undoable `UpdateClip` command; continuous drags coalesce into one
//! undo step.

use crate::app::{drag_key, App, InspectorTab, PunchState};
use crate::color_ui::{color_management, corrections, mask_editor, ColorView, MaskFocus};
use crate::theme;
use crate::widgets::{color_wheel, param_row, slider_f32, ParamSpec};
use eframe::egui::{self, Color32, Stroke};
use nle_core::{
    AudioRole, BlendMode, Clip, ClipKind, Command, CropAspect, CropMode, Direction, Effect, EffectKind, FadeShape, FrameBlendMode, FrameRate, Interpolation, LutRef, ParamKind,
    Retime, RollMode, ShapeKind, ShapeSpec, TextAnimation, Time, TitleParams, TitleStyle, Transition, TransitionKind, VideoFormat,
};
use nle_media::analysis::{normalize_gain_db, TARGET_LUFS};

const INSPECTOR_TAG: u64 = 0x1f;
const SPEED_TAG: u64 = 0x2f;
const RENAME_KEY: u64 = 0x3f;

pub fn show(app: &mut App, ui: &mut egui::Ui) {
    // the panel keeps the width it was given: the tabs wrap and a wide tab
    // scrolls sideways, where before the panel grew to fit each tab's widest row
    ui.horizontal_wrapped(|ui| {
        for (tab, label) in [
            (InspectorTab::Video, "Video"),
            (InspectorTab::Color, "Color"),
            (InspectorTab::Audio, "Audio"),
            (InspectorTab::TitlesTransitions, "Titles & Transitions"),
            (InspectorTab::Motion, "Motion"),
        ] {
            ui.selectable_value(&mut app.inspector_tab, tab, label);
        }
    });
    ui.separator();

    let Some(id) = app.selection.first().copied() else {
        project_settings(app, ui);
        return;
    };
    let Some(original) = app.project.timeline.clip(id).cloned() else { return };
    let local = app.local_time(id);
    let frame = app.project.timeline.frame();
    let mut c = original.clone();
    let layer = crate::viewer::clip_layer_size(app, &original, local);
    // a new press starts a new undo step, even if a field still has the keyboard
    if ui.ctx().input(|i| i.pointer.any_pressed()) {
        app.history.seal();
    }

    ui.horizontal(|ui| {
        let accent = clip_accent(&c);
        ui.label(egui::RichText::new("■").color(accent));
        ui.add(egui::TextEdit::singleline(&mut c.name).desired_width(ui.available_width() - 70.0));
        ui.label(egui::RichText::new(crate::widgets::secs_label(c.duration)).color(theme::TEXT_DIM));
    });
    ui.label(egui::RichText::new(format!("{} · local {}", kind_label(&c), nle_core::format_timecode(local, app.project.timeline.format.rate))).small().color(theme::TEXT_DIM));
    if app.selection.len() > 1 {
        ui.label(egui::RichText::new(format!("{} clips selected — editing the first", app.selection.len())).small().color(theme::SNAP));
    }
    ui.add_space(4.0);

    let mut retime_action: Option<RetimeAction> = None;
    let mut add_title: Option<bool> = None;
    let mut pick_template: Option<(String, TitleParams)> = None;
    let mut templates = std::mem::take(&mut app.title_templates);
    templates.ensure_loaded();
    let mut analyze_beats = false;
    let mut multiband_beats = false;
    let mut scan_kills = false;
    let mut sync_beats = false;
    let mut learn_noise = false;
    let mut toggle_reframe_916 = false;
    let mut punch_request: Option<bool> = None;
    let asset_lufs = if let ClipKind::Asset { asset } = &c.kind { app.asset_loudness_lufs(*asset) } else { None };
    let beat_busy = app.beat_job.is_some();
    let kill_busy = app.kill_job.is_some();
    let mb_busy = app.multiband_job.is_some();
    let busses: Vec<(u64, String)> =
        app.focus.root(&app.project.timeline).mixer.user.iter().map(|u| (u.id, format!("{} → {}", u.name, nle_core::BusId::Role(u.feeds).label()))).collect();
    let detected_space = match &c.kind {
        ClipKind::Asset { asset } => app.project.asset(*asset).and_then(|a| a.video.as_ref()).and_then(|v| nle_core::color::ColorSpace::from_ffmpeg_transfer(&v.color_transfer)),
        _ => None,
    };
    let motion_cx = crate::motion_ui::tab_ctx(app, id, local);
    let mut motion_action = None;
    let motion_ui = &mut app.motion;
    let video_assets: Vec<(nle_core::AssetId, String)> = app.project.assets.iter().filter(|a| a.video.is_some()).map(|a| (a.id, a.name.clone())).collect();
    let color_view = &mut app.color_view;
    let stills = &app.project.stills;
    let grade_ui = &mut app.grade;
    let mut gallery_action = None;
    egui::ScrollArea::both().auto_shrink([false, false]).show(ui, |ui| match app.inspector_tab {
        InspectorTab::Video => video_tab(ui, &mut c, local, frame, layer, &mut retime_action, &mut toggle_reframe_916, &mut app.punch, &mut punch_request, color_view),
        InspectorTab::Color => {
            color_tab(ui, &mut c, local, frame, detected_space, color_view, &video_assets);
            ui.add_space(6.0);
            gallery_action = crate::grade_ui::gallery(ui, stills, grade_ui);
        }
        InspectorTab::Audio => audio_tab(
            ui,
            &mut c,
            local,
            frame,
            asset_lufs,
            beat_busy,
            kill_busy,
            mb_busy,
            &mut analyze_beats,
            &mut multiband_beats,
            &mut scan_kills,
            &mut sync_beats,
            &mut learn_noise,
            &busses,
        ),
        InspectorTab::TitlesTransitions => titles_tab(ui, &mut c, &mut add_title, &mut templates, &mut pick_template),
        InspectorTab::Motion => motion_action = crate::motion_ui::tab(ui, &mut c, &motion_cx, motion_ui),
    });

    // transitions are separate commands (they alter edit points)
    let tr_changed = c.transition_in != original.transition_in || c.transition_out != original.transition_out;
    if c != original {
        let key = Some(drag_key(INSPECTOR_TAG, id));
        if tr_changed {
            if c.transition_out != original.transition_out {
                app.exec_key(Command::SetTransition { id, at_end: true, transition: c.transition_out.clone() }, key);
            }
            if c.transition_in != original.transition_in {
                app.exec_key(Command::SetTransition { id, at_end: false, transition: c.transition_in.clone() }, key);
            }
        }
        if let Some(now) = app.project.timeline.clip(id) {
            c.transition_in = now.transition_in.clone();
            c.transition_out = now.transition_out.clone();
            c.retime = now.retime.clone();
            if c != *now {
                app.exec_key(Command::UpdateClip { clip: Box::new(c) }, key);
            }
        }
    }
    if !ui.ctx().input(|i| i.pointer.any_down()) && !ui.ctx().wants_keyboard_input() {
        app.history.seal();
    }
    if let Some(a) = gallery_action {
        app.gallery_action(a);
    }
    if let Some(lt) = app.color_view.seek_local.take() {
        if let Some(start) = app.project.timeline.clip_start(id) {
            app.seek(start + lt);
        }
    }
    match retime_action {
        Some(RetimeAction::Preset(s)) => app.retime_preset(s),
        Some(RetimeAction::VelocityPreset(name)) => app.apply_velocity_preset(name),
        Some(RetimeAction::Reverse) => app.retime_reverse(),
        Some(RetimeAction::Speed(speed)) => {
            app.exec_key(Command::SetSpeed { id, speed }, Some(drag_key(SPEED_TAG, id)));
        }
        Some(RetimeAction::Ramp(up)) => app.retime_ramp(up),
        Some(RetimeAction::Hold) => app.freeze_frame(),
        Some(RetimeAction::Blend(m)) => app.set_retime_blend(m),
        None => {}
    }
    app.title_templates = templates;
    if let Some(lt) = add_title {
        app.add_title(lt);
    }
    if let Some((name, params)) = pick_template {
        app.add_title_with(&name, params);
    }
    if analyze_beats {
        app.analyze_selected_beats();
    }
    if multiband_beats {
        app.analyze_multiband_beats();
    }
    if scan_kills {
        app.scan_selected_kills();
    }
    if sync_beats {
        app.sync_clips_to_beats();
    }
    if learn_noise {
        learn_noise_at_playhead(app, id);
    }
    if toggle_reframe_916 {
        app.toggle_gaming_reframe_916();
    }
    if let Some(hard) = punch_request {
        app.zoom_punch(hard);
    }
    if let Some(a) = motion_action {
        app.run_motion_action(id, a);
    }
}

/// Learn a noise profile from 1 s of the clip's source starting at the
/// playhead (park it on room tone between words first).
fn learn_noise_at_playhead(app: &mut App, id: nle_core::ClipId) {
    let spans = nle_core::eval::audio_spans(&app.project.timeline);
    let Some(span) = spans.iter().find(|s| s.clip == id && s.start <= app.playhead && app.playhead < s.end()).or_else(|| spans.iter().find(|s| s.clip == id)) else {
        app.status("Learn noise: this clip has no audible span");
        return;
    };
    let Some(src) = app.library.audio.get(&span.asset).cloned() else {
        app.status("Learn noise: the clip's audio is still loading");
        return;
    };
    let rate = src.sample_rate as f64;
    let at = span.source_time(app.playhead.clamp(span.start, span.end())).as_secs_f64().max(0.0);
    let start = (at * rate) as usize;
    let end = start + rate as usize;
    let Some(profile) = nle_media::audio::restore::learn_noise_profile(&src, start, end) else {
        app.status("Learn noise: not enough audio after the playhead");
        return;
    };
    let Some(mut c) = app.project.timeline.clip(id).cloned() else { return };
    c.audio.noise_reduction.profile = Some(profile);
    c.audio.noise_reduction.enabled = true;
    if app.exec(Command::UpdateClip { clip: Box::new(c) }) {
        app.status(format!("Noise profile learned from {at:.2}–{:.2} s of the source", at + 1.0));
    }
}

enum RetimeAction {
    Preset(f64),
    /// A constant speed; negative plays backwards.
    Speed(f64),
    VelocityPreset(&'static str),
    Reverse,
    Ramp(bool),
    Hold,
    Blend(FrameBlendMode),
}

pub fn clip_accent(c: &Clip) -> Color32 {
    match &c.kind {
        ClipKind::Title(_) | ClipKind::Adjustment => theme::TITLE,
        ClipKind::Gap => theme::GAP,
        _ if !c.has_video() => theme::AUDIO,
        _ => theme::VIDEO,
    }
}

fn kind_label(c: &Clip) -> &'static str {
    match &c.kind {
        ClipKind::Asset { .. } if !c.has_video() => "Audio clip",
        ClipKind::Asset { .. } => "Video clip",
        ClipKind::Gap => "Gap",
        ClipKind::Title(_) => "Title",
        ClipKind::Adjustment => "Adjustment layer",
        ClipKind::Compound(_) => "Compound clip",
    }
}

/// A section with a Reset button in its header. True when that was clicked.
fn resettable(ui: &mut egui::Ui, title: &str, body: impl FnOnce(&mut egui::Ui)) -> bool {
    let id = ui.make_persistent_id(("section", title));
    let mut reset = false;
    let mut toggle = false;
    let mut header = egui::collapsing_header::CollapsingState::load_with_default_open(ui.ctx(), id, true).show_header(ui, |ui| {
        toggle = ui.add(egui::Label::new(egui::RichText::new(title).strong()).sense(egui::Sense::click())).clicked();
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            reset = ui.small_button("Reset").on_hover_text(format!("Reset {title} to its defaults")).clicked();
        });
    });
    if toggle {
        header.toggle();
    }
    header.body(body);
    reset
}

fn section(ui: &mut egui::Ui, title: &str, body: impl FnOnce(&mut egui::Ui)) {
    egui::CollapsingHeader::new(egui::RichText::new(title).strong()).default_open(true).show(ui, body);
}

// ---------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
fn video_tab(
    ui: &mut egui::Ui,
    c: &mut Clip,
    t: Time,
    f: Time,
    layer: [f32; 2],
    retime: &mut Option<RetimeAction>,
    toggle_reframe_916: &mut bool,
    punch: &mut PunchState,
    punch_request: &mut Option<bool>,
    view: &mut ColorView,
) {
    if matches!(c.kind, ClipKind::Gap) || (!c.has_video() && !matches!(c.kind, ClipKind::Title(_))) {
        ui.label(egui::RichText::new("No video in this clip.").color(theme::TEXT_DIM));
        if matches!(c.kind, ClipKind::Asset { .. } | ClipKind::Compound(_)) {
            speed_section(ui, c, t, retime);
        }
        return;
    }
    let reset = resettable(ui, "Compositing", |ui| {
        ui.horizontal(|ui| {
            ui.add_sized([96.0, 20.0], egui::Label::new(egui::RichText::new("Blend Mode").color(theme::TEXT_DIM)));
            egui::ComboBox::from_id_salt("blend").selected_text(c.video.blend_mode.label()).show_ui(ui, |ui| {
                for m in BlendMode::ALL {
                    ui.selectable_value(&mut c.video.blend_mode, m, m.label());
                }
            });
        });
        if matches!(c.kind, ClipKind::Asset { .. } | ClipKind::Compound(_)) {
            ui.horizontal(|ui| {
                ui.add_sized([96.0, 20.0], egui::Label::new(egui::RichText::new("Rate Conform").color(theme::TEXT_DIM)));
                egui::ComboBox::from_id_salt("rate-conform").selected_text(c.rate_conform.label()).show_ui(ui, |ui| {
                    for m in FrameBlendMode::ALL {
                        ui.selectable_value(&mut c.rate_conform, m, m.label());
                    }
                });
            });
            let is_916 = c.effects.iter().any(|e| e.enabled && e.kind == EffectKind::GamingReframe916);
            if ui
                .selectable_label(is_916, "📱 Gaming 9:16 Reframe")
                .on_hover_text("Stretch-free vertical layouts: Split Stack, Center Fit + Blur, Full 9:16 Crop (+ aspect-locked killfeed PiP). Switches the sequence to 1080×1920.")
                .clicked()
            {
                *toggle_reframe_916 = true;
            }
        }
        param_row(ui, &ParamSpec::new("Opacity", 0.0, 1.0, 1.0).scaled(100.0, " %").decimals(0), &mut c.video.opacity, t, f);
    });
    if reset {
        let d = nle_core::VideoParams::default();
        c.video.opacity = d.opacity;
        c.video.blend_mode = d.blend_mode;
    }
    if !matches!(c.kind, ClipKind::Adjustment) {
        transform_section(ui, &mut c.video, t, f);
        zoom_punch_section(ui, punch, punch_request);
        crop_section(ui, &mut c.video, t, f, layer);
    }
    effects_section(ui, &mut c.effects, t, f, view);
    if matches!(c.kind, ClipKind::Asset { .. } | ClipKind::Compound(_)) {
        speed_section(ui, c, t, retime);
    }
}

fn transform_section(ui: &mut egui::Ui, v: &mut nle_core::VideoParams, t: Time, f: Time) {
    let reset = resettable(ui, "Transform", |ui| {
        param_row(ui, &ParamSpec::new("Position X", -2.0, 2.0, 0.0).scaled(100.0, " %").decimals(1), &mut v.position_x, t, f);
        param_row(ui, &ParamSpec::new("Position Y", -2.0, 2.0, 0.0).scaled(100.0, " %").decimals(1), &mut v.position_y, t, f);
        param_row(ui, &ParamSpec::new("Rotation", -3600.0, 3600.0, 0.0).scaled(1.0, "°").speed(0.5).decimals(1), &mut v.rotation, t, f);

        let (x0, y0) = (v.scale_x.clone(), v.scale_y.clone());
        param_row(ui, &ParamSpec::new("Scale X", 0.0, 20.0, 1.0).scaled(100.0, " %").decimals(1), &mut v.scale_x, t, f);
        param_row(ui, &ParamSpec::new("Scale Y", 0.0, 20.0, 1.0).scaled(100.0, " %").decimals(1), &mut v.scale_y, t, f);
        if v.scale_lock {
            if v.scale_x != x0 {
                follow_scale(&x0, &v.scale_x, &mut v.scale_y, t);
            } else if v.scale_y != y0 {
                follow_scale(&y0, &v.scale_y, &mut v.scale_x, t);
            }
        }
        ui.checkbox(&mut v.scale_lock, "Lock scale aspect ratio").on_hover_text(
            "Changing one axis scales the other by the same factor, and viewer corner handles keep the shape (Shift frees them). Viewer edge handles always stretch one axis.",
        );

        param_row(ui, &ParamSpec::new("Anchor X", -1.0, 1.0, 0.0).scaled(100.0, " %").decimals(1), &mut v.anchor_x, t, f);
        param_row(ui, &ParamSpec::new("Anchor Y", -1.0, 1.0, 0.0).scaled(100.0, " %").decimals(1), &mut v.anchor_y, t, f);
        ui.horizontal(|ui| {
            ui.add_sized([96.0, 20.0], egui::Label::new(egui::RichText::new("Flip").color(theme::TEXT_DIM)));
            ui.toggle_value(&mut v.flip_horizontal, "Horizontal");
            ui.toggle_value(&mut v.flip_vertical, "Vertical");
        });
    });
    if reset {
        let d = nle_core::VideoParams::default();
        v.position_x = d.position_x;
        v.position_y = d.position_y;
        v.rotation = d.rotation;
        v.scale_x = d.scale_x;
        v.scale_y = d.scale_y;
        v.scale_lock = d.scale_lock;
        v.anchor_x = d.anchor_x;
        v.anchor_y = d.anchor_y;
        v.flip_horizontal = d.flip_horizontal;
        v.flip_vertical = d.flip_vertical;
    }
}

/// With the scale locked, `other` follows what happened to the edited axis.
fn follow_scale(before: &nle_core::Param, after: &nle_core::Param, other: &mut nle_core::Param, t: Time) {
    if before.is_animated() && !after.is_animated() && after.value == 1.0 {
        // the row's reset button
        other.clear_keyframes();
        other.value = 1.0;
        return;
    }
    let (from, to) = (before.at(t), after.at(t));
    if (to - from).abs() > 1e-9 {
        let scaled = if from.abs() > 1e-6 { other.at(t) * to / from } else { to };
        other.set(t, scaled.clamp(0.0, 20.0));
    }
}

fn crop_section(ui: &mut egui::Ui, v: &mut nle_core::VideoParams, t: Time, f: Time, layer: [f32; 2]) {
    let reset = resettable(ui, "Crop", |ui| {
        ui.horizontal(|ui| {
            ui.selectable_value(&mut v.crop_mode, CropMode::Trim, "Trim");
            ui.selectable_value(&mut v.crop_mode, CropMode::KenBurns, "Ken Burns");
        });
        match v.crop_mode {
            CropMode::Trim => {
                let mut preset = None;
                ui.horizontal_wrapped(|ui| {
                    for a in CropAspect::ALL {
                        if ui.selectable_label(v.crop_aspect == a, a.label()).clicked() {
                            preset = Some(a);
                        }
                    }
                });
                let mut before = [v.crop_left.at_f32(t), v.crop_right.at_f32(t), v.crop_top.at_f32(t), v.crop_bottom.at_f32(t)];
                if let Some(a) = preset {
                    v.crop_aspect = a;
                    v.crop_lock = a.ratio().is_some();
                    if let Some(ratio) = a.ratio() {
                        let crop = nle_render::manipulate::aspect_crop(before, layer, ratio);
                        for (p, to) in [&mut v.crop_left, &mut v.crop_right, &mut v.crop_top, &mut v.crop_bottom].into_iter().zip(crop) {
                            p.set(t, to as f64);
                        }
                        before = crop;
                    }
                }
                let mut edited = None;
                for (i, (label, p)) in [("Left", &mut v.crop_left), ("Right", &mut v.crop_right), ("Top", &mut v.crop_top), ("Bottom", &mut v.crop_bottom)].into_iter().enumerate()
                {
                    if param_row(ui, &ParamSpec::new(label, 0.0, 0.999, 0.0).scaled(100.0, " %").decimals(1), p, t, f) && (p.at_f32(t) - before[i]).abs() > 1e-6 {
                        edited = Some((i, p.at_f32(t)));
                    }
                }
                if let Some((edge, value)) = edited {
                    let crop = nle_render::manipulate::set_crop_edge(before, edge, value, v.crop_lock);
                    for (i, p) in [&mut v.crop_left, &mut v.crop_right, &mut v.crop_top, &mut v.crop_bottom].into_iter().enumerate() {
                        let now = if i == edge { value } else { before[i] };
                        if (crop[i] - now).abs() > 1e-6 {
                            p.set(t, crop[i] as f64);
                        }
                    }
                }
                if ui.checkbox(&mut v.crop_lock, "Lock crop aspect ratio").changed() && !v.crop_lock {
                    v.crop_aspect = CropAspect::Free;
                }
            }
            CropMode::KenBurns => {
                for (label, r) in [("Start", &mut v.ken_burns_start), ("End", &mut v.ken_burns_end)] {
                    ui.horizontal(|ui| {
                        ui.add_sized([60.0, 20.0], egui::Label::new(egui::RichText::new(label).color(theme::TEXT_DIM)));
                        for (i, n) in ["x", "y", "w", "h"].iter().enumerate() {
                            ui.add(egui::DragValue::new(&mut r[i]).range(0.0..=1.0).speed(0.005).prefix(format!("{n} ")).max_decimals(3));
                        }
                    });
                }
            }
        }
    });
    if reset {
        let d = nle_core::VideoParams::default();
        v.crop_left = d.crop_left;
        v.crop_right = d.crop_right;
        v.crop_top = d.crop_top;
        v.crop_bottom = d.crop_bottom;
        v.crop_mode = d.crop_mode;
        v.ken_burns_start = d.ken_burns_start;
        v.ken_burns_end = d.ken_burns_end;
        v.crop_aspect = d.crop_aspect;
        v.crop_lock = d.crop_lock;
    }
}

fn speed_section(ui: &mut egui::Ui, c: &Clip, t: Time, retime: &mut Option<RetimeAction>) {
    let reset = resettable(ui, "Speed (Retime)", |ui| {
        let cur = c.retime.as_ref().map(|r| r.speed_at(t)).unwrap_or(1.0);
        ui.horizontal(|ui| {
            ui.add_sized([96.0, 20.0], egui::Label::new(egui::RichText::new("Speed").color(theme::TEXT_DIM)));
            let mut times = cur.abs();
            let mut backwards = cur < 0.0;
            let changed = ui
                .add(
                    egui::DragValue::new(&mut times)
                        .range(nle_core::edit_ops::MIN_SPEED..=nle_core::edit_ops::MAX_SPEED)
                        .clamp_existing_to_range(false)
                        .update_while_editing(false)
                        .speed(0.01)
                        .suffix(" ×")
                        .max_decimals(2),
                )
                .on_hover_text("0.1× to 10×. Sets one speed for the whole clip, replacing any ramp.")
                .changed();
            let flipped = ui.checkbox(&mut backwards, "Reverse").changed();
            if changed {
                *retime = Some(RetimeAction::Speed(if backwards { -times } else { times }));
            } else if flipped {
                *retime = Some(RetimeAction::Reverse);
            }
        });
        ui.horizontal_wrapped(|ui| {
            for (l, s) in [("25%", 0.25), ("50%", 0.5), ("100%", 1.0), ("200%", 2.0), ("400%", 4.0)] {
                if ui.button(l).clicked() {
                    *retime = Some(RetimeAction::Preset(s));
                }
            }
        });
        ui.horizontal_wrapped(|ui| {
            if ui.button("⟲ Reverse").clicked() {
                *retime = Some(RetimeAction::Reverse);
            }
            if ui.button("Ramp ↓ 25%").clicked() {
                *retime = Some(RetimeAction::Ramp(false));
            }
            if ui.button("Ramp ↑ 400%").clicked() {
                *retime = Some(RetimeAction::Ramp(true));
            }
            if ui.button("Hold 2s").on_hover_text("Shift+H").clicked() {
                *retime = Some(RetimeAction::Hold);
            }
        });
        ui.label(egui::RichText::new("Fragmovie Velocity Curves (Ctrl+R lane)").small().color(theme::TEXT_DIM));
        ui.horizontal_wrapped(|ui| {
            for name in Retime::PRESET_NAMES {
                let short = name.split('(').next().unwrap_or(name).trim();
                if ui.small_button(short).on_hover_text(name).clicked() {
                    *retime = Some(RetimeAction::VelocityPreset(name));
                }
            }
        });
        if let Some(r) = &c.retime {
            ui.horizontal(|ui| {
                ui.add_sized([96.0, 20.0], egui::Label::new(egui::RichText::new("Video Quality").color(theme::TEXT_DIM)));
                egui::ComboBox::from_id_salt("blendmode").selected_text(r.blend.label()).show_ui(ui, |ui| {
                    for m in FrameBlendMode::ALL {
                        if ui.selectable_label(r.blend == m, m.label()).clicked() {
                            *retime = Some(RetimeAction::Blend(m));
                        }
                    }
                });
            });
        }
    });
    if reset && c.retime.is_some() {
        *retime = Some(RetimeAction::Speed(1.0));
    }
}

/// Keyframed zoom punch: buttons, intensity, anchor presets.
fn zoom_punch_section(ui: &mut egui::Ui, punch: &mut PunchState, request: &mut Option<bool>) {
    use nle_core::zoom_punch::{PunchAnchor, HARD_INTENSITY, INTENSITY_MAX, INTENSITY_MIN};
    section(ui, "Zoom Punch", |ui| {
        ui.horizontal(|ui| {
            let hit = egui::Button::new(egui::RichText::new("💥 Zoom Punch").strong()).fill(theme::VIDEO.gamma_multiply(0.45));
            if ui.add(hit).on_hover_text("Z — 1.0× at −60 ms → peak (Snap Punch) at +80 ms → hold → 1.0× (Ease In-Out) at +650 ms, at the playhead").clicked() {
                *request = Some(false);
            }
            if ui.button("Hard Punch").on_hover_text(format!("Shift+Z — 1.0× → {HARD_INTENSITY:.2}× Snap Punch over 110 ms, then holds")).clicked() {
                *request = Some(true);
            }
        });
        ui.horizontal(|ui| {
            ui.add_sized([96.0, 20.0], egui::Label::new(egui::RichText::new("Punch Intensity").color(theme::TEXT_DIM)));
            ui.add(egui::Slider::new(&mut punch.intensity, INTENSITY_MIN..=INTENSITY_MAX).step_by(0.01).custom_formatter(|v, _| format!("{v:.2}×")))
                .on_hover_text("Peak zoom of a regular punch");
        });
        punch.intensity = nle_core::zoom_punch::clamp_intensity(punch.intensity);
        ui.horizontal(|ui| {
            ui.add_sized([96.0, 20.0], egui::Label::new(egui::RichText::new("Anchor").color(theme::TEXT_DIM)));
            egui::ComboBox::from_id_salt("punch-anchor").selected_text(punch.anchor.label()).show_ui(ui, |ui| {
                for a in PunchAnchor::PRESETS {
                    if ui.selectable_label(punch.anchor == a, a.label()).clicked() {
                        punch.anchor = a;
                        punch.picking = false;
                    }
                }
                let custom = matches!(punch.anchor, PunchAnchor::Custom { .. });
                if ui.selectable_label(custom, PunchAnchor::Custom { x: 0.0, y: 0.0 }.label()).clicked() {
                    punch.picking = true;
                }
            });
            if ui.selectable_label(punch.picking, "🎯").on_hover_text("Click on the preview to pick the anchor").clicked() {
                punch.picking = !punch.picking;
            }
        });
        if let PunchAnchor::Custom { x, y } = punch.anchor {
            ui.label(egui::RichText::new(format!("Custom anchor {:+.0} %, {:+.0} %", x * 100.0, y * 100.0)).small().color(theme::TEXT_DIM));
        }
        ui.label(
            egui::RichText::new("Scale is always uniform about the anchor. Drag ◆ in the Animation lane to retime, right-click to change ease, Delete to remove.")
                .small()
                .color(theme::TEXT_DIM),
        );
    });
}

fn effects_section(ui: &mut egui::Ui, effects: &mut Vec<Effect>, t: Time, f: Time, view: &mut ColorView) {
    section(ui, &format!("Effects ({})", effects.len()), |ui| {
        ui.horizontal(|ui| {
            ui.menu_button("＋ Add Effect", |ui| {
                let mut cats: Vec<&'static str> = Vec::new();
                for k in EffectKind::ALL {
                    if !cats.contains(&k.category()) {
                        cats.push(k.category());
                    }
                }
                for cat in cats {
                    ui.menu_button(cat, |ui| {
                        for k in EffectKind::ALL.into_iter().filter(|k| k.category() == cat) {
                            if ui.button(k.label()).clicked() {
                                effects.push(Effect::new(k));
                                ui.close_menu();
                            }
                        }
                    });
                }
            });
            if !effects.is_empty() && ui.small_button("Clear").clicked() {
                effects.clear();
            }
        });

        let mut swap: Option<(usize, usize)> = None;
        let mut remove: Option<usize> = None;
        let n = effects.len();
        for (idx, e) in effects.iter_mut().enumerate() {
            e.normalize();
            let id = e.id;
            ui.add_space(2.0);
            egui::Frame::new().fill(theme::BG).stroke(Stroke::new(1.0_f32, theme::BORDER)).corner_radius(4.0).inner_margin(egui::Margin::symmetric(6, 4)).show(ui, |ui| {
                ui.push_id(id, |ui| {
                    ui.horizontal(|ui| {
                        ui.checkbox(&mut e.enabled, "");
                        let title = if e.kind.needs_motion() { format!("{}  [Flow]", e.kind.label()) } else { e.kind.label().to_string() };
                        ui.label(egui::RichText::new(title).strong());
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if ui.small_button("✕").on_hover_text("Remove effect").clicked() {
                                remove = Some(idx);
                            }
                            if ui.add_enabled(idx + 1 < n, egui::Button::new("↓").small()).on_hover_text("Move down").clicked() {
                                swap = Some((idx, idx + 1));
                            }
                            if ui.add_enabled(idx > 0, egui::Button::new("↑").small()).on_hover_text("Move up").clicked() {
                                swap = Some((idx, idx - 1));
                            }
                        });
                    });
                    if e.enabled {
                        let defs = e.kind.params();
                        let mut i = 0;
                        while i < defs.len() {
                            let d = &defs[i];
                            if d.kind == ParamKind::Color && i + 2 < defs.len() && defs[i + 1].kind == ParamKind::Color && defs[i + 2].kind == ParamKind::Color {
                                let label = d.name.strip_suffix(" R").unwrap_or(d.name);
                                ui.horizontal(|ui| {
                                    ui.add_sized([96.0, 20.0], egui::Label::new(egui::RichText::new(label).color(theme::TEXT_DIM)));
                                    let mut rgb = [e.params[i].at_f32(t).clamp(0.0, 1.0), e.params[i + 1].at_f32(t).clamp(0.0, 1.0), e.params[i + 2].at_f32(t).clamp(0.0, 1.0)];
                                    if ui.color_edit_button_rgb(&mut rgb).changed() {
                                        for k in 0..3 {
                                            e.params[i + k].set(t, rgb[k] as f64);
                                        }
                                    }
                                    let animated = (0..3).any(|k| e.params[i + k].is_animated());
                                    let at_key = (0..3).any(|k| e.params[i + k].curve.as_ref().is_some_and(|cv| cv.key_near(t, f.div_i64(2)).is_some()));
                                    let k_col = if at_key {
                                        theme::SNAP
                                    } else if animated {
                                        theme::TEXT
                                    } else {
                                        theme::TEXT_DIM
                                    };
                                    if ui.small_button(egui::RichText::new(if at_key { "◆" } else { "◇" }).color(k_col)).on_hover_text("Toggle keyframe at playhead").clicked()
                                    {
                                        for k in 0..3 {
                                            let pk = &mut e.params[i + k];
                                            if at_key {
                                                if let Some(cv) = &mut pk.curve {
                                                    cv.remove_near(t, f.div_i64(2));
                                                }
                                            } else {
                                                pk.add_keyframe(t);
                                            }
                                        }
                                    }
                                });
                                i += 3;
                                continue;
                            }
                            match d.kind {
                                ParamKind::Choice(opts) => {
                                    let cur = (e.params[i].at(t).round() as usize).min(opts.len().saturating_sub(1));
                                    ui.horizontal(|ui| {
                                        ui.add_sized([96.0, 20.0], egui::Label::new(egui::RichText::new(d.name).color(theme::TEXT_DIM)));
                                        egui::ComboBox::from_id_salt(("fx-choice", id, i)).selected_text(opts.get(cur).copied().unwrap_or("")).show_ui(ui, |ui| {
                                            for (oi, opt) in opts.iter().enumerate() {
                                                let mut sel = cur;
                                                if ui.selectable_value(&mut sel, oi, *opt).clicked() {
                                                    e.params[i].set(t, oi as f64);
                                                }
                                            }
                                        });
                                    });
                                }
                                ParamKind::Integer => {
                                    let spec = ParamSpec::new(d.name, d.min, d.max, d.default).scaled(1.0, d.suffix).decimals(0).speed(0.25);
                                    if param_row(ui, &spec, &mut e.params[i], t, f) {
                                        let v = e.params[i].at(t).round();
                                        e.params[i].set(t, v);
                                    }
                                }
                                ParamKind::Number | ParamKind::Color => {
                                    let span = (d.max - d.min).abs();
                                    let (dec, spd) = if span <= 2.5 {
                                        (2, 0.01)
                                    } else if span <= 20.0 {
                                        (2, 0.05)
                                    } else {
                                        (1, 0.5)
                                    };
                                    let spec = ParamSpec::new(d.name, d.min, d.max, d.default).scaled(1.0, d.suffix).decimals(dec).speed(spd);
                                    param_row(ui, &spec, &mut e.params[i], t, f);
                                }
                            }
                            i += 1;
                        }
                        egui::CollapsingHeader::new("Mask").id_salt(("fx-mask", id)).default_open(e.mask.is_some()).show(ui, |ui| {
                            mask_editor(ui, &mut e.mask, MaskFocus::Effect(id), &mut view.mask_focus, t, Time(f.0 / 2));
                        });
                    }
                });
            });
        }
        if let Some((a, b)) = swap {
            effects.swap(a, b);
        } else if let Some(idx) = remove {
            effects.remove(idx);
        }
    });
}

// ---------------------------------------------------------------------------

fn color_tab(ui: &mut egui::Ui, c: &mut Clip, t: Time, f: Time, detected: Option<nle_core::color::ColorSpace>, view: &mut ColorView, assets: &[(nle_core::AssetId, String)]) {
    if matches!(c.kind, ClipKind::Gap) || !c.has_video() && !matches!(c.kind, ClipKind::Adjustment | ClipKind::Title(_)) {
        ui.label(egui::RichText::new("No video in this clip.").color(theme::TEXT_DIM));
        return;
    }
    let col = &mut c.color;
    ui.horizontal(|ui| {
        ui.checkbox(&mut col.enabled, "Color Correction");
        if ui.small_button("Reset All").clicked() {
            *col = Default::default();
        }
    });
    ui.add_enabled_ui(col.enabled, |ui| {
        color_management(ui, col, detected);
        let reset = resettable(ui, "Color Wheels", |ui| {
            ui.horizontal(|ui| {
                let size = ((ui.available_width() - 24.0) / 3.0).clamp(70.0, 110.0);
                color_wheel(ui, "Shadows (Lift)", &mut col.shadows, size);
                color_wheel(ui, "Midtones (Gamma)", &mut col.midtones, size);
                color_wheel(ui, "Highlights (Gain)", &mut col.highlights, size);
            });
            ui.label(egui::RichText::new("Double-click a wheel to reset it").small().color(theme::TEXT_DIM));
        });
        if reset {
            col.shadows = Default::default();
            col.midtones = Default::default();
            col.highlights = Default::default();
        }
        let reset = resettable(ui, "Basic Color", |ui| {
            param_row(ui, &ParamSpec::new("Brightness", -1.0, 1.0, 0.0).scaled(100.0, "").decimals(0), &mut col.brightness, t, f);
            param_row(ui, &ParamSpec::new("Exposure", -4.0, 4.0, 0.0).scaled(1.0, " st").decimals(2), &mut col.exposure, t, f);
            param_row(ui, &ParamSpec::new("Contrast", -1.0, 1.0, 0.0).scaled(100.0, "").decimals(0), &mut col.contrast, t, f);
            param_row(ui, &ParamSpec::new("Saturation", 0.0, 2.0, 1.0).scaled(100.0, " %").decimals(0), &mut col.saturation, t, f);
            param_row(ui, &ParamSpec::new("Temperature", -1.0, 1.0, 0.0).scaled(100.0, "").decimals(0), &mut col.temperature, t, f);
            param_row(ui, &ParamSpec::new("Tint", -1.0, 1.0, 0.0).scaled(100.0, "").decimals(0), &mut col.tint, t, f);
        });
        if reset {
            let d = nle_core::ColorParams::default();
            col.exposure = d.exposure;
            col.brightness = d.brightness;
            col.contrast = d.contrast;
            col.saturation = d.saturation;
            col.temperature = d.temperature;
            col.tint = d.tint;
        }
        section(ui, "Look LUT", |ui| {
            ui.horizontal(|ui| {
                let label = col.lut.as_ref().map(|l| std::path::Path::new(&l.path).file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default());
                ui.label(label.unwrap_or_else(|| "None".into()));
                if ui.button("Load .cube…").clicked() {
                    if let Some(p) = rfd::FileDialog::new().add_filter("3D LUT", &["cube"]).pick_file() {
                        col.lut = Some(LutRef { path: p.to_string_lossy().to_string(), intensity: 1.0 });
                    }
                }
                if col.lut.is_some() && ui.small_button("✕").clicked() {
                    col.lut = None;
                }
            });
            if let Some(l) = &mut col.lut {
                slider_f32(ui, "Mix", &mut l.intensity, 0.0..=1.0, "");
            }
        });
        corrections(ui, col, view, t, f);
        crate::node_ui::node_graph(ui, col, assets, t, f);
    });
}

// ---------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)] // egui panel: each argument is a separate piece of app state
fn audio_tab(
    ui: &mut egui::Ui,
    c: &mut Clip,
    t: Time,
    f: Time,
    asset_lufs: Option<f32>,
    beat_busy: bool,
    kill_busy: bool,
    mb_busy: bool,
    analyze_beats: &mut bool,
    multiband_beats: &mut bool,
    scan_kills: &mut bool,
    sync_beats: &mut bool,
    learn_noise: &mut bool,
    busses: &[(u64, String)],
) {
    if !c.has_audio() {
        let why = if c.link.is_some() { "This clip's audio is on its linked audio clip." } else { "No audio in this clip." };
        ui.label(egui::RichText::new(why).color(theme::TEXT_DIM));
        return;
    }
    // fades span the audible extent, which J/L cuts widen beyond the video
    let audible = (c.duration + c.audio.audio_lead.max(Time::ZERO) + c.audio.audio_tail.max(Time::ZERO)).as_secs_f64() as f32;
    let a = &mut c.audio;
    let reset = resettable(ui, "Volume & Pan", |ui| {
        ui.checkbox(&mut a.muted, "Mute");
        param_row(ui, &ParamSpec::new("Volume", -60.0, 12.0, 0.0).scaled(1.0, " dB").speed(0.1).decimals(1), &mut a.volume_db, t, f);
        param_row(ui, &ParamSpec::new("Pan", -1.0, 1.0, 0.0).scaled(100.0, "").decimals(0), &mut a.pan, t, f);
        ui.horizontal(|ui| {
            ui.add_sized([96.0, 20.0], egui::Label::new(egui::RichText::new("Role").color(theme::TEXT_DIM)));
            for r in AudioRole::ALL {
                ui.selectable_value(&mut a.role, r, r.label());
            }
        });
        ui.horizontal(|ui| {
            ui.add_sized([96.0, 20.0], egui::Label::new(egui::RichText::new("Bus").color(theme::TEXT_DIM)));
            let current = a.bus.and_then(|id| busses.iter().find(|b| b.0 == id));
            let text = match (a.bus, current) {
                (None, _) => format!("{} (role bus)", a.role.label()),
                (Some(_), Some(b)) => b.1.clone(),
                (Some(_), None) => format!("{} (its bus was deleted)", a.role.label()),
            };
            egui::ComboBox::from_id_salt("clip-audio-bus")
                .selected_text(text)
                .show_ui(ui, |ui| {
                    ui.selectable_value(&mut a.bus, None, format!("{} (role bus)", a.role.label()));
                    for (id, name) in busses {
                        ui.selectable_value(&mut a.bus, Some(*id), name);
                    }
                })
                .response
                .on_hover_text(if busses.is_empty() {
                    "Add submix busses in the Audio Mixer (Ctrl+8) → + Bus"
                } else {
                    "Send this clip through a submix bus (ducking still follows the role)"
                });
        });
    });
    if reset {
        // the role says what the clip is, so it stays
        let d = nle_core::AudioParams::default();
        a.muted = d.muted;
        a.volume_db = d.volume_db;
        a.pan = d.pan;
    }
    section(ui, "Loudness & Voice Clarity", |ui| {
        let norm_label = match asset_lufs {
            Some(l) => format!("-14 LUFS Normalize ({l:.1} LUFS -> {:+.1} dB)", normalize_gain_db(l, TARGET_LUFS)),
            None => "-14 LUFS Loudness Normalize (waiting for analysis)".to_string(),
        };
        if ui.checkbox(&mut a.loudness_normalize, norm_label).changed() && a.loudness_normalize {
            if let Some(l) = asset_lufs {
                a.loudness_gain_db = normalize_gain_db(l, TARGET_LUFS);
            }
        }
        if a.loudness_normalize {
            if let Some(l) = asset_lufs {
                let target = normalize_gain_db(l, TARGET_LUFS);
                if (a.loudness_gain_db - target).abs() > 0.05 {
                    a.loudness_gain_db = target;
                }
            }
        }
        ui.checkbox(&mut a.vocal_clarity, "Vocal Clarity (80 Hz HPF + 3 kHz presence + limiter)");
        ui.horizontal_wrapped(|ui| {
            if ui
                .add_enabled(!beat_busy, egui::Button::new(if beat_busy { "Analyzing beats..." } else { "🥁 Analyze Beats" }))
                .on_hover_text("Detect tempo & beat peaks and add snap markers to the timeline ruler")
                .clicked()
            {
                *analyze_beats = true;
            }
            if ui
                .add_enabled(!mb_busy, egui::Button::new(if mb_busy { "Scanning Kick/Snare..." } else { "🥁 Kick + Snare Beats" }))
                .on_hover_text("Split beat detection into Sub-Bass/Kick (20–120 Hz) and Snare (1.5–6 kHz)")
                .clicked()
            {
                *multiband_beats = true;
            }
            if ui
                .add_enabled(!kill_busy, egui::Button::new(if kill_busy { "Scanning kills..." } else { "🎯 Scan VOD for Kills" }))
                .on_hover_text("Analyze gameplay audio transients & kill-chimes to drop Kill markers")
                .clicked()
            {
                *scan_kills = true;
            }
            if ui.button("⚡ Sync Selected to Beats").on_hover_text("Snap selected clips/cuts cleanly to beat markers").clicked() {
                *sync_beats = true;
            }
        });
    });
    section(ui, "Gaming Audio Mastering", |ui| {
        ui.horizontal(|ui| {
            ui.checkbox(&mut a.lowpass_enabled, "Underwater Low-Pass Sweep");
            if ui.small_button("🌊 Sweep at Playhead").on_hover_text("Automate 20 kHz → 350 Hz muffle into playhead, snapping back open on impact").clicked() {
                a.apply_pre_kill_underwater_sweep(t);
            }
        });
        if a.lowpass_enabled {
            param_row(ui, &ParamSpec::new("Cutoff", 100.0, 20_000.0, 20_000.0).scaled(1.0, " Hz").speed(30.0).decimals(0), &mut a.lowpass_hz, t, f);
            slider_f32(ui, "Resonance Q", &mut a.lowpass_q, 0.5..=4.0, "");
        }
        ui.checkbox(&mut a.sub_bass_punch, "80 Hz Sub-Bass Punch (Kill Drop)");
        if a.sub_bass_punch {
            slider_f32(ui, "Sub Boost", &mut a.sub_bass_db, 0.0..=12.0, " dB");
        }
        ui.label(egui::RichText::new("Master bus: −1.0 dBFS soft-knee limiter (always on)").color(theme::TEXT_DIM));
        ui.checkbox(&mut a.preserve_pitch, "Preserve Pitch on Speed Ramps (WSOLA)");
        ui.checkbox(&mut a.mute_extreme_slowmo, "Auto-Mute Extreme Slow-Mo (< 30%)");
    });
    let reset = resettable(ui, "Fades & Crossfades", |ui| {
        let mut fi = a.fade_in.as_secs_f64() as f32;
        let mut fo = a.fade_out.as_secs_f64() as f32;
        // a fade can't outlast the clip, and fade in and out can't overlap
        if crate::widgets::slider_f32_edits(ui, "Fade In", &mut fi, 0.0..=(audible - fo).max(0.0), " s") {
            a.fade_in = Time::from_secs_f64(fi.clamp(0.0, (audible - fo).max(0.0)) as f64);
        }
        if crate::widgets::slider_f32_edits(ui, "Fade Out", &mut fo, 0.0..=(audible - fi).max(0.0), " s") {
            a.fade_out = Time::from_secs_f64(fo.clamp(0.0, (audible - fi).max(0.0)) as f64);
        }
        ui.horizontal(|ui| {
            ui.add_sized([96.0, 20.0], egui::Label::new(egui::RichText::new("Shape").color(theme::TEXT_DIM)));
            for s in FadeShape::ALL {
                ui.selectable_value(&mut a.fade_shape, s, s.label());
            }
        });
        ui.label(egui::RichText::new("Audio Crossfade").color(theme::TEXT_DIM))
            .on_hover_text("Centred on the cut, using the media beyond it. Without spare media it fades out, then in; with no neighbour it fades the edge.");
        for (label, d) in [("At Start", &mut a.crossfade_in), ("At End", &mut a.crossfade_out)] {
            let mut secs = d.as_secs_f64() as f32;
            if slider_f32(ui, label, &mut secs, 0.0..=5.0, " s") {
                *d = Time::from_secs_f64(secs as f64);
            }
        }
    });
    if reset {
        let d = nle_core::AudioParams::default();
        a.fade_in = d.fade_in;
        a.fade_out = d.fade_out;
        a.fade_shape = d.fade_shape;
        a.crossfade_in = d.crossfade_in;
        a.crossfade_out = d.crossfade_out;
    }
    section(ui, "Restoration", |ui| {
        let nr = &mut a.noise_reduction;
        ui.checkbox(&mut nr.enabled, egui::RichText::new("Noise Reduction").strong())
            .on_hover_text("Spectral noise reduction, rendered in the background (raw audio plays until it is ready). Export always uses the cleaned audio.");
        ui.add_enabled_ui(nr.enabled, |ui| {
            slider_f32(ui, "Reduction", &mut nr.reduction_db, 0.0..=40.0, " dB");
            slider_f32(ui, "Sensitivity", &mut nr.sensitivity, 0.5..=4.0, "");
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new(if nr.profile.is_some() { "Profile: learned" } else { "Profile: automatic (quietest parts)" }).small().color(theme::TEXT_DIM));
                if ui.small_button("Learn").on_hover_text("Park the playhead on room tone (a pause with no speech), then learn 1 s of it").clicked() {
                    *learn_noise = true;
                }
                if nr.profile.is_some() && ui.small_button("Auto").on_hover_text("Forget the learned profile").clicked() {
                    nr.profile = None;
                }
            });
        });
        let vi = &mut a.voice_isolation;
        let backend = nle_media::audio::restore::voice_isolation_backend();
        ui.checkbox(&mut vi.enabled, egui::RichText::new("Voice Isolation").strong()).on_hover_text(match backend {
            Some(name) => format!(
                "Neural speech enhancement ({name}): removes noise, room tone, reverb tails and most background sound around the voice. Other voices and singing can remain."
            ),
            None => "DSP fallback: cuts steady noise, rumble and hiss around the voice. It cannot remove music or game audio — that needs the neural model.".to_string(),
        });
        ui.add_enabled_ui(vi.enabled, |ui| {
            slider_f32(ui, "Amount", &mut vi.amount, 0.0..=1.0, "");
            if backend.is_none() {
                ui.label(egui::RichText::new("No voice-separation model loaded — using the DSP fallback.").small().color(theme::SNAP));
            }
        });
        let lv = &mut a.leveler;
        ui.checkbox(&mut lv.enabled, egui::RichText::new("Dialogue Leveler").strong())
            .on_hover_text("Rides the gain toward the target so quiet and loud passages sit together; pauses are not boosted");
        ui.add_enabled_ui(lv.enabled, |ui| {
            slider_f32(ui, "Target", &mut lv.target_db, -40.0..=-6.0, " dB");
            slider_f32(ui, "Max Gain", &mut lv.max_gain_db, 0.0..=24.0, " dB");
            slider_f32(ui, "Speed", &mut lv.speed_ms, 50.0..=3000.0, " ms");
            slider_f32(ui, "Floor", &mut lv.floor_db, -80.0..=-20.0, " dB");
        });
    });
    section(ui, "EQ (6-band parametric)", |ui| {
        crate::audio_ui::eq_editor(ui, &mut a.eq, egui::Id::new("clip-eq"));
    });
    section(ui, "Dynamics & Effects", |ui| {
        crate::audio_ui::fx_editor(ui, &mut a.fx, egui::Id::new("clip-fx"), None);
    });
    section(ui, "Auto-Duck Under Voice", |ui| {
        let is_voice = a.role == nle_core::AudioRole::Dialogue;
        let d = &mut a.ducking;
        ui.add_enabled_ui(!is_voice, |ui| {
            ui.checkbox(&mut d.enabled, "Auto-Duck Under Voice").on_hover_text("Pull this clip down while Dialogue / VO clips are speaking (preview and export)");
        });
        if is_voice {
            ui.label(egui::RichText::new("Dialogue clips are the voice — set the role to Music / SFX / General to duck it.").small().color(theme::TEXT_DIM));
        }
        ui.add_enabled_ui(d.enabled && !is_voice, |ui| {
            // stored as positive attenuation; shown as the negative gain it applies
            let mut gain = -d.amount_db;
            if ui
                .add(
                    egui::Slider::new(&mut gain, -*nle_core::DuckingSettings::AMOUNT_RANGE_DB.end()..=-*nle_core::DuckingSettings::AMOUNT_RANGE_DB.start())
                        .text("Duck")
                        .suffix(" dB"),
                )
                .changed()
            {
                d.amount_db = -gain;
            }
            slider_f32(ui, "Threshold", &mut d.threshold_db, -60.0..=0.0, " dB");
            slider_f32(ui, "Lookahead", &mut d.lookahead_ms, 0.0..=500.0, " ms");
            slider_f32(ui, "Attack", &mut d.attack_ms, 1.0..=1000.0, " ms");
            slider_f32(ui, "Hold", &mut d.hold_ms, 0.0..=1000.0, " ms");
            slider_f32(ui, "Release", &mut d.release_ms, 20.0..=3000.0, " ms");
            if ui.small_button("Reset to defaults").clicked() {
                *d = nle_core::DuckingSettings { enabled: true, ..Default::default() };
            }
        });
    });
}

// ---------------------------------------------------------------------------

/// Saved user title templates (`title_templates.json` in the data folder).
#[derive(Default)]
pub struct TitleTemplates {
    pub user: Vec<(String, TitleParams)>,
    /// Name typed for "Save as Template".
    pub name: String,
    pub error: Option<String>,
    loaded: bool,
}

impl TitleTemplates {
    fn path() -> std::path::PathBuf {
        nle_media::paths::local_data_dir().join("title_templates.json")
    }

    pub fn ensure_loaded(&mut self) {
        if !self.loaded {
            self.loaded = true;
            self.user = std::fs::read(Self::path()).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
        }
    }

    fn save(&mut self) {
        let path = Self::path();
        let res = path.parent().map_or(Ok(()), std::fs::create_dir_all).and_then(|_| std::fs::write(&path, serde_json::to_vec_pretty(&self.user).unwrap_or_default()));
        self.error = res.err().map(|e| format!("Could not save templates: {e}"));
    }
}

fn titles_tab(ui: &mut egui::Ui, c: &mut Clip, add_title: &mut Option<bool>, tpl: &mut TitleTemplates, pick: &mut Option<(String, TitleParams)>) {
    ui.horizontal(|ui| {
        if ui.button("＋ Basic Title").clicked() {
            *add_title = Some(false);
        }
        if ui.button("＋ Lower Third").clicked() {
            *add_title = Some(true);
        }
    });
    section(ui, "Templates", |ui| {
        ui.label(egui::RichText::new("Click to add at the playhead").small().color(theme::TEXT_DIM));
        ui.horizontal_wrapped(|ui| {
            for (name, p) in TitleParams::templates() {
                if ui.small_button(name).clicked() {
                    *pick = Some((name.into(), p));
                }
            }
        });
        if !tpl.user.is_empty() {
            ui.label(egui::RichText::new("My templates (right-click to delete)").small().color(theme::TEXT_DIM));
            let mut delete = None;
            ui.horizontal_wrapped(|ui| {
                for (i, (name, p)) in tpl.user.iter().enumerate() {
                    let r = ui.small_button(name);
                    if r.clicked() {
                        *pick = Some((name.clone(), p.clone()));
                    }
                    r.context_menu(|ui| {
                        if ui.button("Delete Template").clicked() {
                            delete = Some(i);
                            ui.close_menu();
                        }
                    });
                }
            });
            if let Some(i) = delete {
                tpl.user.remove(i);
                tpl.save();
            }
        }
        if let ClipKind::Title(tp) = &c.kind {
            ui.horizontal(|ui| {
                ui.add(egui::TextEdit::singleline(&mut tpl.name).hint_text("Template name").desired_width(150.0));
                let name = tpl.name.trim().to_string();
                if ui.add_enabled(!name.is_empty(), egui::Button::new("Save as Template")).on_hover_text("Save this title's look and text; same name replaces").clicked() {
                    tpl.user.retain(|(n, _)| *n != name);
                    tpl.user.push((name, TitleParams::clone(tp)));
                    tpl.name.clear();
                    tpl.save();
                }
            });
        }
        if let Some(e) = &tpl.error {
            ui.colored_label(theme::PLAYHEAD, e);
        }
    });
    if let ClipKind::Title(tp) = &mut c.kind {
        section(ui, "Shape", |ui| {
            ui.horizontal_wrapped(|ui| {
                if ui.selectable_label(tp.shape.is_none(), "Text").clicked() {
                    tp.shape = None;
                }
                for k in ShapeKind::ALL {
                    let on = tp.shape.as_ref().is_some_and(|s| s.kind == k);
                    if ui.selectable_label(on, k.label()).clicked() && !on {
                        tp.shape = Some(match tp.shape.take() {
                            Some(s) => ShapeSpec { kind: k, ..s },
                            None => ShapeSpec::new(k),
                        });
                    }
                }
            });
            if let Some(s) = &mut tp.shape {
                ui.label(egui::RichText::new("The shape replaces the text; Color fills it, outline / shadow / animation apply.").small().color(theme::TEXT_DIM));
                slider_f32(ui, "Width", &mut s.size[0], 1.0..=3840.0, " px");
                slider_f32(ui, if s.kind == ShapeKind::Line { "Thickness" } else { "Height" }, &mut s.size[1], 1.0..=2160.0, " px");
                if s.kind == ShapeKind::RoundedRect {
                    slider_f32(ui, "Corner", &mut s.radius, 0.0..=600.0, " px");
                }
                if s.kind == ShapeKind::Star {
                    ui.horizontal(|ui| {
                        ui.add_sized([96.0, 20.0], egui::Label::new(egui::RichText::new("Points").color(theme::TEXT_DIM)));
                        ui.add(egui::Slider::new(&mut s.points, 3..=24));
                    });
                    slider_f32(ui, "Inner", &mut s.inner, 0.05..=1.0, "");
                }
                slider_f32(ui, "Rotation", &mut s.rotation, -180.0..=180.0, "°");
            }
        });
        section(ui, "Text", |ui| {
            ui.add(egui::TextEdit::multiline(&mut tp.text).hint_text("One row per line; \"Role | Name\" makes credit columns").desired_rows(2).desired_width(f32::INFINITY));
            ui.add(egui::TextEdit::singleline(&mut tp.subtitle).hint_text("Subtitle").desired_width(f32::INFINITY));
            ui.horizontal(|ui| {
                ui.add_sized([96.0, 20.0], egui::Label::new(egui::RichText::new("Scroll").color(theme::TEXT_DIM)));
                for r in RollMode::ALL {
                    ui.selectable_value(&mut tp.roll, r, r.label());
                }
            })
            .response
            .on_hover_text("Roll: credits move bottom to top over the clip; Crawl: right to left. Trim the clip to set the speed.");
            ui.horizontal(|ui| {
                ui.add_sized([96.0, 20.0], egui::Label::new(egui::RichText::new("Style").color(theme::TEXT_DIM)));
                for s in TitleStyle::ALL {
                    ui.selectable_value(&mut tp.style, s, s.label());
                }
            });
            ui.horizontal(|ui| {
                ui.add_sized([96.0, 20.0], egui::Label::new(egui::RichText::new("Font").color(theme::TEXT_DIM)));
                ui.add(egui::TextEdit::singleline(&mut tp.font_family).hint_text("system default").desired_width(150.0));
            });
            slider_f32(ui, "Size", &mut tp.font_size, 8.0..=400.0, " px");
            slider_f32(ui, "Tracking", &mut tp.tracking, -20.0..=80.0, " px");
            ui.horizontal(|ui| {
                ui.add_sized([96.0, 20.0], egui::Label::new(egui::RichText::new("Color").color(theme::TEXT_DIM)));
                ui.color_edit_button_rgba_unmultiplied(&mut tp.color);
            });
            slider_f32(ui, "Position X", &mut tp.position[0], 0.0..=1.0, "");
            slider_f32(ui, "Position Y", &mut tp.position[1], 0.0..=1.0, "");
        });
        section(ui, "Animation & Outline", |ui| {
            ui.horizontal(|ui| {
                ui.add_sized([96.0, 20.0], egui::Label::new(egui::RichText::new("Preset").color(theme::TEXT_DIM)));
                egui::ComboBox::from_id_salt("title-anim").selected_text(tp.animation.label()).show_ui(ui, |ui| {
                    for anim in TextAnimation::ALL {
                        ui.selectable_value(&mut tp.animation, anim, anim.label());
                    }
                });
            });
            if tp.animation != TextAnimation::None {
                slider_f32(ui, "Anim Duration", &mut tp.anim_duration, 0.05..=5.0, " s");
            }
            if tp.animation == TextAnimation::WordHighlight {
                ui.horizontal(|ui| {
                    ui.add_sized([96.0, 20.0], egui::Label::new(egui::RichText::new("Highlight").color(theme::TEXT_DIM)));
                    ui.color_edit_button_rgba_unmultiplied(&mut tp.highlight_color);
                });
            }
            ui.separator();
            ui.checkbox(&mut tp.outline_enabled, "Text Outline");
            ui.add_enabled_ui(tp.outline_enabled, |ui| {
                ui.horizontal(|ui| {
                    ui.add_sized([96.0, 20.0], egui::Label::new(egui::RichText::new("Outline Color").color(theme::TEXT_DIM)));
                    ui.color_edit_button_rgba_unmultiplied(&mut tp.outline_color);
                });
                slider_f32(ui, "Outline Width", &mut tp.outline_width, 0.5..=24.0, " px");
            });
        });
        section(ui, "Shadow", |ui| {
            ui.checkbox(&mut tp.shadow_enabled, "Drop Shadow");
            ui.add_enabled_ui(tp.shadow_enabled, |ui| {
                ui.horizontal(|ui| {
                    ui.add_sized([96.0, 20.0], egui::Label::new(egui::RichText::new("Color").color(theme::TEXT_DIM)));
                    ui.color_edit_button_rgba_unmultiplied(&mut tp.shadow_color);
                });
                slider_f32(ui, "Offset X", &mut tp.shadow_offset[0], -40.0..=40.0, " px");
                slider_f32(ui, "Offset Y", &mut tp.shadow_offset[1], -40.0..=40.0, " px");
                slider_f32(ui, "Blur", &mut tp.shadow_blur, 0.0..=40.0, " px");
            });
        });
        section(ui, "Background Pill", |ui| {
            ui.checkbox(&mut tp.pill_enabled, "Pill background");
            ui.add_enabled_ui(tp.pill_enabled, |ui| {
                ui.horizontal(|ui| {
                    ui.add_sized([96.0, 20.0], egui::Label::new(egui::RichText::new("Color").color(theme::TEXT_DIM)));
                    ui.color_edit_button_rgba_unmultiplied(&mut tp.pill_color);
                });
                slider_f32(ui, "Padding", &mut tp.pill_padding, 0.0..=120.0, " px");
                slider_f32(ui, "Radius", &mut tp.pill_radius, 0.0..=120.0, " px");
            });
        });
    }
    section(ui, "Transitions", |ui| {
        transition_editor(ui, "Out (to next clip)", "tr-out", &mut c.transition_out);
        ui.separator();
        transition_editor(ui, "In (from previous clip, or black)", "tr-in", &mut c.transition_in);
    });
}

fn transition_editor(ui: &mut egui::Ui, label: &str, id: &str, tr: &mut Option<Transition>) {
    ui.label(egui::RichText::new(label).color(theme::TEXT_DIM));
    ui.horizontal_wrapped(|ui| {
        if ui.selectable_label(tr.is_none(), "None").clicked() {
            *tr = None;
        }
        for k in TransitionKind::ALL {
            let on = tr.as_ref().is_some_and(|t| t.kind == k);
            if ui.selectable_label(on, k.label()).clicked() {
                match tr {
                    Some(t) => t.kind = k,
                    None => *tr = Some(Transition::new(k, Time::from_secs(1))),
                }
            }
        }
    });
    let Some(t) = tr else { return };
    let mut secs = t.duration.as_secs_f64() as f32;
    if slider_f32(ui, "Duration", &mut secs, 0.1..=5.0, " s") {
        t.duration = Time::from_secs_f64(secs as f64);
    }
    if matches!(t.kind, TransitionKind::WhipPan | TransitionKind::Slide | TransitionKind::Push | TransitionKind::LumaWipe) {
        ui.horizontal(|ui| {
            ui.add_sized([96.0, 20.0], egui::Label::new(egui::RichText::new("Direction").color(theme::TEXT_DIM)));
            for d in Direction::ALL {
                ui.selectable_value(&mut t.direction, d, d.label());
            }
        });
    }
    if t.kind == TransitionKind::LumaWipe {
        slider_f32(ui, "Softness", &mut t.softness, 0.0..=1.0, "");
    }
    ui.horizontal(|ui| {
        ui.add_sized([96.0, 20.0], egui::Label::new(egui::RichText::new("Easing").color(theme::TEXT_DIM)));
        egui::ComboBox::from_id_salt(id).selected_text(t.easing.label()).show_ui(ui, |ui| {
            for e in Interpolation::ALL {
                ui.selectable_value(&mut t.easing, e, e.label());
            }
        });
    });
}

// ---------------------------------------------------------------------------

fn project_settings(app: &mut App, ui: &mut egui::Ui) {
    ui.label(egui::RichText::new("Project").strong().size(14.0));
    ui.label(egui::RichText::new("Select a clip in the timeline to inspect it.").color(theme::TEXT_DIM));
    ui.add_space(6.0);
    let f = app.project.timeline.format;
    let mut changed = false;
    let mut nf: VideoFormat = f;
    // name and format live on the project's real timeline, not on an opened compound
    let stepped_in = !app.focus.is_empty();
    if stepped_in {
        ui.label(egui::RichText::new("Editing inside a compound clip: step out (Ctrl+[) to change the project's name or format.").color(theme::TEXT_DIM));
    }
    ui.add_enabled_ui(!stepped_in, |ui| {
        ui.horizontal(|ui| {
            ui.label("Name");
            let field = ui.text_edit_singleline(&mut app.project.name);
            if field.changed() {
                let name = app.project.name.clone();
                app.exec_key(Command::Rename { name }, Some(RENAME_KEY));
            }
            if field.lost_focus() {
                app.history.seal();
            }
        });
        ui.horizontal(|ui| {
            ui.label("Format");
            egui::ComboBox::from_id_salt("fmt").selected_text(format!("{}×{}", f.width, f.height)).show_ui(ui, |ui| {
                for (w, h, l) in [
                    (1280, 720, "720p HD"),
                    (1920, 1080, "1080p HD"),
                    (2560, 1440, "1440p"),
                    (3840, 2160, "4K UHD"),
                    (1080, 1920, "Vertical 1080×1920"),
                    (1080, 1080, "Square 1080"),
                ] {
                    if ui.selectable_label(f.width == w && f.height == h, l).clicked() {
                        nf.width = w;
                        nf.height = h;
                        changed = true;
                    }
                }
            });
            egui::ComboBox::from_id_salt("rate").selected_text(f.rate.label()).show_ui(ui, |ui| {
                for r in FrameRate::PRESETS {
                    if ui.selectable_label(f.rate == r, r.label()).clicked() {
                        nf.rate = r;
                        changed = true;
                    }
                }
            });
        });
    });
    if changed {
        app.exec(Command::SetFormat { format: nf });
    }
    ui.add_space(8.0);
    theme::heading(ui, "TIMELINE");
    let tl = &app.project.timeline;
    let placed = tl.placed_clips();
    egui::Grid::new("proj-stats").num_columns(2).show(ui, |ui| {
        ui.label("Duration");
        ui.label(nle_core::format_timecode(tl.extent(), tl.format.rate));
        ui.end_row();
        ui.label("Clips");
        ui.label(placed.iter().filter(|p| !p.clip.is_gap()).count().to_string());
        ui.end_row();
        ui.label("Undo steps");
        ui.label(format!("{} / redo {}", app.history.undo_len(), app.history.redo_len()));
        ui.end_row();
        ui.label("Autosave");
        ui.add(egui::Label::new(app.autosaver.path().display().to_string()).truncate());
        ui.end_row();
    });
}
