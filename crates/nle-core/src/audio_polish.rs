//! One-click "Polish Audio" (Fairlight / Essential Sound style): role-aware
//! clean-up of every audible clip, applied as a single undoable batch.
//!
//! * Dialogue — loudness-normalize to -14 LUFS + vocal-clarity chain.
//! * Music    — normalize, sit the bed 6 dB under dialogue, auto-duck under voice.
//! * Effects  — normalize only (keeps gunshot / impact transients intact).

use crate::clip::{AudioRole, Clip, ClipKind};
use crate::command::Command;
use crate::ids::AssetId;
use crate::timeline::Timeline;

/// Undo-stack label for the batch.
pub const POLISH_AUDIO_LABEL: &str = "Polish Audio";
/// Integrated-loudness target per clip (YouTube / streaming).
pub const CLIP_TARGET_LUFS: f32 = -14.0;
/// Music bed level relative to dialogue.
pub const MUSIC_BED_DB: f64 = -6.0;

/// Build the polish batch. `lufs_of` returns the measured integrated loudness
/// of an asset (`None` while unanalysed or silent: the clip is then left
/// un-normalized but still gets its role processing). Returns `None` when
/// nothing would change.
pub fn polish_audio_command(tl: &Timeline, lufs_of: &mut dyn FnMut(AssetId) -> Option<f32>) -> Option<Command> {
    let commands: Vec<Command> = tl
        .all_ids()
        .into_iter()
        .filter_map(|id| tl.clip(id))
        .filter(|c| c.enabled && c.has_audio() && !c.audio.muted && matches!(c.kind, ClipKind::Asset { .. }))
        .filter_map(|c| {
            let lufs = c.asset_id().and_then(&mut *lufs_of);
            let polished = polish_clip(c, lufs);
            (polished.audio != c.audio).then(|| Command::UpdateClip { clip: Box::new(polished) })
        })
        .collect();
    (!commands.is_empty()).then_some(Command::Batch { label: POLISH_AUDIO_LABEL, commands })
}

fn polish_clip(c: &Clip, lufs: Option<f32>) -> Clip {
    let mut out = c.clone();
    let a = &mut out.audio;
    if let Some(l) = lufs {
        a.loudness_normalize = true;
        a.loudness_gain_db = (CLIP_TARGET_LUFS - l).clamp(-40.0, 30.0);
    }
    match a.role {
        AudioRole::Dialogue => a.vocal_clarity = true,
        AudioRole::Music => {
            a.ducking.enabled = true;
            // Only set the bed level when the user hasn't touched/animated volume.
            if a.volume_db.curve.is_none() && a.volume_db.value == 0.0 {
                a.volume_db.value = MUSIC_BED_DB;
            }
        }
        AudioRole::Effects | AudioRole::General => {}
    }
    out
}
