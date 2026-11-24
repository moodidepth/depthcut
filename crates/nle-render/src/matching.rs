//! Shot match: a correction node that brings one frame's colour balance and
//! contrast to a reference frame's (Resolve's Shot Match, Premiere's Color
//! Match). Per channel, the node maps the mean and spread of the frame's
//! working-space values onto the reference's, using only the offset and
//! gain wheels so the result stays editable by hand.

use crate::colormath::{display_decode, to_working, Rgb};
use nle_core::color::{ColorCorrection, WorkingSpace};

/// The name of the node [`shot_match`] makes; matching again replaces it.
pub const SHOT_MATCH_NODE: &str = "Shot Match";

/// Per-channel mean and standard deviation in the working space.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ChannelStats {
    pub mean: Rgb,
    pub sd: Rgb,
}

/// Statistics of an RGBA8 display-referred image (transparent pixels are
/// skipped; large images are sampled).
pub fn stats(rgba: &[u8], ws: WorkingSpace) -> Option<ChannelStats> {
    let px = rgba.len() / 4;
    let step = (px / 65_536).max(1);
    let (mut n, mut s, mut s2) = (0.0f64, [0.0f64; 3], [0.0f64; 3]);
    for p in rgba.as_chunks::<4>().0.iter().step_by(step) {
        if p[3] == 0 {
            continue;
        }
        let w = to_working(display_decode([p[0], p[1], p[2]].map(|v| v as f32 / 255.0)), ws);
        for i in 0..3 {
            s[i] += w[i] as f64;
            s2[i] += (w[i] as f64) * (w[i] as f64);
        }
        n += 1.0;
    }
    if n < 16.0 {
        return None;
    }
    let mean = s.map(|v| v / n);
    let sd = [0, 1, 2].map(|i| (s2[i] / n - mean[i] * mean[i]).max(0.0).sqrt() as f32);
    Some(ChannelStats { mean: mean.map(|v| v as f32), sd })
}

/// The node turning `source` into a match for `reference`.
pub fn match_node(source: &ChannelStats, reference: &ChannelStats) -> ColorCorrection {
    let mut n = ColorCorrection::named(SHOT_MATCH_NODE);
    for i in 0..3 {
        // per channel the node does (c + 0.1·offset)·(1 + 0.5·gain) before
        // anything else non-neutral; aim for a·c + b
        let a = if source.sd[i] > 1e-4 { (reference.sd[i] / source.sd[i]).clamp(0.5, 2.0) } else { 1.0 };
        let b = reference.mean[i] - a * source.mean[i];
        let gain = 2.0 * (a - 1.0);
        let offset = 10.0 * b / a;
        match i {
            0 => (n.gain.r, n.offset.r) = (gain, offset),
            1 => (n.gain.g, n.offset.g) = (gain, offset),
            _ => (n.gain.b, n.offset.b) = (gain, offset),
        }
    }
    n
}

/// Match `source` (RGBA8, rendered without an earlier match node) to
/// `reference` (RGBA8, e.g. a still).
pub fn shot_match(source: &[u8], reference: &[u8], ws: WorkingSpace) -> Option<ColorCorrection> {
    Some(match_node(&stats(source, ws)?, &stats(reference, ws)?))
}

/// `nodes` with any earlier match node replaced by `node` (or `node` added
/// at the end).
pub fn put_match_node(nodes: &mut Vec<ColorCorrection>, node: ColorCorrection) {
    match nodes.iter().position(|n| n.name == SHOT_MATCH_NODE) {
        Some(i) => nodes[i] = node,
        None => nodes.push(node),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::colormath::correct_node;

    /// A varied test image: gradients and a few saturated patches.
    fn image(seed: u32) -> Vec<u8> {
        let mut out = Vec::new();
        for y in 0..96u32 {
            for x in 0..128u32 {
                let h = (x * 7 + y * 13 + seed * 31) % 97;
                out.extend_from_slice(&[(x * 2) as u8, (y * 2 + h) as u8, ((x + y) % 200 + 20) as u8, 255]);
            }
        }
        out
    }

    fn graded(img: &[u8], n: &ColorCorrection, ws: WorkingSpace) -> Vec<u8> {
        img.as_chunks::<4>()
            .0
            .iter()
            .flat_map(|p| {
                let c = correct_node([p[0], p[1], p[2]].map(|v| v as f32 / 255.0), n, ws);
                let q = c.map(|v| (v.clamp(0.0, 1.0) * 255.0).round() as u8);
                [q[0], q[1], q[2], 255]
            })
            .collect()
    }

    #[test]
    fn matched_frame_takes_on_the_references_balance() {
        for ws in [WorkingSpace::Rec709Gamma, WorkingSpace::AcesCct] {
            let reference = image(1);
            // the same scene shot warmer and flatter
            let mut off = ColorCorrection::named("Off");
            off.gain.r = 0.3;
            off.gain.b = -0.2;
            off.contrast = -0.25;
            let source = graded(&reference, &off, ws);
            let node = shot_match(&source, &reference, ws).unwrap();
            let fixed = graded(&source, &node, ws);
            let (r, s, f) = (stats(&reference, ws).unwrap(), stats(&source, ws).unwrap(), stats(&fixed, ws).unwrap());
            for i in 0..3 {
                let before = (s.mean[i] - r.mean[i]).abs() + (s.sd[i] - r.sd[i]).abs();
                let after = (f.mean[i] - r.mean[i]).abs() + (f.sd[i] - r.sd[i]).abs();
                assert!(after < 0.01, "{ws:?} channel {i}: {after} (was {before})");
                assert!(after < before * 0.5 || before < 0.01, "{ws:?} channel {i}: {after} vs {before}");
            }
        }
    }

    #[test]
    fn matching_a_frame_to_itself_is_neutral() {
        let img = image(3);
        let n = shot_match(&img, &img, WorkingSpace::AcesCct).unwrap();
        assert!(n.gain.as_array().iter().chain(n.offset.as_array().iter()).all(|v| v.abs() < 1e-3), "{n:?}");
    }

    #[test]
    fn a_new_match_replaces_the_old_one() {
        let mut nodes = vec![ColorCorrection::named("Look")];
        put_match_node(&mut nodes, ColorCorrection::named(SHOT_MATCH_NODE));
        put_match_node(&mut nodes, ColorCorrection { exposure: 1.0, ..ColorCorrection::named(SHOT_MATCH_NODE) });
        assert_eq!(nodes.len(), 2);
        assert_eq!(nodes[1].exposure, 1.0);
        assert!(stats(&[0, 0, 0, 0], WorkingSpace::AcesCct).is_none());
    }
}
