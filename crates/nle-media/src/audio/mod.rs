//! Audio: decode → 48 kHz stereo `f32` → mixer (EQ, ducking) → cpal.

pub mod decode;
pub mod ducking;
pub mod eq;
pub mod mixer;
pub mod output;
pub mod preview;
pub mod resample;
pub mod vocal;

pub use decode::decode_audio;
pub use ducking::{duck_curve, DuckCurve, DuckEnvelope, Ducker, EnvelopeFollower, ENV_HOP_SECS};
pub use eq::ThreeBandEq;
pub use mixer::{dialogue_envelope, dialogue_spans, mixdown, needs_duck_envelope, to_mono_16k, TimelineMixer};
pub use output::{preview_gain, preview_gain_db, AudioEngine, PREVIEW_VOLUME_MAX};
pub use preview::AudioPreview;
pub use resample::{to_internal, INTERNAL_RATE};
pub use vocal::VocalClarity;
