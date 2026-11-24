//! Graph editor: the keyframe curves of the target clip's parameters, with
//! draggable keys and Bézier tangent handles (View ▸ Graph Editor,
//! Ctrl+Shift+G).
//!
//! * drag a key to move it in time and value (Shift: time only, Ctrl: value only)
//! * select a key to show its tangent handles; drag them to shape the curve
//!   (Alt breaks the pair)
//! * double-click a curve to add a key; right-click a key for
//!   interpolation, smoothing and delete
//! * click the background to move the playhead

use crate::app::{drag_key, App};
use crate::theme;
use eframe::egui::{self, Color32, Pos2, Rect, Sense, Shape, Stroke, Vec2};
use nle_core::keyframe::{Curve, Interpolation};
use nle_core::{Clip, Command, Time};
use std::collections::{BTreeSet, HashMap};

const GRAPH_TAG: u64 = 0x67;

const PALETTE: [Color32; 6] = [
    Color32::from_rgb(0xFF, 0x9F, 0x0A),
    Color32::from_rgb(0x64, 0xD2, 0xFF),
    Color32::from_rgb(0x30, 0xD1, 0x58),
    Color32::from_rgb(0xFF, 0x6B, 0x8B),
    Color32::from_rgb(0xBF, 0x5A, 0xF2),
    Color32::from_rgb(0xFF, 0xD6, 0x0A),
];

#[derive(Default)]
pub struct GraphState {
    pub open: bool,
    /// Parameter names drawn.
    shown: BTreeSet<String>,
    selected: Option<(String, usize)>,
    /// Scale each curve to its own range instead of a shared one.
    normalize: bool,
    /// Value ranges frozen while a key or handle is dragged (the view
    /// would otherwise rescale under the pointer).
    lock: Option<HashMap<String, (f64, f64)>>,
}

enum Act {
    Interp(String, usize, Interpolation),
    Smooth(String, usize),
    Flatten(String, usize),
    Delete(String, usize),
    Add(String, Time),
}

pub fn show(app: &mut App, ctx: &egui::Context) {
    if !app.graph.open {
        return;
    }
    let mut open = true;
    egui::Window::new("Graph Editor").open(&mut open).default_size([860.0, 380.0]).resizable(true).show(ctx, |ui| body(app, ui));
    if !open {
        app.graph.open = false;
    }
}

fn curve_of<'a>(c: &'a mut Clip, name: &str) -> Option<&'a mut Curve> {
    c.animatable_params_mut().into_iter().find(|(n, _)| n == name).and_then(|(_, p)| p.curve.as_mut())
}

fn padded((lo, hi): (f64, f64)) -> (f64, f64) {
    if !(hi - lo).is_finite() || hi - lo < 1e-9 {
        let m = if lo.is_finite() { lo } else { 0.0 };
        return (m - 1.0, m + 1.0);
    }
    let p = (hi - lo) * 0.12;
    (lo - p, hi + p)
}

/// A "nice" grid step near `raw` (1, 2 or 5 × 10^n).
fn nice_step(raw: f64) -> f64 {
    let e = 10f64.powf(raw.max(1e-12).log10().floor());
    let m = raw / e;
    e * if m < 1.5 {
        1.0
    } else if m < 3.5 {
        2.0
    } else if m < 7.5 {
        5.0
    } else {
        10.0
    }
}

fn body(app: &mut App, ui: &mut egui::Ui) {
    let Some(id) = app.target_clip() else {
        ui.label(egui::RichText::new("Select a clip to see its keyframe curves.").color(theme::TEXT_DIM));
        return;
    };
    let Some(clip) = app.project.timeline.clip(id) else { return };
    let dur = clip.duration.max(Time(1));
    let local = app.local_time(id);
    let start = app.project.timeline.clip_start(id).unwrap_or(Time::ZERO);
    let frame = app.project.timeline.frame();
    let mut edited = clip.clone();
    let animated: Vec<String> = edited.animatable_params().iter().filter(|(_, p)| p.curve.as_ref().is_some_and(|c| !c.is_empty())).map(|(n, _)| n.to_string()).collect();

    ui.horizontal(|ui| {
        ui.label(egui::RichText::new(&clip.name).strong());
        ui.label(egui::RichText::new(format!("{} animated parameters", animated.len())).small().color(theme::TEXT_DIM));
    });
    if animated.is_empty() {
        ui.label(egui::RichText::new("No keyframes on this clip yet. Add one with a ◆ button in the Inspector, then shape it here.").color(theme::TEXT_DIM));
        return;
    }
    let st = &mut app.graph;
    st.shown.retain(|n| animated.contains(n));
    if st.shown.is_empty() {
        st.shown.insert(animated[0].clone());
    }
    if st.selected.as_ref().is_some_and(|(n, i)| !st.shown.contains(n) || curve_of(&mut edited, n).is_none_or(|c| *i >= c.keys.len())) {
        st.selected = None;
    }

    let snap = |t: Time| Time(((t.0 as f64 / frame.0 as f64).round() as i64) * frame.0).clamp(Time::ZERO, dur);
    let mut changed = false;
    let mut acts: Vec<Act> = Vec::new();
    let mut seek_to = None;

    ui.horizontal_top(|ui| {
        // ----- parameter list
        ui.vertical(|ui| {
            ui.set_width(180.0);
            ui.horizontal(|ui| {
                if ui.small_button("All").clicked() {
                    st.shown = animated.iter().cloned().collect();
                }
                if ui.small_button("None").clicked() {
                    st.shown.clear();
                }
                ui.checkbox(&mut st.normalize, "Normalize").on_hover_text("Scale each curve to its own range");
            });
            egui::ScrollArea::vertical().id_salt("graph-params").max_height(ui.available_height().max(200.0)).show(ui, |ui| {
                let order: Vec<&String> = animated.iter().filter(|n| st.shown.contains(*n)).collect();
                for n in &animated {
                    let mut on = st.shown.contains(n);
                    ui.horizontal(|ui| {
                        let col = order.iter().position(|o| *o == n).map(|i| PALETTE[i % PALETTE.len()]).unwrap_or(theme::TEXT_DIM);
                        let (r, _) = ui.allocate_exact_size(Vec2::splat(10.0), Sense::hover());
                        ui.painter().circle_filled(r.center(), 4.0, col);
                        if ui.checkbox(&mut on, n.as_str()).changed() {
                            if on {
                                st.shown.insert(n.clone());
                            } else {
                                st.shown.remove(n);
                            }
                        }
                    });
                }
            });
        });
        ui.separator();

        // ----- plot
        let shown: Vec<String> = animated.iter().filter(|n| st.shown.contains(*n)).cloned().collect();
        let mut ranges: HashMap<String, (f64, f64)> = match &st.lock {
            Some(l) => l.clone(),
            None => shown.iter().filter_map(|n| Some((n.clone(), padded(curve_of(&mut edited, n)?.value_range()?)))).collect(),
        };
        if !st.normalize && st.lock.is_none() && !ranges.is_empty() {
            let lo = ranges.values().map(|r| r.0).fold(f64::INFINITY, f64::min);
            let hi = ranges.values().map(|r| r.1).fold(f64::NEG_INFINITY, f64::max);
            ranges.values_mut().for_each(|r| *r = (lo, hi));
        }
        let size = Vec2::new(ui.available_width().max(240.0), (ui.available_height() - 28.0).max(200.0));
        let (rect, bg) = ui.allocate_exact_size(size, Sense::click());
        let plot = rect.shrink2(Vec2::new(12.0, 14.0));
        let painter = ui.painter_at(rect);
        painter.rect_filled(rect, 4.0, Color32::from_rgb(0x16, 0x16, 0x1A));
        let to_x = |t: Time| plot.left() + (t.0 as f64 / dur.0 as f64) as f32 * plot.width();
        let from_x = |x: f32| Time((((x - plot.left()) / plot.width()) as f64 * dur.0 as f64) as i64);
        let to_y = |v: f64, r: (f64, f64)| plot.bottom() - ((v - r.0) / (r.1 - r.0)) as f32 * plot.height();
        let from_y = |y: f32, r: (f64, f64)| r.0 + ((plot.bottom() - y) / plot.height()) as f64 * (r.1 - r.0);

        // grid: seconds across, values up (a shared scale only)
        let secs = dur.as_secs_f64();
        let step = nice_step(secs / 8.0);
        let grid = Stroke::new(1.0_f32, Color32::from_rgb(0x26, 0x26, 0x2C));
        let font = egui::FontId::proportional(10.0);
        let mut s = 0.0;
        while s <= secs + 1e-9 {
            let x = to_x(Time::from_secs_f64(s));
            painter.line_segment([Pos2::new(x, rect.top()), Pos2::new(x, rect.bottom())], grid);
            painter.text(Pos2::new(x + 2.0, rect.bottom() - 2.0), egui::Align2::LEFT_BOTTOM, format!("{s:.2}s"), font.clone(), theme::TEXT_DIM);
            s += step;
        }
        if let Some(r) = ranges.values().next().copied().filter(|_| !st.normalize || shown.len() == 1) {
            let vstep = nice_step((r.1 - r.0) / 5.0);
            let mut v = (r.0 / vstep).ceil() * vstep;
            while v <= r.1 {
                let y = to_y(v, r);
                painter.line_segment([Pos2::new(rect.left(), y), Pos2::new(rect.right(), y)], grid);
                painter.text(
                    Pos2::new(rect.left() + 2.0, y - 1.0),
                    egui::Align2::LEFT_BOTTOM,
                    format!("{v:.3}").trim_end_matches('0').trim_end_matches('.').to_string(),
                    font.clone(),
                    theme::TEXT_DIM,
                );
                v += vstep;
            }
        }
        let px = to_x(local);
        painter.line_segment([Pos2::new(px, rect.top()), Pos2::new(px, rect.bottom())], Stroke::new(1.0_f32, theme::PLAYHEAD));

        let (alt, shift, ctrl) = ui.input(|i| (i.modifiers.alt, i.modifiers.shift, i.modifiers.command));
        let mut drag_started = false;
        for (ci, name) in shown.iter().enumerate() {
            let col = PALETTE[ci % PALETTE.len()];
            let Some(r) = ranges.get(name).copied() else { continue };
            let Some(curve) = curve_of(&mut edited, name) else { continue };
            let n = (plot.width() as usize / 3).clamp(32, 600);
            let pts: Vec<Pos2> = (0..=n)
                .map(|s| {
                    let t = Time(dur.0 * s as i64 / n as i64);
                    Pos2::new(to_x(t), to_y(curve.eval(t), r))
                })
                .collect();
            painter.add(Shape::line(pts, Stroke::new(1.6_f32, col)));
            for i in 0..curve.keys.len() {
                let k = curve.keys[i];
                let kp = Pos2::new(to_x(k.time), to_y(k.value, r));
                let sel = st.selected.as_ref().is_some_and(|(sn, si)| sn == name && *si == i);
                if sel {
                    let (inh, outh) = curve.handle_points(i);
                    for (side, h) in [(0, inh), (1, outh)] {
                        let Some((ht, hv)) = h else { continue };
                        let hp = Pos2::new(to_x(ht), to_y(hv, r));
                        painter.line_segment([kp, hp], Stroke::new(1.0_f32, col.gamma_multiply(0.7)));
                        let hr = ui.interact(Rect::from_center_size(hp, Vec2::splat(12.0)), ui.id().with(("graph-h", name, i, side)), Sense::drag());
                        painter.circle_filled(hp, 3.5, if hr.hovered() || hr.dragged() { col } else { Color32::WHITE });
                        drag_started |= hr.drag_started();
                        if let Some(p) = hr.interact_pointer_pos().filter(|_| hr.dragged()) {
                            let (t, v) = (from_x(p.x), from_y(p.y, r));
                            if side == 0 {
                                curve.set_in_handle(i, t, v, !alt);
                            } else {
                                curve.set_out_handle(i, t, v, !alt);
                            }
                            changed = true;
                        }
                    }
                }
                let kr = ui.interact(Rect::from_center_size(kp, Vec2::splat(12.0)), ui.id().with(("graph-k", name, i)), Sense::click_and_drag());
                let d = if sel || kr.hovered() { 5.5 } else { 4.5 };
                let diamond = vec![kp + Vec2::new(0.0, -d), kp + Vec2::new(d, 0.0), kp + Vec2::new(0.0, d), kp + Vec2::new(-d, 0.0)];
                painter.add(Shape::convex_polygon(
                    diamond,
                    if sel { col } else { Color32::from_rgb(0x20, 0x20, 0x24) },
                    Stroke::new(1.5_f32, if sel { Color32::WHITE } else { col }),
                ));
                if kr.clicked() || kr.drag_started() {
                    st.selected = Some((name.clone(), i));
                }
                drag_started |= kr.drag_started();
                if let Some(p) = kr.interact_pointer_pos().filter(|_| kr.dragged()) {
                    let t = if ctrl { k.time } else { snap(from_x(p.x)) };
                    let v = if shift { k.value } else { from_y(p.y, r) };
                    curve.move_key(i, t, v, frame);
                    changed = true;
                }
                kr.on_hover_text(format!("{name}\n{:.3} s  ·  {:.4}  ·  {}", k.time.as_secs_f64(), k.value, k.interp.label())).context_menu(|ui| {
                    ui.label(egui::RichText::new("Interpolation (to next key)").small().color(theme::TEXT_DIM));
                    for it in Interpolation::ALL {
                        if ui.selectable_label(k.interp == it, it.label()).clicked() {
                            acts.push(Act::Interp(name.clone(), i, it));
                            ui.close_menu();
                        }
                    }
                    ui.separator();
                    if ui.button("Auto Smooth").on_hover_text("Bézier tangents along the neighbours").clicked() {
                        acts.push(Act::Smooth(name.clone(), i));
                        ui.close_menu();
                    }
                    if ui.button("Flatten Tangents").on_hover_text("Ease in and out of this key").clicked() {
                        acts.push(Act::Flatten(name.clone(), i));
                        ui.close_menu();
                    }
                    ui.separator();
                    if ui.button("Delete Key").clicked() {
                        acts.push(Act::Delete(name.clone(), i));
                        ui.close_menu();
                    }
                });
            }
        }
        if bg.double_clicked() {
            if let Some(p) = bg.interact_pointer_pos() {
                let t = snap(from_x(p.x));
                // the curve nearest the pointer
                let near = shown
                    .iter()
                    .filter_map(|n| {
                        let r = ranges.get(n).copied()?;
                        let v = curve_of(&mut edited, n)?.eval(t);
                        Some((n.clone(), (to_y(v, r) - p.y).abs()))
                    })
                    .min_by(|a, b| a.1.total_cmp(&b.1));
                if let Some((n, _)) = near {
                    acts.push(Act::Add(n, t));
                }
            }
        } else if bg.clicked() {
            if let Some(p) = bg.interact_pointer_pos() {
                seek_to = Some(start + from_x(p.x).clamp(Time::ZERO, dur));
            }
        }
        if drag_started {
            st.lock = Some(ranges);
        } else if !ui.input(|i| i.pointer.any_down()) {
            st.lock = None;
        }
    });

    // ----- selected key
    let mut discrete = !acts.is_empty();
    if let Some((name, i)) = st.selected.clone() {
        if let Some(curve) = curve_of(&mut edited, &name) {
            let k = curve.keys[i];
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new(&name).strong());
                let mut secs = k.time.as_secs_f64();
                let mut v = k.value;
                ui.label("Time");
                let rt = ui.add(egui::DragValue::new(&mut secs).speed(frame.as_secs_f64()).suffix(" s").max_decimals(3));
                ui.label("Value");
                let rv = ui.add(egui::DragValue::new(&mut v).speed(0.01).max_decimals(4));
                if rt.changed() || rv.changed() {
                    curve.move_key(i, snap(Time::from_secs_f64(secs)), v, frame);
                    changed = true;
                }
                egui::ComboBox::from_id_salt("graph-interp").selected_text(k.interp.label()).show_ui(ui, |ui| {
                    for it in Interpolation::ALL {
                        if ui.selectable_label(k.interp == it, it.label()).clicked() {
                            acts.push(Act::Interp(name.clone(), i, it));
                        }
                    }
                });
                if ui.small_button("Smooth").clicked() {
                    acts.push(Act::Smooth(name.clone(), i));
                }
                if ui.small_button("Flatten").clicked() {
                    acts.push(Act::Flatten(name.clone(), i));
                }
                if ui.small_button("Delete").clicked() {
                    acts.push(Act::Delete(name.clone(), i));
                }
            });
        }
    } else {
        ui.label(
            egui::RichText::new("Click a key to edit it · double-click a curve to add one · Shift: time only · Ctrl: value only · Alt: break tangents")
                .small()
                .color(theme::TEXT_DIM),
        );
    }
    discrete |= !acts.is_empty();

    for a in acts {
        match a {
            Act::Interp(n, i, it) => {
                if let Some(k) = curve_of(&mut edited, &n).and_then(|c| c.keys.get_mut(i)) {
                    k.interp = it;
                }
            }
            Act::Smooth(n, i) => {
                if let Some(c) = curve_of(&mut edited, &n) {
                    c.auto_smooth(i);
                }
            }
            Act::Flatten(n, i) => {
                if let Some(c) = curve_of(&mut edited, &n) {
                    c.flatten(i);
                }
            }
            Act::Delete(n, i) => {
                if let Some((_, p)) = edited.animatable_params_mut().into_iter().find(|(pn, _)| *pn == n) {
                    if let Some(c) = p.curve.as_mut().filter(|c| i < c.keys.len()) {
                        let k = c.keys.remove(i);
                        if c.keys.is_empty() {
                            p.value = k.value;
                            p.curve = None;
                        }
                    }
                }
                app.graph.selected = None;
            }
            Act::Add(n, t) => {
                if let Some(c) = curve_of(&mut edited, &n) {
                    let v = c.eval(t);
                    c.set_value_at(t, v);
                    let i = c.keys.iter().position(|k| k.time == t);
                    app.graph.selected = i.map(|i| (n.clone(), i));
                }
            }
        }
    }
    if discrete {
        app.exec(Command::UpdateClip { clip: Box::new(edited) });
    } else if changed {
        app.exec_key(Command::UpdateClip { clip: Box::new(edited) }, Some(drag_key(GRAPH_TAG, id)));
    }
    if let Some(t) = seek_to {
        app.seek(t);
    }
}
