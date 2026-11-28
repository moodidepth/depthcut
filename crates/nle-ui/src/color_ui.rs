//! Colour page widgets: colour management, the stacked correction nodes
//! (wheels, log wheels, curves, HSL qualifier, window), shape-mask controls
//! shared with effects, and the viewer's mask gizmo.

use crate::app::{drag_key, App};
use crate::theme;
use crate::widgets::color_wheel;
use eframe::egui::{self, Color32, Pos2, Rect, Sense, Stroke, Vec2};
use nle_core::color::{ColorCorrection, ColorSpace, Curve, HueCurve, MaskShape, NodeValues, Qualifier, ShapeMask, WorkingSpace};
use nle_core::{ClipKind, ColorParams, Command, LutRef, Time};

/// Which mask the viewer gizmo edits.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MaskFocus {
    Node(u64),
    Effect(u64),
    Clip(usize),
}

/// Viewer-side colour state the inspector edits.
#[derive(Default)]
pub struct ColorView {
    /// Qualifier / window preview: (node id, show the black / white matte).
    pub highlight: Option<(u64, bool)>,
    pub mask_focus: Option<MaskFocus>,
    /// A node keyframe button asked to move the playhead here (clip-local).
    pub seek_local: Option<Time>,
}

const MASK_GIZMO_TAG: u64 = 0x6d;

fn sub(ui: &mut egui::Ui, title: &str, salt: u64, open: bool, body: impl FnOnce(&mut egui::Ui)) {
    egui::CollapsingHeader::new(egui::RichText::new(title).strong()).id_salt((title, salt)).default_open(open).show(ui, body);
}

fn label(ui: &mut egui::Ui, text: &str) {
    ui.add_sized([96.0, 20.0], egui::Label::new(egui::RichText::new(text).color(theme::TEXT_DIM)).truncate());
}

fn drag(ui: &mut egui::Ui, text: &str, v: &mut f32, range: std::ops::RangeInclusive<f32>, speed: f32, default: f32) -> bool {
    ui.horizontal(|ui| {
        label(ui, text);
        let mut c = ui.add(egui::DragValue::new(v).range(range).speed(speed).max_decimals(3)).changed();
        if ui.small_button("↺").on_hover_text("Reset").clicked() {
            *v = default;
            c = true;
        }
        c
    })
    .inner
}

fn lut_row(ui: &mut egui::Ui, text: &str, lut: &mut Option<LutRef>) {
    ui.horizontal(|ui| {
        label(ui, text);
        let name = lut.as_ref().map(|l| std::path::Path::new(&l.path).file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default());
        ui.label(name.unwrap_or_else(|| "None".into()));
        if ui.button("Load .cube…").clicked() {
            if let Some(p) = rfd::FileDialog::new().add_filter("3D LUT", &["cube"]).pick_file() {
                *lut = Some(LutRef { path: p.to_string_lossy().to_string(), intensity: 1.0 });
            }
        }
        if lut.is_some() && ui.small_button("✕").clicked() {
            *lut = None;
        }
    });
    if let Some(l) = lut {
        ui.horizontal(|ui| {
            label(ui, "Mix");
            ui.add(egui::Slider::new(&mut l.intensity, 0.0..=1.0));
        });
    }
}

/// Input transform / input LUT and the working space.
pub fn color_management(ui: &mut egui::Ui, col: &mut ColorParams, detected: Option<ColorSpace>) {
    egui::CollapsingHeader::new(egui::RichText::new("Color Management").strong()).default_open(true).show(ui, |ui| {
        ui.horizontal(|ui| {
            label(ui, "Input");
            egui::ComboBox::from_id_salt("cm-input").selected_text(col.input_space.label()).show_ui(ui, |ui| {
                for s in ColorSpace::ALL {
                    ui.selectable_value(&mut col.input_space, s, s.label());
                }
            });
        });
        if let Some(d) = detected.filter(|d| *d != col.input_space) {
            ui.horizontal(|ui| {
                label(ui, "");
                if ui.small_button(format!("Use {} (from media)", d.label())).clicked() {
                    col.input_space = d;
                }
            });
        }
        ui.horizontal(|ui| {
            label(ui, "Working");
            egui::ComboBox::from_id_salt("cm-working").selected_text(col.working.label()).show_ui(ui, |ui| {
                for s in WorkingSpace::ALL {
                    ui.selectable_value(&mut col.working, s, s.label());
                }
            });
        });
        lut_row(ui, "Input LUT", &mut col.input_lut);
        let mut note = String::new();
        if col.input_lut.is_some() {
            note.push_str("The input LUT replaces the input transform. ");
        } else if col.input_space.is_scene_or_hdr() {
            note.push_str("Highlights above display white roll off smoothly. ");
        }
        if col.working == WorkingSpace::AcesCct {
            note.push_str("Wheels and contrast work in ACEScct (log).");
        }
        if !note.is_empty() {
            ui.label(egui::RichText::new(note).small().color(theme::TEXT_DIM));
        }
    });
}

/// The stacked correction nodes after the primary grade.
pub fn corrections(ui: &mut egui::Ui, col: &mut ColorParams, view: &mut ColorView, t: Time, frame: Time) {
    let sel_id = ui.id().with("rcp-node-selected");
    let mut selected: Option<u64> = ui.data(|d| d.get_temp(sel_id));
    if !col.corrections.iter().any(|n| Some(n.id) == selected) {
        selected = col.corrections.first().map(|n| n.id);
    }
    let n = col.corrections.len();
    egui::CollapsingHeader::new(egui::RichText::new(format!("Corrections ({n})")).strong()).default_open(true).show(ui, |ui| {
        ui.horizontal_wrapped(|ui| {
            let mut add = |mut c: ColorCorrection| {
                c.name = format!("Correction {}", col.corrections.len() + 1);
                selected = Some(c.id);
                col.corrections.push(c);
            };
            if ui.button("＋ Correction").on_hover_text("A serial correction after the primary grade").clicked() {
                add(ColorCorrection::default());
            }
            if ui.button("＋ Qualifier").on_hover_text("A correction limited to a hue / saturation / luma range").clicked() {
                add(ColorCorrection { qualifier: Some(Qualifier::default()), ..Default::default() });
            }
            if ui.button("＋ Window").on_hover_text("A correction limited to a shape").clicked() {
                add(ColorCorrection { mask: Some(ShapeMask::new(MaskShape::Ellipse)), ..Default::default() });
            }
        });
        let mut swap = None;
        let mut remove = None;
        let mut dup = None;
        for (i, node) in col.corrections.iter_mut().enumerate() {
            ui.horizontal(|ui| {
                ui.checkbox(&mut node.enabled, "").on_hover_text("Enable");
                let name = if node.name.is_empty() { format!("Correction {}", i + 1) } else { node.name.clone() };
                let mut tags = String::new();
                if node.qualifier.is_some() {
                    tags.push_str(" ◐");
                }
                if node.mask.is_some() {
                    tags.push_str(" ◯");
                }
                if !node.keys.is_empty() {
                    tags.push_str(" ◆");
                }
                if ui.selectable_label(selected == Some(node.id), format!("{}. {name}{tags}", i + 1)).clicked() {
                    selected = Some(node.id);
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.small_button("✕").on_hover_text("Delete").clicked() {
                        remove = Some(i);
                    }
                    if ui.small_button("⧉").on_hover_text("Duplicate").clicked() {
                        dup = Some(i);
                    }
                    if ui.add_enabled(i + 1 < n, egui::Button::new("↓").small()).clicked() {
                        swap = Some((i, i + 1));
                    }
                    if ui.add_enabled(i > 0, egui::Button::new("↑").small()).clicked() {
                        swap = Some((i, i - 1));
                    }
                });
            });
        }
        if let Some((a, b)) = swap {
            col.corrections.swap(a, b);
        } else if let Some(i) = remove {
            let gone = col.corrections.remove(i).id;
            if let Some(g) = &mut col.graph {
                let ids: Vec<u64> = g.nodes.iter().filter(|n| matches!(n.kind, nle_core::node_graph::NodeKind::Corrector { node } if node == gone)).map(|n| n.id).collect();
                for id in ids {
                    g.remove(id);
                }
            }
            if view.highlight.is_some_and(|h| h.0 == gone) {
                view.highlight = None;
            }
            if view.mask_focus == Some(MaskFocus::Node(gone)) {
                view.mask_focus = None;
            }
        } else if let Some(i) = dup {
            let mut c = col.corrections[i].clone();
            c.id = ColorCorrection::default().id;
            c.name = format!("{} copy", if c.name.is_empty() { "Correction" } else { &c.name });
            selected = Some(c.id);
            col.corrections.insert(i + 1, c);
        }
        if let Some(node) = col.corrections.iter_mut().find(|c| Some(c.id) == selected) {
            ui.separator();
            node_editor(ui, node, col.working, view, t, frame);
        } else if col.corrections.is_empty() {
            ui.label(egui::RichText::new("Add a correction to grade on top of the primary: qualify a colour, draw a window, or shape curves.").small().color(theme::TEXT_DIM));
        }
    });
    ui.data_mut(|d| match selected {
        Some(s) => d.insert_temp(sel_id, s),
        None => d.remove::<u64>(sel_id),
    });
}

/// Edits one node. With keyframes the controls show (and edit) the values at
/// the playhead: a change there sets a key, like Resolve's dynamic keyframes.
fn node_editor(ui: &mut egui::Ui, n: &mut ColorCorrection, working: WorkingSpace, view: &mut ColorView, t: Time, frame: Time) {
    let tol = Time(frame.0 / 2);
    let base = NodeValues::of(n);
    let keyed = !n.keys.is_empty();
    if keyed {
        n.values_at(t).apply(n);
    }
    let shown = NodeValues::of(n);
    let mut reset = false;
    ui.horizontal(|ui| {
        label(ui, "Name");
        ui.add(egui::TextEdit::singleline(&mut n.name).desired_width(140.0));
        if ui.small_button("Reset").on_hover_text("Reset this correction and its keyframes (keeps qualifier and window)").clicked() {
            let keep = (n.id, n.name.clone(), n.qualifier, n.mask.clone());
            *n = ColorCorrection { id: keep.0, name: keep.1, qualifier: keep.2, mask: keep.3, ..Default::default() };
            reset = true;
        }
    });
    if !reset {
        node_keys(ui, n, t, tol, view);
    }
    node_controls(ui, n, working, view, t, tol);
    if keyed && !reset && !n.keys.is_empty() {
        if NodeValues::of(n) != shown {
            n.set_key(t, tol);
        }
        base.apply(n);
    }
}

/// Keyframe buttons: set / remove the key at the playhead, jump to the
/// previous / next one, clear them all.
fn node_keys(ui: &mut egui::Ui, n: &mut ColorCorrection, t: Time, tol: Time, view: &mut ColorView) {
    ui.horizontal(|ui| {
        label(ui, "Keyframes");
        let here = n.key_near(t, tol).is_some();
        let prev = n.keys.iter().map(|k| k.time).filter(|k| *k < t - tol).max();
        let next = n.keys.iter().map(|k| k.time).filter(|k| *k > t + tol).min();
        if ui.add_enabled(prev.is_some(), egui::Button::new("◀").small()).on_hover_text("Previous keyframe").clicked() {
            view.seek_local = prev;
        }
        let (icon, tip) = if here { ("◆", "Update the keyframe at the playhead") } else { ("◇", "Add a keyframe at the playhead (later changes here key automatically)") };
        if ui.small_button(icon).on_hover_text(tip).clicked() {
            n.set_key(t, tol);
        }
        if ui.add_enabled(next.is_some(), egui::Button::new("▶").small()).on_hover_text("Next keyframe").clicked() {
            view.seek_local = next;
        }
        if here && ui.small_button("✕").on_hover_text("Remove the keyframe at the playhead").clicked() {
            n.remove_key_near(t, tol);
        }
        if !n.keys.is_empty() {
            if ui.small_button("Clear").on_hover_text("Remove all keyframes, keeping the values at the playhead").clicked() {
                n.clear_keys(t);
            }
            ui.label(egui::RichText::new(format!("{}", n.keys.len())).small().color(theme::TEXT_DIM));
        }
    });
}

fn node_controls(ui: &mut egui::Ui, n: &mut ColorCorrection, working: WorkingSpace, view: &mut ColorView, t: Time, tol: Time) {
    let id = n.id;
    ui.horizontal(|ui| {
        label(ui, "Mix");
        ui.add(egui::Slider::new(&mut n.mix, 0.0..=1.0));
    });
    sub(ui, "Primary Wheels", id, true, |ui| {
        let size = ((ui.available_width() - 24.0) / 4.0).clamp(58.0, 92.0);
        ui.horizontal(|ui| {
            color_wheel(ui, "Lift", &mut n.lift, size);
            color_wheel(ui, "Gamma", &mut n.gamma, size);
            color_wheel(ui, "Gain", &mut n.gain, size);
            color_wheel(ui, "Offset", &mut n.offset, size);
        });
        drag(ui, "Exposure", &mut n.exposure, -4.0..=4.0, 0.01, 0.0);
        drag(ui, "Temperature", &mut n.temperature, -1.0..=1.0, 0.01, 0.0);
        drag(ui, "Tint", &mut n.tint, -1.0..=1.0, 0.01, 0.0);
        drag(ui, "Contrast", &mut n.contrast, -1.0..=1.0, 0.01, 0.0);
        drag(ui, "Pivot", &mut n.pivot, -0.3..=0.3, 0.005, 0.0);
        drag(ui, "Saturation", &mut n.saturation, 0.0..=2.0, 0.01, 1.0);
        let pivot = working.pivot() + n.pivot;
        ui.label(egui::RichText::new(format!("Contrast pivots at {pivot:.3} ({})", working.label())).small().color(theme::TEXT_DIM));
    });
    sub(ui, "Log Wheels", id, false, |ui| {
        let size = ((ui.available_width() - 24.0) / 3.0).clamp(64.0, 100.0);
        ui.horizontal(|ui| {
            color_wheel(ui, "Shadow", &mut n.log_shadows, size);
            color_wheel(ui, "Midtone", &mut n.log_midtones, size);
            color_wheel(ui, "Highlight", &mut n.log_highlights, size);
        });
        drag(ui, "Low Range", &mut n.low_range, 0.0..=1.0, 0.005, 0.333);
        drag(ui, "High Range", &mut n.high_range, 0.0..=1.0, 0.005, 0.667);
        if n.high_range < n.low_range + 0.05 {
            n.high_range = (n.low_range + 0.05).min(1.0);
        }
    });
    sub(ui, "Curves", id, true, |ui| curves(ui, n));
    sub(ui, "Qualifier", id, n.qualifier.is_some(), |ui| qualifier(ui, n, view));
    sub(ui, "Window", id, n.mask.is_some(), |ui| mask_editor(ui, &mut n.mask, MaskFocus::Node(id), &mut view.mask_focus, t, tol));
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum CurveTab {
    Master,
    Red,
    Green,
    Blue,
    HueHue,
    HueSat,
    HueLum,
}

fn curves(ui: &mut egui::Ui, n: &mut ColorCorrection) {
    let tab_id = ui.id().with(("curve-tab", n.id));
    let mut tab: CurveTab = ui.data(|d| d.get_temp(tab_id)).unwrap_or(CurveTab::Master);
    ui.horizontal_wrapped(|ui| {
        for (t, l, used) in [
            (CurveTab::Master, "Y", !n.curve_master.is_identity()),
            (CurveTab::Red, "R", !n.curve_r.is_identity()),
            (CurveTab::Green, "G", !n.curve_g.is_identity()),
            (CurveTab::Blue, "B", !n.curve_b.is_identity()),
            (CurveTab::HueHue, "Hue vs Hue", !n.hue_vs_hue.is_identity()),
            (CurveTab::HueSat, "Hue vs Sat", !n.hue_vs_sat.is_identity()),
            (CurveTab::HueLum, "Hue vs Lum", !n.hue_vs_lum.is_identity()),
        ] {
            let text = if used { format!("{l}•") } else { l.to_string() };
            ui.selectable_value(&mut tab, t, text);
        }
    });
    ui.data_mut(|d| d.insert_temp(tab_id, tab));
    let salt = (n.id, tab as u8);
    let (pts, kind, col) = match tab {
        CurveTab::Master => (&mut n.curve_master.points, CurveKind::Tone, Color32::from_gray(230)),
        CurveTab::Red => (&mut n.curve_r.points, CurveKind::Tone, Color32::from_rgb(240, 80, 80)),
        CurveTab::Green => (&mut n.curve_g.points, CurveKind::Tone, Color32::from_rgb(80, 220, 100)),
        CurveTab::Blue => (&mut n.curve_b.points, CurveKind::Tone, Color32::from_rgb(90, 140, 255)),
        CurveTab::HueHue => (&mut n.hue_vs_hue.points, CurveKind::Hue { range: 0.25 }, Color32::from_gray(230)),
        CurveTab::HueSat => (&mut n.hue_vs_sat.points, CurveKind::Hue { range: 1.0 }, Color32::from_gray(230)),
        CurveTab::HueLum => (&mut n.hue_vs_lum.points, CurveKind::Hue { range: 1.0 }, Color32::from_gray(230)),
    };
    curve_editor(ui, salt, pts, kind, col);
    ui.horizontal(|ui| {
        if ui.small_button("Reset Curve").clicked() {
            pts.clear();
        }
        ui.label(egui::RichText::new("Drag to add / move points · right-click a point to remove · double-click to reset").small().color(theme::TEXT_DIM));
    });
}

#[derive(Clone, Copy, PartialEq)]
pub enum CurveKind {
    /// 0..1 → 0..1 with a diagonal identity.
    Tone,
    /// Periodic over hue; values are offsets in ±`range`.
    Hue { range: f32 },
}

fn hue_rgb(h: f32) -> Color32 {
    let f = |n: f32| {
        let k = (n + h * 6.0) % 6.0;
        let v = 1.0 - (k.min(4.0 - k).clamp(0.0, 1.0));
        (v * 220.0 + 20.0) as u8
    };
    Color32::from_rgb(f(5.0), f(3.0), f(1.0))
}

/// Interactive spline editor over `points` (see [`Curve`] / [`HueCurve`]).
pub fn curve_editor(ui: &mut egui::Ui, salt: impl std::hash::Hash, points: &mut Vec<[f32; 2]>, kind: CurveKind, color: Color32) -> bool {
    let w = ui.available_width().clamp(160.0, 340.0);
    let h = match kind {
        CurveKind::Tone => w * 0.72,
        CurveKind::Hue { .. } => w * 0.5,
    };
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(w, h), Sense::click_and_drag());
    let drag_id = ui.id().with(("curve-drag", salt));
    let p = ui.painter_at(rect.expand(5.0));
    p.rect_filled(rect, 3.0, Color32::from_rgb(0x15, 0x15, 0x18));
    for i in 1..4 {
        let f = i as f32 / 4.0;
        let grid = Stroke::new(0.5_f32, theme::BORDER);
        p.line_segment([Pos2::new(rect.left() + rect.width() * f, rect.top()), Pos2::new(rect.left() + rect.width() * f, rect.bottom())], grid);
        p.line_segment([Pos2::new(rect.left(), rect.top() + rect.height() * f), Pos2::new(rect.right(), rect.top() + rect.height() * f)], grid);
    }
    let (ylo, yhi) = match kind {
        CurveKind::Tone => (0.0, 1.0),
        CurveKind::Hue { range } => (-range, range),
    };
    let to_s = |q: [f32; 2]| Pos2::new(rect.left() + q[0] * rect.width(), rect.bottom() - (q[1] - ylo) / (yhi - ylo) * rect.height());
    let from_s = |s: Pos2| [((s.x - rect.left()) / rect.width()).clamp(0.0, 1.0), (ylo + (rect.bottom() - s.y) / rect.height() * (yhi - ylo)).clamp(ylo, yhi)];
    match kind {
        CurveKind::Tone => {
            p.line_segment([rect.left_bottom(), rect.right_top()], Stroke::new(0.7_f32, Color32::from_gray(70)));
        }
        CurveKind::Hue { .. } => {
            let n = 48;
            for i in 0..n {
                let x0 = rect.left() + rect.width() * i as f32 / n as f32;
                let x1 = rect.left() + rect.width() * (i + 1) as f32 / n as f32;
                p.rect_filled(Rect::from_min_max(Pos2::new(x0, rect.bottom() - 7.0), Pos2::new(x1 + 0.5, rect.bottom())), 0.0, hue_rgb((i as f32 + 0.5) / n as f32));
            }
            p.line_segment([to_s([0.0, 0.0]), to_s([1.0, 0.0])], Stroke::new(0.7_f32, Color32::from_gray(90)));
        }
    }
    let eval = |pts: &[[f32; 2]], x: f32| match kind {
        CurveKind::Tone => Curve { points: pts.to_vec() }.eval(x),
        CurveKind::Hue { range } => HueCurve { points: pts.to_vec() }.eval(x).clamp(-range, range),
    };
    let line: Vec<Pos2> = (0..=96).map(|i| i as f32 / 96.0).map(|x| to_s([x, eval(points, x).clamp(ylo, yhi)])).collect();
    p.add(egui::Shape::line(line, Stroke::new(1.6_f32, color)));
    for q in points.iter() {
        p.circle_filled(to_s(*q), 4.0, color);
        p.circle_stroke(to_s(*q), 4.0, Stroke::new(1.0_f32, Color32::BLACK));
    }

    let near =
        |pos: Pos2, pts: &[[f32; 2]]| pts.iter().enumerate().map(|(i, q)| (i, to_s(*q).distance(pos))).filter(|(_, d)| *d < 9.0).min_by(|a, b| a.1.total_cmp(&b.1)).map(|(i, _)| i);
    let mut changed = false;
    if resp.drag_started() {
        if let Some(pos) = resp.interact_pointer_pos() {
            let idx = match near(pos, points) {
                Some(i) => i,
                None => {
                    if kind == CurveKind::Tone && points.is_empty() {
                        *points = vec![[0.0, 0.0], [1.0, 1.0]];
                    }
                    points.push(from_s(pos));
                    changed = true;
                    points.len() - 1
                }
            };
            ui.data_mut(|d| d.insert_temp(drag_id, idx));
        }
    }
    if resp.dragged() {
        if let (Some(idx), Some(pos)) = (ui.data(|d| d.get_temp::<usize>(drag_id)), resp.interact_pointer_pos()) {
            if let Some(q) = points.get_mut(idx) {
                *q = from_s(pos);
                changed = true;
            }
        }
    }
    if resp.drag_stopped() {
        ui.data_mut(|d| d.remove::<usize>(drag_id));
    }
    if resp.secondary_clicked() {
        if let Some(i) = resp.interact_pointer_pos().and_then(|pos| near(pos, points)) {
            points.remove(i);
            if kind == CurveKind::Tone && points.len() == 2 && points.contains(&[0.0, 0.0]) && points.contains(&[1.0, 1.0]) {
                points.clear();
            }
            changed = true;
        }
    }
    if resp.double_clicked() {
        points.clear();
        changed = true;
    }
    if let Some(pos) = resp.hover_pos() {
        let q = from_s(pos);
        let text = match kind {
            CurveKind::Tone => format!("in {:.2} → out {:.2}", q[0], eval(points, q[0])),
            CurveKind::Hue { .. } => format!("hue {:.0}° · {:+.2}", q[0] * 360.0, eval(points, q[0])),
        };
        p.text(rect.left_top() + Vec2::new(4.0, 2.0), egui::Align2::LEFT_TOP, text, egui::FontId::proportional(10.0), theme::TEXT_DIM);
    }
    changed
}

fn qualifier(ui: &mut egui::Ui, n: &mut ColorCorrection, view: &mut ColorView) {
    let mut on = n.qualifier.is_some();
    ui.horizontal(|ui| {
        if ui.checkbox(&mut on, "HSL Qualifier").changed() {
            n.qualifier = on.then(Qualifier::default);
            if !on && view.highlight.is_some_and(|h| h.0 == n.id) && n.mask.is_none() {
                view.highlight = None;
            }
        }
    });
    if let Some(q) = &mut n.qualifier {
        ui.horizontal(|ui| {
            ui.checkbox(&mut q.use_hue, "");
            label(ui, "Hue");
            let (r, _) = ui.allocate_exact_size(Vec2::splat(14.0), Sense::hover());
            ui.painter().rect_filled(r, 3.0, hue_rgb(q.hue_center));
        });
        ui.add_enabled_ui(q.use_hue, |ui| {
            drag(ui, "  Centre", &mut q.hue_center, 0.0..=1.0, 0.002, 0.07);
            drag(ui, "  Width", &mut q.hue_width, 0.0..=1.0, 0.002, 0.1);
            drag(ui, "  Softness", &mut q.hue_soft, 0.0..=0.5, 0.002, 0.05);
        });
        ui.checkbox(&mut q.use_sat, "Saturation");
        ui.add_enabled_ui(q.use_sat, |ui| {
            drag(ui, "  Low", &mut q.sat_low, 0.0..=1.0, 0.005, 0.1);
            drag(ui, "  High", &mut q.sat_high, 0.0..=1.0, 0.005, 1.0);
            drag(ui, "  Softness", &mut q.sat_soft, 0.0..=0.5, 0.002, 0.05);
        });
        ui.checkbox(&mut q.use_lum, "Luminance");
        ui.add_enabled_ui(q.use_lum, |ui| {
            drag(ui, "  Low", &mut q.lum_low, 0.0..=1.0, 0.005, 0.05);
            drag(ui, "  High", &mut q.lum_high, 0.0..=1.0, 0.005, 0.95);
            drag(ui, "  Softness", &mut q.lum_soft, 0.0..=0.5, 0.002, 0.05);
        });
        drag(ui, "Denoise", &mut q.denoise, 0.0..=1.0, 0.01, 0.0);
        ui.checkbox(&mut q.invert, "Invert key");
    }
    if n.qualifier.is_some() || n.mask.is_some() {
        ui.horizontal(|ui| {
            label(ui, "Highlight");
            let cur = view.highlight.filter(|h| h.0 == n.id);
            if ui.selectable_label(cur.is_none(), "Off").clicked() {
                view.highlight = None;
            }
            if ui.selectable_label(cur == Some((n.id, false)), "Colour").on_hover_text("Keyed pixels in colour over grey").clicked() {
                view.highlight = Some((n.id, false));
            }
            if ui.selectable_label(cur == Some((n.id, true)), "Matte").on_hover_text("Black / white key").clicked() {
                view.highlight = Some((n.id, true));
            }
        });
    }
}

/// Shape / position / feather of an optional mask (correction window or effect mask).
/// `t` is clip-local time (for rotoscoping keys), `tol` half a frame.
pub fn mask_editor(ui: &mut egui::Ui, mask: &mut Option<ShapeMask>, me: MaskFocus, focus: &mut Option<MaskFocus>, t: Time, tol: Time) {
    ui.horizontal(|ui| {
        label(ui, "Shape");
        let text = mask.as_ref().map(|m| m.shape.label()).unwrap_or("None");
        egui::ComboBox::from_id_salt(("mask-shape", me_key(me))).selected_text(text).show_ui(ui, |ui| {
            if ui.selectable_label(mask.is_none(), "None").clicked() {
                *mask = None;
                if *focus == Some(me) {
                    *focus = None;
                }
            }
            for s in MaskShape::ALL {
                if ui.selectable_label(mask.as_ref().is_some_and(|m| m.shape == s), s.label()).clicked() {
                    match mask {
                        Some(m) => {
                            m.shape = s;
                            if s == MaskShape::Polygon && m.points.len() < 3 {
                                m.points = ShapeMask::new(MaskShape::Polygon).points;
                            }
                        }
                        None => *mask = Some(ShapeMask::new(s)),
                    }
                }
            }
        });
        if mask.is_some() {
            let mut editing = *focus == Some(me);
            if ui.toggle_value(&mut editing, "✎ Edit in Viewer").changed() {
                *focus = editing.then_some(me);
            }
        }
    });
    let Some(m) = mask else { return };
    if m.shape != MaskShape::Polygon {
        drag(ui, "Centre X", &mut m.center[0], -0.5..=1.5, 0.002, 0.5);
        drag(ui, "Centre Y", &mut m.center[1], -0.5..=1.5, 0.002, 0.5);
        drag(ui, "Half Width", &mut m.size[0], 0.005..=2.0, 0.002, 0.3);
        drag(ui, "Half Height", &mut m.size[1], 0.005..=2.0, 0.002, 0.3);
        drag(ui, "Rotation", &mut m.rotation, -180.0..=180.0, 0.5, 0.0);
    } else {
        ui.horizontal(|ui| {
            label(ui, "Points");
            ui.label(format!("{}", m.points.len()));
            let mut bez = m.is_bezier();
            if !bez {
                ui.checkbox(&mut m.smooth, "Smooth");
            }
            if ui.toggle_value(&mut bez, "Bézier handles").on_hover_text("Per-point tangents you drag in the viewer").changed() {
                if bez {
                    m.convert_to_bezier();
                } else {
                    m.clear_bezier();
                }
            }
        });
        ui.horizontal(|ui| {
            label(ui, "Roto");
            let near = m.shape_key_near(t, tol);
            let (txt, tip) = if near.is_some() {
                ("◆ Update Key", "Store the shape at the playhead")
            } else {
                ("◇ Key Shape", "Store the shape at the playhead; with keys, every viewer edit keys the shape")
            };
            if ui.small_button(txt).on_hover_text(tip).clicked() {
                if let Some((p, h)) = m.shape_at(t) {
                    m.points = p;
                    m.handles = h;
                }
                m.set_shape_key(t, tol);
            }
            if near.is_some() && ui.small_button("Remove").clicked() {
                m.remove_shape_key_near(t, tol);
            }
            if !m.shape_keys.is_empty() {
                ui.label(egui::RichText::new(format!("{} keys", m.shape_keys.len())).small().color(theme::TEXT_DIM));
                if ui.small_button("Clear").on_hover_text("Keep the shape at the playhead, drop the keys").clicked() {
                    *m = m.at(t);
                }
            }
        });
        ui.label(
            egui::RichText::new("In the viewer: drag points, drag inside to move, click an edge to add a point, right-click to remove. Bézier: drag the round tangent handles (Alt breaks the pair).")
                .small()
                .color(theme::TEXT_DIM),
        );
    }
    drag(ui, "Feather", &mut m.feather, 0.0..=0.5, 0.002, 0.05);
    ui.checkbox(&mut m.invert, "Invert");
}

fn me_key(m: MaskFocus) -> (u8, u64) {
    match m {
        MaskFocus::Node(i) => (0, i),
        MaskFocus::Effect(i) => (1, i),
        MaskFocus::Clip(i) => (2, i as u64),
    }
}

// ---------------------------------------------------------------------------
// viewer gizmo

fn mask_of(clip: &mut nle_core::Clip, f: MaskFocus) -> Option<&mut ShapeMask> {
    match f {
        MaskFocus::Node(id) => clip.color.corrections.iter_mut().find(|n| n.id == id)?.mask.as_mut(),
        MaskFocus::Effect(id) => clip.effects.iter_mut().find(|e| e.id == id)?.mask.as_mut(),
        MaskFocus::Clip(i) => clip.video.masks.get_mut(i).map(|m| &mut m.shape),
    }
}

/// Interactive viewer gizmo for a clip-level `VideoMask` (handles offset,
/// scale/size, rotation, feather, and transformed polygon / Bézier points).
#[allow(clippy::too_many_arguments)]
fn clip_mask_gizmo(app: &mut App, ui: &mut egui::Ui, img: Rect, frame: (f32, f32), id: nle_core::ClipId, clip: &nle_core::Clip, local: Time, tol: Time, idx: usize) -> bool {
    use nle_core::mask::{polygon_pivot, transform_point, untransform_point};
    let mut edited = clip.clone();
    let Some(vm) = edited.video.masks.get_mut(idx) else {
        app.color_view.mask_focus = None;
        return false;
    };
    let roto = !vm.shape.shape_keys.is_empty();
    if let Some((p, h)) = vm.shape.shape_at(local) {
        vm.shape.points = p;
        vm.shape.handles = h;
    }
    let off = [vm.offset[0].at_f32(local), vm.offset[1].at_f32(local)];
    let sc = [vm.scale[0].at_f32(local), vm.scale[1].at_f32(local)];
    let rot = vm.rotation.at_f32(local);
    let feather = vm.feather.at_f32(local).max(0.0);
    let expand = vm.expand.at_f32(local);
    let aspect = frame.0 / frame.1;
    let hpx = img.height();
    let to_s = |uv: [f32; 2]| Pos2::new(img.left() + uv[0] * img.width(), img.top() + uv[1] * img.height());
    let from_s = |p: Pos2| [(p.x - img.left()) / img.width(), (p.y - img.top()) / img.height()];
    let painter = ui.painter().with_clip_rect(img.expand(40.0));
    let col = theme::SNAP;
    let mut changed = false;

    match vm.shape.shape {
        MaskShape::Ellipse | MaskShape::Rectangle => {
            let ctr_uv = [vm.shape.center[0] + off[0], vm.shape.center[1] + off[1]];
            let hs = [(vm.shape.size[0] * sc[0].abs()).max(0.005), (vm.shape.size[1] * sc[1].abs()).max(0.005)];
            let es = [(hs[0] + expand).max(0.002), (hs[1] + expand).max(0.002)];
            let tot_rot = vm.shape.rotation + rot;
            let (s, c) = tot_rot.to_radians().sin_cos();
            let to_pt = |l: [f32; 2]| {
                let q = [l[0] * c - l[1] * s, l[0] * s + l[1] * c];
                let ct = to_s(ctr_uv);
                Pos2::new(ct.x + q[0] * hpx, ct.y + q[1] * hpx)
            };
            let unrot = |p: Pos2| {
                let ct = to_s(ctr_uv);
                let d = [(p.x - ct.x) / hpx, (p.y - ct.y) / hpx];
                [d[0] * c + d[1] * s, -d[0] * s + d[1] * c]
            };
            let make_loop = |rx: f32, ry: f32| -> Vec<Pos2> {
                if vm.shape.shape == MaskShape::Ellipse {
                    (0..64).map(|i| i as f32 / 64.0 * std::f32::consts::TAU).map(|a| to_pt([rx * a.cos(), ry * a.sin()])).collect()
                } else {
                    [[-1.0, -1.0], [1.0, -1.0], [1.0, 1.0], [-1.0, 1.0]].iter().map(|k| to_pt([rx * k[0], ry * k[1]])).collect()
                }
            };
            painter.add(egui::Shape::closed_line(make_loop(es[0], es[1]), Stroke::new(1.5_f32, col)));
            if feather > 0.0 {
                for d in [-feather * 0.5, feather * 0.5] {
                    painter.add(egui::Shape::closed_line(make_loop((es[0] + d).max(0.001), (es[1] + d).max(0.001)), Stroke::new(0.8_f32, col.gamma_multiply(0.5))));
                }
            }
            let ctr = to_s(ctr_uv);
            painter.circle_stroke(ctr, 6.0, Stroke::new(1.5_f32, col));
            let hw = to_pt([es[0], 0.0]);
            let hh = to_pt([0.0, es[1]]);
            let hr = to_pt([0.0, -es[1] - 24.0 / hpx]);
            let hf_edge = to_pt([-es[0], 0.0]);
            let hf = to_pt([-es[0] - feather * 0.5 - 10.0 / hpx, 0.0]);
            painter.line_segment([to_pt([0.0, -es[1]]), hr], Stroke::new(1.0_f32, col));
            painter.line_segment([hf_edge, hf], Stroke::new(1.0_f32, theme::VIDEO));
            for h in [hw, hh] {
                painter.rect_filled(Rect::from_center_size(h, Vec2::splat(10.0)), 2.0, Color32::WHITE);
            }
            painter.circle_filled(hr, 5.0, col);
            painter.circle_filled(hf, 5.0, theme::VIDEO);

            let it = |name: &str, at: Pos2| ui.interact(Rect::from_center_size(at, Vec2::splat(16.0)), ui.id().with(("cmask-gz", idx, name)), Sense::drag());
            let (rw, rh, rr, rc, rf) = (it("w", hw), it("h", hh), it("r", hr), it("c", ctr), it("f", hf).on_hover_text("Drag to adjust mask feather"));
            if let Some(p) = rw.interact_pointer_pos().filter(|_| rw.dragged()) {
                let want = (unrot(p)[0].abs() - expand).max(0.005);
                if vm.scale[0].is_animated() {
                    vm.scale[0].set(local, (want / vm.shape.size[0].max(0.005)).clamp(0.01, 20.0) as f64);
                } else {
                    vm.shape.size[0] = (want / sc[0].abs().max(1e-3)).clamp(0.005, 2.0);
                }
                changed = true;
            }
            if let Some(p) = rh.interact_pointer_pos().filter(|_| rh.dragged()) {
                let want = (unrot(p)[1].abs() - expand).max(0.005);
                if vm.scale[1].is_animated() {
                    vm.scale[1].set(local, (want / vm.shape.size[1].max(0.005)).clamp(0.01, 20.0) as f64);
                } else {
                    vm.shape.size[1] = (want / sc[1].abs().max(1e-3)).clamp(0.005, 2.0);
                }
                changed = true;
            }
            if let Some(p) = rr.interact_pointer_pos().filter(|_| rr.dragged()) {
                let d = p - ctr;
                let deg = d.x.atan2(-d.y).to_degrees() - vm.shape.rotation;
                vm.rotation.set(local, deg as f64);
                changed = true;
            }
            if rc.dragged() {
                let d = rc.drag_delta();
                vm.offset[0].set(local, (off[0] + d.x / img.width()) as f64);
                vm.offset[1].set(local, (off[1] + d.y / img.height()) as f64);
                changed = true;
            }
            if let Some(p) = rf.interact_pointer_pos().filter(|_| rf.dragged()) {
                let dist = (-unrot(p)[0] - es[0] - 10.0 / hpx).max(0.0);
                vm.feather.set(local, (dist * 2.0).clamp(0.0, 0.5) as f64);
                changed = true;
            }
            for r in [&rw, &rh, &rf] {
                if r.hovered() {
                    ui.ctx().set_cursor_icon(egui::CursorIcon::Crosshair);
                }
            }
        }
        MaskShape::Polygon => {
            let pivot = polygon_pivot(&vm.shape.points);
            let fwd = |q: [f32; 2]| transform_point(q, pivot, off, sc, rot, aspect);
            let inv = |q: [f32; 2]| untransform_point(q, pivot, off, sc, rot, aspect);
            let outline: Vec<Pos2> = vm.shape.outline().into_iter().map(|q| to_s(fwd(q))).collect();
            painter.add(egui::Shape::closed_line(outline, Stroke::new(1.5_f32, col)));
            let body = ui.interact(img, ui.id().with(("cmask-body", idx)), Sense::click_and_drag());
            let mut remove = None;
            if vm.shape.is_bezier() {
                let alt = ui.input(|i| i.modifiers.alt);
                for i in 0..vm.shape.points.len() {
                    let base_p = vm.shape.points[i];
                    let at = to_s(fwd(base_p));
                    for side in [0usize, 2] {
                        let h = vm.shape.handles[i];
                        let tip_uv = fwd([base_p[0] + h[side], base_p[1] + h[side + 1]]);
                        let raw = to_s(tip_uv);
                        let zero = raw.distance(at) < 0.5;
                        if zero && side == 0 {
                            continue;
                        }
                        let hp = if zero { at + Vec2::new(14.0, 0.0) } else { raw };
                        painter.line_segment([at, hp], Stroke::new(1.0_f32, col.gamma_multiply(0.7)));
                        let r = ui.interact(Rect::from_center_size(hp, Vec2::splat(12.0)), ui.id().with(("cmask-tan", idx, i, side)), Sense::drag());
                        painter.circle_filled(hp, 3.5, if r.hovered() || r.dragged() { col } else { Color32::WHITE });
                        if let Some(p) = r.interact_pointer_pos().filter(|_| r.dragged()) {
                            let bp = inv(from_s(p));
                            let d = [bp[0] - base_p[0], bp[1] - base_p[1]];
                            vm.shape.handles[i][side] = d[0];
                            vm.shape.handles[i][side + 1] = d[1];
                            if !alt {
                                let o = 2 - side;
                                vm.shape.handles[i][o] = -d[0];
                                vm.shape.handles[i][o + 1] = -d[1];
                            }
                            changed = true;
                        }
                    }
                }
            }
            for i in 0..vm.shape.points.len() {
                let at = to_s(fwd(vm.shape.points[i]));
                let r = ui.interact(Rect::from_center_size(at, Vec2::splat(14.0)), ui.id().with(("cmask-pt", idx, i)), Sense::click_and_drag());
                painter.rect_filled(Rect::from_center_size(at, Vec2::splat(8.0)), 2.0, if r.hovered() { col } else { Color32::WHITE });
                if let Some(pos) = r.interact_pointer_pos().filter(|_| r.dragged()) {
                    vm.shape.points[i] = inv(from_s(pos));
                    changed = true;
                }
                if r.secondary_clicked() && vm.shape.points.len() > 3 {
                    remove = Some(i);
                }
            }
            if let Some(i) = remove {
                vm.shape.remove_point(i);
                changed = true;
            }
            if body.clicked() {
                if let Some(pos) = body.interact_pointer_pos() {
                    let n = vm.shape.points.len();
                    let best = (0..n)
                        .map(|i| {
                            let (a, b) = (to_s(fwd(vm.shape.points[i])), to_s(fwd(vm.shape.points[(i + 1) % n])));
                            let ab = b - a;
                            let t = ((pos - a).dot(ab) / ab.length_sq().max(1e-6)).clamp(0.0, 1.0);
                            (i, (a + ab * t).distance(pos))
                        })
                        .min_by(|a, b| a.1.total_cmp(&b.1));
                    if let Some((i, d)) = best {
                        if d < 10.0 && n < nle_core::color::MAX_MASK_POINTS {
                            vm.shape.insert_point(i + 1, inv(from_s(pos)));
                            changed = true;
                        }
                    }
                }
            }
            let resolved = vm.resolve(local).for_aspect(aspect);
            let prev_pt = body.interact_pointer_pos().unwrap_or_default() - body.drag_delta();
            if body.dragged() && resolved.distance(from_s(prev_pt), aspect) - resolved.expand <= 0.0 {
                let d = body.drag_delta();
                vm.offset[0].set(local, (off[0] + d.x / img.width()) as f64);
                vm.offset[1].set(local, (off[1] + d.y / img.height()) as f64);
                changed = true;
            }
        }
    }
    painter.text(
        img.left_bottom() + Vec2::new(6.0, -6.0),
        egui::Align2::LEFT_BOTTOM,
        format!("Clip Mask {} · drag handles / feather · Esc to finish", idx + 1),
        egui::FontId::proportional(11.0),
        col,
    );
    if ui.input(|i| i.key_pressed(egui::Key::Escape)) {
        app.color_view.mask_focus = None;
    }
    if changed {
        if roto {
            if let Some(vm) = edited.video.masks.get_mut(idx) {
                vm.shape.set_shape_key(local, tol);
            }
        }
        app.exec_key(Command::UpdateClip { clip: Box::new(edited) }, Some(drag_key(MASK_GIZMO_TAG, id)));
    }
    true
}

/// Draws and edits the focused mask of the selected clip over the viewer
/// image. Returns false when there is nothing to edit (the normal gizmo runs).
/// Masks live in the clip's own frame (before its transform).
pub fn mask_gizmo(app: &mut App, ui: &mut egui::Ui, img: Rect, frame: (f32, f32)) -> bool {
    let Some(focus) = app.color_view.mask_focus else { return false };
    let Some(id) = app.selection.first().copied() else { return false };
    let Some(clip) = app.project.timeline.clip(id) else { return false };
    if matches!(clip.kind, ClipKind::Gap) {
        return false;
    }
    let local = app.local_time(id);
    let tol = Time(app.project.timeline.frame().0 / 2);
    if let MaskFocus::Clip(idx) = focus {
        let clip = clip.clone();
        return clip_mask_gizmo(app, ui, img, frame, id, &clip, local, tol, idx);
    }
    let mut edited = clip.clone();
    // a keyframed window: edit it where it is at the playhead, then key it
    let keyed = match focus {
        MaskFocus::Node(nid) => edited.color.corrections.iter_mut().find(|n| n.id == nid && !n.keys.is_empty()).map(|n| {
            let base = NodeValues::of(n);
            n.values_at(local).apply(n);
            (nid, base)
        }),
        MaskFocus::Effect(_) | MaskFocus::Clip(_) => None,
    };
    let Some(m) = mask_of(&mut edited, focus) else {
        app.color_view.mask_focus = None;
        return false;
    };
    // a rotoscoped shape: edit it where it is at the playhead, then key it
    let roto = !m.shape_keys.is_empty();
    if let Some((p, h)) = m.shape_at(local) {
        m.points = p;
        m.handles = h;
    }
    let aspect = frame.0 / frame.1;
    let hpx = img.height();
    let to_s = |uv: [f32; 2]| Pos2::new(img.left() + uv[0] * img.width(), img.top() + uv[1] * img.height());
    let from_s = |p: Pos2| [(p.x - img.left()) / img.width(), (p.y - img.top()) / img.height()];
    let painter = ui.painter().with_clip_rect(img.expand(40.0));
    let col = theme::SNAP;
    let mut changed = false;

    match m.shape {
        MaskShape::Ellipse | MaskShape::Rectangle => {
            let (s, c) = m.rotation.to_radians().sin_cos();
            // local (height units) → screen
            let local = |l: [f32; 2], m: &ShapeMask| {
                let q = [l[0] * c - l[1] * s, l[0] * s + l[1] * c];
                let ctr = to_s(m.center);
                Pos2::new(ctr.x + q[0] * hpx, ctr.y + q[1] * hpx)
            };
            let outline: Vec<Pos2> = if m.shape == MaskShape::Ellipse {
                (0..64).map(|i| i as f32 / 64.0 * std::f32::consts::TAU).map(|a| local([m.size[0] * a.cos(), m.size[1] * a.sin()], m)).collect()
            } else {
                [[-1.0, -1.0], [1.0, -1.0], [1.0, 1.0], [-1.0, 1.0]].iter().map(|k| local([m.size[0] * k[0], m.size[1] * k[1]], m)).collect()
            };
            painter.add(egui::Shape::closed_line(outline, Stroke::new(1.5_f32, col)));
            if m.feather > 0.0 {
                let grow = |d: f32| {
                    let sx = (m.size[0] + d).max(0.001);
                    let sy = (m.size[1] + d).max(0.001);
                    (0..48).map(|i| i as f32 / 48.0 * std::f32::consts::TAU).map(|a| local([sx * a.cos(), sy * a.sin()], m)).collect::<Vec<_>>()
                };
                if m.shape == MaskShape::Ellipse {
                    for d in [-m.feather * 0.5, m.feather * 0.5] {
                        painter.add(egui::Shape::closed_line(grow(d), Stroke::new(0.8_f32, col.gamma_multiply(0.5))));
                    }
                }
            }
            let ctr = to_s(m.center);
            painter.circle_stroke(ctr, 6.0, Stroke::new(1.5_f32, col));
            // handles: width (right), height (bottom), rotation (above)
            let hw = local([m.size[0], 0.0], m);
            let hh = local([0.0, m.size[1]], m);
            let hr = local([0.0, -m.size[1] - 24.0 / hpx], m);
            painter.line_segment([local([0.0, -m.size[1]], m), hr], Stroke::new(1.0_f32, col));
            for (h, r) in [(hw, 5.0), (hh, 5.0)] {
                painter.rect_filled(Rect::from_center_size(h, Vec2::splat(r * 2.0)), 2.0, Color32::WHITE);
            }
            painter.circle_filled(hr, 5.0, col);
            let it = |name: &str, at: Pos2| ui.interact(Rect::from_center_size(at, Vec2::splat(16.0)), ui.id().with(("mask-gz", name)), Sense::drag());
            let (rw, rh, rr, rc) = (it("w", hw), it("h", hh), it("r", hr), it("c", ctr));
            let unrot = |p: Pos2, m: &ShapeMask| {
                let ctr = to_s(m.center);
                let d = [(p.x - ctr.x) / hpx, (p.y - ctr.y) / hpx];
                [d[0] * c + d[1] * s, -d[0] * s + d[1] * c]
            };
            if let Some(p) = rw.interact_pointer_pos().filter(|_| rw.dragged()) {
                m.size[0] = unrot(p, m)[0].abs().max(0.005);
                changed = true;
            }
            if let Some(p) = rh.interact_pointer_pos().filter(|_| rh.dragged()) {
                m.size[1] = unrot(p, m)[1].abs().max(0.005);
                changed = true;
            }
            if let Some(p) = rr.interact_pointer_pos().filter(|_| rr.dragged()) {
                let d = p - to_s(m.center);
                m.rotation = (d.x.atan2(-d.y)).to_degrees();
                changed = true;
            }
            if rc.dragged() {
                let d = rc.drag_delta();
                m.center[0] += d.x / img.width();
                m.center[1] += d.y / img.height();
                changed = true;
            }
            for r in [&rw, &rh] {
                if r.hovered() {
                    ui.ctx().set_cursor_icon(egui::CursorIcon::Crosshair);
                }
            }
        }
        MaskShape::Polygon => {
            let outline: Vec<Pos2> = m.outline().into_iter().map(to_s).collect();
            painter.add(egui::Shape::closed_line(outline, Stroke::new(1.5_f32, col)));
            // registered first so the point handles (added later) sit on top
            let body = ui.interact(img, ui.id().with("mask-body"), Sense::click_and_drag());
            let mut remove = None;
            if m.is_bezier() {
                let alt = ui.input(|i| i.modifiers.alt);
                for i in 0..m.points.len() {
                    let at = to_s(m.points[i]);
                    for side in [0usize, 2] {
                        let h = m.handles[i];
                        let raw = Pos2::new(at.x + h[side] * img.width(), at.y + h[side + 1] * img.height());
                        let zero = raw.distance(at) < 0.5;
                        // a sharp corner offers only its out handle, a little to the right
                        if zero && side == 0 {
                            continue;
                        }
                        let hp = if zero { at + Vec2::new(14.0, 0.0) } else { raw };
                        painter.line_segment([at, hp], Stroke::new(1.0_f32, col.gamma_multiply(0.7)));
                        let r = ui.interact(Rect::from_center_size(hp, Vec2::splat(12.0)), ui.id().with(("mask-tan", i, side)), Sense::drag());
                        painter.circle_filled(hp, 3.5, if r.hovered() || r.dragged() { col } else { Color32::WHITE });
                        if let Some(p) = r.interact_pointer_pos().filter(|_| r.dragged()) {
                            let d = [(p.x - at.x) / img.width(), (p.y - at.y) / img.height()];
                            m.handles[i][side] = d[0];
                            m.handles[i][side + 1] = d[1];
                            if !alt {
                                // keep the pair opposite (smooth point)
                                let o = 2 - side;
                                m.handles[i][o] = -d[0];
                                m.handles[i][o + 1] = -d[1];
                            }
                            changed = true;
                        }
                    }
                }
            }
            for i in 0..m.points.len() {
                let at = to_s(m.points[i]);
                let r = ui.interact(Rect::from_center_size(at, Vec2::splat(14.0)), ui.id().with(("mask-pt", i)), Sense::click_and_drag());
                painter.rect_filled(Rect::from_center_size(at, Vec2::splat(8.0)), 2.0, if r.hovered() { col } else { Color32::WHITE });
                if r.dragged() {
                    let d = r.drag_delta();
                    m.points[i][0] += d.x / img.width();
                    m.points[i][1] += d.y / img.height();
                    changed = true;
                }
                if r.secondary_clicked() && m.points.len() > 3 {
                    remove = Some(i);
                }
            }
            if let Some(i) = remove {
                m.remove_point(i);
                changed = true;
            }
            // click near an edge to insert a point; drag inside to move the shape
            if body.clicked() {
                if let Some(pos) = body.interact_pointer_pos() {
                    let n = m.points.len();
                    let best = (0..n)
                        .map(|i| {
                            let (a, b) = (to_s(m.points[i]), to_s(m.points[(i + 1) % n]));
                            let ab = b - a;
                            let t = ((pos - a).dot(ab) / ab.length_sq().max(1e-6)).clamp(0.0, 1.0);
                            (i, (a + ab * t).distance(pos))
                        })
                        .min_by(|a, b| a.1.total_cmp(&b.1));
                    if let Some((i, d)) = best {
                        if d < 10.0 && n < nle_core::color::MAX_MASK_POINTS {
                            m.insert_point(i + 1, from_s(pos));
                            changed = true;
                        }
                    }
                }
            }
            let inside = |m: &ShapeMask, p: Pos2| ShapeMask { invert: false, feather: 0.0, ..m.clone() }.alpha(from_s(p), aspect) > 0.5;
            if body.dragged() && inside(m, body.interact_pointer_pos().unwrap_or_default() - body.drag_delta()) {
                let d = body.drag_delta();
                for q in &mut m.points {
                    q[0] += d.x / img.width();
                    q[1] += d.y / img.height();
                }
                changed = true;
            }
        }
    }
    painter.text(img.left_bottom() + Vec2::new(6.0, -6.0), egui::Align2::LEFT_BOTTOM, "Mask edit · Esc to finish", egui::FontId::proportional(11.0), col);
    if ui.input(|i| i.key_pressed(egui::Key::Escape)) {
        app.color_view.mask_focus = None;
    }
    if changed {
        if roto {
            if let Some(m) = mask_of(&mut edited, focus) {
                m.set_shape_key(local, tol);
            }
        }
        if let Some((nid, base)) = keyed {
            if let Some(n) = edited.color.corrections.iter_mut().find(|n| n.id == nid) {
                n.set_key(local, tol);
                base.apply(n);
            }
        }
        app.exec_key(Command::UpdateClip { clip: Box::new(edited) }, Some(drag_key(MASK_GIZMO_TAG, id)));
    }
    true
}
