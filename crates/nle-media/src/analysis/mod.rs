//! Offline audio analysis: loudness (BS.1770 / EBU R128), multi-band beat
//! detection, and VOD kill/action transient scanning.

pub mod beats;
pub mod kills;
pub mod loudness;

pub use beats::{analyze as analyze_beats, analyze_multiband, BeatAnalysis, BeatBand, MultiBandBeat};
pub use kills::{analyze_kills_from_samples, KillPeak};
pub use loudness::{integrated_lufs, normalize_gain_db, TARGET_LUFS};
