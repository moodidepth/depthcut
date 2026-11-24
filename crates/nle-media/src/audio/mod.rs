//! Audio: decode → 48 kHz stereo `f32` → mixer (clip EQ / dynamics / inserts,
//! ducking, role busses, master) → cpal. Restoration (noise reduction,
//! voice isolation) is rendered ahead of time, see [`restore`].

pub mod clap_host;
pub mod decode;
pub mod ducking;
pub mod dynamics;
pub mod eq;
pub mod fx;
pub mod meter;
pub mod mixer;
pub mod output;
pub mod plugins;
pub mod preview;
pub mod record;
pub mod resample;
pub mod restore;
pub mod vocal;
pub mod vst3_host;

pub use decode::decode_audio;
pub use ducking::{duck_curve, DuckCurve, DuckEnvelope, Ducker, EnvelopeFollower, ENV_HOP_SECS};
pub use eq::ParametricEqProc;
pub use fx::{FxChain, GainReduction, Leveler};
pub use meter::{LoudnessMeter, MeterReading};
pub use mixer::{dialogue_envelope, dialogue_spans, mixdown, needs_duck_envelope, restore_all, restore_requests, to_mono_16k, StemTap, TimelineMixer};
pub use output::{preview_gain, preview_gain_db, AudioEngine, PREVIEW_VOLUME_MAX};
pub use preview::AudioPreview;
pub use resample::{to_internal, INTERNAL_RATE};
pub use restore::{needs_restore, RestoreKey, Restored};
pub use vocal::VocalClarity;
