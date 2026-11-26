//! Final Cut Pro XML (FCPXML 1.10) export and import.
//!
//! Supported: formats, assets (with `media-rep`), the primary storyline
//! (`asset-clip`, `gap`, `title`, `ref-clip` compound clips), connected
//! clips (`lane`), transitions, `timeMap` retiming (speed, reverse, freeze),
//! transform / blend / crop / volume / pan adjustments with keyframes,
//! audio fades and a "Color Adjustments" video filter.

use crate::asset::{AssetKind, AudioStreamInfo, MediaAsset, VideoStreamInfo};
use crate::clip::{AudioRole, AvFlags, BlendMode, Clip, ClipKind, ColorWheel, CompoundClip, Direction, FadeShape, LutRef, TitleParams, TitleStyle, Transition, TransitionKind};
use crate::ids::AssetId;
use crate::keyframe::{Curve, Interpolation, Keyframe, Param};
use crate::project::Project;
use crate::speed::{Retime, RetimeSegment};
use crate::time::{gcd, FrameRate, Time};
use crate::timeline::{Timeline, VideoFormat};
use crate::xml::{self, Element};
use std::collections::HashMap;
use std::path::PathBuf;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum FcpxmlError {
    #[error(transparent)]
    Xml(#[from] xml::XmlError),
    #[error("not an FCPXML document (root is <{0}>)")]
    NotFcpxml(String),
    #[error("no sequence found in document")]
    NoSequence,
    #[error("unknown resource reference `{0}`")]
    UnknownRef(String),
}

const GAP_START: Time = Time::from_secs(3600);
const COLOR_EFFECT: &str = "Color Adjustments";
/// Colour management + correction nodes travel as JSON inside the colour filter.
const COLOR_PIPELINE_PARAM: &str = "rcp.colorPipeline";
/// RustCut Pro effects: one `filter-video` each, the effect as JSON.
const RCP_EFFECT: &str = "RustCut Pro Effect";
const RCP_EFFECT_PARAM: &str = "rcp.effect";
const RCP_MOTION: &str = "RustCut Pro Motion";
const RCP_MOTION_PARAM: &str = "rcp.motion";

/// Tracks, stabilization, follow and Speed Warp (no FCPXML equivalent).
#[derive(serde::Serialize, serde::Deserialize, Default)]
#[serde(default)]
struct MotionMd {
    tracks: Vec<crate::motion::Track>,
    stabilization: Option<crate::motion::Stabilization>,
    follow: Option<crate::motion::Follow>,
    speed_warp: bool,
}

#[derive(serde::Serialize, serde::Deserialize, Default)]
#[serde(default)]
struct ColorPipelineMd {
    input_space: crate::color::ColorSpace,
    working: crate::color::WorkingSpace,
    input_lut: Option<LutRef>,
    corrections: Vec<crate::color::ColorCorrection>,
}
const ADJUSTMENT_EFFECT: &str = "Adjustment Layer";

fn ts(t: Time) -> String {
    t.to_rational_string()
}

fn num(v: f64) -> String {
    let r = (v * 1_000_000.0).round() / 1_000_000.0;
    let s = format!("{r}");
    if s == "-0" {
        "0".into()
    } else {
        s
    }
}

/// Stable 64-bit FNV-1a hash (deterministic asset uids).
fn fnv1a(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        h ^= *b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

// ---------------------------------------------------------------------------
// paths
// ---------------------------------------------------------------------------

pub fn path_to_url(p: &std::path::Path) -> String {
    let s = p.to_string_lossy().replace('\\', "/");
    let mut enc = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'/' | b'-' | b'_' | b'.' | b'~' | b':' => enc.push(b as char),
            _ => enc.push_str(&format!("%{b:02X}")),
        }
    }
    if has_scheme(&s) {
        // already a URL (the built-in synthetic:// demo media): wrapping it in
        // file:/// made it come back as "/synthetic://…", which is neither
        enc
    } else if enc.starts_with('/') {
        format!("file://{enc}")
    } else {
        format!("file:///{enc}")
    }
}

/// `scheme://…` with a scheme longer than one letter (so not `C:/…`).
fn has_scheme(s: &str) -> bool {
    s.split_once("://").is_some_and(|(scheme, _)| scheme.len() > 1 && scheme.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'+' || b == b'-' || b == b'.'))
}

pub fn url_to_path(u: &str) -> PathBuf {
    let rest = u.strip_prefix("file://").unwrap_or(u);
    let rest = rest.strip_prefix("localhost").unwrap_or(rest);
    let bytes = rest.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hex = |b: u8| (b as char).to_digit(16);
            if let (Some(h), Some(l)) = (hex(bytes[i + 1]), hex(bytes[i + 2])) {
                out.push((h * 16 + l) as u8);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    let mut s = String::from_utf8_lossy(&out).into_owned();
    // "/C:/x" → "C:/x"
    let b = s.as_bytes();
    if b.len() > 2 && b[0] == b'/' && b[2] == b':' && b[1].is_ascii_alphabetic() {
        s.remove(0);
    }
    // "/synthetic://x", as older exports wrote demo media → "synthetic://x"
    if s.starts_with('/') && has_scheme(&s[1..]) {
        s.remove(0);
    }
    PathBuf::from(s)
}

// ---------------------------------------------------------------------------
// export
// ---------------------------------------------------------------------------

struct ExportCtx<'a> {
    project: &'a Project,
    resources: Element,
    next_id: usize,
    asset_refs: HashMap<AssetId, String>,
    format_refs: HashMap<(u32, u32, FrameRate), String>,
    effect_refs: HashMap<String, String>,
    seq_format: String,
    text_style_seq: usize,
}

impl<'a> ExportCtx<'a> {
    fn new_id(&mut self) -> String {
        self.next_id += 1;
        format!("r{}", self.next_id)
    }

    fn format_ref(&mut self, w: u32, h: u32, rate: FrameRate) -> String {
        if let Some(r) = self.format_refs.get(&(w, h, rate)) {
            return r.clone();
        }
        let id = self.new_id();
        let name = format!("FFVideoFormat{}p{}", h, rate.label().trim_end_matches('p').replace('.', ""));
        self.resources.push(
            Element::new("format")
                .set("id", id.clone())
                .set("name", name)
                .set("frameDuration", ts(rate.frame_duration()))
                .set("width", w.to_string())
                .set("height", h.to_string())
                .set("colorSpace", "1-1-1 (Rec. 709)"),
        );
        self.format_refs.insert((w, h, rate), id.clone());
        id
    }

    fn effect_ref(&mut self, name: &str, uid: &str) -> String {
        if let Some(r) = self.effect_refs.get(name) {
            return r.clone();
        }
        let id = self.new_id();
        self.resources.push(Element::new("effect").set("id", id.clone()).set("name", name).set("uid", uid));
        self.effect_refs.insert(name.to_string(), id.clone());
        id
    }

    fn asset_ref(&mut self, id: AssetId) -> String {
        if let Some(r) = self.asset_refs.get(&id) {
            return r.clone();
        }
        let rid = self.new_id();
        self.asset_refs.insert(id, rid.clone());
        let Some(a) = self.project.asset(id) else {
            // dangling reference: emit a placeholder
            self.resources.push(Element::new("asset").set("id", rid.clone()).set("name", "Missing").set("start", "0s").set("duration", "0s"));
            return rid;
        };
        let a = a.clone();
        let mut el = Element::new("asset")
            .set("id", rid.clone())
            .set("name", a.name.clone())
            .set("uid", format!("RCP-{:016X}", fnv1a(a.path.to_string_lossy().as_bytes())))
            .set("start", "0s")
            .set("duration", ts(a.duration))
            .set("hasVideo", if a.has_video() { "1" } else { "0" })
            .set("hasAudio", if a.has_audio() { "1" } else { "0" });
        if let Some(v) = &a.video {
            let f = self.format_ref(v.width, v.height, v.frame_rate);
            el = el.set("format", f).set("videoSources", "1");
        }
        if let Some(au) = &a.audio {
            el = el.set("audioSources", "1").set("audioChannels", au.channels.to_string()).set("audioRate", au.sample_rate.to_string());
        }
        el.push(Element::new("media-rep").set("kind", "original-media").set("src", path_to_url(&a.path)));
        if let Some(p) = &a.proxy_path {
            el.push(Element::new("media-rep").set("kind", "proxy-media").set("src", path_to_url(p)));
        }
        let mut md = Element::new("metadata");
        let kind = match a.kind {
            AssetKind::Video => "video",
            AssetKind::Audio => "audio",
            AssetKind::Image => "image",
            AssetKind::Synthetic => "synthetic",
        };
        md.push(Element::new("md").set("key", "com.rustcutpro.kind").set("value", kind));
        if let Some(v) = &a.video {
            md.push(Element::new("md").set("key", "com.rustcutpro.codec").set("value", v.codec.clone()));
            md.push(Element::new("md").set("key", "com.rustcutpro.pixfmt").set("value", v.pix_fmt.clone()));
            md.push(Element::new("md").set("key", "com.rustcutpro.bitdepth").set("value", v.bit_depth.to_string()));
            md.push(Element::new("md").set("key", "com.rustcutpro.colorspace").set("value", v.color_space.clone()));
        }
        if let Some(au) = &a.audio {
            md.push(Element::new("md").set("key", "com.rustcutpro.audiocodec").set("value", au.codec.clone()));
        }
        if !a.tags.is_empty() {
            md.push(Element::new("md").set("key", "com.apple.proapps.mio.keywords").set("value", a.tags.join(", ")));
        }
        el.push(md);
        self.resources.push(el);
        rid
    }
}

fn keyframe_anim(p: &Param, origin: Time, map: impl Fn(f64) -> String) -> Option<Element> {
    let c = p.curve.as_ref().filter(|c| !c.is_empty())?;
    let mut anim = Element::new("keyframeAnimation");
    for k in &c.keys {
        let interp = match k.interp {
            Interpolation::Linear => "linear",
            Interpolation::EaseIn => "easeIn",
            Interpolation::EaseOut => "easeOut",
            // FCPXML has no overshoot/bounce keyframe types; nearest built-in is "ease".
            Interpolation::EaseInOut | Interpolation::Whip | Interpolation::Overshoot | Interpolation::Bounce | Interpolation::SnapPunch | Interpolation::EaseOutBack => "ease",
            Interpolation::Hold => "hold",
            Interpolation::Bezier => "smooth",
        };
        anim.push(Element::new("keyframe").set("time", ts(origin + k.time)).set("value", map(k.value)).set("interp", interp));
    }
    Some(anim)
}

fn param_el(name: &str, p: &Param, origin: Time, map: impl Fn(f64) -> String) -> Option<Element> {
    keyframe_anim(p, origin, map).map(|a| Element::new("param").set("name", name).with_child(a))
}

fn blend_name(b: BlendMode) -> &'static str {
    b.label()
}

fn adjustments(c: &Clip, origin: Time, fmt: &VideoFormat, ctx: &mut ExportCtx) -> Vec<Element> {
    let mut out = Vec::new();
    let aspect = fmt.width as f64 / fmt.height.max(1) as f64;
    let v = &c.video;
    if c.av.video && !c.is_gap() {
        // crop
        let crop = [&v.crop_left, &v.crop_right, &v.crop_top, &v.crop_bottom];
        if crop.iter().any(|p| p.value != 0.0 || p.is_animated()) {
            out.push(
                Element::new("adjust-crop").set("mode", "trim").with_child(
                    Element::new("trim-rect")
                        .set("left", num(v.crop_left.value * 100.0))
                        .set("right", num(v.crop_right.value * 100.0))
                        .set("top", num(v.crop_top.value * 100.0))
                        .set("bottom", num(v.crop_bottom.value * 100.0)),
                ),
            );
        }
        // transform (FCP: position in % of frame height, y up, rotation CCW)
        let mut tr = Element::new("adjust-transform")
            .set("position", format!("{} {}", num(v.position_x.value * aspect * 100.0), num(-v.position_y.value * 100.0)))
            .set("scale", format!("{} {}", num(v.scale_x.value), num(v.scale_y.value)))
            .set("rotation", num(-v.rotation.value))
            .set("anchor", format!("{} {}", num(v.anchor_x.value * aspect * 100.0), num(-v.anchor_y.value * 100.0)));
        if let Some(e) = param_el("rotation", &v.rotation, origin, |x| num(-x)) {
            tr.push(e);
        }
        // Position/scale keyframes are written per axis using RCP param names.
        for (name, p) in [("rcp.positionX", &v.position_x), ("rcp.positionY", &v.position_y), ("rcp.scaleX", &v.scale_x), ("rcp.scaleY", &v.scale_y)] {
            if let Some(e) = param_el(name, p, origin, num) {
                tr.push(e);
            }
        }
        out.push(tr);
        // compositing
        let mut bl = Element::new("adjust-blend").set("amount", num(v.opacity.value)).set("mode", blend_name(v.blend_mode));
        if let Some(e) = param_el("amount", &v.opacity, origin, num) {
            bl.push(e);
        }
        out.push(bl);
        // color
        let col = &c.color;
        if !col.is_identity() || !col.enabled {
            let fx = ctx.effect_ref(COLOR_EFFECT, "FFColorAdjustmentsEffect");
            let wheel = |w: &ColorWheel| format!("{} {} {} {}", num(w.r as f64), num(w.g as f64), num(w.b as f64), num(w.master as f64));
            let mut f = Element::new("filter-video").set("ref", fx).set("name", COLOR_EFFECT);
            if !col.enabled {
                f = f.set("enabled", "0");
            }
            for (name, p) in [("Exposure", &col.exposure), ("Contrast", &col.contrast), ("Saturation", &col.saturation), ("Temperature", &col.temperature), ("Tint", &col.tint)] {
                let mut pe = Element::new("param").set("name", name).set("value", num(p.value));
                if let Some(a) = keyframe_anim(p, origin, num) {
                    pe.push(a);
                }
                f.push(pe);
            }
            f.push(Element::new("param").set("name", "Shadows").set("value", wheel(&col.shadows)));
            f.push(Element::new("param").set("name", "Midtones").set("value", wheel(&col.midtones)));
            f.push(Element::new("param").set("name", "Highlights").set("value", wheel(&col.highlights)));
            if let Some(l) = &col.lut {
                f.push(Element::new("param").set("name", "LUT").set("value", l.path.clone()));
                f.push(Element::new("param").set("name", "LUT Intensity").set("value", num(l.intensity as f64)));
            }
            let md = ColorPipelineMd { input_space: col.input_space, working: col.working, input_lut: col.input_lut.clone(), corrections: col.corrections.clone() };
            if md.input_space != Default::default() || md.working != Default::default() || md.input_lut.is_some() || !md.corrections.is_empty() {
                if let Ok(json) = serde_json::to_string(&md) {
                    f.push(Element::new("param").set("name", COLOR_PIPELINE_PARAM).set("value", json));
                }
            }
            out.push(f);
        }
        for e in &c.effects {
            let fx = ctx.effect_ref(RCP_EFFECT, "com.rustcutpro.effect");
            let mut f = Element::new("filter-video").set("ref", fx).set("name", e.kind.label());
            if !e.enabled {
                f = f.set("enabled", "0");
            }
            if let Ok(json) = serde_json::to_string(e) {
                f.push(Element::new("param").set("name", RCP_EFFECT_PARAM).set("value", json));
            }
            out.push(f);
        }
        let md = MotionMd {
            tracks: c.tracks.clone(),
            stabilization: c.stabilization.clone(),
            follow: c.video.follow.clone(),
            speed_warp: c.retime.as_ref().is_some_and(|r| r.blend == crate::speed::FrameBlendMode::SpeedWarp),
        };
        if !md.tracks.is_empty() || md.stabilization.is_some() || md.follow.is_some() || md.speed_warp {
            if let Ok(json) = serde_json::to_string(&md) {
                let fx = ctx.effect_ref(RCP_MOTION, "com.rustcutpro.motion");
                out.push(Element::new("filter-video").set("ref", fx).set("name", RCP_MOTION).with_child(Element::new("param").set("name", RCP_MOTION_PARAM).set("value", json)));
            }
        }
    }
    if c.has_audio() {
        let a = &c.audio;
        let mut vol = Element::new("adjust-volume").set("amount", format!("{}dB", num(a.volume_db.value)));
        let mut amount = Element::new("param").set("name", "amount");
        let mut has_amount = false;
        if let Some(anim) = keyframe_anim(&a.volume_db, origin, |x| format!("{}dB", num(x))) {
            amount.push(anim);
            has_amount = true;
        }
        let fade_type = match a.fade_shape {
            FadeShape::Linear => "linear",
            FadeShape::SCurve => "easeIn",
            FadeShape::EqualPower => "_3dB",
        };
        if a.fade_in.is_positive() {
            amount.push(Element::new("fadeIn").set("type", fade_type).set("duration", ts(a.fade_in)));
            has_amount = true;
        }
        if a.fade_out.is_positive() {
            amount.push(Element::new("fadeOut").set("type", fade_type).set("duration", ts(a.fade_out)));
            has_amount = true;
        }
        if has_amount {
            vol.push(amount);
        }
        out.push(vol);
        if a.pan.value != 0.0 || a.pan.is_animated() {
            let mut pan = Element::new("adjust-panner").set("mode", "stereo left/right").set("amount", num(a.pan.value * 100.0));
            if let Some(e) = param_el("amount", &a.pan, origin, |x| num(x * 100.0)) {
                pan.push(e);
            }
            out.push(pan);
        }
    }
    out
}

fn time_map(c: &Clip) -> Option<Element> {
    let r = c.retime.as_ref()?;
    let mut tm = Element::new("timeMap");
    let mut t = Time::ZERO;
    let pt = |t: Time, interp: &str| Element::new("timept").set("time", ts(t)).set("value", ts(c.source_in + r.map(t))).set("interp", interp);
    tm.push(pt(Time::ZERO, "linear"));
    for (i, s) in r.segments.iter().enumerate() {
        t += s.duration;
        let next_ramp = r.segments.get(i + 1).is_some_and(|n| n.ramp.is_positive());
        tm.push(pt(t, if next_ramp { "smooth2" } else { "linear" }));
    }
    Some(tm)
}

fn transition_el(tr: &Transition, offset: Time) -> Element {
    Element::new("transition")
        .set("name", tr.kind.label())
        .set("offset", ts(offset))
        .set("duration", ts(tr.duration))
        .with_child(Element::new("param").set("name", "rcp.direction").set("value", tr.direction.label()))
        .with_child(Element::new("param").set("name", "rcp.softness").set("value", num(tr.softness as f64)))
        .with_child(Element::new("param").set("name", "rcp.easing").set("value", tr.easing.label()))
}

/// Returns (element, start attribute used for child offsets).
/// `<md>` key carrying a connected clip's connection point (FCPXML has no
/// attribute for it; FCP ignores unknown metadata keys).
const CONNECTION_POINT_MD: &str = "com.rustcut.connectionPoint";

/// An audition: the active pick first, then the alternates.
fn audition_el(c: &Clip, offset: Time, fmt: &VideoFormat, ctx: &mut ExportCtx) -> Element {
    let mut au = Element::new("audition");
    if c.lane != 0 {
        au = au.set("lane", c.lane.to_string());
    }
    au = au.set("offset", ts(offset));
    let mut active = c.clone();
    active.audition.clear();
    active.lane = 0;
    au.push(clip_el(&active, offset, fmt, ctx));
    for alt in &c.audition {
        let mut a = alt.clone();
        a.lane = 0;
        a.anchor = Time::ZERO;
        a.connected.clear();
        a.audition.clear();
        au.push(clip_el(&a, offset, fmt, ctx));
    }
    au
}

/// A secondary storyline: `<spine lane=… offset=…>` whose clips continue
/// in the parent clip's time from `offset`.
fn storyline_el(c: &Clip, comp: &CompoundClip, offset: Time, fmt: &VideoFormat, ctx: &mut ExportCtx) -> Element {
    let mut sp = spine_el_at(&comp.spine, offset, fmt, ctx);
    sp.attrs = vec![("lane".into(), c.lane.to_string()), ("offset".into(), ts(offset)), ("name".into(), c.name.clone())];
    sp
}

fn clip_el(c: &Clip, offset: Time, fmt: &VideoFormat, ctx: &mut ExportCtx) -> Element {
    if !c.audition.is_empty() {
        return audition_el(c, offset, fmt, ctx);
    }
    let (mut el, start) = match &c.kind {
        ClipKind::Asset { asset } => {
            let r = ctx.asset_ref(*asset);
            let mut e = Element::new("asset-clip").set("ref", r);
            let role = c.audio.role.fcp_role();
            e = e.set("audioRole", role);
            if c.av.video && !c.av.audio {
                e = e.set("srcEnable", "video");
            } else if c.av.audio && !c.av.video {
                e = e.set("srcEnable", "audio");
            }
            (e, c.source_in)
        }
        ClipKind::Gap => (Element::new("gap"), GAP_START),
        ClipKind::Title(p) => {
            let fx = ctx.effect_ref(p.style.label(), &format!(".../Titles.localized/{}.moti", p.style.label()));
            let mut e = Element::new("title").set("ref", fx);
            ctx.text_style_seq += 1;
            let id = format!("ts{}", ctx.text_style_seq);
            let col = |c: [f32; 4]| format!("{} {} {} {}", num(c[0] as f64), num(c[1] as f64), num(c[2] as f64), num(c[3] as f64));
            e.push(
                Element::new("param")
                    .set("name", "Position")
                    .set("key", "9999/999166631/999166633/1/100/101")
                    .set("value", format!("{} {}", num(p.position[0] as f64), num(p.position[1] as f64))),
            );
            e.push(
                Element::new("param")
                    .set("name", "rcp.pill")
                    .set("value", format!("{} {} {} {}", u8::from(p.pill_enabled), col(p.pill_color), num(p.pill_padding as f64), num(p.pill_radius as f64))),
            );
            e.push(Element::new("param").set("name", "rcp.shadowBlur").set("value", num(p.shadow_blur as f64)));
            e.push(Element::new("text").with_child(Element::new("text-style").set("ref", id.clone()).with_text(p.text.clone())));
            if !p.subtitle.is_empty() {
                e.push(Element::new("text").with_child(Element::new("text-style").set("ref", id.clone()).with_text(p.subtitle.clone())));
            }
            let mut style = Element::new("text-style")
                .set("font", p.font_family.clone())
                .set("fontSize", num(p.font_size as f64))
                .set("fontColor", col(p.color))
                .set("tracking", num(p.tracking as f64));
            if p.shadow_enabled {
                style = style.set("shadowColor", col(p.shadow_color)).set("shadowOffset", format!("{} {}", num(p.shadow_offset[0] as f64), num(p.shadow_offset[1] as f64)));
            }
            e.push(Element::new("text-style-def").set("id", id).with_child(style));
            (e, Time::ZERO)
        }
        ClipKind::Adjustment => {
            let fx = ctx.effect_ref(ADJUSTMENT_EFFECT, ".../Titles.localized/Adjustment Layer.moti");
            (Element::new("title").set("ref", fx), Time::ZERO)
        }
        ClipKind::Compound(comp) => {
            let rid = ctx.new_id();
            let mut seq = Element::new("sequence").set("format", ctx.seq_format.clone()).set("duration", ts(comp.duration()));
            seq.push(spine_el(&comp.spine, fmt, ctx));
            ctx.resources.push(Element::new("media").set("id", rid.clone()).set("name", c.name.clone()).with_child(seq));
            (Element::new("ref-clip").set("ref", rid), c.source_in)
        }
    };
    // attributes in FCP order
    let mut attrs = Vec::new();
    if c.lane != 0 {
        attrs.push(("lane".to_string(), c.lane.to_string()));
    }
    attrs.push(("offset".to_string(), ts(offset)));
    attrs.push(("name".to_string(), c.name.clone()));
    attrs.push(("start".to_string(), ts(start)));
    attrs.push(("duration".to_string(), ts(c.duration)));
    if !c.enabled {
        attrs.push(("enabled".to_string(), "0".to_string()));
    }
    let existing = std::mem::take(&mut el.attrs);
    el.attrs = existing.into_iter().take_while(|(k, _)| k == "ref").collect();
    el.attrs.extend(attrs);
    // re-append any element-specific attrs after the common ones
    if let ClipKind::Asset { .. } = c.kind {
        let role = c.audio.role.fcp_role();
        el.attrs.push(("audioRole".into(), role.into()));
        if c.av.video && !c.av.audio {
            el.attrs.push(("srcEnable".into(), "video".into()));
        } else if c.av.audio && !c.av.video {
            el.attrs.push(("srcEnable".into(), "audio".into()));
        }
    }

    let mut body: Vec<Element> = Vec::new();
    if let Some(tm) = time_map(c) {
        body.push(tm);
    }
    if let Some(b) = c.retime.as_ref().map(|r| r.blend) {
        let v = match b {
            crate::speed::FrameBlendMode::FloorFrame => "floor",
            crate::speed::FrameBlendMode::FrameBlend => "frameBlending",
            crate::speed::FrameBlendMode::OpticalFlow | crate::speed::FrameBlendMode::SpeedWarp => "opticalFlow",
            crate::speed::FrameBlendMode::WeightedAccumulation => "weightedAccumulation",
        };
        body.push(Element::new("conform-rate").set("scaleEnabled", "0").set("srcFrameRate", v));
    }
    body.extend(adjustments(c, start, fmt, ctx));
    // Titles: params/text must come first, then adjustments
    let title_children = std::mem::take(&mut el.children);
    for n in title_children {
        if let xml::Node::Element(e) = n {
            el.push(e);
        }
    }
    for b in body {
        el.push(b);
    }
    for cc in &c.connected {
        let child = match &cc.kind {
            ClipKind::Compound(comp) if comp.storyline && cc.audition.is_empty() => storyline_el(cc, comp, start + cc.offset, fmt, ctx),
            _ => clip_el(cc, start + cc.offset, fmt, ctx),
        };
        el.push(child);
    }
    if c.lane != 0 && !c.anchor.is_zero() {
        el.push(Element::new("metadata").with_child(Element::new("md").set("key", CONNECTION_POINT_MD).set("value", ts(c.anchor))));
    }
    el
}

fn spine_el(spine: &[Clip], fmt: &VideoFormat, ctx: &mut ExportCtx) -> Element {
    spine_el_at(spine, Time::ZERO, fmt, ctx)
}

fn spine_el_at(spine: &[Clip], base: Time, fmt: &VideoFormat, ctx: &mut ExportCtx) -> Element {
    let mut sp = Element::new("spine");
    let mut acc = base;
    for (i, c) in spine.iter().enumerate() {
        let prev_real = i > 0 && !spine[i - 1].is_gap();
        if let Some(tr) = &c.transition_in {
            if !prev_real {
                sp.push(transition_el(tr, acc));
            } else if spine[i - 1].transition_out.is_none() {
                sp.push(transition_el(tr, acc - tr.duration.div_i64(2)));
            }
        }
        sp.push(clip_el(c, acc, fmt, ctx));
        acc += c.duration;
        if let Some(tr) = &c.transition_out {
            let next_real = spine.get(i + 1).is_some_and(|n| !n.is_gap());
            let off = if next_real { acc - tr.duration.div_i64(2) } else { acc - tr.duration };
            sp.push(transition_el(tr, off));
        }
    }
    sp
}

/// Serialize a project as an FCPXML 1.10 document.
pub fn export(project: &Project) -> String {
    let tl = &project.timeline;
    let mut ctx = ExportCtx {
        project,
        resources: Element::new("resources"),
        next_id: 0,
        asset_refs: HashMap::new(),
        format_refs: HashMap::new(),
        effect_refs: HashMap::new(),
        seq_format: String::new(),
        text_style_seq: 0,
    };
    ctx.seq_format = ctx.format_ref(tl.format.width, tl.format.height, tl.format.rate);
    // emit every library asset, used or not, so the browser round-trips
    for a in &project.assets {
        ctx.asset_ref(a.id);
    }
    let spine = spine_el(&tl.spine, &tl.format, &mut ctx);
    let seq = Element::new("sequence")
        .set("format", ctx.seq_format.clone())
        .set("duration", ts(tl.duration()))
        .set("tcStart", "0s")
        .set("tcFormat", "NDF")
        .set("audioLayout", "stereo")
        .set("audioRate", "48k")
        .with_child(spine);
    let proj = Element::new("project").set("name", tl.name.clone()).with_child(seq);
    let event = Element::new("event").set("name", project.name.clone()).with_child(proj);
    let library = Element::new("library").with_child(event);
    let root = Element::new("fcpxml").set("version", "1.10").with_child(ctx.resources).with_child(library);
    root.to_xml_string()
}

// ---------------------------------------------------------------------------
// import
// ---------------------------------------------------------------------------

struct ImportCtx<'a> {
    assets: HashMap<String, AssetId>,
    formats: HashMap<String, VideoFormat>,
    effects: HashMap<String, String>,
    media: HashMap<String, &'a Element>,
    text_styles: HashMap<String, Element>,
}

fn parse_rate(frame_duration: &str) -> Option<FrameRate> {
    let s = frame_duration.trim().strip_suffix('s')?;
    let (n, d) = match s.split_once('/') {
        Some((n, d)) => (n.parse::<u64>().ok()?, d.parse::<u64>().ok()?),
        None => (s.parse::<u64>().ok()?, 1),
    };
    if n == 0 {
        return None;
    }
    let g = gcd(n, d);
    Some(FrameRate::new((d / g) as u32, (n / g) as u32))
}

fn attr_time(e: &Element, k: &str) -> Option<Time> {
    e.attr(k).and_then(Time::parse_rational)
}

fn floats(s: &str) -> Vec<f64> {
    s.split_whitespace().filter_map(|x| x.trim_end_matches("dB").parse().ok()).collect()
}

fn parse_anim(param: &Element, origin: Time, map: impl Fn(&str) -> Option<f64>) -> Option<Curve> {
    let anim = param.child("keyframeAnimation")?;
    let mut c = Curve::new();
    for k in anim.children_named("keyframe") {
        let t = attr_time(k, "time")? - origin;
        let v = map(k.attr("value")?)?;
        let interp = match k.attr("interp").unwrap_or("linear") {
            "easeIn" => Interpolation::EaseIn,
            "easeOut" => Interpolation::EaseOut,
            "ease" => Interpolation::EaseInOut,
            "hold" => Interpolation::Hold,
            "smooth" => Interpolation::Bezier,
            _ => Interpolation::Linear,
        };
        c.insert(Keyframe::new(t, v).with_interp(interp));
    }
    Some(c)
}

fn first_f(s: &str) -> Option<f64> {
    floats(s).first().copied()
}

fn apply_adjustments(el: &Element, c: &mut Clip, origin: Time, fmt: &VideoFormat) {
    let aspect = fmt.width as f64 / fmt.height.max(1) as f64;
    for adj in el.elements() {
        match adj.name.as_str() {
            "adjust-crop" => {
                if let Some(r) = adj.child("trim-rect") {
                    let g = |k| r.attr(k).and_then(first_f).unwrap_or(0.0) / 100.0;
                    c.video.crop_left.value = g("left");
                    c.video.crop_right.value = g("right");
                    c.video.crop_top.value = g("top");
                    c.video.crop_bottom.value = g("bottom");
                }
            }
            "adjust-transform" => {
                if let Some(p) = adj.attr("position").map(floats) {
                    if p.len() == 2 {
                        c.video.position_x.value = p[0] / 100.0 / aspect;
                        c.video.position_y.value = -p[1] / 100.0;
                    }
                }
                if let Some(p) = adj.attr("scale").map(floats) {
                    if p.len() == 2 {
                        c.video.scale_x.value = p[0];
                        c.video.scale_y.value = p[1];
                    }
                }
                if let Some(r) = adj.attr("rotation").and_then(first_f) {
                    c.video.rotation.value = -r;
                }
                if let Some(p) = adj.attr("anchor").map(floats) {
                    if p.len() == 2 {
                        c.video.anchor_x.value = p[0] / 100.0 / aspect;
                        c.video.anchor_y.value = -p[1] / 100.0;
                    }
                }
                for p in adj.children_named("param") {
                    let target = match p.attr("name") {
                        Some("rotation") => Some((&mut c.video.rotation, -1.0)),
                        Some("rcp.positionX") => Some((&mut c.video.position_x, 1.0)),
                        Some("rcp.positionY") => Some((&mut c.video.position_y, 1.0)),
                        Some("rcp.scaleX") => Some((&mut c.video.scale_x, 1.0)),
                        Some("rcp.scaleY") => Some((&mut c.video.scale_y, 1.0)),
                        _ => None,
                    };
                    if let Some((param, k)) = target {
                        param.curve = parse_anim(p, origin, |s| first_f(s).map(|v| v * k));
                    }
                }
            }
            "adjust-blend" => {
                if let Some(a) = adj.attr("amount").and_then(first_f) {
                    c.video.opacity.value = a;
                }
                if let Some(m) = adj.attr("mode") {
                    c.video.blend_mode = BlendMode::from_name(m).unwrap_or_default();
                }
                if let Some(p) = adj.children_named("param").find(|p| p.attr("name") == Some("amount")) {
                    c.video.opacity.curve = parse_anim(p, origin, first_f);
                }
            }
            "adjust-volume" => {
                if let Some(a) = adj.attr("amount").and_then(first_f) {
                    c.audio.volume_db.value = a;
                }
                if let Some(p) = adj.children_named("param").find(|p| p.attr("name") == Some("amount")) {
                    c.audio.volume_db.curve = parse_anim(p, origin, first_f);
                    for (tag, is_in) in [("fadeIn", true), ("fadeOut", false)] {
                        if let Some(f) = p.child(tag) {
                            let d = attr_time(f, "duration").unwrap_or_default();
                            c.audio.fade_shape = match f.attr("type") {
                                Some("linear") => FadeShape::Linear,
                                Some("_3dB") => FadeShape::EqualPower,
                                _ => FadeShape::SCurve,
                            };
                            if is_in {
                                c.audio.fade_in = d;
                            } else {
                                c.audio.fade_out = d;
                            }
                        }
                    }
                }
            }
            "adjust-panner" => {
                if let Some(a) = adj.attr("amount").and_then(first_f) {
                    c.audio.pan.value = a / 100.0;
                }
                if let Some(p) = adj.children_named("param").find(|p| p.attr("name") == Some("amount")) {
                    c.audio.pan.curve = parse_anim(p, origin, |s| first_f(s).map(|v| v / 100.0));
                }
            }
            "filter-video" if adj.attr("name") == Some(COLOR_EFFECT) => {
                c.color.enabled = adj.attr("enabled") != Some("0");
                let mut lut_path = None;
                let mut lut_int = 1.0;
                for p in adj.children_named("param") {
                    let v = p.attr("value").unwrap_or("");
                    let wheel = |s: &str| {
                        let f = floats(s);
                        ColorWheel {
                            r: *f.first().unwrap_or(&0.0) as f32,
                            g: *f.get(1).unwrap_or(&0.0) as f32,
                            b: *f.get(2).unwrap_or(&0.0) as f32,
                            master: *f.get(3).unwrap_or(&0.0) as f32,
                        }
                    };
                    let scalar = match p.attr("name") {
                        Some("Exposure") => Some(&mut c.color.exposure),
                        Some("Contrast") => Some(&mut c.color.contrast),
                        Some("Saturation") => Some(&mut c.color.saturation),
                        Some("Temperature") => Some(&mut c.color.temperature),
                        Some("Tint") => Some(&mut c.color.tint),
                        Some("Shadows") => {
                            c.color.shadows = wheel(v);
                            None
                        }
                        Some("Midtones") => {
                            c.color.midtones = wheel(v);
                            None
                        }
                        Some("Highlights") => {
                            c.color.highlights = wheel(v);
                            None
                        }
                        Some("LUT") => {
                            lut_path = Some(v.to_string());
                            None
                        }
                        Some("LUT Intensity") => {
                            lut_int = first_f(v).unwrap_or(1.0) as f32;
                            None
                        }
                        Some(COLOR_PIPELINE_PARAM) => {
                            if let Ok(md) = serde_json::from_str::<ColorPipelineMd>(v) {
                                c.color.input_space = md.input_space;
                                c.color.working = md.working;
                                c.color.input_lut = md.input_lut;
                                c.color.corrections = md.corrections;
                            }
                            None
                        }
                        _ => None,
                    };
                    if let Some(param) = scalar {
                        param.value = first_f(v).unwrap_or(param.value);
                        param.curve = parse_anim(p, origin, first_f);
                    }
                }
                c.color.lut = lut_path.map(|path| LutRef { path, intensity: lut_int });
            }
            "filter-video" if adj.children_named("param").any(|p| p.attr("name") == Some(RCP_MOTION_PARAM)) => {
                let json = adj.children_named("param").find(|p| p.attr("name") == Some(RCP_MOTION_PARAM)).and_then(|p| p.attr("value"));
                if let Some(md) = json.and_then(|j| serde_json::from_str::<MotionMd>(j).ok()) {
                    c.tracks = md.tracks;
                    c.stabilization = md.stabilization;
                    c.video.follow = md.follow;
                    if md.speed_warp {
                        if let Some(r) = c.retime.as_mut() {
                            r.blend = crate::speed::FrameBlendMode::SpeedWarp;
                        }
                    }
                }
            }
            "filter-video" => {
                let json = adj.children_named("param").find(|p| p.attr("name") == Some(RCP_EFFECT_PARAM)).and_then(|p| p.attr("value"));
                if let Some(mut e) = json.and_then(|j| serde_json::from_str::<crate::effects::Effect>(j).ok()) {
                    e.enabled = adj.attr("enabled") != Some("0");
                    e.normalize();
                    c.effects.push(e);
                }
            }
            "timeMap" => {
                let pts: Vec<(Time, Time, bool)> =
                    adj.children_named("timept").filter_map(|p| Some((attr_time(p, "time")?, attr_time(p, "value")?, p.attr("interp") == Some("smooth2")))).collect();
                if pts.len() >= 2 {
                    let mut segs = Vec::new();
                    for w in pts.windows(2) {
                        let dt = w[1].0 - w[0].0;
                        if !dt.is_positive() {
                            continue;
                        }
                        let speed = (w[1].1 - w[0].1).as_secs_f64() / dt.as_secs_f64();
                        let mut seg = RetimeSegment::new(dt, (speed * 1e6).round() / 1e6);
                        if w[0].2 {
                            seg.ramp = dt.div_i64(4);
                        }
                        segs.push(seg);
                    }
                    let start_offset = pts[0].1 - c.source_in;
                    let blend = c.retime.as_ref().map(|r| r.blend).unwrap_or_default();
                    c.retime = Some(Retime { segments: segs, start_offset, lead_speed: None, blend, velocity_curve: None });
                }
            }
            "conform-rate" => {
                let b = match adj.attr("srcFrameRate") {
                    Some("frameBlending") => crate::speed::FrameBlendMode::FrameBlend,
                    Some("opticalFlow") => crate::speed::FrameBlendMode::OpticalFlow,
                    Some("weightedAccumulation") => crate::speed::FrameBlendMode::WeightedAccumulation,
                    _ => crate::speed::FrameBlendMode::FloorFrame,
                };
                if let Some(r) = c.retime.as_mut() {
                    r.blend = b;
                } else {
                    c.retime = Some(Retime { segments: Vec::new(), start_offset: Time::ZERO, lead_speed: None, blend: b, velocity_curve: None });
                }
            }
            _ => {}
        }
    }
    if c.retime.as_ref().is_some_and(|r| r.segments.is_empty()) {
        c.retime = None;
    }
}

fn parse_color4(s: &str) -> Option<[f32; 4]> {
    let f = floats(s);
    (f.len() >= 3).then(|| [f[0] as f32, f[1] as f32, f[2] as f32, *f.get(3).unwrap_or(&1.0) as f32])
}

fn parse_title(el: &Element, ctx: &ImportCtx, effect: &str) -> TitleParams {
    let style = TitleStyle::ALL.into_iter().find(|s| s.label() == effect).unwrap_or_default();
    let mut p = TitleParams { style, ..Default::default() };
    let texts: Vec<&Element> = el.children_named("text").collect();
    p.text = texts.first().map(|t| t.text()).unwrap_or_default();
    p.subtitle = texts.get(1).map(|t| t.text()).unwrap_or_default();
    for prm in el.children_named("param") {
        let v = prm.attr("value").unwrap_or("");
        match prm.attr("name") {
            Some("Position") => {
                let f = floats(v);
                if f.len() == 2 {
                    p.position = [f[0] as f32, f[1] as f32];
                }
            }
            Some("rcp.pill") => {
                let f = floats(v);
                if f.len() >= 7 {
                    p.pill_enabled = f[0] != 0.0;
                    p.pill_color = [f[1] as f32, f[2] as f32, f[3] as f32, f[4] as f32];
                    p.pill_padding = f[5] as f32;
                    p.pill_radius = f[6] as f32;
                }
            }
            Some("rcp.shadowBlur") => p.shadow_blur = first_f(v).unwrap_or(p.shadow_blur as f64) as f32,
            _ => {}
        }
    }
    // text style: inline def or global
    let style_ref = texts.first().and_then(|t| t.child("text-style")).and_then(|s| s.attr("ref")).map(str::to_string);
    let def = el
        .children_named("text-style-def")
        .find(|d| d.attr("id").map(str::to_string) == style_ref)
        .and_then(|d| d.child("text-style").cloned())
        .or_else(|| style_ref.as_ref().and_then(|r| ctx.text_styles.get(r).cloned()));
    if let Some(s) = def {
        if let Some(f) = s.attr("font") {
            p.font_family = f.to_string();
        }
        if let Some(v) = s.attr("fontSize").and_then(first_f) {
            p.font_size = v as f32;
        }
        if let Some(v) = s.attr("tracking").and_then(first_f) {
            p.tracking = v as f32;
        }
        if let Some(c) = s.attr("fontColor").and_then(parse_color4) {
            p.color = c;
        }
        match s.attr("shadowColor").and_then(parse_color4) {
            Some(c) => {
                p.shadow_enabled = true;
                p.shadow_color = c;
                if let Some(o) = s.attr("shadowOffset").map(floats) {
                    if o.len() == 2 {
                        p.shadow_offset = [o[0] as f32, o[1] as f32];
                    }
                }
            }
            None => p.shadow_enabled = false,
        }
    }
    p
}

fn is_clip_element(name: &str) -> bool {
    matches!(name, "asset-clip" | "gap" | "title" | "ref-clip" | "clip" | "video" | "audio" | "audition")
}

/// Parse a clip element. Returns `(clip, offset attr, start attr)`.
fn parse_clip(el: &Element, ctx: &ImportCtx, fmt: &VideoFormat, depth: usize) -> Result<Option<(Clip, Time, Time)>, FcpxmlError> {
    let offset = attr_time(el, "offset").unwrap_or_default();
    let start = attr_time(el, "start").unwrap_or_default();
    let duration = attr_time(el, "duration").unwrap_or_default();
    let name = el.attr("name").unwrap_or("").to_string();
    let lane: i32 = el.attr("lane").and_then(|l| l.parse().ok()).unwrap_or(0);

    let asset_ref_clip = |r: &str| -> Result<Clip, FcpxmlError> {
        let aid = *ctx.assets.get(r).ok_or_else(|| FcpxmlError::UnknownRef(r.to_string()))?;
        let mut c = Clip::new(name.clone(), ClipKind::Asset { asset: aid }, duration);
        c.source_in = start;
        Ok(c)
    };

    if el.name == "audition" {
        // the first take is the active pick; the rest are alternates
        let mut takes = el.elements().filter(|e| is_clip_element(&e.name) && e.name != "audition");
        let Some(first) = takes.next() else {
            return Ok(None);
        };
        let Some((mut active, _, start)) = parse_clip(first, ctx, fmt, depth + 1)? else {
            return Ok(None);
        };
        for t in takes {
            if let Some((mut alt, _, _)) = parse_clip(t, ctx, fmt, depth + 1)? {
                alt.lane = 0;
                alt.offset = Time::ZERO;
                active.audition.push(alt);
            }
        }
        active.lane = lane;
        return Ok(Some((active, offset, start)));
    }

    let mut clip = match el.name.as_str() {
        "asset-clip" | "video" | "audio" => {
            let r = el.attr("ref").unwrap_or("");
            let mut c = asset_ref_clip(r)?;
            match (el.name.as_str(), el.attr("srcEnable")) {
                ("video", _) | (_, Some("video")) => c.av = AvFlags { video: true, audio: false },
                ("audio", _) | (_, Some("audio")) => c.av = AvFlags { video: false, audio: true },
                _ => {}
            }
            c.audio.role = AudioRole::from_fcp_role(el.attr("audioRole"));
            c
        }
        "clip" => {
            // legacy wrapper: <clip><video ref=…/><audio ref=…/></clip>
            let inner = el.elements().find(|e| matches!(e.name.as_str(), "video" | "audio" | "asset-clip"));
            match inner.and_then(|i| i.attr("ref")) {
                Some(r) => asset_ref_clip(r)?,
                None => Clip::gap(duration),
            }
        }
        "gap" => {
            let mut c = Clip::gap(duration);
            c.name = if name.is_empty() { "Gap".into() } else { name.clone() };
            c
        }
        "title" => {
            let effect = el.attr("ref").and_then(|r| ctx.effects.get(r)).map(String::as_str).unwrap_or("Basic Title");
            if effect == ADJUSTMENT_EFFECT {
                let mut c = Clip::adjustment(duration);
                c.name = name.clone();
                c
            } else {
                let mut c = Clip::title(parse_title(el, ctx, effect), duration);
                c.name = name.clone();
                c
            }
        }
        "ref-clip" => {
            let r = el.attr("ref").unwrap_or("");
            let media = ctx.media.get(r).ok_or_else(|| FcpxmlError::UnknownRef(r.to_string()))?;
            let seq = media.find("sequence").ok_or(FcpxmlError::NoSequence)?;
            let spine = match seq.child("spine") {
                Some(s) if depth < 32 => parse_spine(s, ctx, fmt, depth + 1)?,
                _ => Vec::new(),
            };
            let comp = CompoundClip::new(spine);
            let md = comp.duration();
            let mut c = Clip::new(name.clone(), ClipKind::Compound(Box::new(comp)), duration);
            c.source_in = start;
            c.media_duration = Some(md);
            c
        }
        _ => return Ok(None),
    };
    clip.lane = lane;
    clip.duration = duration;
    clip.enabled = el.attr("enabled") != Some("0");
    if let ClipKind::Asset { .. } = clip.kind {
        clip.media_duration = None; // filled from asset by caller
    }
    apply_adjustments(el, &mut clip, start, fmt);
    for child in el.elements().filter(|e| e.attr("lane").is_some()) {
        if child.name == "spine" {
            // secondary storyline
            let spine = if depth < 32 { parse_spine(child, ctx, fmt, depth + 1)? } else { Vec::new() };
            let comp = CompoundClip::storyline(spine);
            let len = comp.duration();
            if !len.is_positive() {
                continue;
            }
            let mut sl = Clip::new(child.attr("name").filter(|n| !n.is_empty()).unwrap_or("Storyline"), ClipKind::Compound(Box::new(comp)), len);
            sl.media_duration = Some(len);
            sl.lane = child.attr("lane").and_then(|l| l.parse().ok()).unwrap_or(1);
            sl.offset = attr_time(child, "offset").unwrap_or_default() - start;
            clip.connected.push(sl);
        } else if is_clip_element(&child.name) {
            if let Some((mut cc, coff, _)) = parse_clip(child, ctx, fmt, depth + 1)? {
                cc.offset = coff - start;
                clip.connected.push(cc);
            }
        }
    }
    if let Some(md) = el.child("metadata") {
        for m in md.elements().filter(|m| m.name == "md" && m.attr("key") == Some(CONNECTION_POINT_MD)) {
            clip.anchor = attr_time(m, "value").unwrap_or_default().max(Time::ZERO);
        }
    }
    Ok(Some((clip, offset, start)))
}

fn parse_transition(el: &Element) -> Transition {
    let kind = el.attr("name").and_then(TransitionKind::from_label).unwrap_or_default();
    let mut t = Transition::new(kind, attr_time(el, "duration").unwrap_or(Time::from_secs(1)));
    for p in el.elements().filter(|e| e.name == "param") {
        let v = p.attr("value").unwrap_or("");
        match p.attr("name") {
            Some("rcp.direction") => t.direction = Direction::ALL.into_iter().find(|d| d.label() == v).unwrap_or_default(),
            Some("rcp.softness") => t.softness = first_f(v).unwrap_or(0.1) as f32,
            Some("rcp.easing") => t.easing = Interpolation::ALL.into_iter().find(|i| i.label() == v).unwrap_or_default(),
            _ => {}
        }
    }
    t
}

fn parse_spine(spine: &Element, ctx: &ImportCtx, fmt: &VideoFormat, depth: usize) -> Result<Vec<Clip>, FcpxmlError> {
    let mut clips: Vec<Clip> = Vec::new();
    let mut pending_in: Option<Transition> = None;
    for el in spine.elements() {
        if el.name == "transition" {
            let tr = parse_transition(el);
            match clips.last_mut() {
                Some(prev) if !prev.is_gap() && prev.transition_out.is_none() => prev.transition_out = Some(tr),
                _ => pending_in = Some(tr),
            }
            continue;
        }
        if let Some((mut c, _, _)) = parse_clip(el, ctx, fmt, depth)? {
            c.lane = 0;
            c.offset = Time::ZERO;
            if let Some(tr) = pending_in.take() {
                c.transition_in = Some(tr);
            }
            clips.push(c);
        }
    }
    Ok(clips)
}

fn fill_media_durations(clips: &mut [Clip], assets: &[MediaAsset]) {
    for c in clips {
        if let ClipKind::Asset { asset } = c.kind {
            c.media_duration = assets.iter().find(|a| a.id == asset).and_then(MediaAsset::clip_media_duration);
        }
        if let ClipKind::Compound(comp) = &mut c.kind {
            fill_media_durations(&mut comp.spine, assets);
        }
        fill_media_durations(&mut c.connected, assets);
        fill_media_durations(&mut c.audition, assets);
    }
}

/// Parse an FCPXML document into a project.
pub fn import(src: &str) -> Result<Project, FcpxmlError> {
    let root = xml::parse(src)?;
    if root.name != "fcpxml" {
        return Err(FcpxmlError::NotFcpxml(root.name));
    }
    let mut ctx = ImportCtx { assets: HashMap::new(), formats: HashMap::new(), effects: HashMap::new(), media: HashMap::new(), text_styles: HashMap::new() };
    let mut assets: Vec<MediaAsset> = Vec::new();

    if let Some(res) = root.child("resources") {
        for r in res.elements() {
            let id = r.attr("id").unwrap_or("").to_string();
            match r.name.as_str() {
                "format" => {
                    let rate = r.attr("frameDuration").and_then(parse_rate).unwrap_or_default();
                    let w = r.attr("width").and_then(|v| v.parse().ok()).unwrap_or(1920);
                    let h = r.attr("height").and_then(|v| v.parse().ok()).unwrap_or(1080);
                    ctx.formats.insert(id, VideoFormat { width: w, height: h, rate });
                }
                "effect" => {
                    ctx.effects.insert(id, r.attr("name").unwrap_or("").to_string());
                }
                "media" => {
                    ctx.media.insert(id, r);
                }
                _ => {}
            }
        }
        for r in res.children_named("asset") {
            let id = r.attr("id").unwrap_or("").to_string();
            let src = r.children_named("media-rep").find(|m| m.attr("kind") != Some("proxy-media")).and_then(|m| m.attr("src")).or_else(|| r.attr("src")).unwrap_or("");
            let proxy = r.children_named("media-rep").find(|m| m.attr("kind") == Some("proxy-media")).and_then(|m| m.attr("src"));
            let duration = attr_time(r, "duration").unwrap_or_default();
            let mut a = MediaAsset::new(r.attr("name").unwrap_or("Media"), url_to_path(src), duration);
            a.proxy_path = proxy.map(url_to_path);
            let md: HashMap<String, String> =
                r.child("metadata").map(|m| m.children_named("md").filter_map(|e| Some((e.attr("key")?.to_string(), e.attr("value")?.to_string()))).collect()).unwrap_or_default();
            let mdv = |k: &str| md.get(k).cloned();
            a.kind = match mdv("com.rustcutpro.kind").as_deref() {
                Some("audio") => AssetKind::Audio,
                Some("image") => AssetKind::Image,
                Some("synthetic") => AssetKind::Synthetic,
                _ if r.attr("hasVideo") == Some("1") => AssetKind::Video,
                _ if r.attr("hasAudio") == Some("1") => AssetKind::Audio,
                _ => AssetKind::Video,
            };
            if r.attr("hasVideo") == Some("1") {
                let f = r.attr("format").and_then(|f| ctx.formats.get(f)).copied().unwrap_or_default();
                a.video = Some(VideoStreamInfo {
                    width: f.width,
                    height: f.height,
                    frame_rate: f.rate,
                    codec: mdv("com.rustcutpro.codec").unwrap_or_else(|| "unknown".into()),
                    pix_fmt: mdv("com.rustcutpro.pixfmt").unwrap_or_else(|| "yuv420p".into()),
                    bit_depth: mdv("com.rustcutpro.bitdepth").and_then(|v| v.parse().ok()).unwrap_or(8),
                    color_space: mdv("com.rustcutpro.colorspace").unwrap_or_else(|| "bt709".into()),
                    color_transfer: String::new(),
                    color_primaries: String::new(),
                    is_vfr: false,
                    pts_table: Vec::new(),
                });
            }
            if r.attr("hasAudio") == Some("1") {
                a.audio = Some(AudioStreamInfo {
                    sample_rate: r.attr("audioRate").and_then(|v| v.trim_end_matches('k').parse::<u32>().ok()).map(|v| if v < 1000 { v * 1000 } else { v }).unwrap_or(48_000),
                    channels: r.attr("audioChannels").and_then(|v| v.parse().ok()).unwrap_or(2),
                    codec: mdv("com.rustcutpro.audiocodec").unwrap_or_else(|| "aac".into()),
                });
            }
            if let Some(k) = mdv("com.apple.proapps.mio.keywords") {
                a.tags = k.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect();
            }
            ctx.assets.insert(id, a.id);
            assets.push(a);
        }
    }

    let library = root.child("library");
    let event = library.and_then(|l| l.child("event"));
    let project_el = root.find("project");
    let seq = project_el.and_then(|p| p.child("sequence")).or_else(|| root.child("library").and_then(|l| l.find("sequence"))).ok_or(FcpxmlError::NoSequence)?;
    let fmt = seq.attr("format").and_then(|f| ctx.formats.get(f)).copied().unwrap_or_default();
    let spine = match seq.child("spine") {
        Some(s) => parse_spine(s, &ctx, &fmt, 0)?,
        None => Vec::new(),
    };
    let tl_name = project_el.and_then(|p| p.attr("name")).unwrap_or("Imported Project").to_string();
    let proj_name = event.and_then(|e| e.attr("name")).unwrap_or(&tl_name).to_string();
    let mut timeline = Timeline::new(tl_name, fmt);
    timeline.spine = spine;
    fill_media_durations(&mut timeline.spine, &assets);
    timeline.relink_follows();
    let mut project = Project::new(proj_name);
    project.assets = assets;
    project.timeline = timeline;
    project.reserve_ids();
    Ok(project)
}
