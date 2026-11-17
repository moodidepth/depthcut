//! Title text layout and glyph rasterisation (ab_glyph) into a coverage
//! mask. Effects (shadow, pill, colour) are applied on the GPU.

use ab_glyph::{point, Font, FontVec, PxScale, ScaleFont};
use nle_core::{TitleAnim, TitleParams, TitleStyle};
use std::path::{Path, PathBuf};

/// System fonts used for titles.
pub struct FontBook {
    regular: Option<FontVec>,
    bold: Option<FontVec>,
    /// Heaviest available sans for viral captions (family "Heavy").
    heavy: Option<FontVec>,
    dirs: Vec<PathBuf>,
}

const REGULAR_CANDIDATES: &[&str] = &["segoeui.ttf", "arial.ttf", "DejaVuSans.ttf", "NotoSans-Regular.ttf", "LiberationSans-Regular.ttf", "Cantarell-Regular.otf", "Ubuntu-R.ttf"];
const BOLD_CANDIDATES: &[&str] = &["segoeuib.ttf", "arialbd.ttf", "DejaVuSans-Bold.ttf", "NotoSans-Bold.ttf", "LiberationSans-Bold.ttf", "Cantarell-Bold.otf", "Ubuntu-B.ttf"];

const HEAVY_CANDIDATES: &[&str] = &[
    "Montserrat-Black.ttf",
    "Montserrat-ExtraBold.ttf",
    "Poppins-ExtraBold.ttf",
    "Poppins-Black.ttf",
    "seguibl.ttf",
    "ariblk.ttf",
    "NotoSans-Black.ttf",
    "LiberationSans-Bold.ttf",
];

fn font_dirs() -> Vec<PathBuf> {
    let mut v = Vec::new();
    if cfg!(windows) {
        let windir = std::env::var_os("WINDIR").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("C:\\Windows"));
        v.push(windir.join("Fonts"));
        if let Some(local) = std::env::var_os("LOCALAPPDATA") {
            v.push(PathBuf::from(local).join("Microsoft\\Windows\\Fonts"));
        }
    } else {
        for d in ["/usr/share/fonts", "/usr/local/share/fonts", "/System/Library/Fonts", "/Library/Fonts"] {
            v.push(PathBuf::from(d));
        }
        if let Some(home) = std::env::var_os("HOME") {
            v.push(PathBuf::from(home).join(".local/share/fonts"));
        }
    }
    v
}

fn find_file(dirs: &[PathBuf], name: &str, depth: usize) -> Option<PathBuf> {
    for d in dirs {
        let p = d.join(name);
        if p.is_file() {
            return Some(p);
        }
        if depth > 0 {
            if let Ok(rd) = std::fs::read_dir(d) {
                let subs: Vec<PathBuf> = rd.filter_map(|e| e.ok()).map(|e| e.path()).filter(|p| p.is_dir()).collect();
                if let Some(f) = find_file(&subs, name, depth - 1) {
                    return Some(f);
                }
            }
        }
    }
    None
}

fn load(path: &Path) -> Option<FontVec> {
    FontVec::try_from_vec(std::fs::read(path).ok()?).ok()
}

impl FontBook {
    pub fn system() -> Self {
        let dirs = font_dirs();
        let pick = |cands: &[&str]| cands.iter().find_map(|c| find_file(&dirs, c, 3)).and_then(|p| load(&p));
        let regular = pick(REGULAR_CANDIDATES);
        let bold = pick(BOLD_CANDIDATES);
        let heavy = pick(HEAVY_CANDIDATES);
        if regular.is_none() {
            log::warn!("no system font found; titles will not render");
        }
        FontBook { regular, bold, heavy, dirs }
    }

    pub fn has_fonts(&self) -> bool {
        self.regular.is_some()
    }

    /// Try `<family>.ttf/.otf` first, then the defaults.
    fn family(&self, family: &str, bold: bool) -> Option<FontVec> {
        let f = family.trim();
        if !f.is_empty() && !f.eq_ignore_ascii_case("sans") {
            let base = f.replace(' ', "");
            let names = if bold {
                vec![format!("{base}-Bold.ttf"), format!("{base}bd.ttf"), format!("{base}-Bold.otf")]
            } else {
                vec![format!("{base}.ttf"), format!("{base}-Regular.ttf"), format!("{base}.otf"), format!("{base}-Regular.otf")]
            };
            for n in names {
                if let Some(font) = find_file(&self.dirs, &n, 3).and_then(|p| load(&p)) {
                    return Some(font);
                }
            }
        }
        None
    }
}

/// Rasterised title ready for the GPU title shader.
#[derive(Clone, Debug, PartialEq)]
pub struct TextMask {
    pub width: u32,
    pub height: u32,
    /// Four 8-bit channels per pixel, row-major (`Rgba8Unorm`): glyph
    /// coverage, coverage of the highlighted (spoken) word, coverage of
    /// upcoming (dimmed) words, unused.
    pub data: Vec<u8>,
    /// Where the mask sits in the frame (px): x, y, w, h.
    pub rect: [f32; 4],
    /// Background pill rectangle (px): x0, y0, x1, y1.
    pub pill: [f32; 4],
}

impl TextMask {
    /// Glyph coverage at mask pixel (x, y).
    pub fn coverage(&self, x: u32, y: u32) -> u8 {
        self.data[((y * self.width + x) * 4) as usize]
    }
    /// Highlighted-word coverage at mask pixel (x, y).
    pub fn highlight(&self, x: u32, y: u32) -> u8 {
        self.data[((y * self.width + x) * 4 + 1) as usize]
    }
    /// Upcoming (not yet spoken, dimmed) word coverage at mask pixel (x, y).
    pub fn dimmed(&self, x: u32, y: u32) -> u8 {
        self.data[((y * self.width + x) * 4 + 2) as usize]
    }
}

struct Line<'a> {
    text: &'a str,
    font: &'a FontVec,
    scale: PxScale,
    /// Index of this line's first word in the title's word list.
    first_word: usize,
}

fn line_width(l: &Line, tracking: f32) -> f32 {
    let sf = l.font.as_scaled(l.scale);
    let mut w = 0.0;
    let mut prev = None;
    for c in l.text.chars() {
        let id = l.font.glyph_id(c);
        if let Some(p) = prev {
            w += sf.kern(p, id);
        }
        w += sf.h_advance(id) + tracking;
        prev = Some(id);
    }
    (w - tracking).max(0.0)
}

struct Placed {
    c: char,
    x: f32,
    advance: f32,
    word: Option<usize>,
}

/// Pen positions of every character of a line (x relative to the line start).
fn place(l: &Line, tracking: f32) -> Vec<Placed> {
    let sf = l.font.as_scaled(l.scale);
    let mut out = Vec::new();
    let mut x = 0.0;
    let mut prev = None;
    let mut word = l.first_word;
    let mut in_word = false;
    for c in l.text.chars() {
        let id = l.font.glyph_id(c);
        if let Some(p) = prev {
            x += sf.kern(p, id);
        }
        let w = if c.is_whitespace() {
            if in_word {
                word += 1;
            }
            in_word = false;
            None
        } else {
            in_word = true;
            Some(word)
        };
        let advance = sf.h_advance(id);
        out.push(Placed { c, x, advance, word: w });
        x += advance + tracking;
        prev = Some(id);
    }
    out
}

/// Lay out and rasterise a title for a `frame_w × frame_h` frame with its
/// animation state (typewriter, tracking, highlighted word + pop).
pub fn rasterize(p: &TitleParams, anim: &TitleAnim, frame_w: u32, frame_h: u32, fonts: &FontBook) -> Option<TextMask> {
    let regular = fonts.regular.as_ref()?;
    let k = frame_h as f32 / 1080.0;
    let lower_third = p.style == TitleStyle::LowerThird;
    let heavy = p.font_family.trim().eq_ignore_ascii_case(nle_core::captions::HEAVY_FONT);
    let custom_main = if heavy { None } else { fonts.family(&p.font_family, lower_third) };
    let main_font = if heavy {
        fonts.heavy.as_ref().or(fonts.bold.as_ref()).unwrap_or(regular)
    } else {
        custom_main.as_ref().or(if lower_third { fonts.bold.as_ref() } else { None }).unwrap_or(regular)
    };
    let main_scale = PxScale::from((p.font_size * k).max(4.0));
    let sub_scale = PxScale::from((p.font_size * k * 0.55).max(4.0));
    let tracking = (p.tracking + anim.tracking_add) * k;

    let main_words = p.text.split_whitespace().count();
    let mut lines = vec![Line { text: p.text.as_str(), font: main_font, scale: main_scale, first_word: 0 }];
    if !p.subtitle.is_empty() {
        lines.push(Line { text: p.subtitle.as_str(), font: regular, scale: sub_scale, first_word: main_words });
    }
    let widths: Vec<f32> = lines.iter().map(|l| line_width(l, tracking)).collect();
    let heights: Vec<f32> = lines
        .iter()
        .map(|l| {
            let s = l.font.as_scaled(l.scale);
            s.ascent() - s.descent() + s.line_gap()
        })
        .collect();
    let block_w = widths.iter().copied().fold(1.0, f32::max).ceil();
    let block_h = heights.iter().sum::<f32>().ceil().max(1.0);

    let (fw, fh) = (frame_w as f32, frame_h as f32);
    let anchor = [p.position[0] * fw, p.position[1] * fh];
    let origin = if lower_third { anchor } else { [anchor[0] - block_w * 0.5, anchor[1] - block_h * 0.5] };
    // margin for the popped word and the outline
    let outline = if p.outline_enabled { p.outline_width * k } else { 0.0 };
    let pop = (anim.word_scale - 1.0).max(0.0);
    let lift = anim.word_lift * k;
    let margin = (outline + main_scale.y * pop * 0.6 + lift.abs() + 2.0).ceil();
    let (w, h) = ((block_w + 2.0 * margin) as u32, (block_h + 2.0 * margin) as u32);
    let mut data = vec![0u8; (w * h * 4) as usize];

    let mut visible = anim.visible_chars.unwrap_or(usize::MAX);
    // mask-local boxes (x0, y0, x1, y1) of the active word (popped) and the
    // word the pill glides from, for the sliding word pill
    let mut cur_box: Option<[f32; 4]> = None;
    let mut from_box: Option<[f32; 4]> = None;
    let mut y = margin;
    for (i, l) in lines.iter().enumerate() {
        let sf = l.font.as_scaled(l.scale);
        let x0 = if lower_third { margin } else { margin + (block_w - widths[i]) * 0.5 };
        let baseline = y + sf.ascent();
        let mid_y = y + heights[i] * 0.5;
        let placed = place(l, tracking);
        // extent of the highlighted word on this line (for the pop scale)
        let hw = anim.highlight_word;
        let word_span = |want: Option<usize>| {
            placed.iter().filter(|g| g.word.is_some() && g.word == want).fold(None, |acc: Option<(f32, f32)>, g| {
                let (a, b) = (g.x, g.x + g.advance);
                Some(acc.map_or((a, b), |(lo, hi)| (lo.min(a), hi.max(b))))
            })
        };
        let span = word_span(hw);
        if p.word_pill {
            let (top, bot) = (y, y + heights[i]);
            if let Some((lo, hi)) = span {
                // the popped, lifted word
                let (cx, cy, s) = (x0 + (lo + hi) * 0.5, mid_y, anim.word_scale.max(1.0));
                let (hw_, hh) = ((hi - lo) * 0.5 * s, (bot - top) * 0.5 * s);
                cur_box = Some([cx - hw_, cy - hh + lift, cx + hw_, cy + hh + lift]);
            }
            if let Some((lo, hi)) = word_span(anim.pill_from) {
                from_box = Some([x0 + lo, top, x0 + hi, bot]);
            }
        }
        for g in &placed {
            if visible == 0 {
                break;
            }
            visible -= 1;
            let highlighted = g.word.is_some() && g.word == hw;
            let dimmed = p.dim_color.is_some() && matches!((g.word, anim.dim_from), (Some(wi), Some(d)) if wi >= d);
            let (gx, gy, scale) = match (highlighted, span) {
                (true, Some((lo, hi))) if anim.word_scale != 1.0 || lift != 0.0 => {
                    let cx = x0 + (lo + hi) * 0.5;
                    let s = anim.word_scale;
                    let gx = cx + (x0 + g.x - cx) * s;
                    let gy = mid_y + (baseline - mid_y) * s + lift;
                    (gx, gy, PxScale::from(l.scale.y * s))
                }
                _ => (x0 + g.x, baseline, l.scale),
            };
            let id = l.font.glyph_id(g.c);
            let glyph = id.with_scale_and_position(scale, point(gx, gy));
            if let Some(og) = l.font.outline_glyph(glyph) {
                let b = og.px_bounds();
                og.draw(|px, py, cov| {
                    let px = b.min.x as i32 + px as i32;
                    let py = b.min.y as i32 + py as i32;
                    if px >= 0 && py >= 0 && (px as u32) < w && (py as u32) < h {
                        let idx = ((py as u32 * w + px as u32) * 4) as usize;
                        let v = (cov.clamp(0.0, 1.0) * 255.0) as u8;
                        data[idx] = data[idx].max(v);
                        if highlighted {
                            data[idx + 1] = data[idx + 1].max(v);
                        } else if dimmed {
                            data[idx + 2] = data[idx + 2].max(v);
                        }
                    }
                });
            }
        }
        y += heights[i];
    }
    let pad = p.pill_padding * k;
    let rect = [origin[0] - margin, origin[1] - margin, w as f32, h as f32];
    let mut pill = [origin[0] - pad, origin[1] - pad * 0.6, origin[0] + block_w + pad, origin[1] + block_h + pad * 0.6];
    if p.word_pill {
        pill = match cur_box {
            Some(cur) => {
                let b = word_pill_box(from_box, cur, anim.pill_glide);
                let (px, py) = (pad * 0.45, pad * 0.2);
                let (ox, oy) = (rect[0], rect[1]);
                [ox + b[0] - px, oy + b[1] - py, ox + b[2] + px, oy + b[3] + py]
            }
            // no active word: no pill
            None => [0.0; 4],
        };
    }
    Some(TextMask { width: w, height: h, data, rect, pill })
}

/// Sliding word pill: exponential glide (`glide` 0..1) of the box from the
/// previous word to the active one.
pub fn word_pill_box(from: Option<[f32; 4]>, to: [f32; 4], glide: f32) -> [f32; 4] {
    let g = glide.clamp(0.0, 1.0);
    match from {
        Some(f) => std::array::from_fn(|i| f[i] + (to[i] - f[i]) * g),
        None => to,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn typewriter_and_highlight_masks() {
        let fonts = FontBook::system();
        if !fonts.has_fonts() {
            return;
        }
        let p = TitleParams { text: "Hello big world".into(), font_size: 120.0, ..Default::default() };
        let full = rasterize(&p, &TitleAnim::default(), 1920, 1080, &fonts).unwrap();
        let lit = |m: &TextMask| (0..m.height).flat_map(|y| (0..m.width).map(move |x| (x, y))).filter(|&(x, y)| m.coverage(x, y) > 0).count();
        let none = TitleAnim { visible_chars: Some(0), ..Default::default() };
        assert_eq!(lit(&rasterize(&p, &none, 1920, 1080, &fonts).unwrap()), 0);
        let half = TitleAnim { visible_chars: Some(5), ..Default::default() };
        let n_half = lit(&rasterize(&p, &half, 1920, 1080, &fonts).unwrap());
        assert!(n_half > 0 && n_half < lit(&full));
        // the highlighted word is marked in the second channel, and only it
        let hl = TitleAnim { highlight_word: Some(1), word_scale: 1.2, ..Default::default() };
        let m = rasterize(&p, &hl, 1920, 1080, &fonts).unwrap();
        let marked: Vec<u32> = (0..m.height).flat_map(|y| (0..m.width).map(move |x| (x, y))).filter(|&(x, y)| m.highlight(x, y) > 0).map(|(x, _)| x).collect();
        assert!(!marked.is_empty());
        let (lo, hi) = (*marked.iter().min().unwrap() as f32, *marked.iter().max().unwrap() as f32);
        let mid = m.width as f32 / 2.0;
        assert!(lo < mid && hi > mid - 1.0 && hi - lo < m.width as f32 * 0.5, "middle word highlighted: {lo}..{hi} of {}", m.width);

        // the word pill wraps the active word and glides from the previous one
        let wp = TitleParams { word_pill: true, pill_enabled: true, ..p.clone() };
        let at = |glide: f32| {
            let a = TitleAnim { highlight_word: Some(1), pill_from: Some(0), pill_glide: glide, ..Default::default() };
            rasterize(&wp, &a, 1920, 1080, &fonts).unwrap().pill
        };
        let (start, half, end) = (at(0.0), at(0.5), at(1.0));
        assert!(start[0] < end[0] && start[2] < end[2], "pill moves right: {start:?} -> {end:?}");
        assert!((half[0] - (start[0] + end[0]) * 0.5).abs() < 0.5);
        let ml = rasterize(&wp, &TitleAnim { highlight_word: Some(1), ..Default::default() }, 1920, 1080, &fonts).unwrap();
        let x_mid = ml.rect[0] + (lo + hi) * 0.5;
        assert!(end[0] < x_mid && end[2] > x_mid, "pill centred on the word");
        // the lift moves the highlighted word up
        let up = rasterize(&p, &TitleAnim { highlight_word: Some(1), word_lift: -6.0, ..Default::default() }, 1920, 1080, &fonts).unwrap();
        let top_y = |m: &TextMask| (0..m.height).find(|&y| (0..m.width).any(|x| m.highlight(x, y) > 0)).unwrap() as f32 + m.rect[1];
        let flat = rasterize(&p, &TitleAnim { highlight_word: Some(1), ..Default::default() }, 1920, 1080, &fonts).unwrap();
        assert!(top_y(&up) < top_y(&flat) - 3.0, "lifted {} vs {}", top_y(&up), top_y(&flat));

        // three tones: word 0 spoken (plain), word 1 active (g), word 2 upcoming (b)
        let tp = TitleParams { dim_color: Some([1.0, 1.0, 1.0, 0.42]), ..p.clone() };
        let a = TitleAnim { highlight_word: Some(1), dim_from: Some(2), ..Default::default() };
        let m = rasterize(&tp, &a, 1920, 1080, &fonts).unwrap();
        let px = |f: &dyn Fn(u32, u32) -> bool| (0..m.height).flat_map(|y| (0..m.width).map(move |x| (x, y))).filter(|&(x, y)| f(x, y)).map(|(x, _)| x).collect::<Vec<_>>();
        let dim = px(&|x, y| m.dimmed(x, y) > 0);
        let hl = px(&|x, y| m.highlight(x, y) > 0);
        assert!(!dim.is_empty() && *dim.iter().min().unwrap() > *hl.iter().max().unwrap(), "only the last word is dimmed");
        assert!(px(&|x, y| m.dimmed(x, y) > 0 && m.highlight(x, y) > 0).is_empty());
        // without a dim colour nothing is dimmed
        let plain = rasterize(&p, &a, 1920, 1080, &fonts).unwrap();
        assert!((0..plain.height).all(|y| (0..plain.width).all(|x| plain.dimmed(x, y) == 0)));
    }

    #[test]
    fn word_pill_box_lerps() {
        let a = [0.0, 0.0, 10.0, 10.0];
        let b = [20.0, 2.0, 40.0, 14.0];
        assert_eq!(word_pill_box(None, b, 0.0), b);
        assert_eq!(word_pill_box(Some(a), b, 0.0), a);
        assert_eq!(word_pill_box(Some(a), b, 1.0), b);
        assert_eq!(word_pill_box(Some(a), b, 0.5), [10.0, 1.0, 25.0, 12.0]);
    }
}
