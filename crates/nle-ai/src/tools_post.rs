//! Copilot tools beyond the cut: colour looks, masked corrections, caption
//! styling, audio polish / clip audio, and export delivery presets.
//!
//! Every tool is validated against the live timeline and turned into
//! undoable commands, like the editing tools in `tools.rs`. Generating
//! captions and opening the export dialog are app work (a Whisper job, a
//! dialog): those return a [`UiRequest`] the app carries out. Nothing here
//! ever starts an export or writes a file.

use crate::tools::{ArgReader, CopilotEnv};
use nle_core::clip::ColorWheel;
use nle_core::color::{ColorCorrection, MaskShape, ShapeMask};
use nle_core::{AudioRole, CaptionTrack, ClipId, Command, Time, Timeline};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

/// App-side work a Copilot action asks for.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "request", rename_all = "snake_case")]
pub enum UiRequest {
    /// Run Whisper over the timeline audio (Captions panel ▸ Generate).
    GenerateCaptions { language: Option<String> },
    /// Open the Render dialog with a delivery preset selected. The user still
    /// picks the file and presses Export.
    /// `codec` (one of [`EXPORT_CODECS`]) overrides the preset's encoder choice.
    OpenExport {
        delivery: String,
        #[serde(default)]
        codec: Option<String>,
    },
}

pub const COLOR_LOOKS: [&str; 8] = ["neutral", "warm", "cool", "teal_orange", "punchy", "faded", "black_and_white", "moody"];
pub const MASK_STYLES: [&str; 3] = ["vignette", "spotlight", "custom"];
pub const CAPTION_PRESETS: [&str; 3] = ["bold_pop", "clean", "word_pill"];
pub const CAPTION_COLORS: [&str; 4] = ["yellow", "coral", "green", "white"];
pub const AUDIO_ROLES: [&str; 4] = ["dialogue", "music", "effects", "general"];
/// Delivery presets the Render dialog knows (`GamingDeliveryPreset` in nle-render).
pub const EXPORT_DELIVERIES: [&str; 6] = ["match_project", "youtube_1440p60", "youtube_4k60", "shorts_9x16", "discord_25mb", "discord_50mb"];
/// Video codec families the Render dialog can be switched to (`ExportCodec` in nle-render).
pub const EXPORT_CODECS: [&str; 5] = ["h264", "hevc", "av1", "prores", "dnxhr"];

/// Name of the correction node a Copilot look lives in (replaced, not stacked,
/// when the look changes).
pub const LOOK_NODE: &str = "Copilot Look";
pub const MASK_NODE: &str = "Copilot Mask";

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ColorGradeArgs {
    pub clip_id: Option<u64>,
    pub look: Option<String>,
    /// 0..1, how strongly the look applies.
    pub intensity: Option<f64>,
    pub exposure: Option<f64>,
    pub contrast: Option<f64>,
    pub saturation: Option<f64>,
    pub temperature: Option<f64>,
    pub tint: Option<f64>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MaskArgs {
    pub clip_id: Option<u64>,
    pub style: String,
    pub shape: String,
    pub center: [f64; 2],
    /// Half width / half height in frame-height units.
    pub size: [f64; 2],
    pub feather: f64,
    pub invert: bool,
    pub exposure: Option<f64>,
    pub saturation: Option<f64>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct CaptionStyleArgs {
    pub preset: Option<String>,
    pub font_size: Option<f64>,
    pub position_y: Option<f64>,
    pub uppercase: Option<bool>,
    pub word_highlight: Option<bool>,
    pub highlight_color: Option<String>,
    pub sync_offset_ms: Option<i64>,
    pub enabled: Option<bool>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct AudioArgs {
    pub clip_id: Option<u64>,
    pub volume_db: Option<f64>,
    pub role: Option<String>,
    pub ducking: Option<bool>,
    pub noise_reduction: Option<bool>,
    pub voice_isolation: Option<bool>,
    pub leveler: Option<bool>,
    pub fade_in_secs: Option<f64>,
    pub fade_out_secs: Option<f64>,
}

fn in_range(field: &str, v: f64, lo: f64, hi: f64) -> Result<f64, String> {
    if (lo..=hi).contains(&v) {
        Ok(v)
    } else {
        Err(format!("`{field}` = {v} must be in {lo}..={hi}"))
    }
}

fn opt_range(a: &ArgReader, field: &str, lo: f64, hi: f64) -> Result<Option<f64>, String> {
    a.num(field)?.map(|v| in_range(field, v, lo, hi)).transpose()
}

fn one_of(field: &str, v: &str, allowed: &[&str]) -> Result<String, String> {
    let n = v.trim().to_lowercase().replace([' ', '-', '/'], "_").replace("&", "and");
    let n = match n.as_str() {
        "bw" | "b_and_w" | "mono" | "monochrome" | "grayscale" | "greyscale" => "black_and_white".to_string(),
        "teal_and_orange" | "orange_teal" | "orange_and_teal" | "cinematic" => "teal_orange".to_string(),
        "reset" | "none" | "natural" => "neutral".to_string(),
        "vo" | "voice" | "voiceover" => "dialogue".to_string(),
        "sfx" | "fx" => "effects".to_string(),
        "project" | "default" => "match_project".to_string(),
        "tiktok" | "shorts" | "reels" | "9_16" | "9:16" | "vertical" => "shorts_9x16".to_string(),
        "youtube" | "youtube_1440p" => "youtube_1440p60".to_string(),
        "youtube_4k" | "4k" => "youtube_4k60".to_string(),
        "discord" => "discord_25mb".to_string(),
        "h_264" | "avc" | "x264" | "libx264" | "mp4" => "h264".to_string(),
        "h265" | "h_265" | "x265" | "hevc_main10" => "hevc".to_string(),
        "dnxhd" => "dnxhr".to_string(),
        _ => n,
    };
    if allowed.contains(&n.as_str()) {
        Ok(n)
    } else {
        Err(format!("`{field}` '{v}' must be one of {}", allowed.join(" | ")))
    }
}

pub(crate) fn parse_color_grade(a: &ArgReader) -> Result<ColorGradeArgs, String> {
    let args = ColorGradeArgs {
        clip_id: a.id("clip_id")?,
        look: a.text("look")?.map(|l| one_of("look", &l, &COLOR_LOOKS)).transpose()?,
        intensity: opt_range(a, "intensity", 0.0, 1.0)?,
        exposure: opt_range(a, "exposure", -3.0, 3.0)?,
        contrast: opt_range(a, "contrast", -1.0, 1.0)?,
        saturation: opt_range(a, "saturation", 0.0, 2.0)?,
        temperature: opt_range(a, "temperature", -1.0, 1.0)?,
        tint: opt_range(a, "tint", -1.0, 1.0)?,
    };
    if args.look.is_none() && [args.exposure, args.contrast, args.saturation, args.temperature, args.tint].iter().all(Option::is_none) {
        return Err("give a `look` or at least one of exposure / contrast / saturation / temperature / tint".into());
    }
    Ok(args)
}

pub(crate) fn parse_mask(a: &ArgReader) -> Result<MaskArgs, String> {
    let style = one_of("style", &a.text("style")?.unwrap_or_else(|| "custom".into()), &MASK_STYLES)?;
    let (shape, center, size, feather, invert, exposure, saturation) = match style.as_str() {
        "vignette" => ("ellipse", [0.5, 0.5], [0.85, 0.55], 0.3, true, Some(-0.7), None),
        "spotlight" => ("ellipse", [0.5, 0.5], [0.35, 0.35], 0.15, false, Some(0.35), None),
        _ => ("ellipse", [0.5, 0.5], [0.3, 0.3], 0.05, false, None, None),
    };
    let shape = match a.text("shape")? {
        Some(s) => one_of("shape", &s, &["ellipse", "rectangle"])?,
        None => shape.to_string(),
    };
    let args = MaskArgs {
        clip_id: a.id("clip_id")?,
        style,
        shape,
        center: [opt_range(a, "center_x", 0.0, 1.0)?.unwrap_or(center[0]), opt_range(a, "center_y", 0.0, 1.0)?.unwrap_or(center[1])],
        size: [opt_range(a, "width", 0.02, 2.0)?.unwrap_or(size[0]), opt_range(a, "height", 0.02, 2.0)?.unwrap_or(size[1])],
        feather: opt_range(a, "feather", 0.0, 0.5)?.unwrap_or(feather),
        invert: a.flag("invert")?.unwrap_or(invert),
        exposure: opt_range(a, "exposure", -3.0, 3.0)?.or(exposure),
        saturation: opt_range(a, "saturation", 0.0, 2.0)?.or(saturation),
    };
    if args.exposure.is_none() && args.saturation.is_none() {
        return Err("a custom mask needs `exposure` or `saturation` (a mask with no correction changes nothing)".into());
    }
    Ok(args)
}

pub(crate) fn parse_caption_style(a: &ArgReader) -> Result<CaptionStyleArgs, String> {
    let ms = nle_core::captions::SYNC_OFFSET_MAX_MS as f64;
    let sync = opt_range(a, "sync_offset_ms", -ms, ms)?;
    if sync.is_some_and(|v| v.fract() != 0.0) {
        return Err("`sync_offset_ms` must be a whole number".into());
    }
    let args = CaptionStyleArgs {
        preset: a.text("preset")?.map(|p| one_of("preset", &p, &CAPTION_PRESETS)).transpose()?,
        font_size: opt_range(a, "font_size", 16.0, 200.0)?,
        position_y: opt_range(a, "position_y", 0.05, 0.95)?,
        uppercase: a.flag("uppercase")?,
        word_highlight: a.flag("word_highlight")?,
        highlight_color: a.text("highlight_color")?.map(|c| one_of("highlight_color", &c, &CAPTION_COLORS)).transpose()?,
        sync_offset_ms: sync.map(|v| v as i64),
        enabled: a.flag("enabled")?,
    };
    if args == CaptionStyleArgs::default() {
        return Err("nothing to change: give a `preset` or a style field".into());
    }
    Ok(args)
}

pub(crate) fn parse_audio(a: &ArgReader) -> Result<AudioArgs, String> {
    let args = AudioArgs {
        clip_id: a.id("clip_id")?,
        volume_db: opt_range(a, "volume_db", -60.0, 12.0)?,
        role: a.text("role")?.map(|r| one_of("role", &r, &AUDIO_ROLES)).transpose()?,
        ducking: a.flag("ducking")?,
        noise_reduction: a.flag("noise_reduction")?,
        voice_isolation: a.flag("voice_isolation")?,
        leveler: a.flag("leveler")?,
        fade_in_secs: opt_range(a, "fade_in_secs", 0.0, 30.0)?,
        fade_out_secs: opt_range(a, "fade_out_secs", 0.0, 30.0)?,
    };
    if (AudioArgs { clip_id: None, ..args.clone() }) == AudioArgs::default() {
        return Err("nothing to change: give volume_db, role, ducking, noise_reduction, voice_isolation, leveler or a fade".into());
    }
    Ok(args)
}

pub(crate) fn parse_language(a: &ArgReader) -> Result<Option<String>, String> {
    match a.text("language")? {
        None => Ok(None),
        Some(l) if l.eq_ignore_ascii_case("auto") => Ok(None),
        Some(l) if (2..=3).contains(&l.len()) && l.chars().all(|c| c.is_ascii_alphabetic()) => Ok(Some(l.to_lowercase())),
        Some(l) => Err(format!("`language` '{l}' must be a 2–3 letter code (en, es, de…) or auto")),
    }
}

pub(crate) fn parse_delivery(a: &ArgReader) -> Result<String, String> {
    one_of("delivery", &a.req_text("delivery")?, &EXPORT_DELIVERIES)
}

/// Optional `codec` of `set_export_preset` (`None` keeps the preset's choice).
pub(crate) fn parse_codec(a: &ArgReader) -> Result<Option<String>, String> {
    a.text("codec")?.map(|c| one_of("codec", &c.replace('.', ""), &EXPORT_CODECS)).transpose()
}

// ---------------------------------------------------------------------------
// planning

/// Wheel push toward a colour (r, g, b ∈ −1..1).
fn wheel(r: f32, g: f32, b: f32, master: f32) -> ColorWheel {
    ColorWheel { r, g, b, master }
}

/// The correction node for a look (`None` for neutral).
pub fn look_node(look: &str, intensity: f32) -> Option<ColorCorrection> {
    let mut n = ColorCorrection::named(LOOK_NODE);
    n.mix = intensity.clamp(0.0, 1.0);
    match look {
        "warm" => {
            n.temperature = 0.25;
            n.tint = 0.03;
            n.saturation = 1.08;
        }
        "cool" => {
            n.temperature = -0.25;
            n.tint = -0.02;
        }
        "teal_orange" => {
            n.lift = wheel(-0.04, 0.02, 0.05, 0.0);
            n.gain = wheel(0.06, 0.02, -0.05, 0.0);
            n.contrast = 0.1;
            n.saturation = 1.1;
        }
        "punchy" => {
            n.contrast = 0.22;
            n.saturation = 1.25;
        }
        "faded" => {
            n.contrast = -0.15;
            n.lift = wheel(0.0, 0.0, 0.0, 0.05);
            n.saturation = 0.85;
        }
        "black_and_white" => {
            n.saturation = 0.0;
            n.contrast = 0.12;
        }
        "moody" => {
            n.exposure = -0.3;
            n.contrast = 0.15;
            n.saturation = 0.8;
            n.temperature = -0.1;
        }
        _ => return None,
    }
    Some(n)
}

fn video_clip(tl: &Timeline, id: ClipId) -> Result<nle_core::Clip, String> {
    let c = tl.clip(id).cloned().ok_or("clip missing")?;
    if !c.has_video() {
        return Err(format!("'{}' has no picture to grade", c.name));
    }
    Ok(c)
}

fn set_param(field: &str, p: &mut nle_core::Param, v: f64) -> Result<(), String> {
    if p.curve.as_ref().is_some_and(|c| !c.is_empty()) {
        return Err(format!("{field} is keyframed on this clip; change it in the inspector"));
    }
    p.value = v;
    Ok(())
}

pub fn plan_color_grade(tl: &Timeline, id: ClipId, a: &ColorGradeArgs) -> Result<(Command, String), String> {
    let mut clip = video_clip(tl, id)?;
    let c = &mut clip.color;
    let mut did = Vec::new();
    if let Some(look) = &a.look {
        c.corrections.retain(|n| n.name != LOOK_NODE);
        if let Some(node) = look_node(look, a.intensity.unwrap_or(1.0) as f32) {
            c.corrections.insert(0, node);
            did.push(format!("look '{look}'"));
        } else {
            did.push("look removed".into());
        }
    }
    let fields: [(&str, Option<f64>, &mut nle_core::Param); 5] = [
        ("exposure", a.exposure, &mut c.exposure),
        ("contrast", a.contrast, &mut c.contrast),
        ("saturation", a.saturation, &mut c.saturation),
        ("temperature", a.temperature, &mut c.temperature),
        ("tint", a.tint, &mut c.tint),
    ];
    for (name, v, p) in fields {
        if let Some(v) = v {
            set_param(name, p, v)?;
            did.push(format!("{name} {v:+.2}"));
        }
    }
    c.enabled = true;
    let summary = format!("Graded '{}': {}", clip.name, did.join(", "));
    Ok((Command::UpdateClip { clip: Box::new(clip) }, summary))
}

pub fn plan_mask(tl: &Timeline, id: ClipId, a: &MaskArgs) -> Result<(Command, String), String> {
    let mut clip = video_clip(tl, id)?;
    let mut mask = ShapeMask::new(if a.shape == "rectangle" { MaskShape::Rectangle } else { MaskShape::Ellipse });
    mask.center = [a.center[0] as f32, a.center[1] as f32];
    mask.size = [a.size[0] as f32, a.size[1] as f32];
    mask.feather = a.feather as f32;
    mask.invert = a.invert;
    let mut node = ColorCorrection::named(if a.style == "custom" { MASK_NODE.to_string() } else { format!("{MASK_NODE}: {}", a.style) });
    node.exposure = a.exposure.unwrap_or(0.0) as f32;
    node.saturation = a.saturation.unwrap_or(1.0) as f32;
    node.mask = Some(mask);
    if clip.color.corrections.len() >= 16 {
        return Err(format!("'{}' already has 16 corrections", clip.name));
    }
    // a second vignette replaces the first instead of darkening twice
    let name = node.name.clone();
    if a.style != "custom" {
        clip.color.corrections.retain(|n| n.name != name);
    }
    clip.color.corrections.push(node);
    clip.color.enabled = true;
    let summary = format!("{} {} mask on '{}'", a.style, a.shape, clip.name);
    Ok((Command::UpdateClip { clip: Box::new(clip) }, summary))
}

pub fn apply_caption_preset(track: &mut CaptionTrack, preset: &str) {
    let s = &mut track.style;
    match preset {
        "bold_pop" => {
            s.uppercase = true;
            s.word_highlight = true;
            s.outline = true;
            s.pill = false;
            s.word_pill = false;
            s.highlight_color = nle_core::captions::HIGHLIGHT_YELLOW;
            s.dim_upcoming = true;
        }
        "clean" => {
            s.uppercase = false;
            s.word_highlight = false;
            s.outline = false;
            s.pill = true;
            s.word_pill = false;
            s.dim_upcoming = false;
        }
        "word_pill" => {
            s.uppercase = true;
            s.word_highlight = true;
            s.pill = false;
            s.word_pill = true;
            s.outline = true;
            s.highlight_color = nle_core::captions::HIGHLIGHT_CORAL;
        }
        _ => {}
    }
}

fn caption_color(name: &str) -> [f32; 4] {
    match name {
        "coral" => nle_core::captions::HIGHLIGHT_CORAL,
        "green" => nle_core::captions::HIGHLIGHT_GREEN,
        "white" => [1.0; 4],
        _ => nle_core::captions::HIGHLIGHT_YELLOW,
    }
}

pub fn plan_caption_style(tl: &Timeline, a: &CaptionStyleArgs) -> Result<(Command, String), String> {
    if tl.captions.is_empty() {
        return Err("there are no captions yet — call generate_captions first".into());
    }
    let mut track = tl.captions.clone();
    if let Some(p) = &a.preset {
        apply_caption_preset(&mut track, p);
    }
    let s = &mut track.style;
    if let Some(v) = a.font_size {
        s.font_size = v as f32;
    }
    if let Some(v) = a.position_y {
        s.position_y = v as f32;
    }
    if let Some(v) = a.uppercase {
        s.uppercase = v;
    }
    if let Some(v) = a.word_highlight {
        s.word_highlight = v;
    }
    if let Some(c) = &a.highlight_color {
        s.highlight_color = caption_color(c);
    }
    if let Some(v) = a.sync_offset_ms {
        s.sync_offset_ms = v as i32;
    }
    if let Some(v) = a.enabled {
        track.enabled = v;
    }
    let what = a.preset.as_deref().map(|p| format!("'{p}' style")).unwrap_or_else(|| "style".into());
    Ok((Command::SetCaptions { captions: Box::new(track) }, format!("Caption {what} on {} block(s)", tl.captions.segments.len())))
}

pub fn plan_polish_audio(tl: &Timeline, env: &CopilotEnv) -> Result<(Vec<Command>, String), String> {
    let mut unmeasured = 0;
    let cmd = nle_core::audio_polish::polish_audio_command(tl, &mut |id| {
        let l = env.lufs.get(&id).copied();
        unmeasured += usize::from(l.is_none());
        l
    })
    .ok_or("audio is already polished (or there are no audible clips)")?;
    let commands = match cmd {
        Command::Batch { commands, .. } => commands,
        c => vec![c],
    };
    let mut summary = format!("Polished audio on {} clip(s)", commands.len());
    if unmeasured > 0 {
        summary.push_str(&format!(" ({unmeasured} not loudness-normalised: audio not analysed yet)"));
    }
    Ok((commands, summary))
}

pub fn plan_audio(tl: &Timeline, id: ClipId, a: &AudioArgs) -> Result<(Command, String), String> {
    let mut clip = tl.clip(id).cloned().ok_or("clip missing")?;
    if !clip.has_audio() {
        return Err(format!("'{}' has no audio", clip.name));
    }
    let au = &mut clip.audio;
    let mut did = Vec::new();
    if let Some(v) = a.volume_db {
        set_param("volume", &mut au.volume_db, v)?;
        did.push(format!("volume {v:+.1} dB"));
    }
    if let Some(r) = &a.role {
        au.role = match r.as_str() {
            "music" => AudioRole::Music,
            "effects" => AudioRole::Effects,
            "general" => AudioRole::General,
            _ => AudioRole::Dialogue,
        };
        did.push(format!("role {r}"));
    }
    let toggles: [(&str, Option<bool>, &mut bool); 4] = [
        ("ducking", a.ducking, &mut au.ducking.enabled),
        ("noise reduction", a.noise_reduction, &mut au.noise_reduction.enabled),
        ("voice isolation", a.voice_isolation, &mut au.voice_isolation.enabled),
        ("leveler", a.leveler, &mut au.leveler.enabled),
    ];
    for (name, v, slot) in toggles {
        if let Some(v) = v {
            *slot = v;
            did.push(format!("{name} {}", if v { "on" } else { "off" }));
        }
    }
    let half = Time(clip.duration.0 / 2);
    for (name, v, slot) in [("fade in", a.fade_in_secs, &mut au.fade_in), ("fade out", a.fade_out_secs, &mut au.fade_out)] {
        if let Some(v) = v {
            let t = Time::from_secs_f64(v);
            if t > half {
                return Err(format!("{name} {v:.2}s is longer than half of '{}'", clip.name));
            }
            *slot = t;
            did.push(format!("{name} {v:.2}s"));
        }
    }
    let summary = format!("Audio on '{}': {}", clip.name, did.join(", "));
    Ok((Command::UpdateClip { clip: Box::new(clip) }, summary))
}

pub fn plan_generate_captions(tl: &Timeline, language: &Option<String>) -> Result<(UiRequest, String), String> {
    if nle_core::audio_spans(tl).is_empty() {
        return Err("the timeline has no audible clips to transcribe".into());
    }
    let lang = language.as_deref().unwrap_or("auto-detected language");
    Ok((UiRequest::GenerateCaptions { language: language.clone() }, format!("Transcribing captions ({lang})")))
}

pub fn plan_export(delivery: &str, codec: Option<&str>) -> (UiRequest, String) {
    let with = codec.map(|c| format!(" and codec '{c}'")).unwrap_or_default();
    (UiRequest::OpenExport { delivery: delivery.into(), codec: codec.map(Into::into) }, format!("Render dialog opened with '{delivery}'{with} — choose a file and press Export"))
}

/// Function declarations (Gemini schema dialect) for the tools in this module.
pub fn declarations() -> Vec<Value> {
    vec![
        json!({
            "name": "color_grade",
            "description": "Grade a clip: apply a look (stored as one replaceable correction node) and/or set primary exposure, contrast, saturation, temperature, tint.",
            "parameters": { "type": "OBJECT", "properties": {
                "clip_id": { "type": "INTEGER" },
                "look": { "type": "STRING", "description": COLOR_LOOKS.join(" | ") },
                "intensity": { "type": "NUMBER", "description": "0..1 look strength (default 1)" },
                "exposure": { "type": "NUMBER", "description": "stops, -3..3" },
                "contrast": { "type": "NUMBER", "description": "-1..1" },
                "saturation": { "type": "NUMBER", "description": "0..2, 1 neutral" },
                "temperature": { "type": "NUMBER", "description": "-1 cool .. 1 warm" },
                "tint": { "type": "NUMBER", "description": "-1 green .. 1 magenta" }
            }}
        }),
        json!({
            "name": "add_mask",
            "description": "Add a masked colour correction to a clip: a vignette, a spotlight, or a custom ellipse/rectangle with an exposure/saturation change inside it.",
            "parameters": { "type": "OBJECT", "properties": {
                "clip_id": { "type": "INTEGER" },
                "style": { "type": "STRING", "description": MASK_STYLES.join(" | ") },
                "shape": { "type": "STRING", "description": "ellipse | rectangle" },
                "center_x": { "type": "NUMBER", "description": "0..1 across the frame" },
                "center_y": { "type": "NUMBER", "description": "0..1 down the frame" },
                "width": { "type": "NUMBER", "description": "half width in frame heights, 0.02..2" },
                "height": { "type": "NUMBER", "description": "half height in frame heights, 0.02..2" },
                "feather": { "type": "NUMBER", "description": "0..0.5" },
                "invert": { "type": "BOOLEAN" },
                "exposure": { "type": "NUMBER", "description": "stops inside the mask, -3..3" },
                "saturation": { "type": "NUMBER", "description": "0..2 inside the mask" }
            }}
        }),
        json!({
            "name": "generate_captions",
            "description": "Transcribe the timeline audio with Whisper into word-timed captions (runs in the background).",
            "parameters": { "type": "OBJECT", "properties": {
                "language": { "type": "STRING", "description": "2-letter code, or auto" }
            }}
        }),
        json!({
            "name": "style_captions",
            "description": "Restyle the existing captions.",
            "parameters": { "type": "OBJECT", "properties": {
                "preset": { "type": "STRING", "description": CAPTION_PRESETS.join(" | ") },
                "font_size": { "type": "NUMBER", "description": "16..200 px at 1080p" },
                "position_y": { "type": "NUMBER", "description": "0.05 top .. 0.95 bottom" },
                "uppercase": { "type": "BOOLEAN" },
                "word_highlight": { "type": "BOOLEAN" },
                "highlight_color": { "type": "STRING", "description": CAPTION_COLORS.join(" | ") },
                "sync_offset_ms": { "type": "INTEGER", "description": "-300..300, + = later" },
                "enabled": { "type": "BOOLEAN" }
            }}
        }),
        json!({
            "name": "polish_audio",
            "description": "One-click audio polish over the whole timeline: loudness-normalise every clip to -14 LUFS, vocal clarity on dialogue, music bed -6 dB with auto-ducking.",
            "parameters": { "type": "OBJECT", "properties": {} }
        }),
        json!({
            "name": "adjust_audio",
            "description": "Change one clip's audio: volume, role, ducking, noise reduction, voice isolation, dialogue leveler, fades.",
            "parameters": { "type": "OBJECT", "properties": {
                "clip_id": { "type": "INTEGER" },
                "volume_db": { "type": "NUMBER", "description": "-60..12" },
                "role": { "type": "STRING", "description": AUDIO_ROLES.join(" | ") },
                "ducking": { "type": "BOOLEAN" },
                "noise_reduction": { "type": "BOOLEAN" },
                "voice_isolation": { "type": "BOOLEAN" },
                "leveler": { "type": "BOOLEAN" },
                "fade_in_secs": { "type": "NUMBER" },
                "fade_out_secs": { "type": "NUMBER" }
            }}
        }),
        json!({
            "name": "set_export_preset",
            "description": "Open the Render dialog with a delivery preset selected. It does not start the export; the user picks the file and confirms. Set `codec` whenever the user names one (\"H.264\", \"HEVC\", \"AV1\", \"ProRes\"); otherwise the preset picks.",
            "parameters": { "type": "OBJECT", "properties": {
                "delivery": { "type": "STRING", "description": EXPORT_DELIVERIES.join(" | ") },
                "codec": { "type": "STRING", "description": EXPORT_CODECS.join(" | ") }
            }, "required": ["delivery"] }
        }),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::{apply_copilot_actions_with, offline_copilot_plan, CopilotToolCall};
    use crate::ActionOutcome;
    use nle_core::clip::AvFlags;
    use nle_core::{Clip, History, MediaAsset};

    fn timeline() -> (Timeline, ClipId, MediaAsset) {
        let mut tl = Timeline::default();
        let asset = MediaAsset::new("ace.mp4", "/clips/ace.mp4", Time::from_secs(10));
        let mut c = Clip::from_asset(&asset);
        c.av = AvFlags { video: true, audio: true };
        let id = c.id;
        tl.append(c).unwrap();
        (tl, id, asset)
    }

    fn run_with(tl: &mut Timeline, h: &mut History, calls: &[CopilotToolCall], env: &CopilotEnv) -> Vec<ActionOutcome> {
        let mut ph = Time::ZERO;
        apply_copilot_actions_with(calls, tl, h, &mut ph, &[], env)
    }

    fn run(tl: &mut Timeline, calls: &[CopilotToolCall]) -> Vec<ActionOutcome> {
        run_with(tl, &mut History::default(), calls, &CopilotEnv::default())
    }

    fn call(name: &str, args: Value) -> Result<CopilotToolCall, String> {
        CopilotToolCall::from_name_and_args(name, &args)
    }

    #[test]
    fn parsing_validates_ranges_and_names() {
        assert!(call("color_grade", json!({})).is_err(), "nothing to do");
        assert!(call("color_grade", json!({ "exposure": 9 })).is_err());
        assert!(call("color_grade", json!({ "look": "vaporwave" })).is_err());
        let CopilotToolCall::ColorGrade(a) = call("color_grade", json!({ "look": "Teal & Orange" })).unwrap() else { panic!() };
        assert_eq!(a.look.as_deref(), Some("teal_orange"));
        assert!(call("add_mask", json!({ "style": "custom" })).is_err(), "custom mask without a correction");
        assert!(call("add_mask", json!({ "style": "vignette", "feather": 2 })).is_err());
        assert!(call("style_captions", json!({})).is_err());
        assert!(call("style_captions", json!({ "sync_offset_ms": 900 })).is_err());
        assert!(call("style_captions", json!({ "uppercase": "maybe" })).is_err());
        assert!(call("style_captions", json!({ "uppercase": "true" })).is_ok());
        assert!(call("adjust_audio", json!({ "clip_id": 3 })).is_err(), "clip id alone changes nothing");
        assert!(call("adjust_audio", json!({ "volume_db": 40 })).is_err());
        assert!(call("generate_captions", json!({ "language": "english please" })).is_err());
        assert_eq!(call("generate_captions", json!({ "language": "auto" })).unwrap(), CopilotToolCall::GenerateCaptions { language: None });
        assert_eq!(call("set_export_preset", json!({ "delivery": "TikTok" })).unwrap(), CopilotToolCall::SetExportPreset { delivery: "shorts_9x16".into(), codec: None });
        for (name, want) in [("H.264", "h264"), ("h264", "h264"), ("AVC", "h264"), ("H.265", "hevc"), ("HEVC", "hevc"), ("AV1", "av1"), ("ProRes", "prores")] {
            let got = call("set_export_preset", json!({ "delivery": "match_project", "codec": name })).unwrap();
            assert_eq!(got, CopilotToolCall::SetExportPreset { delivery: "match_project".into(), codec: Some(want.into()) }, "{name}");
        }
        assert!(call("set_export_preset", json!({ "delivery": "match_project", "codec": "vp9" })).is_err());
        assert!(call("set_export_preset", json!({})).is_err());
        assert!(call("color_grade", json!({ "look": "x".repeat(500) })).is_err(), "over-long text");
    }

    #[test]
    fn look_replaces_its_node_and_undoes() {
        let (mut tl, id, _) = timeline();
        let before = tl.clip(id).unwrap().clone();
        let mut h = History::default();
        let warm = call("color_grade", json!({ "look": "warm", "saturation": 1.2 })).unwrap();
        let out = run_with(&mut tl, &mut h, &[warm], &CopilotEnv::default());
        assert!(out[0].applied, "{out:?}");
        let c = tl.clip(id).unwrap();
        assert_eq!(c.color.corrections.iter().filter(|n| n.name == LOOK_NODE).count(), 1);
        assert_eq!(c.color.saturation.value, 1.2);
        assert!(h.undo(&mut tl));
        assert_eq!(tl.clip(id).unwrap(), &before);

        run(&mut tl, &[call("color_grade", json!({ "look": "warm" })).unwrap()]);
        run(&mut tl, &[call("color_grade", json!({ "look": "cool" })).unwrap()]);
        let c = tl.clip(id).unwrap();
        assert_eq!(c.color.corrections.iter().filter(|n| n.name == LOOK_NODE).count(), 1, "a new look replaces the old one");
        assert!(c.color.corrections[0].temperature < 0.0);
        run(&mut tl, &[call("color_grade", json!({ "look": "neutral" })).unwrap()]);
        assert!(tl.clip(id).unwrap().color.corrections.iter().all(|n| n.name != LOOK_NODE));
    }

    #[test]
    fn animated_params_are_refused_not_overwritten() {
        let (mut tl, id, _) = timeline();
        let c = tl.clip_mut(id).unwrap();
        let mut curve = nle_core::Curve::default();
        curve.insert(nle_core::Keyframe::new(Time::ZERO, 0.5));
        c.color.exposure.curve = Some(curve);
        let out = run(&mut tl, &[call("color_grade", json!({ "exposure": 1.0 })).unwrap()]);
        assert!(!out[0].applied);
        assert!(out[0].detail.contains("keyframed"), "{}", out[0].detail);
    }

    #[test]
    fn vignette_mask_is_inverted_and_not_doubled() {
        let (mut tl, id, _) = timeline();
        let v = call("add_mask", json!({ "style": "vignette" })).unwrap();
        run(&mut tl, std::slice::from_ref(&v));
        run(&mut tl, &[v]);
        let c = tl.clip(id).unwrap();
        let nodes: Vec<_> = c.color.corrections.iter().filter(|n| n.name.starts_with(MASK_NODE)).collect();
        assert_eq!(nodes.len(), 1);
        assert!(nodes[0].mask.as_ref().unwrap().invert);
        assert!(nodes[0].exposure < 0.0);
    }

    #[test]
    fn captions_need_segments_and_requests_are_deduplicated() {
        let (mut tl, _, _) = timeline();
        let out = run(&mut tl, &[call("style_captions", json!({ "preset": "bold_pop" })).unwrap()]);
        assert!(!out[0].applied && out[0].detail.contains("generate_captions"));
        tl.captions.segments.push(nle_core::captions::CaptionSegment::new(Time::ZERO, Time::from_secs(1), "nice shot"));
        let out = run(&mut tl, &[call("style_captions", json!({ "preset": "bold_pop", "font_size": 90 })).unwrap()]);
        assert!(out[0].applied);
        assert!(tl.captions.style.uppercase);
        assert_eq!(tl.captions.style.font_size, 90.0);

        let gen = call("generate_captions", json!({ "language": "en" })).unwrap();
        let exp = call("set_export_preset", json!({ "delivery": "discord" })).unwrap();
        let mut h = History::default();
        let out = run_with(&mut tl, &mut h, &[gen.clone(), gen, exp], &CopilotEnv::default());
        assert_eq!(out[0].request, Some(UiRequest::GenerateCaptions { language: Some("en".into()) }));
        assert!(!out[1].applied, "second Whisper job in one reply");
        assert_eq!(out[2].request, Some(UiRequest::OpenExport { delivery: "discord_25mb".into(), codec: None }));
        assert!(!h.can_undo(), "requests change nothing on the timeline");
    }

    #[test]
    fn audio_tools_use_measured_loudness() {
        let (mut tl, id, asset) = timeline();
        let mut env = CopilotEnv::default();
        env.lufs.insert(asset.id, -20.0);
        let mut h = History::default();
        let out = run_with(&mut tl, &mut h, &[CopilotToolCall::PolishAudio], &env);
        assert!(out[0].applied, "{out:?}");
        let c = tl.clip(id).unwrap();
        assert!(c.audio.loudness_normalize);
        assert!((c.audio.loudness_gain_db - 6.0).abs() < 0.01);

        let a = call("adjust_audio", json!({ "role": "music", "volume_db": -8, "fade_in_secs": 0.5 })).unwrap();
        let out = run(&mut tl, &[a]);
        assert!(out[0].applied, "{out:?}");
        let c = tl.clip(id).unwrap();
        assert_eq!(c.audio.role, AudioRole::Music);
        assert_eq!(c.audio.volume_db.value, -8.0);
        assert!(!run(&mut tl, &[call("adjust_audio", json!({ "fade_out_secs": 9 })).unwrap()])[0].applied, "fade longer than half the clip");
    }

    #[test]
    fn offline_planner_maps_finishing_requests() {
        let (tl, _, _) = timeline();
        let (_, calls) = offline_copilot_plan("give it a cinematic teal look with a vignette", &tl, Time::ZERO, &[]);
        assert!(calls.iter().any(|c| matches!(c, CopilotToolCall::ColorGrade(a) if a.look.as_deref() == Some("teal_orange"))));
        assert!(calls.iter().any(|c| matches!(c, CopilotToolCall::AddMask(_))));
        let (_, calls) = offline_copilot_plan("export for tiktok", &tl, Time::ZERO, &[]);
        assert_eq!(calls, vec![CopilotToolCall::SetExportPreset { delivery: "shorts_9x16".into(), codec: None }]);
        let (_, calls) = offline_copilot_plan("export the timeline as a 1080p H.264 MP4", &tl, Time::ZERO, &[]);
        assert_eq!(calls, vec![CopilotToolCall::SetExportPreset { delivery: "match_project".into(), codec: Some("h264".into()) }]);
        let (_, calls) = offline_copilot_plan("add captions and clean up the audio", &tl, Time::ZERO, &[]);
        assert!(calls.contains(&CopilotToolCall::GenerateCaptions { language: None }));
        assert!(calls.contains(&CopilotToolCall::PolishAudio));
    }

    #[test]
    fn schemas_cover_every_tool() {
        let decls = crate::tools::gemini_tool_declarations();
        let names: Vec<&str> = decls["functionDeclarations"].as_array().unwrap().iter().filter_map(|d| d["name"].as_str()).collect();
        for t in ["color_grade", "add_mask", "generate_captions", "style_captions", "polish_audio", "adjust_audio", "set_export_preset", "drop_marker"] {
            assert!(names.contains(&t), "{t} missing from schema");
        }
        let oa = crate::tools::openai_tool_declarations();
        assert_eq!(oa.as_array().unwrap().len(), names.len());
    }
}
