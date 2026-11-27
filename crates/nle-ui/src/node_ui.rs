//! Node compositor editor: the clip's correction nodes (plus effects, media,
//! solids, merges and mixers) as boxes and wires on a canvas.

use crate::theme;
use crate::widgets::{color_well, param_row, ParamSpec};
use eframe::egui::{self, Color32, Pos2, Rect, Sense, Stroke, Vec2};
use nle_core::clip::BlendMode;
use nle_core::color::ColorCorrection;
use nle_core::effects::ParamKind;
use nle_core::node_graph::{graph_effects, MergeParams, NodeGraph, NodeKind};
use nle_core::{AssetId, ColorParams, Effect, Time};

const NODE_W: f32 = 132.0;
const HEADER_H: f32 = 22.0;
const PORT_H: f32 = 18.0;
const PORT_R: f32 = 5.0;
const CANVAS_H: f32 = 280.0;

#[derive(Clone, Default)]
struct CanvasState {
    pan: Vec2,
    selected: Option<u64>,
    /// A wire being dragged out of this node's output.
    wire_from: Option<u64>,
    error: Option<&'static str>,
}

fn node_height(inputs: usize) -> f32 {
    HEADER_H + inputs.max(1) as f32 * PORT_H + 6.0
}

fn in_port(r: Rect, k: usize) -> Pos2 {
    Pos2::new(r.left(), r.top() + HEADER_H + PORT_H * 0.5 + k as f32 * PORT_H)
}

fn out_port(r: Rect) -> Pos2 {
    Pos2::new(r.right(), r.top() + HEADER_H + PORT_H * 0.5)
}

fn accent(kind: &NodeKind) -> Color32 {
    match kind {
        NodeKind::Source | NodeKind::Output => theme::TEXT_DIM,
        NodeKind::Corrector { .. } => theme::SNAP,
        NodeKind::Effect { .. } => theme::TITLE,
        NodeKind::MediaIn { .. } | NodeKind::Solid { .. } => theme::AUDIO,
        NodeKind::Merge(_) | NodeKind::Parallel | NodeKind::Layer { .. } => theme::VIDEO,
    }
}

fn wire(painter: &egui::Painter, a: Pos2, b: Pos2, stroke: Stroke) {
    let dx = ((b.x - a.x).abs() * 0.5).max(30.0);
    let pts = [a, a + Vec2::new(dx, 0.0), b - Vec2::new(dx, 0.0), b];
    painter.add(egui::epaint::CubicBezierShape::from_points_stroke(pts, false, Color32::TRANSPARENT, stroke));
}

/// Columns by depth from the inputs, rows in order.
fn tidy(g: &mut NodeGraph) {
    let mut depth: std::collections::HashMap<u64, usize> = g.nodes.iter().map(|n| (n.id, 0)).collect();
    for _ in 0..g.nodes.len() {
        let mut changed = false;
        for l in &g.links {
            let d = depth.get(&l.from).copied().unwrap_or(0) + 1;
            if depth.get(&l.to).is_some_and(|cur| *cur < d) {
                depth.insert(l.to, d);
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
    let max = depth.values().copied().max().unwrap_or(0);
    if let Some(o) = g.output_id() {
        depth.insert(o, max.max(1));
    }
    let mut rows = std::collections::HashMap::new();
    for n in &mut g.nodes {
        let d = depth[&n.id];
        let row = rows.entry(d).or_insert(0usize);
        n.pos = [12.0 + d as f32 * (NODE_W + 36.0), 12.0 + *row as f32 * 76.0];
        *row += 1;
    }
}

fn blend_combo(ui: &mut egui::Ui, salt: u64, b: &mut BlendMode) {
    egui::ComboBox::from_id_salt(("rcp-graph-blend", salt)).selected_text(b.label()).show_ui(ui, |ui| {
        for m in BlendMode::ALL {
            ui.selectable_value(b, m, m.label());
        }
    });
}

/// The node graph section of the colour tab. Shares the corrections list's
/// selection (`sel_id`) so picking a corrector node opens it there.
pub fn node_graph(ui: &mut egui::Ui, col: &mut ColorParams, assets: &[(AssetId, String)], t: Time, frame: Time) {
    let sel_id = ui.id().with("rcp-node-selected");
    let state_id = ui.id().with("rcp-graph-canvas");
    let title = if col.graph.is_some() { "Node Graph" } else { "Node Graph (off)" };
    egui::CollapsingHeader::new(egui::RichText::new(title).strong()).id_salt("rcp-node-graph").default_open(col.graph.is_some()).show(ui, |ui| {
        let Some(graph) = &mut col.graph else {
            ui.label(
                egui::RichText::new("Wire the corrections as nodes: run them in parallel, layer them with blend modes, merge in other media, solids and effects.")
                    .small()
                    .color(theme::TEXT_DIM),
            );
            if ui.button("Use Node Graph").on_hover_text("Start from the current serial order").clicked() {
                let mut g = NodeGraph::from_stack(&col.corrections);
                tidy(&mut g);
                col.graph = Some(g);
            }
            return;
        };
        let mut st: CanvasState = ui.data(|d| d.get_temp(state_id)).unwrap_or_default();
        let mut new_corrector = None;
        let mut back_to_serial = false;
        let place = [-st.pan.x + 24.0 + (graph.nodes.len() % 5) as f32 * 14.0, -st.pan.y + 24.0 + (graph.nodes.len() % 5) as f32 * 14.0];
        ui.horizontal_wrapped(|ui| {
            ui.menu_button("＋ Node", |ui| {
                let mut add = |g: &mut NodeGraph, kind: NodeKind| {
                    st.selected = Some(g.add(kind, place));
                };
                if ui.button("Corrector").clicked() {
                    let c = ColorCorrection::named(format!("Correction {}", col.corrections.len() + 1));
                    new_corrector = Some(c.id);
                    add(graph, NodeKind::Corrector { node: c.id });
                    col.corrections.push(c);
                    ui.close_menu();
                }
                ui.menu_button("Effect", |ui| {
                    egui::ScrollArea::vertical().max_height(320.0).show(ui, |ui| {
                        for k in graph_effects() {
                            if ui.button(k.label()).clicked() {
                                add(graph, NodeKind::Effect { effect: Effect::new(k) });
                                ui.close_menu();
                            }
                        }
                    });
                });
                ui.menu_button("Media In", |ui| {
                    if assets.is_empty() {
                        ui.label("No video in the project");
                    }
                    for (id, name) in assets {
                        if ui.button(name).clicked() {
                            add(graph, NodeKind::MediaIn { asset: *id, offset: Time::ZERO });
                            ui.close_menu();
                        }
                    }
                });
                if ui.button("Solid").clicked() {
                    add(graph, NodeKind::Solid { color: [0.0, 0.0, 0.0, 1.0] });
                    ui.close_menu();
                }
                ui.separator();
                if ui.button("Merge").on_hover_text("Foreground over background, with position, scale, rotation and a blend mode").clicked() {
                    add(graph, NodeKind::Merge(MergeParams::default()));
                    ui.close_menu();
                }
                if ui.button("Parallel Mixer").on_hover_text("Adds up each input's change from the Source").clicked() {
                    add(graph, NodeKind::Parallel);
                    ui.close_menu();
                }
                if ui.button("Layer Mixer").on_hover_text("Stacks its inputs, In 1 at the bottom").clicked() {
                    add(graph, NodeKind::Layer { blend: BlendMode::Normal });
                    ui.close_menu();
                }
            });
            if ui.button("Tidy").on_hover_text("Lay the nodes out left to right").clicked() {
                tidy(graph);
                st.pan = Vec2::ZERO;
            }
            if ui.button("Back to Serial").on_hover_text("Drop the wiring: the corrections run in list order").clicked() {
                back_to_serial = true;
            }
        });
        if let Some(id) = new_corrector {
            ui.data_mut(|d| d.insert_temp(sel_id, id));
        }

        // ----- canvas
        let (resp, painter) = ui.allocate_painter(Vec2::new(ui.available_width(), CANVAS_H), Sense::click_and_drag());
        let canvas = resp.rect;
        painter.rect_filled(canvas, 4, theme::VIEWER_BG);
        painter.rect_stroke(canvas, 4, Stroke::new(1.0_f32, theme::BORDER), egui::StrokeKind::Inside);
        if resp.dragged() && st.wire_from.is_none() {
            st.pan += resp.drag_delta();
        }
        if resp.clicked() {
            st.selected = None;
        }
        let origin = canvas.min.to_vec2() + st.pan;
        let rects: Vec<(u64, Rect, usize)> = graph
            .nodes
            .iter()
            .map(|n| {
                let ins = graph.input_count(n.id);
                (n.id, Rect::from_min_size(Pos2::new(n.pos[0], n.pos[1]) + origin, Vec2::new(NODE_W, node_height(ins))), ins)
            })
            .collect();
        let rect_of = |id: u64| rects.iter().find(|r| r.0 == id).map(|r| r.1);

        // wires under the nodes
        for l in &graph.links {
            if let (Some(a), Some(b)) = (rect_of(l.from), rect_of(l.to)) {
                wire(&painter, out_port(a), in_port(b, l.port as usize), Stroke::new(2.0_f32, theme::TEXT_DIM));
            }
        }

        let pointer = ui.ctx().pointer_latest_pos();
        let mut connect: Option<(u64, u64, usize)> = None;
        let mut disconnect: Option<(u64, usize)> = None;
        for (id, r, ins) in &rects {
            let Some(node) = graph.node(*id) else { continue };
            let selected = st.selected == Some(*id);
            painter.rect_filled(*r, 5, if node.enabled { theme::PANEL } else { theme::BG });
            painter.rect_stroke(*r, 5, Stroke::new(if selected { 2.0_f32 } else { 1.0 }, if selected { theme::SELECTION } else { theme::BORDER }), egui::StrokeKind::Inside);
            let head = Rect::from_min_size(r.min, Vec2::new(NODE_W, HEADER_H));
            painter.rect_filled(head.shrink(1.0), 4, accent(&node.kind).gamma_multiply(0.35));
            let name = match &node.kind {
                NodeKind::Corrector { node: c } => col.corrections.iter().position(|n| n.id == *c).map_or("Corrector (missing)".to_string(), |i| {
                    let n = &col.corrections[i];
                    if n.name.is_empty() {
                        format!("Correction {}", i + 1)
                    } else {
                        n.name.clone()
                    }
                }),
                NodeKind::MediaIn { asset, .. } => assets.iter().find(|a| a.0 == *asset).map_or("Media In (offline)".into(), |a| a.1.clone()),
                k => k.label().to_string(),
            };
            painter.with_clip_rect(head.intersect(canvas)).text(
                head.left_center() + Vec2::new(6.0, 0.0),
                egui::Align2::LEFT_CENTER,
                name,
                egui::FontId::proportional(12.0),
                theme::TEXT,
            );
            let node_resp = ui.interact(r.intersect(canvas), state_id.with(("node", *id)), Sense::click_and_drag());
            if node_resp.dragged() && st.wire_from.is_none() {
                let d = node_resp.drag_delta();
                if let Some(n) = graph.node_mut(*id) {
                    n.pos = [n.pos[0] + d.x, n.pos[1] + d.y];
                }
            }
            if node_resp.clicked() || node_resp.drag_started() {
                st.selected = Some(*id);
                if let Some(NodeKind::Corrector { node: c }) = graph.node(*id).map(|n| &n.kind) {
                    let c = *c;
                    ui.data_mut(|d| d.insert_temp(sel_id, c));
                }
            }
            let Some(node) = graph.node(*id) else { continue };
            // inputs
            for k in 0..*ins {
                let p = in_port(*r, k);
                let linked = graph.input(*id, k).is_some();
                painter.circle(p, PORT_R, if linked { theme::TEXT } else { theme::BG }, Stroke::new(1.0_f32, theme::TEXT));
                painter.text(p + Vec2::new(PORT_R + 4.0, 0.0), egui::Align2::LEFT_CENTER, node.kind.port_name(k), egui::FontId::proportional(10.0), theme::TEXT_DIM);
                let pr = ui.interact(Rect::from_center_size(p, Vec2::splat(PORT_R * 3.0)).intersect(canvas), state_id.with(("in", *id, k)), Sense::click());
                if pr.clicked() && linked {
                    disconnect = Some((*id, k));
                }
                if let (Some(from), Some(pos)) = (st.wire_from, pointer) {
                    if Rect::from_center_size(p, Vec2::splat(PORT_R * 4.0)).contains(pos) && ui.ctx().input(|i| i.pointer.any_released()) {
                        connect = Some((from, *id, k));
                    }
                }
            }
            // output
            if node.kind != NodeKind::Output {
                let p = out_port(*r);
                painter.circle(p, PORT_R, theme::VIDEO, Stroke::new(1.0_f32, theme::TEXT));
                let pr = ui.interact(Rect::from_center_size(p, Vec2::splat(PORT_R * 3.0)).intersect(canvas), state_id.with(("out", *id)), Sense::drag());
                if pr.drag_started() {
                    st.wire_from = Some(*id);
                }
            }
        }
        if let (Some(from), Some(pos)) = (st.wire_from, pointer) {
            if let Some(a) = rect_of(from) {
                wire(&painter, out_port(a), pos, Stroke::new(2.0_f32, theme::SNAP));
            }
        }
        if ui.ctx().input(|i| !i.pointer.any_down()) {
            st.wire_from = None;
        }
        if let Some((from, to, port)) = connect {
            st.error = graph.connect(from, to, port).err().map(|e| e.message());
            st.wire_from = None;
        }
        if let Some((to, port)) = disconnect {
            graph.disconnect(to, port);
        }
        if let Some(e) = st.error {
            ui.label(egui::RichText::new(e).small().color(theme::PLAYHEAD));
        }
        ui.label(
            egui::RichText::new("Drag from an output (right) to an input (left) to wire; click a wired input to cut it; drag the background to pan.")
                .small()
                .color(theme::TEXT_DIM),
        );

        // ----- selected node
        let mut delete = None;
        if let Some(n) = st.selected.and_then(|id| graph.node_mut(id)) {
            ui.separator();
            ui.horizontal(|ui| {
                ui.checkbox(&mut n.enabled, "").on_hover_text("Enabled (off: the first input passes through)");
                ui.label(egui::RichText::new(n.kind.label()).strong());
                if !matches!(n.kind, NodeKind::Source | NodeKind::Output) {
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.small_button("✕").on_hover_text("Delete node").clicked() {
                            delete = Some(n.id);
                        }
                    });
                }
            });
            node_params(ui, n.id, &mut n.kind, assets, t, frame);
        }
        if let Some(id) = delete {
            if let Some(NodeKind::Corrector { node }) = graph.remove(id) {
                col.corrections.retain(|c| c.id != node);
            }
            st.selected = None;
        }
        if back_to_serial {
            col.graph = None;
        }
        ui.data_mut(|d| d.insert_temp(state_id, st));
    });
}

fn node_params(ui: &mut egui::Ui, id: u64, kind: &mut NodeKind, assets: &[(AssetId, String)], t: Time, f: Time) {
    let dim = |ui: &mut egui::Ui, s: &str| {
        ui.label(egui::RichText::new(s).small().color(theme::TEXT_DIM));
    };
    match kind {
        NodeKind::Source => dim(ui, "The clip after the primary grade."),
        NodeKind::Output => dim(ui, "What the clip's effects and transform receive. Unconnected: the Source."),
        NodeKind::Corrector { .. } => dim(ui, "Edit this corrector in the Corrections list above."),
        NodeKind::Parallel => dim(ui, "Each input's change from the Source, added up (connect correctors fed by the Source)."),
        NodeKind::Layer { blend } => {
            ui.horizontal(|ui| {
                ui.label("Blend");
                blend_combo(ui, id, blend);
            });
            dim(ui, "In 1 is the bottom layer; each next input is blended over it.");
        }
        NodeKind::Solid { color } => {
            ui.horizontal(|ui| {
                ui.label("Colour");
                color_well(ui, color);
            });
        }
        NodeKind::MediaIn { asset, offset } => {
            let name = assets.iter().find(|a| a.0 == *asset).map_or("(offline)".to_string(), |a| a.1.clone());
            ui.horizontal(|ui| {
                ui.label("Media");
                egui::ComboBox::from_id_salt(("rcp-graph-media", id)).selected_text(name).show_ui(ui, |ui| {
                    for (aid, n) in assets {
                        ui.selectable_value(asset, *aid, n);
                    }
                });
            });
            let mut secs = offset.as_secs_f64();
            ui.horizontal(|ui| {
                ui.label("Offset");
                if ui.add(egui::DragValue::new(&mut secs).speed(0.04).suffix(" s")).changed() {
                    *offset = Time::from_secs_f64(secs);
                }
            });
            dim(ui, "Shown at the clip's own time plus the offset, fitted into the frame (not graded).");
        }
        NodeKind::Merge(m) => {
            ui.horizontal(|ui| {
                ui.label("Blend");
                blend_combo(ui, id, &mut m.blend);
            });
            let specs = [
                ParamSpec::new("Opacity", 0.0, 1.0, 1.0),
                ParamSpec::new("Position X", -2.0, 2.0, 0.0),
                ParamSpec::new("Position Y", -2.0, 2.0, 0.0),
                ParamSpec::new("Scale", 0.0, 8.0, 1.0),
                ParamSpec::new("Rotation", -360.0, 360.0, 0.0).scaled(1.0, "°").decimals(1).speed(0.5),
            ];
            for ((_, p), spec) in m.params_mut().into_iter().zip(specs.iter()) {
                param_row(ui, spec, p, t, f);
            }
        }
        NodeKind::Effect { effect } => {
            let defs = effect.kind.params();
            for (i, d) in defs.iter().enumerate() {
                match d.kind {
                    ParamKind::Choice(opts) => {
                        let cur = (effect.params[i].at(t).round() as usize).min(opts.len().saturating_sub(1));
                        ui.horizontal(|ui| {
                            ui.add_sized([96.0, 20.0], egui::Label::new(egui::RichText::new(d.name).color(theme::TEXT_DIM)));
                            egui::ComboBox::from_id_salt(("rcp-graph-choice", id, i)).selected_text(opts.get(cur).copied().unwrap_or("")).show_ui(ui, |ui| {
                                for (oi, opt) in opts.iter().enumerate() {
                                    if ui.selectable_label(oi == cur, *opt).clicked() {
                                        effect.params[i].set(t, oi as f64);
                                    }
                                }
                            });
                        });
                    }
                    _ => {
                        let span = (d.max - d.min).abs();
                        let (dec, spd) = if d.kind == ParamKind::Integer {
                            (0, 0.25)
                        } else if span <= 2.5 {
                            (2, 0.01)
                        } else if span <= 20.0 {
                            (2, 0.05)
                        } else {
                            (1, 0.5)
                        };
                        let spec = ParamSpec::new(d.name, d.min, d.max, d.default).scaled(1.0, d.suffix).decimals(dec).speed(spd);
                        if param_row(ui, &spec, &mut effect.params[i], t, f) && d.kind == ParamKind::Integer {
                            let v = effect.params[i].at(t).round();
                            effect.params[i].set(t, v);
                        }
                    }
                }
            }
        }
    }
}
