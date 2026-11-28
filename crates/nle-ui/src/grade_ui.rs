//! Grading across clips: Copy / Paste Grade, Apply Grade from Previous Clip,
//! Paste Attributes, and the stills gallery (grab a still, apply its grade,
//! compare it with the viewer in a split, shot-match a clip to it).

use crate::app::{App, NoBlock};
use crate::theme;
use eframe::egui::{self, Color32, CornerRadius, Pos2, Rect, Sense, Stroke, StrokeKind, Vec2};
use nle_core::attributes::{self, AttributeSet, KeyTiming};
use nle_core::{eval, Clip, ClipId, ClipKind, ColorParams, Command, Still, Time};
use nle_render::matching::{put_match_node, shot_match, SHOT_MATCH_NODE};
use nle_render::RenderOptions;
use std::collections::HashMap;
use std::path::PathBuf;

/// The open Paste Attributes window.
pub struct PasteDialog {
    pub sel: AttributeSet,
    pub timing: KeyTiming,
}

/// What a click in the gallery asks for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GalleryAction {
    Grab,
    Apply(u64),
    Match(u64),
    Compare(u64),
    Delete(u64),
}

pub struct GradeUi {
    /// Copied grade and the duration of the clip it came from.
    pub copied: Option<(ColorParams, Time)>,
    pub paste: Option<PasteDialog>,
    /// The groups ticked last time, offered again.
    pub last_sel: AttributeSet,
    pub last_timing: KeyTiming,
    /// The still shown left of the split in the viewer.
    pub compare: Option<u64>,
    /// Split position, 0..1 of the image width.
    pub split: f32,
    /// Loaded still images (None: no file).
    images: HashMap<u64, Option<egui::TextureHandle>>,
}

impl Default for GradeUi {
    fn default() -> Self {
        let (last_sel, last_timing) = load_paste_choice().unwrap_or((AttributeSet::NONE, KeyTiming::Maintain));
        GradeUi { copied: None, paste: None, last_sel, last_timing, compare: None, split: 0.5, images: HashMap::new() }
    }
}

/// The Paste Attributes ticks, remembered between sessions so the dialog
/// never starts with everything (video included) ticked.
fn paste_choice_file() -> PathBuf {
    crate::media::data_dir().join("paste_attributes.json")
}

fn load_paste_choice() -> Option<(AttributeSet, KeyTiming)> {
    let text = std::fs::read_to_string(paste_choice_file()).ok()?;
    serde_json::from_str(&text).ok()
}

fn save_paste_choice(sel: AttributeSet, timing: KeyTiming) {
    let file = paste_choice_file();
    let _ = std::fs::create_dir_all(file.parent().unwrap_or(std::path::Path::new(".")));
    match serde_json::to_string(&(sel, timing)) {
        Ok(json) => {
            if let Err(e) = std::fs::write(&file, json) {
                log::warn!("could not remember the Paste Attributes choice: {e}");
            }
        }
        Err(e) => log::warn!("could not remember the Paste Attributes choice: {e}"),
    }
}

impl GradeUi {
    fn image(&mut self, ctx: &egui::Context, still: &Still) -> Option<egui::TextureHandle> {
        self.images
            .entry(still.id)
            .or_insert_with(|| {
                let (px, w, h) = read_png(&still_path(still))?;
                Some(ctx.load_texture(format!("rcp-still-{}", still.id), egui::ColorImage::from_rgba_unmultiplied([w as usize, h as usize], &px), egui::TextureOptions::LINEAR))
            })
            .clone()
    }
}

pub fn stills_dir() -> PathBuf {
    nle_media::paths::local_data_dir().join("stills")
}

pub fn still_path(s: &Still) -> PathBuf {
    stills_dir().join(s.file_name())
}

fn write_png(path: &std::path::Path, w: u32, h: u32, rgba: &[u8]) -> Result<(), String> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    let file = std::fs::File::create(path).map_err(|e| e.to_string())?;
    let mut enc = png::Encoder::new(std::io::BufWriter::new(file), w, h);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    let mut writer = enc.write_header().map_err(|e| e.to_string())?;
    writer.write_image_data(rgba).map_err(|e| e.to_string())
}

fn read_png(path: &std::path::Path) -> Option<(Vec<u8>, u32, u32)> {
    let file = std::fs::File::open(path).ok()?;
    let mut reader = png::Decoder::new(std::io::BufReader::new(file)).read_info().ok()?;
    let mut buf = vec![0; reader.output_buffer_size()?];
    let info = reader.next_frame(&mut buf).ok()?;
    if info.color_type != png::ColorType::Rgba || info.bit_depth != png::BitDepth::Eight {
        return None;
    }
    buf.truncate(info.buffer_size());
    Some((buf, info.width, info.height))
}

/// Clips a grade can go on.
fn gradeable(c: &Clip) -> bool {
    !c.is_gap() && (c.has_video() || matches!(c.kind, ClipKind::Adjustment | ClipKind::Title(_)))
}

impl App {
    /// Selected clips, else the clip under the playhead; only ones with video.
    fn grade_targets(&self) -> Vec<ClipId> {
        let ids: Vec<ClipId> = if self.selection.is_empty() { self.target_clip().into_iter().collect() } else { self.selection.clone() };
        ids.into_iter().filter(|id| self.project.timeline.clip(*id).is_some_and(gradeable)).collect()
    }

    pub fn copy_grade(&mut self) {
        let Some(c) = self.grade_targets().first().and_then(|id| self.project.timeline.clip(*id)) else {
            self.status("Select a clip with video to copy its grade");
            return;
        };
        let msg = format!("Copied the grade of \u{201c}{}\u{201d}", c.name);
        self.grade.copied = Some((c.color.clone(), c.duration));
        self.status(msg);
    }

    pub fn paste_grade(&mut self) {
        let Some((g, d)) = self.grade.copied.clone() else {
            self.status("Copy a grade first (Color \u{25b8} Copy Grade)");
            return;
        };
        self.apply_grade(&g, d, "Paste Grade");
    }

    /// Put `grade` (from a clip `dur` long) on the grade targets.
    pub fn apply_grade(&mut self, grade: &ColorParams, dur: Time, label: &'static str) {
        let ids = self.grade_targets();
        let cmd = {
            let targets: Vec<&Clip> = ids.iter().filter_map(|id| self.project.timeline.clip(*id)).collect();
            attributes::paste_grade_commands(grade, dur, &targets, KeyTiming::Maintain, label)
        };
        match cmd {
            Some(cmd) => {
                if self.exec(cmd) {
                    self.status(format!("{label}: {} clip{}", ids.len(), if ids.len() == 1 { "" } else { "s" }));
                }
            }
            None => self.status("Select clips with video to grade"),
        }
    }

    /// Resolve's "apply grade from one clip prior": the grade of the clip
    /// before the target on its lane.
    pub fn grade_from_previous(&mut self) {
        let Some(id) = self.grade_targets().first().copied() else {
            self.status("Select a clip with video");
            return;
        };
        let placed = self.project.timeline.placed_clips();
        let Some(me) = placed.iter().find(|p| p.clip.id == id) else { return };
        let prev = placed.iter().filter(|p| p.lane == me.lane && p.end() <= me.start && p.clip.id != id && gradeable(p.clip)).max_by_key(|p| p.end());
        let Some(prev) = prev.map(|p| (p.clip.color.clone(), p.clip.duration)) else {
            self.status("No earlier clip on this lane to take a grade from");
            return;
        };
        drop(placed);
        let cmd = self.project.timeline.clip(id).and_then(|c| attributes::paste_grade_commands(&prev.0, prev.1, &[c], KeyTiming::Maintain, "Grade from Previous Clip"));
        if let Some(cmd) = cmd {
            self.exec(cmd);
            self.status("Applied the previous clip's grade");
        }
    }

    pub fn open_paste_attributes(&mut self) {
        if self.copied.is_empty() {
            self.status("Copy a clip first (Ctrl+C), then select clips and Paste Attributes");
            return;
        }
        if self.action_targets().is_empty() {
            self.status("Select the clips to paste attributes onto");
            return;
        }
        self.grade.paste = Some(PasteDialog { sel: self.grade.last_sel, timing: self.grade.last_timing });
    }

    pub fn paste_attributes_window(&mut self, ctx: &egui::Context) {
        let Some(mut d) = self.grade.paste.take() else { return };
        let Some(src) = self.copied.first().map(|c| c.clip.clone()) else { return };
        let mut open = true;
        let mut go = false;
        let mut cancel = false;
        egui::Window::new("Paste Attributes").open(&mut open).collapsible(false).resizable(false).anchor(egui::Align2::CENTER_CENTER, Vec2::ZERO).show(ctx, |ui| {
            ui.label(egui::RichText::new(format!("From \u{201c}{}\u{201d}", src.name)).color(theme::TEXT_DIM));
            ui.add_space(4.0);
            let s = &mut d.sel;
            ui.horizontal(|ui| {
                if ui.small_button("All video").on_hover_text("Tick transform, crop, compositing, color and video effects").clicked() {
                    (s.transform, s.crop, s.compositing, s.color, s.effects) = (true, true, true, true, true);
                }
                if ui.small_button("All audio").on_hover_text("Tick volume and audio effects").clicked() {
                    (s.volume, s.audio_effects) = (true, true);
                }
                if ui.small_button("Clear").on_hover_text("Untick everything").clicked() {
                    *s = AttributeSet::NONE;
                }
            });
            ui.label(egui::RichText::new("Video").strong());
            ui.checkbox(&mut s.transform, "Transform (position, scale, rotation, anchor)");
            ui.checkbox(&mut s.crop, "Crop");
            ui.checkbox(&mut s.compositing, "Compositing (opacity, blend mode)");
            ui.checkbox(&mut s.color, "Color (grade, nodes, LUT)");
            ui.checkbox(&mut s.effects, "Video effects");
            ui.label(egui::RichText::new("Audio").strong());
            ui.checkbox(&mut s.volume, "Volume");
            ui.checkbox(&mut s.audio_effects, "Audio effects (EQ, dynamics, restoration)");
            ui.label(egui::RichText::new("Time").strong());
            ui.checkbox(&mut s.speed, "Speed (constant rate; ramps are not copied)");
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                ui.label("Keyframes");
                ui.radio_value(&mut d.timing, KeyTiming::Maintain, "Maintain timing");
                ui.radio_value(&mut d.timing, KeyTiming::Stretch, "Stretch to fit");
            });
            ui.separator();
            ui.horizontal(|ui| {
                if ui.add_enabled(d.sel.any(), egui::Button::new("Paste")).on_disabled_hover_text("Tick what to paste first").clicked() {
                    go = true;
                }
                if ui.button("Cancel").clicked() {
                    cancel = true;
                }
            });
        });
        if go {
            self.grade.last_sel = d.sel;
            self.grade.last_timing = d.timing;
            save_paste_choice(d.sel, d.timing);
            let ids = self.action_targets();
            let cmd = {
                let targets: Vec<&Clip> = ids.iter().filter_map(|id| self.project.timeline.clip(*id)).collect();
                attributes::paste_commands(&src, &targets, &d.sel, d.timing)
            };
            match cmd {
                Some(cmd) => {
                    if self.exec(cmd) {
                        self.status(format!("Pasted attributes onto {} clip{}", ids.len(), if ids.len() == 1 { "" } else { "s" }));
                    }
                }
                None => self.status("Nothing to paste onto (the copied clip itself is skipped)"),
            }
        } else if open && !cancel {
            self.grade.paste = Some(d);
        }
    }

    /// The timeline frame at the playhead rendered without the viewer's
    /// overlays (wipe, source clip, angles), as RGBA8.
    fn render_frame(&mut self, tl: &nle_core::Timeline) -> Option<(Vec<u8>, u32, u32)> {
        let rate = self.project.timeline.format.rate;
        let t = Time::from_frames(self.playhead.frames_floor(rate), rate);
        let comp = eval::evaluate(tl, t);
        let p = self.preview.as_mut()?;
        p.renderer.render(&comp, &mut NoBlock(&self.project), RenderOptions::default());
        let px = p.renderer.read_display();
        let (w, h) = p.renderer.size();
        p.last = None;
        Some((px, w, h))
    }

    pub fn grab_still(&mut self) {
        let tl = self.project.timeline.clone();
        let Some((mut px, w, h)) = self.render_frame(&tl) else {
            self.status("Grab Still needs the GPU viewer");
            return;
        };
        for p in px.as_chunks_mut::<4>().0 {
            p[3] = 255;
        }
        let (grade, dur, name) = match self.target_clip().and_then(|id| self.project.timeline.clip(id)).filter(|c| gradeable(c)) {
            Some(c) => (c.color.clone(), c.duration, c.name.clone()),
            None => (ColorParams::default(), Time::ZERO, "Frame".to_string()),
        };
        let tc = nle_core::format_timecode(self.playhead, self.project.timeline.format.rate);
        let n = self.project.stills.len() + 1;
        let still = Still::new(format!("{n}. {name}"), grade, dur, tc, w, h);
        if let Err(e) = write_png(&still_path(&still), w, h, &px) {
            self.status(format!("Couldn't save the still: {e}"));
            return;
        }
        self.project.stills.push(still);
        self.assets_changed = true;
        self.status(format!("Grabbed still {n}"));
    }

    /// Add (or redo) a "Shot Match" node on the target clip that brings its
    /// balance and contrast to the still's.
    pub fn match_to_still(&mut self, still: u64) {
        let Some(s) = self.project.stills.iter().find(|s| s.id == still).cloned() else { return };
        let Some((reference, _, _)) = read_png(&still_path(&s)) else {
            self.status("This still's image is missing \u{2014} grab it again to match against it");
            return;
        };
        let Some(id) = self.grade_targets().first().copied() else {
            self.status("Select the clip to match");
            return;
        };
        // the frame without an earlier match, so matching again converges
        let mut tl = self.project.timeline.clone();
        if let Some(c) = tl.clip_mut(id) {
            c.color.corrections.retain(|n| n.name != SHOT_MATCH_NODE);
        }
        let Some((source, _, _)) = self.render_frame(&tl) else { return };
        let Some(mut clip) = self.project.timeline.clip(id).cloned() else { return };
        let Some(node) = shot_match(&source, &reference, clip.color.working) else {
            self.status("Not enough picture to match");
            return;
        };
        clip.color.enabled = true;
        put_match_node(&mut clip.color.corrections, node);
        if self.exec(Command::UpdateClip { clip: Box::new(clip) }) {
            self.status(format!("Matched to \u{201c}{}\u{201d} (a \u{201c}{SHOT_MATCH_NODE}\u{201d} node you can adjust)", s.name));
        }
    }

    pub fn delete_still(&mut self, still: u64) {
        if let Some(i) = self.project.stills.iter().position(|s| s.id == still) {
            let s = self.project.stills.remove(i);
            let _ = std::fs::remove_file(still_path(&s));
            self.grade.images.remove(&still);
            if self.grade.compare == Some(still) {
                self.grade.compare = None;
            }
            self.assets_changed = true;
        }
    }

    pub fn toggle_still_compare(&mut self) {
        if self.grade.compare.is_some() {
            self.grade.compare = None;
        } else if let Some(s) = self.project.stills.last() {
            self.grade.compare = Some(s.id);
        } else {
            self.status("Grab a still first (Color \u{25b8} Grab Still)");
        }
    }

    pub fn gallery_action(&mut self, a: GalleryAction) {
        match a {
            GalleryAction::Grab => self.grab_still(),
            GalleryAction::Apply(id) => {
                if let Some(s) = self.project.stills.iter().find(|s| s.id == id) {
                    let (g, d) = (s.grade.clone(), s.clip_duration);
                    self.apply_grade(&g, d, "Apply Still Grade");
                }
            }
            GalleryAction::Match(id) => self.match_to_still(id),
            GalleryAction::Compare(id) => self.grade.compare = if self.grade.compare == Some(id) { None } else { Some(id) },
            GalleryAction::Delete(id) => self.delete_still(id),
        }
    }
}

/// The gallery section of the Color tab.
pub fn gallery(ui: &mut egui::Ui, stills: &[Still], g: &mut GradeUi) -> Option<GalleryAction> {
    let mut out = None;
    egui::CollapsingHeader::new(egui::RichText::new(format!("Gallery ({})", stills.len())).strong()).default_open(true).show(ui, |ui| {
        ui.horizontal(|ui| {
            if ui.button("\u{1f4f7} Grab Still").on_hover_text("Save the viewer frame and this clip's grade").clicked() {
                out = Some(GalleryAction::Grab);
            }
            if g.compare.is_some() && ui.button("End Compare").clicked() {
                g.compare = None;
            }
        });
        if stills.is_empty() {
            ui.label(
                egui::RichText::new("Stills keep a frame and its grade: apply the grade to other clips, compare them side by side in the viewer, or shot-match a clip to one.")
                    .small()
                    .color(theme::TEXT_DIM),
            );
        }
        let ctx = ui.ctx().clone();
        for s in stills.iter().rev() {
            ui.horizontal(|ui| {
                let tw = 96.0;
                let th = if s.width > 0 { tw * s.height as f32 / s.width as f32 } else { 54.0 };
                let (r, resp) = ui.allocate_exact_size(Vec2::new(tw, th.clamp(24.0, 120.0)), Sense::click());
                match g.image(&ctx, s) {
                    Some(tex) => {
                        ui.painter().image(tex.id(), r, Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)), Color32::WHITE);
                    }
                    None => {
                        ui.painter().rect_filled(r, CornerRadius::same(2), theme::GAP);
                        ui.painter().text(r.center(), egui::Align2::CENTER_CENTER, "no image", egui::FontId::proportional(10.0), theme::TEXT_DIM);
                    }
                }
                if g.compare == Some(s.id) {
                    ui.painter().rect_stroke(r, CornerRadius::same(2), Stroke::new(2.0_f32, theme::PLAYHEAD), StrokeKind::Inside);
                }
                if resp.on_hover_text("Click to compare in the viewer").clicked() {
                    out = Some(GalleryAction::Compare(s.id));
                }
                ui.vertical(|ui| {
                    ui.label(&s.name);
                    if !s.timecode.is_empty() {
                        ui.label(egui::RichText::new(&s.timecode).small().monospace().color(theme::TEXT_DIM));
                    }
                    ui.horizontal(|ui| {
                        if ui.small_button("Apply").on_hover_text("Apply this still's grade to the selected clips").clicked() {
                            out = Some(GalleryAction::Apply(s.id));
                        }
                        if ui.small_button("Match").on_hover_text("Shot match the selected clip to this still").clicked() {
                            out = Some(GalleryAction::Match(s.id));
                        }
                        let cmp = if g.compare == Some(s.id) { "Uncompare" } else { "Compare" };
                        if ui.small_button(cmp).clicked() {
                            out = Some(GalleryAction::Compare(s.id));
                        }
                        if ui.small_button("\u{2715}").on_hover_text("Delete still").clicked() {
                            out = Some(GalleryAction::Delete(s.id));
                        }
                    });
                });
            });
        }
    });
    out
}

/// The compared still, drawn over the viewer image left of the split.
/// Returns whether the split handle is being dragged.
pub fn viewer_compare(app: &mut App, ui: &mut egui::Ui, painter: &egui::Painter, img: Rect) -> bool {
    let Some(id) = app.grade.compare else { return false };
    let Some(still) = app.project.stills.iter().find(|s| s.id == id).cloned() else {
        app.grade.compare = None;
        return false;
    };
    let Some(tex) = app.grade.image(ui.ctx(), &still) else { return false };
    let x = img.left() + app.grade.split * img.width();
    let left = Rect::from_min_max(img.min, Pos2::new(x, img.bottom()));
    painter.with_clip_rect(left.intersect(painter.clip_rect())).image(tex.id(), img, Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)), Color32::WHITE);
    painter.line_segment([Pos2::new(x, img.top()), Pos2::new(x, img.bottom())], Stroke::new(1.5_f32, Color32::WHITE));
    painter.text(Pos2::new(x - 8.0, img.top() + 8.0), egui::Align2::RIGHT_TOP, format!("STILL {}", still.name), egui::FontId::proportional(10.0), Color32::WHITE);
    painter.text(Pos2::new(x + 8.0, img.top() + 8.0), egui::Align2::LEFT_TOP, "CURRENT", egui::FontId::proportional(10.0), Color32::WHITE);
    let handle = Rect::from_center_size(Pos2::new(x, img.bottom() - 40.0), Vec2::splat(18.0));
    let hr = ui.interact(handle.expand(4.0), ui.id().with("still-split"), Sense::drag());
    painter.circle_filled(handle.center(), 9.0, theme::SNAP);
    painter.text(handle.center(), egui::Align2::CENTER_CENTER, "\u{27f7}", egui::FontId::proportional(11.0), Color32::BLACK);
    if hr.dragged() {
        if let Some(p) = hr.interact_pointer_pos() {
            app.grade.split = ((p.x - img.left()) / img.width()).clamp(0.0, 1.0);
        }
        return true;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn still_png_round_trips() {
        let dir = std::env::temp_dir().join(format!("rcp-still-test-{}", std::process::id()));
        let path = dir.join("s.png");
        let px: Vec<u8> = (0..4 * 6 * 4).map(|i| (i * 7 % 256) as u8).collect();
        write_png(&path, 4, 6, &px).unwrap();
        assert_eq!(read_png(&path), Some((px, 4, 6)));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
