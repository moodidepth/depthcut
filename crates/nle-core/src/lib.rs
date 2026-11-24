//! # nle-core
//!
//! Pure data model of the Rust Cut Pro editor — no I/O beyond autosave and
//! project files, no GPU, no FFmpeg. Everything here is deterministic and
//! unit-tested:
//!
//! * [`timeline`] — the Magnetic Timeline (primary storyline, connected
//!   clips, compound clips, gaps, ripple/insert/blade/trim/slip/slide).
//! * [`keyframe`] — cubic Bézier keyframe curves and animatable [`Param`]s.
//! * [`speed`] — retiming: speed presets, smooth ramps, reverse, freeze frames.
//! * [`command`] — undoable [`Command`]s and the [`History`] stack.
//! * [`eval`] — resolves the timeline at an instant into render/audio graphs.
//! * [`autosave`] — atomic 30-second crash-recovery snapshots.
//! * [`fcpxml`] — Final Cut Pro XML 1.10 import/export.
//! * [`color`] — colour management (input spaces, ACEScct working space) and stacked
//!   correction nodes with curves, HSL qualifier and shape masks.
//! * [`effects`] — stackable GPU effects (motion blur, halation, shake, grain, glow, blurs, chroma key).
//! * [`text_anim`] — animated Text+ presets (pop, typewriter, slide-up, tracking, glitch, word highlight).
//! * [`captions`] — word-timed captions track with SRT import/export.
//! * [`keymap`] — customisable keyboard shortcuts with FCP / Resolve presets (TOML).

pub mod asset;
pub mod audio_fx;
pub mod audio_polish;
pub mod autosave;
pub mod caption_follow;
pub mod captions;
pub mod clip;
pub mod color;
pub mod command;
pub mod edit_ops;
pub mod edl;
pub mod effects;
pub mod eval;
pub mod fcpxml;
pub mod focus;
pub mod ids;
pub mod interchange;
pub mod keyframe;
pub mod keymap;
pub mod library;
mod links;
pub mod media_mgmt;
pub mod motion;
pub mod otio;
pub mod presets;
pub mod project;
pub mod speed;
pub mod text_anim;
pub mod three_point;
pub mod time;
pub mod timeline;
pub mod tracks;
pub mod xml;
pub mod zoom_punch;

pub use asset::{AssetKind, Attribution, AudioStreamInfo, MediaAsset, VideoStreamInfo, YuvMatrix};
pub use audio_fx::{
    AudioFx, BusId, BusSettings, CompressorSettings, DeEsserSettings, DelaySettings, EqBand, EqBandType, EqFilter, FilterSlope, GateMode, GateSettings, LevelerSettings,
    MixerSettings, NoiseReductionSettings, ParametricEq, PluginInsert, ReverbSettings, VoiceIsolationSettings,
};
pub use captions::{CaptionSegment, CaptionStyle, CaptionTrack, CaptionWord};
pub use clip::{
    AudioParams, AudioRole, AvFlags, BlendMode, Clip, ClipKind, ColorParams, ColorWheel, CompoundClip, CropAspect, CropMode, Direction, DuckingSettings, EqSettings, FadeShape,
    LutRef, TitleParams, TitleStyle, Transition, TransitionKind, VideoParams,
};
pub use command::{Command, History};
pub use edit_ops::CopiedClip;
pub use effects::{Effect, EffectKind, MatteKind, ParamDef, ParamKind, ResolvedEffect};
pub use eval::{audio_spans, evaluate, AudioSpan, Composition, Layer, LayerSource, RenderNode};
pub use ids::{AssetId, ClipId};
pub use keyframe::{Curve, Handle, Interpolation, Keyframe, Param};
pub use keymap::{Action, ActionMap, KeyBinding, KeyCode};
pub use library::{AssetMetadata, Bin, Collection, KindFilter, Library, Rating, SmartCollection, SmartRule};
pub use presets::{ColorPreset, MontagePreset, PresetCategory};
pub use project::Project;
pub use speed::{FrameBlendMode, Retime, RetimeSegment};
pub use text_anim::{TextAnimation, TitleAnim};
pub use time::{format_timecode, format_timecode_df, parse_timecode, parse_timecode_df, FrameRate, Time, TICKS_PER_SECOND};
pub use timeline::{ClipLoc, EditError, Marker, MarkerKind, Timeline, VideoFormat};
pub use tracks::TrackState;
