//! Fairlight-style audio UI: the 6-band parametric EQ editor (drag the
//! handles: x = frequency, y = gain, scroll = Q, double-click = reset), the
//! insert editors (expander/gate, compressor, de-esser, delay, reverb), the
//! restoration / leveler controls, and the Audio Mixer drawer (Ctrl+8) with
//! user submix busses and fader / balance automation (Read, Touch).

use crate::theme;
use crate::widgets::slider_f32;
use eframe::egui::{self, Color32, Stroke};
use nle_core::{
    AudioFx, AudioRole, AutoLane, AutoPoint, BusId, BusSettings, EqBandType, FilterSlope, GateMode, MixerSettings, ParametricEq, PluginInsert, Time, BUS_SLOTS, MAX_USER_BUSSES,
};
use nle_media::audio::plugins::{self, PluginInfo};
use nle_media::audio::{GainReduction, MeterReading, ParametricEqProc};

// ---------------------------------------------------------------------------
// Parametric EQ editor
// ---------------------------------------------------------------------------

/// Handle index: 0 = HPF, 1..=6 = bands, 7 = LPF.
const HPF: usize = 0;
const LPF: usize = 7;
const HANDLE_COLORS: [Color32; 8] = [
    Color32::from_rgb(0x8E, 0x8E, 0x93),
    Color32::from_rgb(0xFF, 0x45, 0x3A),
    Color32::from_rgb(0xFF, 0x9F, 0x0A),
    Color32::from_rgb(0xFF, 0xD6, 0x0A),
    Color32::from_rgb(0x30, 0xD1, 0x58),
    Color32::from_rgb(0x0A, 0x84, 0xFF),
    Color32::from_rgb(0xBF, 0x5A, 0xF2),
    Color32::from_rgb(0x8E, 0x8E, 0x93),
];
/// Visible gain range of the plot (±dB).
const DB_SPAN: f32 = 24.0;
const PICK_RADIUS: f32 = 14.0;

fn freq_to_x(r: egui::Rect, f: f32) -> f32 {
    r.left() + (f.max(20.0) / 20.0).log(1000.0) * r.width()
}

fn x_to_freq(r: egui::Rect, x: f32) -> f32 {
    20.0 * 1000f32.powf(((x - r.left()) / r.width()).clamp(0.0, 1.0))
}

fn db_to_y(r: egui::Rect, db: f32) -> f32 {
    r.center().y - (db / DB_SPAN).clamp(-1.0, 1.0) * r.height() * 0.5
}

fn y_to_db(r: egui::Rect, y: f32) -> f32 {
    (r.center().y - y) / (r.height() * 0.5) * DB_SPAN
}

fn handle_pos(eq: &ParametricEq, i: usize, r: egui::Rect) -> egui::Pos2 {
    match i {
        HPF => egui::pos2(freq_to_x(r, eq.hpf.freq), db_to_y(r, 0.0)),
        LPF => egui::pos2(freq_to_x(r, eq.lpf.freq), db_to_y(r, 0.0)),
        _ => {
            let b = &eq.bands[i - 1];
            let g = if b.kind.has_gain() { b.gain_db } else { -DB_SPAN * 0.75 };
            egui::pos2(freq_to_x(r, b.freq), db_to_y(r, g))
        }
    }
}

fn handle_enabled(eq: &ParametricEq, i: usize) -> bool {
    match i {
        HPF => eq.hpf.enabled,
        LPF => eq.lpf.enabled,
        _ => eq.bands[i - 1].enabled,
    }
}

fn pick(eq: &ParametricEq, r: egui::Rect, p: egui::Pos2) -> Option<usize> {
    (0..8).map(|i| (i, handle_pos(eq, i, r).distance(p))).filter(|(_, d)| *d < PICK_RADIUS).min_by(|a, b| a.1.total_cmp(&b.1)).map(|(i, _)| i)
}

fn handle_label(i: usize) -> String {
    match i {
        HPF => "HP".into(),
        LPF => "LP".into(),
        _ => i.to_string(),
    }
}

fn freq_label(f: f32) -> String {
    if f >= 1_000.0 {
        format!("{:.1} kHz", f / 1_000.0)
    } else {
        format!("{f:.0} Hz")
    }
}

/// Edit `eq` in place. `id` must be unique per EQ instance.
pub fn eq_editor(ui: &mut egui::Ui, eq: &mut ParametricEq, id: egui::Id) {
    ui.horizontal(|ui| {
        ui.checkbox(&mut eq.enabled, "Enabled");
        ui.add(egui::DragValue::new(&mut eq.output_db).range(-24.0..=24.0).speed(0.1).prefix("Out ").suffix(" dB").max_decimals(1));
        if ui.small_button("Flat").on_hover_text("Reset every band (keeps the EQ on/off state)").clicked() {
            *eq = ParametricEq { enabled: eq.enabled, ..Default::default() };
        }
    });
    let sel_id = id.with("sel");
    let grab_id = id.with("grab");
    let mut sel: usize = ui.data(|d| d.get_temp(sel_id)).unwrap_or(1);
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(ui.available_width(), 150.0), egui::Sense::click_and_drag());

    // --- interaction ---------------------------------------------------------
    if resp.drag_started() {
        let hit = resp.interact_pointer_pos().and_then(|p| pick(eq, rect, p));
        if let Some(i) = hit {
            sel = i;
        }
        ui.data_mut(|d| d.insert_temp(grab_id, hit));
    }
    if resp.dragged() {
        let grabbed: Option<usize> = ui.data(|d| d.get_temp(grab_id)).flatten();
        if let (Some(i), Some(p)) = (grabbed, resp.interact_pointer_pos()) {
            let f = x_to_freq(rect, p.x).clamp(20.0, 20_000.0);
            let g = y_to_db(rect, p.y).clamp(*ParametricEq::GAIN_RANGE.start(), *ParametricEq::GAIN_RANGE.end());
            eq.enabled = true;
            match i {
                HPF => {
                    eq.hpf.enabled = true;
                    eq.hpf.freq = f;
                }
                LPF => {
                    eq.lpf.enabled = true;
                    eq.lpf.freq = f;
                }
                _ => {
                    let b = &mut eq.bands[i - 1];
                    b.enabled = true;
                    b.freq = f;
                    if b.kind.has_gain() {
                        b.gain_db = (g * 10.0).round() / 10.0;
                    }
                }
            }
        }
    }
    if resp.drag_stopped() {
        ui.data_mut(|d| d.insert_temp::<Option<usize>>(grab_id, None));
    }
    if resp.clicked() {
        if let Some(i) = resp.interact_pointer_pos().and_then(|p| pick(eq, rect, p)) {
            sel = i;
        }
    }
    if resp.double_clicked() {
        match resp.interact_pointer_pos().and_then(|p| pick(eq, rect, p)) {
            Some(HPF) => eq.hpf.enabled = false,
            Some(LPF) => eq.lpf.enabled = false,
            Some(i) => eq.bands[i - 1].gain_db = 0.0,
            None => {}
        }
    }
    // scroll over the plot = Q of the hovered (else selected) band
    if resp.hovered() {
        let scroll = ui.input(|i| i.smooth_scroll_delta.y);
        if scroll != 0.0 {
            let target = resp.hover_pos().and_then(|p| pick(eq, rect, p)).unwrap_or(sel);
            if (1..=6).contains(&target) {
                let b = &mut eq.bands[target - 1];
                b.q = (b.q * (1.0 + scroll * 0.004)).clamp(*ParametricEq::Q_RANGE.start(), *ParametricEq::Q_RANGE.end());
                sel = target;
            }
            // the plot ate the scroll: keep the inspector from scrolling too
            ui.input_mut(|i| i.smooth_scroll_delta = egui::Vec2::ZERO);
        }
    }
    ui.data_mut(|d| d.insert_temp(sel_id, sel));

    // --- drawing ---------------------------------------------------------------
    let p = ui.painter_at(rect);
    p.rect_filled(rect, egui::CornerRadius::same(4), theme::BG);
    for db in [-18.0f32, -12.0, -6.0, 6.0, 12.0, 18.0] {
        let y = db_to_y(rect, db);
        p.line_segment([egui::pos2(rect.left(), y), egui::pos2(rect.right(), y)], Stroke::new(0.5_f32, theme::BORDER));
        p.text(egui::pos2(rect.left() + 2.0, y), egui::Align2::LEFT_CENTER, format!("{db:+.0}"), egui::FontId::proportional(8.0), theme::TEXT_DIM);
    }
    p.line_segment([rect.left_center(), rect.right_center()], Stroke::new(0.8_f32, theme::WIDGET_HOVER));
    for (f, l) in [(50.0f32, "50"), (100.0, "100"), (200.0, "200"), (500.0, "500"), (1000.0, "1k"), (2000.0, "2k"), (5000.0, "5k"), (10000.0, "10k")] {
        let x = freq_to_x(rect, f);
        p.line_segment([egui::pos2(x, rect.top()), egui::pos2(x, rect.bottom())], Stroke::new(0.5_f32, theme::BORDER));
        p.text(egui::pos2(x, rect.bottom() - 2.0), egui::Align2::CENTER_BOTTOM, l, egui::FontId::proportional(8.0), theme::TEXT_DIM);
    }
    let n = 200;
    let freqs: Vec<f32> = (0..=n).map(|i| x_to_freq(rect, rect.left() + rect.width() * i as f32 / n as f32)).collect();
    // selected band's own contribution, filled
    if (1..=6).contains(&sel) && eq.enabled && eq.bands[sel - 1].enabled {
        let b = eq.bands[sel - 1];
        let c = HANDLE_COLORS[sel];
        let zero = db_to_y(rect, 0.0);
        let pts: Vec<egui::Pos2> = freqs.iter().map(|&f| egui::pos2(freq_to_x(rect, f), db_to_y(rect, ParametricEqProc::band_response_db(48_000.0, &b, f)))).collect();
        for w in pts.windows(2) {
            let quad = vec![w[0], w[1], egui::pos2(w[1].x, zero), egui::pos2(w[0].x, zero)];
            p.add(egui::Shape::convex_polygon(quad, c.gamma_multiply(0.18), Stroke::NONE));
        }
    }
    let proc = ParametricEqProc::new(48_000.0, *eq);
    let curve: Vec<egui::Pos2> = freqs.iter().map(|&f| egui::pos2(freq_to_x(rect, f), db_to_y(rect, if eq.enabled { proc.response_db(f) } else { 0.0 }))).collect();
    p.add(egui::Shape::line(curve, Stroke::new(1.8_f32, if eq.enabled { theme::AUDIO } else { theme::TEXT_DIM })));
    let hovered = resp.hover_pos().and_then(|q| pick(eq, rect, q));
    for (i, &c) in HANDLE_COLORS.iter().enumerate() {
        let pos = handle_pos(eq, i, rect);
        let on = handle_enabled(eq, i);
        let r = if i == sel || hovered == Some(i) { 7.0 } else { 5.5 };
        if on {
            p.circle_filled(pos, r, c);
        } else {
            p.circle_stroke(pos, r, Stroke::new(1.2_f32, c.gamma_multiply(0.6)));
        }
        if i == sel {
            p.circle_stroke(pos, r + 2.5, Stroke::new(1.0_f32, theme::TEXT));
        }
        p.text(pos + egui::vec2(0.0, -r - 2.0), egui::Align2::CENTER_BOTTOM, handle_label(i), egui::FontId::proportional(8.5), if on { theme::TEXT } else { theme::TEXT_DIM });
    }
    resp.on_hover_text("Drag a handle: frequency / gain · Scroll: Q · Double-click: reset band (or switch a filter off)");

    // --- selected handle controls ---------------------------------------------
    ui.horizontal(|ui| {
        for (i, &hc) in HANDLE_COLORS.iter().enumerate() {
            let t = egui::RichText::new(handle_label(i)).color(if handle_enabled(eq, i) { hc } else { theme::TEXT_DIM });
            if ui.selectable_label(sel == i, t).clicked() {
                sel = i;
                ui.data_mut(|d| d.insert_temp(sel_id, sel));
            }
        }
    });
    ui.horizontal_wrapped(|ui| match sel {
        HPF | LPF => {
            let flt = if sel == HPF { &mut eq.hpf } else { &mut eq.lpf };
            ui.checkbox(&mut flt.enabled, if sel == HPF { "High-pass" } else { "Low-pass" });
            ui.add_enabled_ui(flt.enabled, |ui| {
                let speed = flt.freq * 0.01;
                ui.add(egui::DragValue::new(&mut flt.freq).range(20.0..=20_000.0).speed(speed).custom_formatter(|v, _| freq_label(v as f32)));
                egui::ComboBox::from_id_salt(id.with(("slope", sel))).selected_text(flt.slope.label()).show_ui(ui, |ui| {
                    for s in FilterSlope::ALL {
                        ui.selectable_value(&mut flt.slope, s, s.label());
                    }
                });
            });
        }
        _ => {
            let b = &mut eq.bands[sel - 1];
            ui.checkbox(&mut b.enabled, format!("Band {sel}"));
            ui.add_enabled_ui(b.enabled, |ui| {
                egui::ComboBox::from_id_salt(id.with(("type", sel))).selected_text(b.kind.label()).width(84.0).show_ui(ui, |ui| {
                    for k in EqBandType::ALL {
                        ui.selectable_value(&mut b.kind, k, k.label());
                    }
                });
                let speed = b.freq * 0.01;
                ui.add(egui::DragValue::new(&mut b.freq).range(20.0..=20_000.0).speed(speed).custom_formatter(|v, _| freq_label(v as f32)));
                if b.kind.has_gain() {
                    ui.add(egui::DragValue::new(&mut b.gain_db).range(ParametricEq::GAIN_RANGE).speed(0.1).suffix(" dB").max_decimals(1));
                }
                ui.add(egui::DragValue::new(&mut b.q).range(ParametricEq::Q_RANGE).speed(0.01).prefix("Q ").max_decimals(2));
            });
        }
    });
}

// ---------------------------------------------------------------------------
// Inserts
// ---------------------------------------------------------------------------

/// Gain-reduction bar (0–20 dB).
fn gr_bar(ui: &mut egui::Ui, db: f32) {
    let (r, _) = ui.allocate_exact_size(egui::vec2(70.0, 10.0), egui::Sense::hover());
    let p = ui.painter();
    p.rect_filled(r, egui::CornerRadius::same(2), theme::BG);
    let w = (db / 20.0).clamp(0.0, 1.0) * r.width();
    p.rect_filled(egui::Rect::from_min_size(egui::pos2(r.right() - w, r.top()), egui::vec2(w, r.height())), egui::CornerRadius::same(2), theme::SNAP);
    p.text(r.left_center() + egui::vec2(2.0, 0.0), egui::Align2::LEFT_CENTER, format!("GR {db:.1}"), egui::FontId::proportional(8.0), theme::TEXT);
}

fn insert_header(ui: &mut egui::Ui, enabled: &mut bool, title: &str, gr: Option<f32>) -> bool {
    ui.horizontal(|ui| {
        ui.checkbox(enabled, egui::RichText::new(title).strong());
        if let (true, Some(g)) = (*enabled, gr) {
            gr_bar(ui, g);
        }
    });
    *enabled
}

/// Expander/gate → compressor → de-esser → delay → reverb. `gr` shows live
/// gain reduction (busses only; clips have no per-clip meters).
pub fn fx_editor(ui: &mut egui::Ui, fx: &mut AudioFx, id: egui::Id, gr: Option<GainReduction>) {
    let g = &mut fx.gate;
    if insert_header(ui, &mut g.enabled, "Expander / Gate", gr.map(|r| r.gate)) {
        ui.indent(id.with("gate"), |ui| {
            ui.horizontal(|ui| {
                ui.selectable_value(&mut g.mode, GateMode::Expander, "Expander");
                ui.selectable_value(&mut g.mode, GateMode::Gate, "Gate");
            });
            slider_f32(ui, "Threshold", &mut g.threshold_db, -80.0..=0.0, " dB");
            if g.mode == GateMode::Expander {
                slider_f32(ui, "Ratio", &mut g.ratio, 1.0..=8.0, ":1");
            }
            slider_f32(ui, "Range", &mut g.range_db, 0.0..=80.0, " dB");
            slider_f32(ui, "Attack", &mut g.attack_ms, 0.1..=50.0, " ms");
            slider_f32(ui, "Hold", &mut g.hold_ms, 0.0..=500.0, " ms");
            slider_f32(ui, "Release", &mut g.release_ms, 10.0..=2000.0, " ms");
        });
    }
    let c = &mut fx.compressor;
    if insert_header(ui, &mut c.enabled, "Compressor", gr.map(|r| r.comp)) {
        ui.indent(id.with("comp"), |ui| {
            slider_f32(ui, "Threshold", &mut c.threshold_db, -60.0..=0.0, " dB");
            slider_f32(ui, "Ratio", &mut c.ratio, 1.0..=20.0, ":1");
            slider_f32(ui, "Attack", &mut c.attack_ms, 0.1..=200.0, " ms");
            slider_f32(ui, "Release", &mut c.release_ms, 10.0..=2000.0, " ms");
            slider_f32(ui, "Knee", &mut c.knee_db, 0.0..=24.0, " dB");
            slider_f32(ui, "Makeup", &mut c.makeup_db, 0.0..=24.0, " dB");
            compressor_curve_plot(ui, c);
        });
    }
    let d = &mut fx.de_esser;
    if insert_header(ui, &mut d.enabled, "De-Esser", gr.map(|r| r.de_esser)) {
        ui.indent(id.with("dees"), |ui| {
            slider_f32(ui, "Frequency", &mut d.freq, 2_000.0..=12_000.0, " Hz");
            slider_f32(ui, "Threshold", &mut d.threshold_db, -60.0..=0.0, " dB");
            slider_f32(ui, "Reduction", &mut d.reduction_db, 0.0..=24.0, " dB");
        });
    }
    let dl = &mut fx.delay;
    if insert_header(ui, &mut dl.enabled, "Delay", None) {
        ui.indent(id.with("delay"), |ui| {
            slider_f32(ui, "Time", &mut dl.time_ms, 1.0..=2000.0, " ms");
            slider_f32(ui, "Feedback", &mut dl.feedback, 0.0..=0.95, "");
            slider_f32(ui, "Mix", &mut dl.mix, 0.0..=1.0, "");
            slider_f32(ui, "Damping", &mut dl.damping_hz, 500.0..=20_000.0, " Hz");
            ui.checkbox(&mut dl.ping_pong, "Ping-pong");
        });
    }
    let rv = &mut fx.reverb;
    if insert_header(ui, &mut rv.enabled, "Reverb", None) {
        ui.indent(id.with("reverb"), |ui| {
            slider_f32(ui, "Room Size", &mut rv.room_size, 0.0..=1.0, "");
            slider_f32(ui, "Damping", &mut rv.damping, 0.0..=1.0, "");
            slider_f32(ui, "Width", &mut rv.width, 0.0..=1.0, "");
            slider_f32(ui, "Pre-delay", &mut rv.pre_delay_ms, 0.0..=200.0, " ms");
            slider_f32(ui, "Mix", &mut rv.mix, 0.0..=1.0, "");
        });
    }
}

/// Static in → out curve of the compressor.
fn compressor_curve_plot(ui: &mut egui::Ui, c: &nle_core::CompressorSettings) {
    let side = 90.0;
    let (r, _) = ui.allocate_exact_size(egui::vec2(side, side), egui::Sense::hover());
    let p = ui.painter_at(r);
    p.rect_filled(r, egui::CornerRadius::same(3), theme::BG);
    let map = |db: f32| (db + 60.0) / 60.0;
    p.line_segment([r.left_bottom(), r.right_top()], Stroke::new(0.5_f32, theme::BORDER));
    let pts: Vec<egui::Pos2> = (0..=60)
        .map(|i| {
            let l = -60.0 + i as f32;
            let out = l + nle_media::audio::dynamics::compressor_curve(l, c.threshold_db, c.ratio, c.knee_db) + c.makeup_db;
            egui::pos2(r.left() + map(l) * side, r.bottom() - map(out).clamp(0.0, 1.0) * side)
        })
        .collect();
    p.add(egui::Shape::line(pts, Stroke::new(1.5_f32, theme::AUDIO)));
    let ty = r.bottom() - map(c.threshold_db) * side;
    p.line_segment([egui::pos2(r.left(), ty), egui::pos2(r.right(), ty)], Stroke::new(0.5_f32, theme::SNAP.gamma_multiply(0.6)));
}

// ---------------------------------------------------------------------------
// Audio Mixer drawer
// ---------------------------------------------------------------------------

/// Display state of the mixer drawer (ballistics live in the UI; the audio
/// thread only reports raw readings).
pub struct MixerView {
    pub open: bool,
    /// Mixer slot ([`BUS_SLOTS`]) whose EQ / inserts are shown next to the strips.
    pub selected: Option<usize>,
    /// Displayed per-channel level (dBFS) with a 24 dB/s fall.
    level: [[f32; 2]; BUS_SLOTS],
    /// Peak hold (dBFS, seconds left).
    hold: [[(f32, f32); 2]; BUS_SLOTS],
    last: [MeterReading; BUS_SLOTS],
    gr: [GainReduction; BUS_SLOTS],
    /// Touch write armed per slot.
    armed: [bool; BUS_SLOTS],
    /// The touch pass being recorded.
    touch: Option<Touch>,
    /// CLAP / VST3 plug-ins found by the last scan (scanned when the add menu first opens).
    clap_found: Option<Vec<PluginInfo>>,
    /// Plug-ins that failed to load in the last scan / browse.
    clap_errors: Vec<String>,
}

impl Default for MixerView {
    fn default() -> Self {
        MixerView {
            open: false,
            selected: None,
            level: [[-90.0; 2]; BUS_SLOTS],
            hold: [[(-90.0, 0.0); 2]; BUS_SLOTS],
            last: [MeterReading::default(); BUS_SLOTS],
            gr: Default::default(),
            armed: [false; BUS_SLOTS],
            touch: None,
            clap_found: None,
            clap_errors: Vec::new(),
        }
    }
}

/// What the drawer asks the app to do besides editing `mix`.
#[derive(Default)]
pub struct MixerRequests {
    pub reset_loudness: bool,
}

/// The timeline clock the automation controls work against.
#[derive(Clone, Copy)]
pub struct AutoClock {
    pub t: Time,
    pub playing: bool,
    /// Timeline length (the automation editor's width).
    pub end: Time,
}

/// A fader / balance move being written while the timeline plays.
struct Touch {
    slot: usize,
    pan: bool,
    /// The lane before the pass.
    base: AutoLane,
    /// The static value, for a lane that had no points yet.
    fallback: f32,
    pass: Vec<AutoPoint>,
}

impl Touch {
    /// `base` with the pass written in (thinned for the final commit).
    fn lane(&self, thin: bool) -> AutoLane {
        let mut lane = self.base.clone();
        let pass = if thin { AutoLane::thin(&self.pass, if self.pan { 0.005 } else { 0.1 }) } else { self.pass.clone() };
        if let (true, Some(a), Some(b)) = (lane.is_empty(), pass.first(), pass.last()) {
            // a new lane returns to the static value around the pass
            let glide = Time::from_secs_f64(0.01);
            lane.set(a.time - glide, self.fallback, Time::ZERO);
            lane.set(b.time + glide, self.fallback, Time::ZERO);
        }
        lane.write_pass(&pass);
        lane
    }
}

fn lane_mut(s: &mut BusSettings, pan: bool) -> &mut AutoLane {
    if pan {
        &mut s.pan_auto
    } else {
        &mut s.gain_auto
    }
}

/// Apply a fader (`pan` = false) or balance move to `v`. Touch-armed while
/// playing: the move is written as automation at the playhead. On an
/// automated lane: a key is set at the playhead. Otherwise: the static value.
#[allow(clippy::too_many_arguments)]
fn automate(view: &mut MixerView, slot: usize, pan: bool, s: &mut BusSettings, clock: AutoClock, v: f32, changed: bool, held: bool) {
    if view.armed[slot] && clock.playing && (held || changed) {
        let fresh = !matches!(&view.touch, Some(t) if t.slot == slot && t.pan == pan);
        // a loop back to an earlier time starts a new pass on the result so far
        let looped = matches!(&view.touch, Some(t) if !fresh && t.pass.last().is_some_and(|p| clock.t < p.time));
        if fresh || looped {
            let base = if looped { view.touch.as_ref().map(|t| t.lane(true)) } else { None }.unwrap_or_else(|| lane_mut(s, pan).clone());
            view.touch = Some(Touch { slot, pan, base, fallback: if pan { s.pan } else { s.gain_db }, pass: Vec::new() });
        }
        let Some(t) = view.touch.as_mut() else { return };
        match t.pass.last_mut() {
            Some(p) if p.time == clock.t => p.value = v,
            _ => t.pass.push(AutoPoint { time: clock.t, value: v }),
        }
        let lane = t.lane(false);
        s.auto_off = false;
        *lane_mut(s, pan) = lane;
        return;
    }
    if !changed {
        return;
    }
    let off = s.auto_off;
    let lane = lane_mut(s, pan);
    if !off && !lane.is_empty() {
        lane.set(clock.t, v, Time::from_secs_f64(0.02));
    } else if pan {
        s.pan = v;
    } else {
        s.gain_db = v;
    }
}

/// End a touch pass once the mouse is up or playback stopped.
fn finish_touch(ui: &egui::Ui, mix: &mut MixerSettings, view: &mut MixerView, clock: AutoClock) {
    if view.touch.is_none() || (clock.playing && ui.input(|i| i.pointer.any_down())) {
        return;
    }
    let Some(t) = view.touch.take() else { return };
    if let Some(s) = mix.slot_mut(t.slot) {
        *lane_mut(s, t.pan) = t.lane(true);
    }
}

/// A small editor for one automation lane over `[0, end]`: drag a point
/// (time and value), double-click to add, right-click to remove.
fn lane_editor(ui: &mut egui::Ui, lane: &mut AutoLane, range: std::ops::RangeInclusive<f32>, clock: AutoClock, id: egui::Id, fmt: &dyn Fn(f32) -> String) {
    let w = ui.available_width().clamp(200.0, 420.0);
    let (r, resp) = ui.allocate_exact_size(egui::vec2(w, 70.0), egui::Sense::click_and_drag());
    let p = ui.painter_at(r);
    p.rect_filled(r, egui::CornerRadius::same(2), theme::BG);
    let end = clock.end.max(lane.points.last().map_or(Time::ZERO, |k| k.time)).max(Time::from_secs(1)).as_secs_f64();
    let (lo, hi) = (*range.start(), *range.end());
    let x = |t: Time| r.left() + (t.as_secs_f64() / end).clamp(0.0, 1.0) as f32 * r.width();
    let y = |v: f32| r.bottom() - ((v - lo) / (hi - lo)).clamp(0.0, 1.0) * r.height();
    let t_of = |px: f32| Time::from_secs_f64(((px - r.left()) / r.width()).clamp(0.0, 1.0) as f64 * end);
    let v_of = |py: f32| lo + ((r.bottom() - py) / r.height()).clamp(0.0, 1.0) * (hi - lo);
    let pts: Vec<egui::Pos2> = lane.points.iter().map(|k| egui::pos2(x(k.time), y(k.value))).collect();
    if let (Some(a), Some(b)) = (pts.first(), pts.last()) {
        let mut line = vec![egui::pos2(r.left(), a.y)];
        line.extend(&pts);
        line.push(egui::pos2(r.right(), b.y));
        p.add(egui::Shape::line(line, Stroke::new(1.5_f32, theme::AUDIO)));
    }
    for q in &pts {
        p.circle_filled(*q, 3.0, theme::TEXT);
    }
    let px = x(clock.t);
    p.line_segment([egui::pos2(px, r.top()), egui::pos2(px, r.bottom())], Stroke::new(1.0_f32, theme::PLAYHEAD));
    let drag_id = id.with("drag");
    let nearest = |pos: egui::Pos2| pts.iter().enumerate().map(|(i, q)| (i, q.distance(pos))).filter(|(_, d)| *d < 8.0).min_by(|a, b| a.1.total_cmp(&b.1)).map(|(i, _)| i);
    if let Some(pos) = resp.interact_pointer_pos() {
        if resp.drag_started() {
            ui.data_mut(|d| d.insert_temp(drag_id, nearest(pos)));
        }
        if resp.dragged() {
            if let Some(Some(i)) = ui.data(|d| d.get_temp::<Option<usize>>(drag_id)) {
                if let Some(k) = lane.points.get_mut(i) {
                    k.time = t_of(pos.x).max(Time::ZERO);
                    k.value = v_of(pos.y);
                }
                // keep the order without losing which point is held
                let held = lane.points.get(i).copied();
                lane.points.sort_by_key(|k| k.time);
                if let Some(h) = held {
                    let j = lane.points.iter().position(|k| *k == h);
                    ui.data_mut(|d| d.insert_temp(drag_id, j));
                }
            }
        }
        if resp.double_clicked() {
            lane.set(t_of(pos.x), v_of(pos.y), Time::ZERO);
        }
        if resp.secondary_clicked() {
            if let Some(i) = nearest(pos) {
                lane.points.remove(i);
            }
        }
    }
    let hover = resp.hover_pos().map(|pos| format!("\n{:.2} s · {}", t_of(pos.x).as_secs_f64(), fmt(v_of(pos.y)))).unwrap_or_default();
    resp.on_hover_text(format!("Drag a point · double-click to add · right-click to remove{hover}"));
}

const METER_MIN: f32 = -60.0;
const METER_MAX: f32 = 6.0;

fn role_color(bus: BusId) -> Color32 {
    match bus {
        BusId::Role(AudioRole::Dialogue) => Color32::from_rgb(0x5E, 0xB0, 0xFF),
        BusId::Role(AudioRole::Music) => theme::AUDIO,
        BusId::Role(AudioRole::Effects) => theme::SNAP,
        BusId::Role(AudioRole::General) => theme::TEXT_DIM,
        BusId::Master => theme::TEXT,
    }
}

fn lufs_text(v: Option<f32>) -> String {
    v.map_or("—".into(), |v| format!("{v:.1}"))
}

impl MixerView {
    /// Fold a fresh reading into the display ballistics.
    pub fn feed(&mut self, readings: Option<[MeterReading; BUS_SLOTS]>, dt: f32) {
        let Some(r) = readings else { return };
        for (b, m) in r.iter().enumerate() {
            for c in 0..2 {
                let db = if m.peak[c] > 1e-5 { 20.0 * m.peak[c].log10() } else { -90.0 };
                let lv = &mut self.level[b][c];
                *lv = if db > *lv { db } else { (*lv - 24.0 * dt).max(db) };
                let h = &mut self.hold[b][c];
                if db >= h.0 {
                    *h = (db, 1.5);
                } else {
                    h.1 -= dt;
                    if h.1 <= 0.0 {
                        h.0 = (h.0 - 24.0 * dt).max(*lv);
                    }
                }
            }
            // gain reduction: instant up, 30 dB/s down
            let g = &mut self.gr[b];
            let fall = 30.0 * dt;
            g.gate = m.gr.gate.max(g.gate - fall).max(0.0);
            g.comp = m.gr.comp.max(g.comp - fall).max(0.0);
            g.de_esser = m.gr.de_esser.max(g.de_esser - fall).max(0.0);
        }
        self.last = r;
    }
}

fn meter(ui: &mut egui::Ui, view: &MixerView, b: usize, height: f32) {
    let (r, resp) = ui.allocate_exact_size(egui::vec2(22.0, height), egui::Sense::hover());
    let p = ui.painter_at(r);
    p.rect_filled(r, egui::CornerRadius::same(2), theme::BG);
    let y = |db: f32| r.bottom() - ((db - METER_MIN) / (METER_MAX - METER_MIN)).clamp(0.0, 1.0) * r.height();
    for c in 0..2 {
        let x0 = r.left() + 2.0 + c as f32 * 10.0;
        let lv = view.level[b][c];
        let top = y(lv);
        // green → yellow (−12) → red (−3): drawn as three stacked spans
        for (lo, hi, col) in [(METER_MIN, -12.0, theme::AUDIO), (-12.0, -3.0, theme::SELECTION), (-3.0, METER_MAX, theme::PLAYHEAD)] {
            if lv > lo {
                let a = y(lo);
                let bb = y(hi.min(lv)).max(top);
                p.rect_filled(egui::Rect::from_min_max(egui::pos2(x0, bb), egui::pos2(x0 + 8.0, a)), egui::CornerRadius::ZERO, col);
            }
        }
        let h = view.hold[b][c].0;
        if h > METER_MIN {
            let hy = y(h);
            p.line_segment([egui::pos2(x0, hy), egui::pos2(x0 + 8.0, hy)], Stroke::new(1.5_f32, if h > -0.1 { theme::PLAYHEAD } else { theme::TEXT }));
        }
    }
    for db in [0.0f32, -6.0, -12.0, -24.0, -48.0] {
        let yy = y(db);
        p.line_segment([egui::pos2(r.left(), yy), egui::pos2(r.left() + 2.0, yy)], Stroke::new(0.5_f32, theme::TEXT_DIM));
    }
    let m = &view.last[b];
    resp.on_hover_text(format!(
        "Peak L {:.1} / R {:.1} dBFS\nMomentary {} LUFS\nShort-term {} LUFS\nIntegrated {} LUFS\nTrue peak {} dBTP",
        view.level[b][0],
        view.level[b][1],
        lufs_text(m.momentary),
        lufs_text(m.short_term),
        lufs_text(m.integrated),
        lufs_text(m.true_peak_db)
    ));
}

/// Which strip is drawn.
struct StripInfo<'a> {
    slot: usize,
    label: &'a str,
    /// Shown under the name (a user bus's destination).
    sub: Option<&'a str>,
    color: Color32,
    master: bool,
}

/// One channel strip with a `fader_h` tall fader; returns the strip's height
/// without the fader, so the caller can fit the next frame's fader to the drawer.
#[allow(clippy::too_many_arguments)]
fn strip(ui: &mut egui::Ui, info: StripInfo, s: &mut BusSettings, view: &mut MixerView, req: &mut MixerRequests, solo_active: bool, clock: AutoClock, fader_h: f32) -> f32 {
    let b = info.slot;
    let selected = view.selected == Some(b);
    let frame = egui::Frame::new()
        .fill(if selected { theme::WIDGET } else { theme::PANEL })
        .stroke(Stroke::new(1.0_f32, if selected { info.color } else { theme::BORDER }))
        .corner_radius(egui::CornerRadius::same(4))
        .inner_margin(egui::Margin::same(4));
    let shown = frame.show(ui, |ui| {
        ui.set_width(96.0);
        ui.vertical_centered(|ui| {
            ui.add(egui::Label::new(egui::RichText::new(info.label).strong().color(info.color)).truncate());
            if let Some(sub) = info.sub {
                ui.label(egui::RichText::new(sub).small().color(theme::TEXT_DIM));
            }
            let dimmed = s.mute || (solo_active && !s.solo && !info.master);
            let touching = |view: &MixerView, pan: bool| matches!(&view.touch, Some(t) if t.slot == b && t.pan == pan);
            let read_gain = (!s.auto_off && !s.gain_auto.is_empty()) || touching(view, false);
            let read_pan = (!s.auto_off && !s.pan_auto.is_empty()) || touching(view, true);
            let mut gain = if read_gain { s.gain_db_at(clock.t) } else { s.gain_db };
            let mut pan = if read_pan { s.pan_at(clock.t) } else { s.pan };
            let h = fader_h;
            ui.horizontal(|ui| {
                ui.add_space(4.0);
                meter(ui, view, b, h);
                ui.spacing_mut().slider_width = h;
                let r = ui
                    .add(egui::Slider::new(&mut gain, BusSettings::FADER_RANGE).vertical().show_value(false).step_by(0.1))
                    .on_hover_text(format!("{gain:+.1} dB (double-click the value below to type)"));
                automate(view, b, false, s, clock, gain, r.changed(), r.dragged());
            });
            let r = ui.add(egui::DragValue::new(&mut gain).range(BusSettings::FADER_RANGE).speed(0.1).suffix(" dB").max_decimals(1));
            automate(view, b, false, s, clock, gain, r.changed(), r.dragged());
            ui.spacing_mut().slider_width = 80.0;
            let r = ui.add(egui::Slider::new(&mut pan, -1.0..=1.0).show_value(false)).on_hover_text(match pan {
                p if p.abs() < 0.005 => "Balance: centre".to_string(),
                p if p < 0.0 => format!("Balance: L {:.0}", -p * 100.0),
                p => format!("Balance: R {:.0}", p * 100.0),
            });
            automate(view, b, true, s, clock, pan, r.changed(), r.dragged());
            ui.horizontal(|ui| {
                let has = !s.gain_auto.is_empty() || !s.pan_auto.is_empty();
                let read =
                    egui::Button::new(egui::RichText::new("Read").small().color(if has && !s.auto_off { theme::AUDIO } else { theme::TEXT_DIM })).selected(has && !s.auto_off);
                let tip = if has {
                    "Automation: on = the fader and balance follow it, off = the static settings"
                } else {
                    "No automation yet: arm Touch and move the fader while playing"
                };
                if ui.add_enabled(has, read).on_hover_text(tip).on_disabled_hover_text(tip).clicked() {
                    s.auto_off = !s.auto_off;
                }
                let armed = view.armed[b];
                let label = if touching(view, false) || touching(view, true) { "● Touch" } else { "Touch" };
                let w = egui::Button::new(egui::RichText::new(label).small().color(if armed { theme::PLAYHEAD } else { theme::TEXT })).selected(armed);
                if ui.add(w).on_hover_text("Touch write: while playing, fader and balance moves are recorded; on release they glide back to the existing automation").clicked() {
                    view.armed[b] = !armed;
                }
            });
            ui.horizontal(|ui| {
                let m = egui::Button::new(egui::RichText::new("M").strong().color(if s.mute { theme::BG } else { theme::TEXT })).fill(if s.mute {
                    theme::PLAYHEAD
                } else {
                    theme::WIDGET
                });
                if ui.add(m).on_hover_text("Mute (applies to export)").clicked() {
                    s.mute = !s.mute;
                }
                if !info.master {
                    let so = egui::Button::new(egui::RichText::new("S").strong().color(if s.solo { theme::BG } else { theme::TEXT })).fill(if s.solo {
                        theme::SELECTION
                    } else {
                        theme::WIDGET
                    });
                    if ui.add(so).on_hover_text("Solo (monitoring only — export ignores it)").clicked() {
                        s.solo = !s.solo;
                    }
                }
                let fx_on = s.eq.is_active() || s.fx.is_active() || s.plugins.iter().any(|p| !p.bypass);
                let fx = egui::Button::new(egui::RichText::new("FX").color(if fx_on { info.color } else { theme::TEXT })).selected(selected);
                if ui.add(fx).on_hover_text("EQ, inserts, CLAP / VST3 plug-ins and automation for this bus").clicked() {
                    view.selected = if selected { None } else { Some(b) };
                }
            });
            let m = view.last[b];
            let line = |ui: &mut egui::Ui, k: &str, v: String| {
                ui.label(egui::RichText::new(format!("{k} {v}")).small().monospace().color(if dimmed { theme::TEXT_DIM } else { theme::TEXT }));
            };
            line(ui, "M", lufs_text(m.momentary));
            line(ui, "S", lufs_text(m.short_term));
            line(ui, "I", lufs_text(m.integrated));
            line(ui, "TP", lufs_text(m.true_peak_db));
            if info.master && ui.small_button("Reset I/TP").on_hover_text("Restart integrated loudness and the true-peak hold on every bus").clicked() {
                req.reset_loudness = true;
            }
        });
    });
    shown.response.rect.height() - fader_h
}

/// CLAP / VST3 inserts of one bus: bypass, remove, parameters, add.
fn plugin_inserts(ui: &mut egui::Ui, plugins: &mut Vec<PluginInsert>, view: &mut MixerView, b: usize) {
    ui.label(egui::RichText::new("After the built-in EQ and inserts, before the fader. Audio effects only; no plug-in windows.").small().color(theme::TEXT_DIM));
    let mut remove = None;
    for (k, p) in plugins.iter_mut().enumerate() {
        ui.push_id(("clap", b, k), |ui| {
            ui.horizontal(|ui| {
                let mut on = !p.bypass;
                if ui
                    .checkbox(&mut on, egui::RichText::new(&p.name).strong())
                    .on_hover_text(format!(
                        "{} {}
{}
Untick to bypass",
                        plugins::format_name(std::path::Path::new(&p.path)),
                        p.id,
                        p.path
                    ))
                    .changed()
                {
                    p.bypass = !on;
                }
                if ui.small_button("✕").on_hover_text("Remove plug-in").clicked() {
                    remove = Some(k);
                }
            });
            match plugins::param_infos(std::path::Path::new(&p.path), &p.id) {
                Ok(infos) if infos.is_empty() => {
                    ui.label(egui::RichText::new("No parameters").small().color(theme::TEXT_DIM));
                }
                Ok(infos) => {
                    for info in infos {
                        let mut v = p.param(info.id).unwrap_or(info.default);
                        let slider = egui::Slider::new(&mut v, info.min..=info.max).text(&info.name).step_by(if info.stepped { 1.0 } else { 0.0 });
                        let r = ui.add(slider).on_hover_text("Double-click to reset");
                        if r.double_clicked() {
                            p.set_param(info.id, info.default);
                        } else if r.changed() {
                            p.set_param(info.id, v);
                        }
                    }
                }
                Err(e) => {
                    ui.label(egui::RichText::new(format!("Not loaded: {e}")).small().color(theme::PLAYHEAD));
                }
            }
        });
        ui.add_space(2.0);
    }
    if let Some(k) = remove {
        plugins.remove(k);
    }
    let add = |plugins: &mut Vec<PluginInsert>, info: &PluginInfo| {
        plugins.push(PluginInsert { path: info.path.to_string_lossy().into_owned(), id: info.id.clone(), name: info.name.clone(), bypass: false, params: Vec::new() });
    };
    ui.menu_button(theme::alpha("+ Add plug-in"), |ui| {
        if view.clap_found.is_none() {
            let (found, errors) = plugins::scan();
            view.clap_found = Some(found);
            view.clap_errors = errors;
        }
        let found = view.clap_found.clone().unwrap_or_default();
        if found.is_empty() {
            ui.label(egui::RichText::new("No CLAP or VST3 effects in the standard folders").small().color(theme::TEXT_DIM));
        }
        for info in &found {
            let kind = plugins::format_name(&info.path);
            let label = if info.vendor.is_empty() { format!("{} ({kind})", info.name) } else { format!("{} — {} ({kind})", info.name, info.vendor) };
            if ui.button(label).on_hover_text(info.path.display().to_string()).clicked() {
                add(plugins, info);
                ui.close_menu();
            }
        }
        ui.separator();
        if ui.button("Browse for a .clap / .vst3 file…").clicked() {
            ui.close_menu();
            if let Some(path) = rfd::FileDialog::new().add_filter("CLAP or VST3 plug-in", &["clap", "vst3"]).pick_file() {
                match plugins::describe(&path) {
                    Ok(v) if v.is_empty() => view.clap_errors.push(format!("{}: no plug-ins inside", path.display())),
                    Ok(v) => v.iter().for_each(|info| add(plugins, info)),
                    Err(e) => view.clap_errors.push(e),
                }
            }
        }
        if ui.button("Rescan plug-in folders").clicked() {
            view.clap_found = None;
        }
    });
    for e in &view.clap_errors {
        ui.label(egui::RichText::new(e).small().color(theme::PLAYHEAD));
    }
}

/// Colour of a mixer slot: a user bus takes its destination's.
fn slot_color(mix: &MixerSettings, slot: usize) -> Color32 {
    match slot {
        0..5 => role_color(BusId::ALL[slot]),
        _ => mix.user.get(slot - 5).map_or(theme::TEXT, |u| role_color(BusId::Role(u.feeds)).gamma_multiply(0.8)),
    }
}

/// The mixer drawer contents. Edits `mix` in place.
pub fn mixer_panel(ui: &mut egui::Ui, mix: &mut MixerSettings, view: &mut MixerView, clock: AutoClock) -> MixerRequests {
    let mut req = MixerRequests::default();
    let solo_active = mix.any_solo();
    if view.selected.is_some_and(|b| mix.slot(b).is_none()) {
        view.selected = None;
    }
    ui.horizontal(|ui| {
        ui.label(egui::RichText::new("Audio Mixer").strong());
        ui.label(
            egui::RichText::new("Clips route to their bus (Inspector → Audio) → role bus → Master (−1 dBFS limiter). Loudness per EBU R128; meters follow what the speakers play.")
                .small()
                .color(theme::TEXT_DIM),
        );
        if solo_active {
            ui.label(egui::RichText::new("SOLO").strong().color(theme::SELECTION));
        }
    });
    // The fader takes what the drawer has left after the rest of a strip. That
    // rest is measured from the last frame, not guessed: sizing the fader from
    // the available height minus a too-small constant made every strip a bit
    // taller than the drawer, the drawer grew to fit, and the next frame's
    // fader grew again until it hit its cap; the drawer could not be shrunk.
    let overhead_id = ui.id().with("mixer-strip-overhead");
    let overhead = ui.data(|d| d.get_temp::<f32>(overhead_id)).unwrap_or(230.0);
    let fader_h = (ui.available_height() - overhead).clamp(80.0, 260.0);
    let mut tallest = 0.0_f32;
    ui.horizontal_top(|ui| {
        egui::ScrollArea::horizontal().id_salt("mixer-strips").auto_shrink([true, false]).show(ui, |ui| {
            ui.horizontal_top(|ui| {
                let mut strip = |ui: &mut egui::Ui, info: StripInfo, s: &mut BusSettings, view: &mut MixerView, req: &mut MixerRequests| {
                    tallest = tallest.max(strip(ui, info, s, view, req, solo_active, clock, fader_h));
                };
                for bus in &BusId::ALL[..4] {
                    let info = StripInfo { slot: bus.index(), label: bus.label(), sub: None, color: role_color(*bus), master: false };
                    strip(ui, info, mix.bus_mut(*bus), view, &mut req);
                }
                for u in 0..mix.user.len().min(MAX_USER_BUSSES) {
                    let color = slot_color(mix, 5 + u);
                    let ub = &mut mix.user[u];
                    let (name, sub) = (ub.name.clone(), format!("→ {}", BusId::Role(ub.feeds).label()));
                    let info = StripInfo { slot: 5 + u, label: &name, sub: Some(&sub), color, master: false };
                    strip(ui, info, &mut ub.settings, view, &mut req);
                }
                if mix.user.len() < MAX_USER_BUSSES {
                    ui.menu_button("+ Bus", |ui| {
                        ui.label(egui::RichText::new("A submix bus feeding…").small().color(theme::TEXT_DIM));
                        for role in AudioRole::ALL {
                            if ui.button(BusId::Role(role).label()).clicked() {
                                let n = mix.user.len() + 1;
                                if mix.add_user_bus(format!("Bus {n}"), role).is_some() {
                                    view.selected = Some(4 + mix.user.len());
                                }
                                ui.close_menu();
                            }
                        }
                    })
                    .response
                    .on_hover_text("Add a submix bus: route clips to it in Inspector → Audio, process them together, then it feeds a role bus");
                }
                let info = StripInfo { slot: 4, label: BusId::Master.label(), sub: None, color: role_color(BusId::Master), master: true };
                strip(ui, info, mix.bus_mut(BusId::Master), view, &mut req);
            });
        });
        if let Some(b) = view.selected {
            ui.separator();
            egui::ScrollArea::vertical().id_salt("mixer-inserts").auto_shrink([false, false]).show(ui, |ui| {
                let color = slot_color(mix, b);
                let mut delete = false;
                if b < 5 {
                    let bus = BusId::ALL[b];
                    ui.horizontal(|ui| {
                        ui.label(egui::RichText::new(format!("{} bus", bus.label())).strong().color(color));
                        if ui.small_button("Reset bus").clicked() {
                            *mix.bus_mut(bus) = BusSettings::default();
                        }
                    });
                } else if let Some(ub) = mix.user.get_mut(b - 5) {
                    ui.horizontal(|ui| {
                        ui.add(egui::TextEdit::singleline(&mut ub.name).desired_width(120.0).text_color(color));
                        egui::ComboBox::from_id_salt(("bus-feeds", ub.id)).selected_text(format!("→ {}", BusId::Role(ub.feeds).label())).show_ui(ui, |ui| {
                            for role in AudioRole::ALL {
                                ui.selectable_value(&mut ub.feeds, role, BusId::Role(role).label());
                            }
                        });
                        if ui.small_button("Reset").clicked() {
                            ub.settings = BusSettings::default();
                        }
                        delete = ui.small_button("Delete bus").on_hover_text("Clips sent here go back to their role bus").clicked();
                    });
                }
                if delete {
                    mix.user.remove(b - 5);
                    // the strips after it moved down one slot
                    for k in b..BUS_SLOTS {
                        view.armed[k] = view.armed.get(k + 1).copied().unwrap_or(false);
                    }
                    view.touch = None;
                    view.selected = None;
                    return;
                }
                let gr = view.gr[b];
                let Some(s) = mix.slot_mut(b) else { return };
                let n = s.gain_auto.points.len() + s.pan_auto.points.len();
                egui::CollapsingHeader::new(if n == 0 { "Automation".to_string() } else { format!("Automation ({n} points)") }).id_salt(("bus-auto", b)).default_open(n > 0).show(
                    ui,
                    |ui| {
                        ui.label(egui::RichText::new("Fader").small());
                        lane_editor(ui, &mut s.gain_auto, -60.0..=12.0, clock, egui::Id::new(("lane-gain", b)), &|v| format!("{v:+.1} dB"));
                        ui.label(egui::RichText::new("Balance").small());
                        lane_editor(ui, &mut s.pan_auto, -1.0..=1.0, clock, egui::Id::new(("lane-pan", b)), &|v| format!("{v:+.2}"));
                        ui.horizontal(|ui| {
                            if ui.small_button("Key at playhead").on_hover_text("Add the current fader and balance as points at the playhead").clicked() {
                                let (g, p) = (s.gain_db_at(clock.t), s.pan_at(clock.t));
                                s.gain_auto.set(clock.t, g, Time::from_secs_f64(0.02));
                                s.pan_auto.set(clock.t, p, Time::from_secs_f64(0.02));
                                s.auto_off = false;
                            }
                            if ui.add_enabled(!s.gain_auto.is_empty(), egui::Button::new("Clear fader").small()).clicked() {
                                s.gain_auto = AutoLane::default();
                            }
                            if ui.add_enabled(!s.pan_auto.is_empty(), egui::Button::new("Clear balance").small()).clicked() {
                                s.pan_auto = AutoLane::default();
                            }
                        });
                    },
                );
                ui.collapsing("EQ", |ui| eq_editor(ui, &mut s.eq, egui::Id::new(("bus-eq", b))));
                ui.collapsing("Inserts", |ui| fx_editor(ui, &mut s.fx, egui::Id::new(("bus-fx", b)), Some(gr)));
                let n = s.plugins.len();
                egui::CollapsingHeader::new(if n == 0 { "Plug-ins".to_string() } else { format!("Plug-ins ({n})") })
                    .id_salt(("bus-clap", b))
                    .default_open(n > 0)
                    .show(ui, |ui| plugin_inserts(ui, &mut s.plugins, view, b));
            });
        }
    });
    if tallest > 0.0 && (tallest - overhead).abs() > 0.5 {
        ui.data_mut(|d| d.insert_temp(overhead_id, tallest));
        ui.ctx().request_repaint();
    }
    finish_touch(ui, mix, view, clock);
    req
}
