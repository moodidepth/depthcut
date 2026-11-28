//! Reusable inspector widgets: keyframeable parameter rows and colour wheels.

use crate::theme;
use eframe::egui::{self, Color32, Pos2, Sense, Stroke, Vec2};
use nle_core::{AudioTrackInfo, ColorWheel, Param, Time};

/// Display mapping for a parameter (`shown = value * scale`).
pub struct ParamSpec {
    pub label: &'static str,
    pub min: f64,
    pub max: f64,
    pub scale: f64,
    pub speed: f64,
    pub suffix: &'static str,
    pub default: f64,
    pub decimals: usize,
}

impl ParamSpec {
    pub const fn new(label: &'static str, min: f64, max: f64, default: f64) -> Self {
        ParamSpec { label, min, max, scale: 1.0, speed: 0.01, suffix: "", default, decimals: 2 }
    }
    pub const fn scaled(mut self, scale: f64, suffix: &'static str) -> Self {
        self.scale = scale;
        self.suffix = suffix;
        self
    }
    pub const fn speed(mut self, speed: f64) -> Self {
        self.speed = speed;
        self
    }
    pub const fn decimals(mut self, d: usize) -> Self {
        self.decimals = d;
        self
    }
}

/// One row: label · value drag · keyframe diamond · reset.
/// `t` is clip-local time. Returns true when the parameter changed.
pub fn param_row(ui: &mut egui::Ui, spec: &ParamSpec, p: &mut Param, t: Time, frame: Time) -> bool {
    let mut changed = false;
    ui.horizontal(|ui| {
        ui.add_sized([96.0, 20.0], egui::Label::new(egui::RichText::new(spec.label).color(theme::TEXT_DIM)).truncate());
        let mut shown = p.at(t) * spec.scale;
        let resp = ui.add(
            egui::DragValue::new(&mut shown)
                .speed(spec.speed * spec.scale)
                .range(spec.min * spec.scale..=spec.max * spec.scale)
                .suffix(spec.suffix)
                .max_decimals(spec.decimals)
                .min_decimals(spec.decimals.min(1)),
        );
        if resp.changed() {
            p.set(t, shown / spec.scale);
            changed = true;
        }
        // keyframe diamond
        let on_key = p.curve.as_ref().and_then(|c| c.key_near(t, frame)).is_some();
        let (rect, r) = ui.allocate_exact_size(Vec2::splat(18.0), Sense::click());
        let c = rect.center();
        let d = 5.5;
        let pts = vec![c + Vec2::new(0.0, -d), c + Vec2::new(d, 0.0), c + Vec2::new(0.0, d), c + Vec2::new(-d, 0.0)];
        let col = if on_key {
            theme::SNAP
        } else if p.is_animated() {
            theme::TEXT
        } else {
            theme::TEXT_DIM
        };
        let fill = if on_key { col } else { Color32::TRANSPARENT };
        ui.painter().add(egui::Shape::convex_polygon(pts, fill, Stroke::new(1.2_f32, if r.hovered() { Color32::WHITE } else { col })));
        if r.on_hover_text(if on_key { "Delete keyframe" } else { "Add keyframe" }).clicked() {
            if on_key {
                if let Some(curve) = &mut p.curve {
                    curve.remove_near(t, frame);
                    if curve.is_empty() {
                        p.clear_keyframes();
                    }
                }
            } else {
                p.add_keyframe(t);
            }
            changed = true;
        }
        if ui.small_button("↺").on_hover_text("Reset").clicked() {
            p.clear_keyframes();
            p.value = spec.default;
            changed = true;
        }
    });
    changed
}

/// Simple labelled slider for non-animated `f32` settings.
pub fn slider_f32(ui: &mut egui::Ui, label: &str, v: &mut f32, range: std::ops::RangeInclusive<f32>, suffix: &str) -> bool {
    ui.horizontal(|ui| {
        ui.add_sized([96.0, 20.0], egui::Label::new(egui::RichText::new(label).color(theme::TEXT_DIM)).truncate());
        ui.add(egui::Slider::new(v, range).suffix(suffix)).changed()
    })
    .inner
}

/// Picker for the audio track a multi-track recording plays (Medal / OBS
/// clips keep the game, voice chat and the mic apart). Returns the track
/// clicked, if it differs from `selected`.
pub fn audio_track_combo(ui: &mut egui::Ui, salt: impl std::hash::Hash, tracks: &[AudioTrackInfo], selected: usize) -> Option<usize> {
    let mut pick = selected;
    let current = tracks.get(selected).map(|t| t.label()).unwrap_or_else(|| "Track 0".into());
    egui::ComboBox::from_id_salt(salt)
        .selected_text(current)
        .width(220.0)
        .show_ui(ui, |ui| {
            for t in tracks {
                let ch = match t.channels {
                    1 => "mono".to_string(),
                    2 => "stereo".to_string(),
                    n => format!("{n} ch"),
                };
                ui.selectable_value(&mut pick, t.index, format!("{} · {ch}", t.label()));
            }
        })
        .response
        .on_hover_text(
            "Which audio track of the file plays, shows as a waveform and is analysed (kills, beats, loudness). Pick the game track to leave voice chat and the mic out.",
        );
    (pick != selected).then_some(pick)
}

/// A colour button that writes back only what the user picks. egui's own
/// button round-trips the colour through HSVA every frame, which nudges
/// values like 0.04 to 0.04000002: an edit nobody made, recorded on sight.
pub fn color_well(ui: &mut egui::Ui, color: &mut [f32; 4]) -> bool {
    let mut c = *color;
    let changed = ui.color_edit_button_rgba_unmultiplied(&mut c).changed();
    if changed {
        *color = c;
    }
    changed
}

/// [`slider_f32`] that clamps only what the user drags or types, so a value already
/// beyond `range` is shown as-is rather than rewritten (and recorded) on sight.
pub fn slider_f32_edits(ui: &mut egui::Ui, label: &str, v: &mut f32, range: std::ops::RangeInclusive<f32>, suffix: &str) -> bool {
    ui.horizontal(|ui| {
        ui.add_sized([96.0, 20.0], egui::Label::new(egui::RichText::new(label).color(theme::TEXT_DIM)).truncate());
        // an empty range (0..=0 when the other fade fills the clip) would otherwise
        // print as many decimals as the float has
        ui.add(egui::Slider::new(v, range).suffix(suffix).clamping(egui::SliderClamping::Edits).max_decimals(2)).changed()
    })
    .inner
}

const WHEEL_MAX: f32 = 0.5;

fn wheel_xy(w: &ColorWheel) -> Vec2 {
    // (r, g, b) = m·(cos a, cos(a-120°), cos(a+120°))  ⇒  1.5·m·(cos a, sin a)
    let x = w.r - 0.5 * w.g - 0.5 * w.b;
    let y = 0.866_025_4 * (w.g - w.b);
    Vec2::new(x, y) / (1.5 * WHEEL_MAX)
}

fn set_wheel_xy(w: &mut ColorWheel, p: Vec2) {
    let p = if p.length() > 1.0 { p.normalized() } else { p };
    let m = p.length() * WHEEL_MAX;
    let a = p.y.atan2(p.x);
    let third = std::f32::consts::TAU / 3.0;
    w.r = m * a.cos();
    w.g = m * (a - third).cos();
    w.b = m * (a + third).cos();
}

fn hue_color(a: f32) -> Color32 {
    let third = std::f32::consts::TAU / 3.0;
    let f = |x: f32| ((0.5 + 0.5 * x).clamp(0.0, 1.0) * 255.0) as u8;
    Color32::from_rgb(f(a.cos()), f((a - third).cos()), f((a + third).cos()))
}

/// Lift / gamma / gain colour wheel with a master slider underneath.
pub fn color_wheel(ui: &mut egui::Ui, label: &str, w: &mut ColorWheel, size: f32) -> bool {
    let mut changed = false;
    ui.vertical(|ui| {
        ui.set_width(size + 8.0);
        ui.label(egui::RichText::new(label).small().color(theme::TEXT_DIM));
        let (rect, resp) = ui.allocate_exact_size(Vec2::splat(size), Sense::click_and_drag());
        let c = rect.center();
        let r = size * 0.5 - 3.0;
        let p = ui.painter();
        // hue ring
        let n = 48;
        for i in 0..n {
            let a0 = i as f32 / n as f32 * std::f32::consts::TAU;
            let a1 = (i + 1) as f32 / n as f32 * std::f32::consts::TAU;
            // screen y is down: draw angle a at (cos a, -sin a)
            let pt = |a: f32, rr: f32| Pos2::new(c.x + a.cos() * rr, c.y - a.sin() * rr);
            p.add(egui::Shape::convex_polygon(vec![pt(a0, r), pt(a1, r), pt(a1, r - 5.0), pt(a0, r - 5.0)], hue_color((a0 + a1) * 0.5), Stroke::NONE));
        }
        p.circle_filled(c, r - 6.0, Color32::from_rgb(0x1C, 0x1C, 0x1F));
        p.line_segment([Pos2::new(c.x - r + 8.0, c.y), Pos2::new(c.x + r - 8.0, c.y)], Stroke::new(0.5_f32, theme::BORDER));
        p.line_segment([Pos2::new(c.x, c.y - r + 8.0), Pos2::new(c.x, c.y + r - 8.0)], Stroke::new(0.5_f32, theme::BORDER));
        let xy = wheel_xy(w);
        let puck = Pos2::new(c.x + xy.x * (r - 8.0), c.y - xy.y * (r - 8.0));
        p.circle_filled(puck, 5.0, Color32::WHITE);
        p.circle_stroke(puck, 5.0, Stroke::new(1.0_f32, Color32::BLACK));
        if resp.dragged() || resp.clicked() {
            if let Some(pos) = resp.interact_pointer_pos() {
                let v = Vec2::new((pos.x - c.x) / (r - 8.0), -(pos.y - c.y) / (r - 8.0));
                set_wheel_xy(w, v);
                changed = true;
            }
        }
        if resp.double_clicked() {
            w.r = 0.0;
            w.g = 0.0;
            w.b = 0.0;
            w.master = 0.0;
            changed = true;
        }
        let s = ui.add(egui::Slider::new(&mut w.master, -1.0..=1.0).show_value(false));
        changed |= s.on_hover_text("Master").changed();
    });
    changed
}

/// Timecode-ish compact seconds label: `12.5s`, `1:02.0`.
pub fn secs_label(t: Time) -> String {
    let s = t.as_secs_f64();
    if s >= 60.0 {
        format!("{}:{:04.1}", (s / 60.0).floor(), s % 60.0)
    } else {
        format!("{s:.1}s")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn color_well_leaves_an_untouched_colour_alone() {
        let ctx = egui::Context::default();
        let start = [0.04, 0.52, 1.0, 0.85];
        let mut c = start;
        // the raw egui button drifts this colour; the well must not
        let mut raw = start;
        for _ in 0..3 {
            let _ = ctx.run(egui::RawInput::default(), |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    assert!(!color_well(ui, &mut c));
                    ui.color_edit_button_rgba_unmultiplied(&mut raw);
                });
            });
        }
        assert_eq!(c, start);
        assert_ne!(raw, start, "egui no longer drifts colours; color_well may be unnecessary");
    }
}
