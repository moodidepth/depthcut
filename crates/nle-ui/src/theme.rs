//! Final Cut Pro-style dark theme.

use eframe::egui::{self, Color32, CornerRadius, Stroke};

pub const BG: Color32 = Color32::from_rgb(0x18, 0x18, 0x1A);
pub const PANEL: Color32 = Color32::from_rgb(0x22, 0x22, 0x25);
pub const BORDER: Color32 = Color32::from_rgb(0x2C, 0x2C, 0x30);
pub const VIDEO: Color32 = Color32::from_rgb(0x0A, 0x84, 0xFF);
pub const AUDIO: Color32 = Color32::from_rgb(0x30, 0xD1, 0x58);
pub const TITLE: Color32 = Color32::from_rgb(0xBF, 0x5A, 0xF2);
pub const SNAP: Color32 = Color32::from_rgb(0xFF, 0x9F, 0x0A);
pub const PLAYHEAD: Color32 = Color32::from_rgb(0xFF, 0x45, 0x3A);

pub const TEXT: Color32 = Color32::from_rgb(0xE5, 0xE5, 0xEA);
pub const TEXT_DIM: Color32 = Color32::from_rgb(0x8E, 0x8E, 0x93);
pub const SELECTION: Color32 = Color32::from_rgb(0xFF, 0xD6, 0x0A);
pub const GAP: Color32 = Color32::from_rgb(0x34, 0x34, 0x38);
pub const VIEWER_BG: Color32 = Color32::from_rgb(0x0B, 0x0B, 0x0C);
pub const WIDGET: Color32 = Color32::from_rgb(0x2E, 0x2E, 0x33);
pub const WIDGET_HOVER: Color32 = Color32::from_rgb(0x3A, 0x3A, 0x40);
pub const SKIMMER: Color32 = Color32::from_rgb(0xFF, 0x6B, 0x8A);

/// Darker fill used for a clip body of the given accent colour.
pub fn clip_fill(accent: Color32) -> Color32 {
    let [r, g, b, _] = accent.to_array();
    Color32::from_rgb((r as f32 * 0.42) as u8, (g as f32 * 0.42) as u8, (b as f32 * 0.42) as u8)
}

pub fn apply(ctx: &egui::Context) {
    let mut v = egui::Visuals::dark();
    v.panel_fill = PANEL;
    v.window_fill = PANEL;
    v.extreme_bg_color = BG;
    v.faint_bg_color = Color32::from_rgb(0x26, 0x26, 0x2A);
    v.window_stroke = Stroke::new(1.0_f32, BORDER);
    v.window_corner_radius = CornerRadius::same(8);
    v.menu_corner_radius = CornerRadius::same(6);
    v.selection.bg_fill = VIDEO.gamma_multiply(0.75);
    v.selection.stroke = Stroke::new(1.0_f32, Color32::WHITE);
    v.hyperlink_color = VIDEO;
    v.override_text_color = Some(TEXT);
    let r = CornerRadius::same(5);
    v.widgets.noninteractive.bg_fill = PANEL;
    v.widgets.noninteractive.weak_bg_fill = PANEL;
    v.widgets.noninteractive.bg_stroke = Stroke::new(1.0_f32, BORDER);
    v.widgets.noninteractive.fg_stroke = Stroke::new(1.0_f32, TEXT);
    v.widgets.noninteractive.corner_radius = r;
    v.widgets.inactive.bg_fill = WIDGET;
    v.widgets.inactive.weak_bg_fill = WIDGET;
    v.widgets.inactive.bg_stroke = Stroke::NONE;
    v.widgets.inactive.fg_stroke = Stroke::new(1.0_f32, TEXT);
    v.widgets.inactive.corner_radius = r;
    v.widgets.hovered.bg_fill = WIDGET_HOVER;
    v.widgets.hovered.weak_bg_fill = WIDGET_HOVER;
    v.widgets.hovered.bg_stroke = Stroke::new(1.0_f32, Color32::from_gray(80));
    v.widgets.hovered.corner_radius = r;
    v.widgets.active.bg_fill = VIDEO.gamma_multiply(0.8);
    v.widgets.active.weak_bg_fill = VIDEO.gamma_multiply(0.8);
    v.widgets.active.corner_radius = r;
    v.widgets.open.weak_bg_fill = WIDGET_HOVER;
    v.widgets.open.corner_radius = r;
    ctx.set_visuals(v);
    // Ctrl +/- zooms the timeline, not the whole UI
    ctx.options_mut(|o| o.zoom_with_keyboard = false);
    ctx.style_mut(|s| {
        s.spacing.item_spacing = egui::vec2(6.0, 5.0);
        s.spacing.button_padding = egui::vec2(8.0, 3.0);
        s.spacing.interact_size.y = 22.0;
        s.spacing.slider_width = 130.0;
    });
}

/// Add a system symbol font as a fallback so arrows, tool icons and
/// geometric glyphs missing from egui's bundled fonts render correctly.
pub fn install_fonts(ctx: &egui::Context) {
    let candidates: &[&str] = if cfg!(target_os = "windows") {
        &[r"C:\Windows\Fonts\seguisym.ttf", r"C:\Windows\Fonts\segoeui.ttf"]
    } else {
        &[
            "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
            "/usr/share/fonts/TTF/DejaVuSans.ttf",
            "/usr/share/fonts/dejavu/DejaVuSans.ttf",
            "/usr/share/fonts/noto/NotoSansSymbols2-Regular.ttf",
            "/usr/share/fonts/truetype/noto/NotoSansSymbols2-Regular.ttf",
        ]
    };
    let mut fonts = egui::FontDefinitions::default();
    let mut added = 0;
    for (i, path) in candidates.iter().enumerate() {
        if let Ok(bytes) = std::fs::read(path) {
            let name = format!("system-symbols-{i}");
            fonts.font_data.insert(name.clone(), std::sync::Arc::new(egui::FontData::from_owned(bytes)));
            for fam in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
                fonts.families.entry(fam).or_default().push(name.clone());
            }
            added += 1;
        }
    }
    if added > 0 {
        ctx.set_fonts(fonts);
    }
}

/// Small section heading used in the inspector and browser.
pub fn heading(ui: &mut egui::Ui, text: &str) {
    ui.add_space(4.0);
    ui.label(egui::RichText::new(text).size(11.5).strong().color(TEXT_DIM));
}
