//! Structured AI Copilot timeline tool definitions, execution engine (via undoable
//! `nle_core::Command`s), quick prompt chips, and deterministic offline fallback.

use crate::tools_post::{self as post, AudioArgs, CaptionStyleArgs, ColorGradeArgs, MaskArgs, UiRequest};
use nle_core::presets::MontagePreset;
use nle_core::timeline::Marker;
use nle_core::{ClipId, Command, History, Retime, Time, Timeline, Transition, TransitionKind};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

/// The 5 quick one-click gaming montage prompt chips displayed in the AI Copilot dock.
pub const QUICK_PROMPTS: [(&str, &str); 5] = [
    ("Find all kills & drop markers", "Find all kills in this clip and drop Kill markers on every kill peak."),
    ("Sync clips to music beats", "Sync selected clips and velocity peaks to the timeline beat markers."),
    ("Valorant Flow Sync & Shake", "Apply the Valorant Flow Sync velocity curve and Hard Kill Impact S_Shake Decay preset."),
    ("Build 15s fast-paced fragmovie", "Tighten cuts around beats, sync clips to beat markers, add Valorant Vibrant Clean CC, and add Whip-Pan transitions."),
    ("Convert to 9:16 TikTok/Shorts", "Convert the active gameplay clip to 9:16 TikTok/Shorts with top-right killfeed PiP."),
];

/// Structured tool calls invoked by the AI Copilot (cloud LLM or local fallback).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "tool", rename_all = "snake_case")]
pub enum CopilotToolCall {
    JumpToTimecode { time_secs: f64 },
    ApplyVelocityPreset { clip_id: Option<u64>, preset_name: String },
    AddEffectPreset { clip_id: Option<u64>, preset_name: String },
    CutAndRipple { clip_id: Option<u64>, time_secs: f64, trim_mode: String },
    SyncToBeatMarkers { clip_ids: Vec<u64> },
    DropMarker { time_secs: f64, label: String, kind: String },
    AddTransition { clip_id: Option<u64>, transition_kind: String, duration_frames: i64 },
    ColorGrade(ColorGradeArgs),
    AddMask(MaskArgs),
    GenerateCaptions { language: Option<String> },
    StyleCaptions(CaptionStyleArgs),
    PolishAudio,
    AdjustAudio(AudioArgs),
    SetExportPreset { delivery: String },
}

/// What the app knows that the timeline doesn't, for tools that need it.
#[derive(Clone, Debug, Default)]
pub struct CopilotEnv {
    /// Measured integrated loudness per asset (for `polish_audio`).
    pub lufs: std::collections::HashMap<nle_core::AssetId, f32>,
}

/// Most actions one model reply may carry; the rest are rejected.
pub const MAX_ACTIONS_PER_REPLY: usize = 32;
/// Longest text argument accepted (labels, preset names).
pub const MAX_TEXT_ARG: usize = 200;

impl CopilotToolCall {
    pub fn name(&self) -> &'static str {
        match self {
            Self::JumpToTimecode { .. } => "jump_to_timecode",
            Self::ApplyVelocityPreset { .. } => "apply_velocity_preset",
            Self::AddEffectPreset { .. } => "add_effect_preset",
            Self::CutAndRipple { .. } => "cut_and_ripple",
            Self::SyncToBeatMarkers { .. } => "sync_to_beat_markers",
            Self::DropMarker { .. } => "drop_marker",
            Self::AddTransition { .. } => "add_transition",
            Self::ColorGrade(_) => "color_grade",
            Self::AddMask(_) => "add_mask",
            Self::GenerateCaptions { .. } => "generate_captions",
            Self::StyleCaptions(_) => "style_captions",
            Self::PolishAudio => "polish_audio",
            Self::AdjustAudio(_) => "adjust_audio",
            Self::SetExportPreset { .. } => "set_export_preset",
        }
    }

    /// Tools that hand work to the app (at most one of each per reply).
    fn request_kind(&self) -> Option<u8> {
        match self {
            Self::GenerateCaptions { .. } => Some(0),
            Self::SetExportPreset { .. } => Some(1),
            _ => None,
        }
    }

    /// Parse a provider tool-call (`name`, `args` JSON object) into a typed `CopilotToolCall`.
    /// Strict schema validation: unknown tools, missing required fields, wrong
    /// types, non-finite numbers and unknown enum values are rejected with a
    /// reason (never silently replaced by defaults). Timeline bounds are
    /// checked later against the live timeline in [`apply_copilot_actions`].
    pub fn from_name_and_args(name: &str, args: &Value) -> Result<Self, String> {
        let empty = serde_json::Map::new();
        let obj = match args {
            Value::Object(m) => m,
            Value::Null => &empty,
            _ => return Err("arguments must be a JSON object".into()),
        };
        let a = ArgReader(obj);
        match name {
            "jump_to_timecode" => Ok(Self::JumpToTimecode { time_secs: a.req_secs("time_secs")? }),
            "apply_velocity_preset" => Ok(Self::ApplyVelocityPreset { clip_id: a.id("clip_id")?, preset_name: a.req_text("preset_name")? }),
            "add_effect_preset" => Ok(Self::AddEffectPreset { clip_id: a.id("clip_id")?, preset_name: a.req_text("preset_name")? }),
            "cut_and_ripple" => {
                let mode = a.req_text("trim_mode")?;
                let trim_mode = parse_trim_mode(&mode).ok_or_else(|| format!("trim_mode '{mode}' must be one of blade | ripple_start | ripple_end"))?;
                Ok(Self::CutAndRipple { clip_id: a.id("clip_id")?, time_secs: a.req_secs("time_secs")?, trim_mode: trim_mode.into() })
            }
            "sync_to_beat_markers" => {
                let clip_ids = match obj.get("clip_ids") {
                    None | Some(Value::Null) => Vec::new(),
                    Some(Value::Array(arr)) if arr.len() > 1000 => return Err("clip_ids has more than 1000 entries".into()),
                    Some(Value::Array(arr)) => arr.iter().map(|v| as_id(v).ok_or_else(|| format!("clip_ids entry {v} is not a clip id"))).collect::<Result<_, _>>()?,
                    Some(v) => return Err(format!("clip_ids must be an array of integers, got {v}")),
                };
                Ok(Self::SyncToBeatMarkers { clip_ids })
            }
            "drop_marker" => {
                let kind = a.req_text("kind")?;
                let kind = parse_marker_kind(&kind).ok_or_else(|| format!("marker kind '{kind}' must be one of {}", MARKER_KINDS.join(" | ")))?;
                let label = a.text("label")?.unwrap_or_else(|| kind.replace('_', " "));
                Ok(Self::DropMarker { time_secs: a.req_secs("time_secs")?, label, kind: kind.into() })
            }
            "add_transition" => {
                let kind = a.req_text("transition_kind")?;
                parse_transition_kind(&kind).ok_or_else(|| format!("transition_kind '{kind}' must be one of {}", TRANSITION_KINDS.join(" | ")))?;
                let frames = a.num("duration_frames")?.unwrap_or(8.0);
                if frames.fract() != 0.0 || !(2.0..=120.0).contains(&frames) {
                    return Err(format!("duration_frames {frames} must be an integer in 2..=120"));
                }
                Ok(Self::AddTransition { clip_id: a.id("clip_id")?, transition_kind: kind, duration_frames: frames as i64 })
            }
            "color_grade" => Ok(Self::ColorGrade(post::parse_color_grade(&a)?)),
            "add_mask" => Ok(Self::AddMask(post::parse_mask(&a)?)),
            "generate_captions" => Ok(Self::GenerateCaptions { language: post::parse_language(&a)? }),
            "style_captions" => Ok(Self::StyleCaptions(post::parse_caption_style(&a)?)),
            "polish_audio" => Ok(Self::PolishAudio),
            "adjust_audio" => Ok(Self::AdjustAudio(post::parse_audio(&a)?)),
            "set_export_preset" => Ok(Self::SetExportPreset { delivery: post::parse_delivery(&a)? }),
            other => Err(format!("unknown tool '{}'", other.chars().take(64).collect::<String>())),
        }
    }
}

/// Typed accessors over a tool-call argument object.
pub(crate) struct ArgReader<'a>(pub(crate) &'a serde_json::Map<String, Value>);

impl ArgReader<'_> {
    pub(crate) fn flag(&self, k: &str) -> Result<Option<bool>, String> {
        match self.0.get(k) {
            None | Some(Value::Null) => Ok(None),
            Some(Value::Bool(b)) => Ok(Some(*b)),
            Some(Value::String(s)) if s.eq_ignore_ascii_case("true") || s == "1" => Ok(Some(true)),
            Some(Value::String(s)) if s.eq_ignore_ascii_case("false") || s == "0" => Ok(Some(false)),
            Some(v) => Err(format!("`{k}` must be true or false, got {v}")),
        }
    }
    pub(crate) fn num(&self, k: &str) -> Result<Option<f64>, String> {
        let n = match self.0.get(k) {
            None | Some(Value::Null) => return Ok(None),
            Some(Value::Number(n)) => n.as_f64(),
            // some models quote numbers
            Some(Value::String(s)) => s.trim().trim_end_matches('s').parse::<f64>().ok(),
            Some(_) => None,
        };
        match n {
            Some(v) if v.is_finite() => Ok(Some(v)),
            _ => Err(format!("`{k}` must be a finite number")),
        }
    }
    pub(crate) fn req_secs(&self, k: &str) -> Result<f64, String> {
        let v = self.num(k)?.ok_or_else(|| format!("missing required `{k}`"))?;
        if v < 0.0 {
            return Err(format!("`{k}` = {v} is negative"));
        }
        Ok(v)
    }
    pub(crate) fn id(&self, k: &str) -> Result<Option<u64>, String> {
        match self.0.get(k) {
            None | Some(Value::Null) => Ok(None),
            Some(v) => as_id(v).map(Some).ok_or_else(|| format!("`{k}` = {v} is not a clip id")),
        }
    }
    pub(crate) fn text(&self, k: &str) -> Result<Option<String>, String> {
        match self.0.get(k) {
            None | Some(Value::Null) => Ok(None),
            Some(Value::String(s)) if s.trim().is_empty() => Ok(None),
            Some(Value::String(s)) if s.chars().count() > MAX_TEXT_ARG => Err(format!("`{k}` is longer than {MAX_TEXT_ARG} characters")),
            Some(Value::String(s)) => Ok(Some(s.trim().to_string())),
            Some(v) => Err(format!("`{k}` must be a string, got {v}")),
        }
    }
    pub(crate) fn req_text(&self, k: &str) -> Result<String, String> {
        self.text(k)?.ok_or_else(|| format!("missing required `{k}`"))
    }
}

fn as_id(v: &Value) -> Option<u64> {
    match v {
        Value::Number(n) => n.as_u64().or_else(|| n.as_f64().filter(|f| *f >= 0.0 && f.fract() == 0.0).map(|f| f as u64)),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

pub const MARKER_KINDS: [&str; 5] = ["kill", "beat", "sub_bass", "snare", "standard"];
pub const TRANSITION_KINDS: [&str; 8] = ["whip_pan", "zoom_punch", "cross_dissolve", "flash_white", "dip_to_black", "slide", "push", "luma_wipe"];

fn parse_trim_mode(s: &str) -> Option<&'static str> {
    let l = s.trim().to_lowercase();
    if l.contains("start") || l == "q" {
        Some("ripple_start")
    } else if l.contains("end") || l == "w" {
        Some("ripple_end")
    } else if ["blade", "cut", "split", "razor"].iter().any(|k| l.contains(k)) {
        Some("blade")
    } else {
        None
    }
}

fn parse_marker_kind(s: &str) -> Option<&'static str> {
    match s.trim().to_lowercase().replace([' ', '-'], "_").as_str() {
        "kill" | "headshot" | "ace" => Some("kill"),
        "beat" => Some("beat"),
        "sub_bass" | "subbass" | "kick" | "bass" => Some("sub_bass"),
        "snare" => Some("snare"),
        "standard" | "marker" | "note" | "default" => Some("standard"),
        _ => None,
    }
}

/// Gemini `functionDeclarations` schema block for every Copilot tool.
pub fn gemini_tool_declarations() -> Value {
    let mut v = editing_declarations();
    if let Some(arr) = v["functionDeclarations"].as_array_mut() {
        arr.extend(post::declarations());
    }
    v
}

fn editing_declarations() -> Value {
    json!({
        "functionDeclarations": [
            {
                "name": "jump_to_timecode",
                "description": "Move the timeline playhead to a specific timestamp in seconds.",
                "parameters": {
                    "type": "OBJECT",
                    "properties": {
                        "time_secs": { "type": "NUMBER", "description": "Timestamp in seconds (>= 0)" }
                    },
                    "required": ["time_secs"]
                }
            },
            {
                "name": "apply_velocity_preset",
                "description": "Apply a fragmovie cubic-Bezier velocity preset (Valorant Flow Sync, CS2 AWP Double-Pump, Clean Skill Showcase, Bullet-Time Kill Ramp, Smooth Ease).",
                "parameters": {
                    "type": "OBJECT",
                    "properties": {
                        "clip_id": { "type": "INTEGER", "description": "Optional target clip ID; defaults to selected clip or clip under playhead" },
                        "preset_name": { "type": "STRING" }
                    },
                    "required": ["preset_name"]
                }
            },
            {
                "name": "add_effect_preset",
                "description": "Apply a built-in Argon montage preset (RSMB Clean Flick, Hard Kill Impact S_Shake Decay, Valorant Vibrant Clean CC, CS2 Moody Fragmovie CC, Ace Final-Kill Freeze Outline, Gaming 9:16 Split-Crop, etc.).",
                "parameters": {
                    "type": "OBJECT",
                    "properties": {
                        "clip_id": { "type": "INTEGER" },
                        "preset_name": { "type": "STRING" }
                    },
                    "required": ["preset_name"]
                }
            },
            {
                "name": "cut_and_ripple",
                "description": "Blade split or Q/W ripple trim at a timeline timecode.",
                "parameters": {
                    "type": "OBJECT",
                    "properties": {
                        "clip_id": { "type": "INTEGER" },
                        "time_secs": { "type": "NUMBER" },
                        "trim_mode": { "type": "STRING", "description": "blade | ripple_start | ripple_end" }
                    },
                    "required": ["time_secs", "trim_mode"]
                }
            },
            {
                "name": "sync_to_beat_markers",
                "description": "Snap and align clips to the timeline's detected music beat markers.",
                "parameters": {
                    "type": "OBJECT",
                    "properties": {
                        "clip_ids": { "type": "ARRAY", "items": { "type": "INTEGER" } }
                    }
                }
            },
            {
                "name": "drop_marker",
                "description": "Drop a color-coded marker (kill, beat, sub_bass, snare, standard) at time_secs.",
                "parameters": {
                    "type": "OBJECT",
                    "properties": {
                        "time_secs": { "type": "NUMBER" },
                        "label": { "type": "STRING" },
                        "kind": { "type": "STRING", "description": "kill | beat | sub_bass | snare | standard" }
                    },
                    "required": ["time_secs", "label", "kind"]
                }
            },
            {
                "name": "add_transition",
                "description": "Attach an incoming transition (whip_pan, zoom_punch, cross_dissolve, flash_white, dip_to_black, slide, push, luma_wipe) to a clip.",
                "parameters": {
                    "type": "OBJECT",
                    "properties": {
                        "clip_id": { "type": "INTEGER" },
                        "transition_kind": { "type": "STRING" },
                        "duration_frames": { "type": "INTEGER" }
                    },
                    "required": ["transition_kind", "duration_frames"]
                }
            }
        ]
    })
}

/// OpenAI / OpenRouter compatible `tools` array for every Copilot tool.
pub fn openai_tool_declarations() -> Value {
    let decls = gemini_tool_declarations();
    let funcs: Vec<Value> = decls["functionDeclarations"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .map(|f| {
                    json!({
                        "type": "function",
                        "function": {
                            "name": f["name"],
                            "description": f["description"],
                            "parameters": lowercase_json_types(&f["parameters"]),
                        }
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    Value::Array(funcs)
}

fn lowercase_json_types(v: &Value) -> Value {
    match v {
        Value::Object(map) => {
            let mut out = serde_json::Map::new();
            for (k, val) in map {
                if k == "type" {
                    if let Some(s) = val.as_str() {
                        out.insert(k.clone(), Value::String(s.to_lowercase()));
                        continue;
                    }
                }
                out.insert(k.clone(), lowercase_json_types(val));
            }
            Value::Object(out)
        }
        Value::Array(arr) => Value::Array(arr.iter().map(lowercase_json_types).collect()),
        other => other.clone(),
    }
}

/// Target clip for an action: an explicit `clip_id` must exist (it never
/// silently falls back to another clip); otherwise selection → clip under
/// the playhead → first clip.
fn target_clip(tl: &Timeline, clip_id: Option<u64>, playhead: Time, selection: &[ClipId]) -> Result<ClipId, String> {
    match clip_id {
        Some(raw) => tl.clip(ClipId(raw)).filter(|c| !c.is_gap()).map(|c| c.id).ok_or_else(|| format!("clip {raw} does not exist on the timeline")),
        None => resolve_target_clip(tl, None, playhead, selection).ok_or_else(|| "timeline has no clips".to_string()),
    }
}

fn resolve_target_clip(tl: &Timeline, clip_id: Option<u64>, playhead: Time, selection: &[ClipId]) -> Option<ClipId> {
    if let Some(raw) = clip_id {
        let id = ClipId(raw);
        if tl.clip(id).is_some() {
            return Some(id);
        }
    }
    if let Some(&id) = selection.first() {
        if tl.clip(id).is_some() {
            return Some(id);
        }
    }
    if let Some((idx, _)) = tl.spine_index_at(playhead) {
        if let Some(c) = tl.spine.get(idx) {
            if !c.is_gap() {
                return Some(c.id);
            }
        }
    }
    tl.spine.iter().find(|c| !c.is_gap()).map(|c| c.id)
}

/// Badge for one Copilot action: `Applied` or `Skipped: reason`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ActionOutcome {
    pub tool: String,
    pub applied: bool,
    pub detail: String,
    /// App work this action still needs (captions job, Render dialog).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request: Option<UiRequest>,
}

impl ActionOutcome {
    pub fn applied(tool: &str, detail: impl Into<String>) -> Self {
        Self { tool: tool.into(), applied: true, detail: detail.into(), request: None }
    }
    pub fn skipped(tool: &str, reason: impl Into<String>) -> Self {
        Self { tool: tool.into(), applied: false, detail: reason.into(), request: None }
    }
}

/// A validated action: the undoable commands it needs plus an optional playhead move.
struct PlannedAction {
    commands: Vec<Command>,
    playhead: Option<Time>,
    summary: String,
    request: Option<UiRequest>,
}

const COPILOT_UNDO_LABEL: &str = "AI Copilot Edit";

fn secs_in_bounds(tl: &Timeline, field: &str, secs: f64, allow_end: bool) -> Result<Time, String> {
    let end = tl.extent();
    let t = Time::from_secs_f64(secs);
    if !secs.is_finite() || secs < 0.0 || t > end || (!allow_end && t == end) {
        return Err(format!("{field} {secs:.2}s is outside the timeline (0–{:.2}s)", end.as_secs_f64()));
    }
    Ok(t)
}

/// Validate `call` against the live timeline and turn it into commands.
/// Nothing is mutated here.
fn plan_tool_call(call: &CopilotToolCall, tl: &Timeline, playhead: Time, selection: &[ClipId], env: &CopilotEnv) -> Result<PlannedAction, String> {
    let one = |cmd: Command, summary: String| Ok(PlannedAction { commands: vec![cmd], playhead: None, summary, request: None });
    let ask = |request: UiRequest, summary: String| Ok(PlannedAction { commands: Vec::new(), playhead: None, summary, request: Some(request) });
    match call {
        CopilotToolCall::JumpToTimecode { time_secs } => {
            let t = secs_in_bounds(tl, "time", *time_secs, true)?;
            Ok(PlannedAction { commands: Vec::new(), playhead: Some(t), summary: format!("Playhead → {:.2}s", t.as_secs_f64()), request: None })
        }
        CopilotToolCall::ApplyVelocityPreset { clip_id, preset_name } => {
            let id = target_clip(tl, *clip_id, playhead, selection)?;
            let clip = tl.clip(id).ok_or("clip missing")?;
            let retime = Retime::velocity_preset(preset_name, clip.duration)
                .or_else(|| MontagePreset::find(preset_name).and_then(|p| p.velocity_preset).and_then(|vp| Retime::velocity_preset(&vp, clip.duration)))
                .ok_or_else(|| format!("unknown velocity preset '{preset_name}' (known: {})", Retime::PRESET_NAMES.join(", ")))?;
            one(Command::SetRetime { id, retime: Some(retime) }, format!("Velocity '{preset_name}' on '{}'", clip.name))
        }
        CopilotToolCall::AddEffectPreset { clip_id, preset_name } => {
            let id = target_clip(tl, *clip_id, playhead, selection)?;
            let mut clip = tl.clip(id).cloned().ok_or("clip missing")?;
            let clip_start = tl.clip_start(id).unwrap_or(Time::ZERO);
            let local = (playhead - clip_start).clamp(Time::ZERO, clip.duration);

            let lower = preset_name.to_lowercase();
            if lower.contains("9:16") || lower.contains("split-crop") || lower.contains("tiktok") || lower.contains("shorts") {
                if clip.effects.iter().any(|e| e.kind == nle_core::EffectKind::GamingReframe916) {
                    return Err(format!("'{}' already has the 9:16 reframe", clip.name));
                }
                clip.effects.push(nle_core::Effect::new(nle_core::EffectKind::GamingReframe916));
                let name = clip.name.clone();
                return one(Command::UpdateClip { clip: Box::new(clip) }, format!("Gaming 9:16 Split-Crop on '{name}'"));
            }

            let preset = MontagePreset::find(preset_name)
                .or_else(|| {
                    let alias = if lower.contains("shake") || lower.contains("impact") {
                        "Hard Kill Impact"
                    } else if lower.contains("valorant") {
                        "Valorant Vibrant Clean CC"
                    } else if lower.contains("cs2") || lower.contains("moody") {
                        "CS2 Moody Fragmovie CC"
                    } else if lower.contains("rsmb") {
                        "RSMB Clean Flick"
                    } else {
                        return None;
                    };
                    MontagePreset::find(alias)
                })
                .ok_or_else(|| {
                    let names: Vec<String> = MontagePreset::built_in().into_iter().map(|p| p.name).collect();
                    format!("unknown preset '{preset_name}' (known: {})", names.join(", "))
                })?;

            preset.apply_to_clip(&mut clip, local);
            let name = clip.name.clone();
            let commands = vec![Command::ReplaceClip { label: "Apply Preset", clip: Box::new(clip) }];
            Ok(PlannedAction { commands, playhead: None, summary: format!("Preset '{}' on '{name}'", preset.name), request: None })
        }
        CopilotToolCall::CutAndRipple { clip_id, time_secs, trim_mode } => {
            let at = secs_in_bounds(tl, "cut", *time_secs, false)?.snap_to_frame(tl.format.rate);
            if at <= Time::ZERO {
                return Err("cannot cut at the very start of the timeline".into());
            }
            let secs = at.as_secs_f64();
            match parse_trim_mode(trim_mode) {
                Some("ripple_start") => {
                    let id = target_clip(tl, *clip_id, at, selection)?;
                    one(Command::RippleTrimStartTo { id, at }, format!("Ripple-trimmed start to {secs:.2}s"))
                }
                Some("ripple_end") => {
                    let id = target_clip(tl, *clip_id, at, selection)?;
                    one(Command::RippleTrimEndTo { id, at }, format!("Ripple-trimmed end to {secs:.2}s"))
                }
                Some(_) => match clip_id {
                    Some(raw) => {
                        let id = target_clip(tl, Some(*raw), at, selection)?;
                        let start = tl.clip_start(id).unwrap_or(Time::ZERO);
                        let end = start + tl.clip(id).map(|c| c.duration).unwrap_or(Time::ZERO);
                        if at <= start || at >= end {
                            return Err(format!("{secs:.2}s is outside clip {raw} ({:.2}–{:.2}s)", start.as_secs_f64(), end.as_secs_f64()));
                        }
                        one(Command::BladeClip { id, at }, format!("Blade split clip {raw} at {secs:.2}s"))
                    }
                    None => one(Command::Blade { at }, format!("Blade split at {secs:.2}s")),
                },
                None => Err(format!("unknown trim_mode '{trim_mode}'")),
            }
        }
        CopilotToolCall::SyncToBeatMarkers { clip_ids } => {
            let ids: Vec<ClipId> = if clip_ids.is_empty() {
                if selection.is_empty() {
                    tl.spine.iter().filter(|c| !c.is_gap()).map(|c| c.id).collect()
                } else {
                    selection.to_vec()
                }
            } else {
                clip_ids.iter().map(|&raw| target_clip(tl, Some(raw), playhead, selection)).collect::<Result<_, _>>()?
            };
            if ids.is_empty() {
                return Err("no clips to sync".into());
            }
            if !tl.markers.iter().any(|m| !matches!(m.kind, nle_core::MarkerKind::Standard | nle_core::MarkerKind::Kill)) {
                return Err("no beat markers on the timeline (run beat detection first)".into());
            }
            let n = ids.len();
            one(Command::SyncClipsToBeats { ids }, format!("Synced {n} clip(s) to beat markers"))
        }
        CopilotToolCall::DropMarker { time_secs, label, kind } => {
            let t = secs_in_bounds(tl, "marker", *time_secs, true)?;
            let marker = match parse_marker_kind(kind) {
                Some("kill") => Marker::kill(t, label.clone()),
                Some("beat") => {
                    let mut m = Marker::beat(t);
                    m.name = label.clone();
                    m
                }
                Some("sub_bass") => Marker::sub_bass(t),
                Some("snare") => Marker::snare(t),
                Some(_) => Marker::new(t, label.clone()),
                None => return Err(format!("unknown marker kind '{kind}'")),
            };
            let half_frame = Time(tl.frame().0 / 2);
            if tl.markers.iter().any(|m| m.kind == marker.kind && (m.time - t).abs() <= half_frame) {
                return Err(format!("a {kind} marker already exists at {:.2}s", t.as_secs_f64()));
            }
            let mut markers = tl.markers.clone();
            markers.push(marker);
            one(Command::SetMarkers { markers }, format!("{kind} marker '{label}' at {:.2}s", t.as_secs_f64()))
        }
        CopilotToolCall::AddTransition { clip_id, transition_kind, duration_frames } => {
            let id = target_clip(tl, *clip_id, playhead, selection)?;
            let kind = parse_transition_kind(transition_kind).ok_or_else(|| format!("unknown transition '{transition_kind}'"))?;
            if !(2..=120).contains(duration_frames) {
                return Err(format!("duration_frames {duration_frames} must be in 2..=120"));
            }
            let dur = Time(tl.frame().0 * duration_frames);
            let clip = tl.clip(id).ok_or("clip missing")?;
            if dur > clip.duration {
                return Err(format!("{duration_frames}-frame transition is longer than '{}'", clip.name));
            }
            one(
                Command::SetTransition { id, at_end: false, transition: Some(Transition::new(kind, dur)) },
                format!("{} transition ({duration_frames}f) on '{}'", kind.label(), clip.name),
            )
        }
        CopilotToolCall::ColorGrade(a) => {
            let (cmd, s) = post::plan_color_grade(tl, target_clip(tl, a.clip_id, playhead, selection)?, a)?;
            one(cmd, s)
        }
        CopilotToolCall::AddMask(a) => {
            let (cmd, s) = post::plan_mask(tl, target_clip(tl, a.clip_id, playhead, selection)?, a)?;
            one(cmd, s)
        }
        CopilotToolCall::StyleCaptions(a) => {
            let (cmd, s) = post::plan_caption_style(tl, a)?;
            one(cmd, s)
        }
        CopilotToolCall::PolishAudio => {
            let (commands, summary) = post::plan_polish_audio(tl, env)?;
            Ok(PlannedAction { commands, playhead: None, summary, request: None })
        }
        CopilotToolCall::AdjustAudio(a) => {
            let (cmd, s) = post::plan_audio(tl, target_clip(tl, a.clip_id, playhead, selection)?, a)?;
            one(cmd, s)
        }
        CopilotToolCall::GenerateCaptions { language } => {
            let (r, s) = post::plan_generate_captions(tl, language)?;
            ask(r, s)
        }
        CopilotToolCall::SetExportPreset { delivery } => {
            let (r, s) = post::plan_export(delivery);
            ask(r, s)
        }
    }
}

fn nothing_reason(call: &CopilotToolCall) -> String {
    match call {
        CopilotToolCall::SyncToBeatMarkers { .. } => "clips are already aligned to the beat markers".into(),
        _ => "nothing to change".into(),
    }
}

static COPILOT_RUN: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

/// Validate every action against the *live* timeline and apply the valid ones.
/// Each action is atomic (it applies completely or not at all), and the whole
/// run is a single undo step, so one Ctrl+Z reverts everything the Copilot did.
/// Returns one `Applied` / `Skipped: reason` outcome per action, in order.
pub fn apply_copilot_actions(calls: &[CopilotToolCall], tl: &mut Timeline, history: &mut History, playhead: &mut Time, selection: &[ClipId]) -> Vec<ActionOutcome> {
    apply_copilot_actions_with(calls, tl, history, playhead, selection, &CopilotEnv::default())
}

/// [`apply_copilot_actions`] with app-side facts (measured loudness). Actions
/// that need the app ([`UiRequest`]) come back `applied` with the request set;
/// the caller carries them out and may downgrade the outcome if that fails.
pub fn apply_copilot_actions_with(
    calls: &[CopilotToolCall],
    tl: &mut Timeline,
    history: &mut History,
    playhead: &mut Time,
    selection: &[ClipId],
    env: &CopilotEnv,
) -> Vec<ActionOutcome> {
    let key = Some(0xC0F1_0000_0000_0000 ^ COPILOT_RUN.fetch_add(1, std::sync::atomic::Ordering::Relaxed));
    let mut outcomes = Vec::with_capacity(calls.len());
    for (i, call) in calls.iter().enumerate() {
        let tool = call.name();
        if i >= MAX_ACTIONS_PER_REPLY {
            outcomes.push(ActionOutcome::skipped(tool, format!("more than {MAX_ACTIONS_PER_REPLY} actions in one reply")));
            continue;
        }
        // one Whisper job / one dialog per reply
        if call.request_kind().is_some() && calls[..i].iter().any(|c| c.request_kind() == call.request_kind()) {
            outcomes.push(ActionOutcome::skipped(tool, "already requested earlier in this reply"));
            continue;
        }
        let plan = match plan_tool_call(call, tl, *playhead, selection, env) {
            Ok(p) => p,
            Err(reason) => {
                outcomes.push(ActionOutcome::skipped(tool, reason));
                continue;
            }
        };
        if !plan.commands.is_empty() {
            let cmd = Command::Batch { label: COPILOT_UNDO_LABEL, commands: plan.commands };
            match history.execute_coalesced(tl, cmd, key) {
                Ok(()) => {}
                Err(nle_core::timeline::EditError::Nothing) => {
                    outcomes.push(ActionOutcome::skipped(tool, nothing_reason(call)));
                    continue;
                }
                Err(e) => {
                    outcomes.push(ActionOutcome::skipped(tool, e.to_string()));
                    continue;
                }
            }
        }
        if let Some(t) = plan.playhead {
            *playhead = t;
        }
        let mut out = ActionOutcome::applied(tool, plan.summary);
        out.request = plan.request;
        outcomes.push(out);
    }
    history.seal();
    *playhead = (*playhead).min(tl.extent().max(Time::ZERO));
    outcomes
}

/// Execute a single `CopilotToolCall` through `history` as one undo step.
pub fn execute_tool_call(call: &CopilotToolCall, tl: &mut Timeline, history: &mut History, playhead: &mut Time, selection: &[ClipId]) -> Result<String, String> {
    let outcome = apply_copilot_actions(std::slice::from_ref(call), tl, history, playhead, selection).remove(0);
    if outcome.applied || matches!(call, CopilotToolCall::SyncToBeatMarkers { .. }) && outcome.detail == nothing_reason(call) {
        Ok(outcome.detail)
    } else {
        Err(outcome.detail)
    }
}

fn parse_transition_kind(s: &str) -> Option<TransitionKind> {
    let lower = s.trim().to_lowercase();
    Some(if lower.contains("whip") {
        TransitionKind::WhipPan
    } else if lower.contains("zoom") || lower.contains("punch") {
        TransitionKind::ZoomPunch
    } else if lower.contains("white") || lower.contains("flash") {
        TransitionKind::DipToWhite
    } else if lower.contains("black") {
        TransitionKind::DipToBlack
    } else if lower.contains("slide") {
        TransitionKind::Slide
    } else if lower.contains("push") {
        TransitionKind::Push
    } else if lower.contains("wipe") || lower.contains("luma") {
        TransitionKind::LumaWipe
    } else if lower.contains("dissolve") || lower.contains("cross") || lower.contains("fade") {
        TransitionKind::CrossDissolve
    } else {
        return None;
    })
}

/// Deterministic local/offline planner used when no cloud API key is configured
/// or the remote endpoint is unavailable. Ensures all 5 quick chips and common
/// fragmovie commands produce clean, undoable tool calls.
pub fn offline_copilot_plan(prompt: &str, tl: &Timeline, playhead: Time, selection: &[ClipId]) -> (String, Vec<CopilotToolCall>) {
    let lower = prompt.to_lowercase();
    let target = resolve_target_clip(tl, None, playhead, selection).map(|id| id.0);
    let mut calls = Vec::new();
    let mut notes = Vec::new();

    let wants_export = ["export", "render", "deliver"].iter().any(|w| lower.contains(w));
    if (lower.contains("9:16") || lower.contains("tiktok") || lower.contains("shorts")) && !wants_export {
        calls.push(CopilotToolCall::AddEffectPreset { clip_id: target, preset_name: "Gaming 9:16 Split-Crop".into() });
        notes.push("Enabled dual-viewport 9:16 Split-Crop with killfeed PiP.");
    }
    if lower.contains("kill") && (lower.contains("marker") || lower.contains("find")) {
        // Place Kill markers at the centers of the first non-gap clips if no kill markers exist yet.
        let non_gap: Vec<_> = tl.placed_clips().into_iter().filter(|p| !p.clip.is_gap()).collect();
        if non_gap.is_empty() {
            calls.push(CopilotToolCall::DropMarker { time_secs: playhead.as_secs_f64(), label: "Kill 1".into(), kind: "kill".into() });
        } else {
            for (i, p) in non_gap.iter().take(3).enumerate() {
                let mid = (p.start + p.clip.duration.div_i64(2)).as_secs_f64();
                calls.push(CopilotToolCall::DropMarker { time_secs: mid, label: format!("Kill {}", i + 1), kind: "kill".into() });
            }
        }
        notes.push("Marked kill peaks on timeline.");
    }
    if lower.contains("beat") || lower.contains("sync") || lower.contains("fragmovie") {
        if tl.markers.is_empty() && tl.extent() > Time::from_secs(1) {
            calls.push(CopilotToolCall::DropMarker { time_secs: (tl.extent().as_secs_f64() * 0.5).max(0.5), label: "Kick Drop".into(), kind: "sub_bass".into() });
        }
        calls.push(CopilotToolCall::SyncToBeatMarkers { clip_ids: target.into_iter().collect() });
        notes.push("Synced clips to timeline beat markers.");
    }
    if lower.contains("velocity") || lower.contains("flow sync") || lower.contains("fragmovie") {
        let vp = if lower.contains("awp") || lower.contains("cs2") { "CS2 AWP / Deagle Double-Pump" } else { "Valorant Flow Sync" };
        calls.push(CopilotToolCall::ApplyVelocityPreset { clip_id: target, preset_name: vp.into() });
        notes.push("Applied fragmovie cubic-Bezier velocity curve.");
    }
    if lower.contains("shake") || lower.contains("fragmovie") || lower.contains("vibrant") {
        let fx = if lower.contains("vibrant") || lower.contains("fragmovie") { "Valorant Vibrant Clean CC" } else { "Hard Kill Impact (S_Shake Decay)" };
        calls.push(CopilotToolCall::AddEffectPreset { clip_id: target, preset_name: fx.into() });
        if lower.contains("fragmovie") || lower.contains("whip") {
            calls.push(CopilotToolCall::AddTransition { clip_id: target, transition_kind: "whip_pan".into(), duration_frames: 8 });
        }
        notes.push("Applied signature FX / transition preset.");
    }
    let has = |words: &[&str]| words.iter().any(|w| lower.contains(w));
    let look = [
        (&["black and white", "b&w", "monochrome"][..], "black_and_white"),
        (&["teal", "cinematic"][..], "teal_orange"),
        (&["moody", "dark look"][..], "moody"),
        (&["faded", "film look"][..], "faded"),
        (&["punchy", "pop the colo"][..], "punchy"),
        (&["warm"][..], "warm"),
        (&["cool", "cold"][..], "cool"),
    ]
    .into_iter()
    .find(|(w, _)| has(w));
    if let Some((_, look)) = look {
        calls.push(CopilotToolCall::ColorGrade(ColorGradeArgs {
            clip_id: target,
            look: Some(look.into()),
            intensity: None,
            exposure: None,
            contrast: None,
            saturation: None,
            temperature: None,
            tint: None,
        }));
        notes.push("Applied a colour look.");
    }
    if has(&["vignette"]) {
        calls.push(CopilotToolCall::AddMask(MaskArgs {
            clip_id: target,
            style: "vignette".into(),
            shape: "ellipse".into(),
            center: [0.5, 0.5],
            size: [0.85, 0.55],
            feather: 0.3,
            invert: true,
            exposure: Some(-0.7),
            saturation: None,
        }));
        notes.push("Added a vignette.");
    }
    if has(&["caption", "subtitle", "transcri"]) {
        if tl.captions.is_empty() || has(&["generate", "transcri", "add caption", "add subtitle"]) {
            calls.push(CopilotToolCall::GenerateCaptions { language: None });
            notes.push("Started caption transcription.");
        } else {
            calls.push(CopilotToolCall::StyleCaptions(CaptionStyleArgs { preset: Some("bold_pop".into()), ..Default::default() }));
            notes.push("Restyled captions.");
        }
    }
    if has(&["polish audio", "clean audio", "clean up the audio", "fix audio", "fix the audio", "normalize", "normalise", "loudness"]) {
        calls.push(CopilotToolCall::PolishAudio);
        notes.push("Polished the audio mix.");
    }
    if wants_export {
        let delivery = if has(&["discord"]) {
            "discord_25mb"
        } else if has(&["tiktok", "shorts", "reels", "9:16", "vertical"]) {
            "shorts_9x16"
        } else if has(&["4k"]) {
            "youtube_4k60"
        } else if has(&["youtube"]) {
            "youtube_1440p60"
        } else {
            "match_project"
        };
        calls.push(CopilotToolCall::SetExportPreset { delivery: delivery.into() });
        notes.push("Opened the Render dialog with a delivery preset.");
    }
    if calls.is_empty() {
        if let Some(secs_word) = lower.split_whitespace().find_map(|w| w.trim_end_matches('s').parse::<f64>().ok()) {
            calls.push(CopilotToolCall::JumpToTimecode { time_secs: secs_word });
            notes.push("Jumped playhead to requested timecode.");
        } else {
            notes.push("The offline planner couldn't map that request to a timeline action — no changes made. Try a quick chip or configure a cloud/local model.");
        }
    }

    (notes.join(" "), calls)
}
