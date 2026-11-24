//! Maths for editing a layer's crop and transform by hand (inspector, viewer).

use crate::transform::{fit_size, layer_to_output};
use nle_core::eval::{ResolvedVideo, MAX_CROP_SUM};

/// Crop insets as fractions of the layer: `[left, right, top, bottom]`.
pub type Crop = [f32; 4];

/// Smallest part of a layer a crop leaves visible on either axis.
pub const MIN_VISIBLE: f32 = 1.0 - MAX_CROP_SUM;

fn centred(crop: Crop, w: f32, h: f32) -> Crop {
    let cx = (crop[0] + 1.0 - crop[1]) * 0.5;
    let cy = (crop[2] + 1.0 - crop[3]) * 0.5;
    let l = (cx - w * 0.5).max(0.0).min(1.0 - w);
    let t = (cy - h * 0.5).max(0.0).min(1.0 - h);
    [l, 1.0 - l - w, t, 1.0 - t - h]
}

/// The largest crop of a `layer`-sized picture with `ratio` (width over
/// height), as close as possible to the centre of the current crop.
pub fn aspect_crop(current: Crop, layer: [f32; 2], ratio: f32) -> Crop {
    let picture = layer[0].max(1.0) / layer[1].max(1.0);
    let (w, h) = if ratio > picture { (1.0, picture / ratio) } else { (ratio / picture, 1.0) };
    centred(current, w, h)
}

/// Move one edge of `crop` to `value`. With `locked`, the other axis follows
/// about its own centre so the cropped picture keeps its shape.
pub fn set_crop_edge(crop: Crop, edge: usize, value: f32, locked: bool) -> Crop {
    let opposite = edge ^ 1;
    let room = (MAX_CROP_SUM - crop[opposite]).max(0.0);
    let mut out = crop;
    out[edge] = value.max(0.0).min(room);
    if !locked {
        return out;
    }
    let (across, along) = if edge < 2 { (2, 0) } else { (0, 2) };
    let size = |c: &Crop, axis: usize| (1.0 - c[axis] - c[axis + 1]).max(MIN_VISIBLE);
    let per = size(&crop, across) / size(&crop, along);
    let len = size(&out, along).min(1.0 / per).max(MIN_VISIBLE / per);
    let other = (len * per).min(1.0);
    out[edge] = (1.0 - out[opposite] - len).max(0.0);
    let mid = (crop[across] + 1.0 - crop[across + 1]) * 0.5;
    let near = (mid - other * 0.5).max(0.0).min(1.0 - other);
    out[across] = near;
    out[across + 1] = 1.0 - near - other;
    out
}

/// A handle on a layer's box: -1, 0 or 1 per axis, as it shows before
/// rotation (`[-1, -1]` = top left, `[1, 0]` = right edge).
pub type Side = [i32; 2];

pub const CORNERS: [Side; 4] = [[-1, -1], [1, -1], [1, 1], [-1, 1]];
pub const EDGES: [Side; 4] = [[0, -1], [1, 0], [0, 1], [-1, 0]];

/// Scale range of the handles.
pub const MIN_SCALE: f32 = 0.01;
pub const MAX_SCALE: f32 = 20.0;

/// Where a drag snapped: an x and a y in output pixels.
pub type Guides = [Option<f32>; 2];

fn rotate(v: &ResolvedVideo, d: [f32; 2]) -> [f32; 2] {
    let (sin, cos) = v.rotation.to_radians().sin_cos();
    [cos * d[0] - sin * d[1], sin * d[0] + cos * d[1]]
}

fn unrotate(v: &ResolvedVideo, d: [f32; 2]) -> [f32; 2] {
    let (sin, cos) = v.rotation.to_radians().sin_cos();
    [cos * d[0] + sin * d[1], -sin * d[0] + cos * d[1]]
}

/// The point a layer rotates and scales about, in output pixels.
pub fn pivot(v: &ResolvedVideo, layer: [f32; 2], frame: [f32; 2]) -> [f32; 2] {
    let fitted = fit_size(layer[0], layer[1], frame[0], frame[1]);
    [0, 1].map(|i| frame[i] * (0.5 + v.position[i]) + v.anchor[i] * fitted[i])
}

/// A handle in fitted layer pixels from the pivot. The box is the crop's: a
/// flip leaves it in place.
fn handle_local(v: &ResolvedVideo, layer: [f32; 2], frame: [f32; 2], side: Side) -> [f32; 2] {
    let fitted = fit_size(layer[0], layer[1], frame[0], frame[1]);
    [0, 1].map(|i| {
        let (near, far) = (v.crop[i * 2], 1.0 - v.crop[i * 2 + 1]);
        let uv = match side[i] {
            s if s < 0 => near,
            s if s > 0 => far,
            _ => (near + far) * 0.5,
        };
        (uv - 0.5 - v.anchor[i]) * fitted[i]
    })
}

/// A handle in output pixels.
pub fn handle_point(v: &ResolvedVideo, layer: [f32; 2], frame: [f32; 2], side: Side) -> [f32; 2] {
    let h = handle_local(v, layer, frame, side);
    let d = rotate(v, [h[0] * v.scale[0], h[1] * v.scale[1]]);
    let c = pivot(v, layer, frame);
    [c[0] + d[0], c[1] + d[1]]
}

/// The box of a layer on screen (top left, top right, bottom right, bottom
/// left before rotation), whichever way it is flipped.
pub fn box_corners(v: &ResolvedVideo, layer: [f32; 2], frame: [f32; 2]) -> [[f32; 2]; 4] {
    CORNERS.map(|side| handle_point(v, layer, frame, side))
}

/// Whether `p` is inside a convex box.
pub fn inside(corners: &[[f32; 2]; 4], p: [f32; 2]) -> bool {
    let turns = [0, 1, 2, 3].map(|i| {
        let (a, b) = (corners[i], corners[(i + 1) % 4]);
        (b[0] - a[0]) * (p[1] - a[1]) - (b[1] - a[1]) * (p[0] - a[0])
    });
    turns.iter().all(|&t| t >= 0.0) || turns.iter().all(|&t| t <= 0.0)
}

/// The nearest frame edge or centre line within `tolerance` of any of `values`,
/// as `(distance to move, line)`.
fn nearest(values: &[f32], size: f32, tolerance: f32) -> Option<(f32, f32)> {
    values.iter().flat_map(|&v| [0.0, size * 0.5, size].map(|line| (line - v, line))).filter(|(d, _)| d.abs() <= tolerance).min_by(|a, b| a.0.abs().total_cmp(&b.0.abs()))
}

/// How far to nudge a moved box so its edges or centre land on the frame's.
pub fn snap_move(corners: &[[f32; 2]; 4], frame: [f32; 2], tolerance: f32) -> ([f32; 2], Guides) {
    let (mut shift, mut guides) = ([0.0; 2], [None; 2]);
    for i in 0..2 {
        let lo = corners.iter().map(|c| c[i]).fold(f32::INFINITY, f32::min);
        let hi = corners.iter().map(|c| c[i]).fold(f32::NEG_INFINITY, f32::max);
        if let Some((d, line)) = nearest(&[lo, (lo + hi) * 0.5, hi], frame[i], tolerance) {
            shift[i] = d;
            guides[i] = Some(line);
        }
    }
    (shift, guides)
}

/// The output axes a handle can be snapped on: both for a corner, the one it
/// travels along for an edge that is square to the frame, else neither.
fn snap_axes(v: &ResolvedVideo, side: Side) -> [bool; 2] {
    if side[0] != 0 && side[1] != 0 {
        return [true; 2];
    }
    let moves = rotate(v, [side[0] as f32, side[1] as f32]).map(|d| d.abs() > 1e-3);
    if moves == [true; 2] {
        [false; 2]
    } else {
        moves
    }
}

fn snap_point(v: &ResolvedVideo, side: Side, p: [f32; 2], frame: [f32; 2], tolerance: f32) -> ([f32; 2], Guides) {
    let (axes, mut out, mut guides) = (snap_axes(v, side), p, [None; 2]);
    for i in 0..2 {
        if let Some((d, line)) = nearest(&[p[i]], frame[i], tolerance).filter(|_| axes[i]) {
            out[i] += d;
            guides[i] = Some(line);
        }
    }
    (out, guides)
}

/// The scale that takes handle `side` to `to` about the pivot. With `keep`
/// both axes change by one factor. A zero `tolerance` does not snap.
pub fn resize(v: &ResolvedVideo, layer: [f32; 2], frame: [f32; 2], side: Side, to: [f32; 2], keep: bool, tolerance: f32) -> ([f32; 2], Guides) {
    let h = handle_local(v, layer, frame, side);
    let c = pivot(v, layer, frame);
    let (to, mut guides) = if keep || tolerance <= 0.0 { (to, [None; 2]) } else { snap_point(v, side, to, frame, tolerance) };
    let local = unrotate(v, [to[0] - c[0], to[1] - c[1]]);
    let mut scale = v.scale;
    if !keep {
        for i in 0..2 {
            // a handle level with the pivot cannot scale that axis
            if side[i] != 0 && h[i].abs() > 1e-3 {
                scale[i] = (local[i] / h[i]).clamp(MIN_SCALE, MAX_SCALE);
            }
        }
        return (scale, guides);
    }
    let arm = [0, 1].map(|i| if side[i] != 0 { v.scale[i] * h[i] } else { 0.0 });
    let len2 = arm[0] * arm[0] + arm[1] * arm[1];
    if len2 < 1e-6 {
        return (scale, guides);
    }
    let low = v.scale.map(|s| MIN_SCALE / s.abs().max(1e-4));
    let high = v.scale.map(|s| MAX_SCALE / s.abs().max(1e-4));
    let limit = |k: f32| k.min(high[0].min(high[1])).max(low[0].max(low[1]));
    let mut k = limit((local[0] * arm[0] + local[1] * arm[1]) / len2);
    scale = v.scale.map(|s| s * k);
    if tolerance <= 0.0 {
        return (scale, guides);
    }
    // the shape is fixed, so only the nearer line can be hit
    let at = handle_point(&ResolvedVideo { scale, ..v.clone() }, layer, frame, side);
    let axes = snap_axes(v, side);
    let hit = (0..2)
        .filter(|&i| axes[i] && (at[i] - c[i]).abs() > 1e-3)
        .filter_map(|i| nearest(&[at[i]], frame[i], tolerance).map(|(d, line)| (i, d, line)))
        .min_by(|a, b| a.1.abs().total_cmp(&b.1.abs()));
    if let Some((i, _, line)) = hit {
        let wanted = k * (line - c[i]) / (at[i] - c[i]);
        if wanted > 0.0 && limit(wanted) == wanted {
            k = wanted;
            scale = v.scale.map(|s| s * k);
            guides[i] = Some(line);
        }
    }
    (scale, guides)
}

/// Drag crop handle `side` to `to`. Returns the crop, the position that keeps
/// the picture still under it (a flip mirrors about the crop's centre) and
/// the lines it snapped to.
pub fn crop_drag(v: &ResolvedVideo, layer: [f32; 2], frame: [f32; 2], side: Side, to: [f32; 2], locked: bool, tolerance: f32) -> (Crop, [f32; 2], Guides) {
    let (to, guides) = if locked || tolerance <= 0.0 { (to, [None; 2]) } else { snap_point(v, side, to, frame, tolerance) };
    let Some(inv) = layer_to_output(v, layer, frame).inverse() else { return (v.crop, v.position, guides) };
    let (a, b) = (inv.apply(handle_point(v, layer, frame, side)), inv.apply(to));
    // the picture's own side under that handle
    let own = [0, 1].map(|i| if v.flip[i] { -side[i] } else { side[i] });
    let inset = [0, 1].map(|i| -(own[i] as f32) * (b[i] - a[i]));
    let edge = [0, 1].map(|i| i * 2 + (own[i] > 0) as usize);

    let mut crop = v.crop;
    if locked && side[0] != 0 && side[1] != 0 {
        let size = [0, 1].map(|i| (1.0 - crop[i * 2] - crop[i * 2 + 1]).max(MIN_VISIBLE));
        let change = [0, 1].map(|i| -inset[i] / size[i]);
        let k = 1.0 + if change[0].abs() >= change[1].abs() { change[0] } else { change[1] };
        let room = [0, 1].map(|i| (1.0 - crop[edge[i] ^ 1]) / size[i]);
        let k = k.min(room[0].min(room[1])).max(MIN_VISIBLE / size[0].min(size[1]));
        for i in 0..2 {
            crop[edge[i]] = (1.0 - crop[edge[i] ^ 1] - size[i] * k).max(0.0);
        }
    } else {
        for i in (0..2).filter(|&i| side[i] != 0) {
            crop = set_crop_edge(crop, edge[i], crop[edge[i]] + inset[i], locked);
        }
    }

    let fitted = fit_size(layer[0], layer[1], frame[0], frame[1]);
    let slid = [0, 1].map(|i| if v.flip[i] { v.scale[i] * fitted[i] * ((crop[i * 2] - crop[i * 2 + 1]) - (v.crop[i * 2] - v.crop[i * 2 + 1])) } else { 0.0 });
    let slid = rotate(v, slid);
    (crop, [0, 1].map(|i| v.position[i] - slid[i] / frame[i]), guides)
}

/// The turn from direction `from` to `to` (degrees), the short way round.
pub fn turn(from: f32, to: f32) -> f32 {
    (to - from + 180.0).rem_euclid(360.0) - 180.0
}

#[cfg(test)]
mod tests {
    use super::*;

    const LAYER: [f32; 2] = [1440.0, 1080.0];
    const FRAME: [f32; 2] = [1920.0, 1080.0];

    fn near(a: [f32; 2], b: [f32; 2], within: f32) -> bool {
        (a[0] - b[0]).abs() <= within && (a[1] - b[1]).abs() <= within
    }

    /// Layers that are moved, turned, cropped, flipped and off-anchor.
    fn layers() -> Vec<ResolvedVideo> {
        let mut out = Vec::new();
        for rotation in [0.0, 30.0, 90.0, -135.0] {
            for flip in [[false, false], [true, false], [false, true], [true, true]] {
                out.push(ResolvedVideo { position: [0.1, -0.05], scale: [0.8, 0.6], rotation, anchor: [0.05, -0.1], crop: [0.2, 0.05, 0.1, 0.15], flip, ..Default::default() });
            }
        }
        out
    }

    #[test]
    fn box_corners_are_where_the_renderer_draws_them() {
        for v in layers() {
            let drawn = crate::transform::layer_corners(&ResolvedVideo { flip: [false; 2], ..v.clone() }, LAYER, FRAME);
            let shown = crate::transform::layer_corners(&v, LAYER, FRAME);
            for (a, b) in box_corners(&v, LAYER, FRAME).into_iter().zip(drawn) {
                assert!(near(a, b, 0.01), "{v:?}");
                assert!(shown.iter().any(|&c| near(a, c, 0.01)), "{v:?}");
            }
        }
    }

    #[test]
    fn a_resized_handle_follows_the_pointer() {
        for v in layers() {
            for side in CORNERS.into_iter().chain(EDGES) {
                for keep in [false, true] {
                    let want = if keep { [v.scale[0] * 1.2, v.scale[1] * 1.2] } else { [0, 1].map(|i| if side[i] != 0 { v.scale[i] * [1.3, 0.7][i] } else { v.scale[i] }) };
                    let to = handle_point(&ResolvedVideo { scale: want, ..v.clone() }, LAYER, FRAME, side);
                    let (got, _) = resize(&v, LAYER, FRAME, side, to, keep, 0.0);
                    assert!(near(got, want, 1e-3), "{side:?} keep {keep}: {got:?} for {want:?}");
                }
            }
        }
    }

    #[test]
    fn resize_stays_in_range_and_keeps_the_shape_there() {
        let v = ResolvedVideo { scale: [0.8, 0.4], ..Default::default() };
        for to in [[-90000.0, -90000.0], [90000.0, 90000.0], [960.0, 540.0]] {
            let (s, _) = resize(&v, LAYER, FRAME, [1, 1], to, true, 0.0);
            assert!(s.iter().all(|&x| (MIN_SCALE * 0.999..=MAX_SCALE * 1.001).contains(&x)), "{s:?}");
            assert!((s[0] / s[1] - 2.0).abs() < 1e-3, "{s:?}");
        }
    }

    #[test]
    fn moves_snap_to_edges_and_centre() {
        let boxed = |x: f32, y: f32| [[x, y], [x + 400.0, y], [x + 400.0, y + 200.0], [x, y + 200.0]];
        for (x, y, shift, lines) in
            [(4.0, 437.0, [-4.0, 3.0], [Some(0.0), Some(540.0)]), (1517.0, 883.0, [3.0, -3.0], [Some(1920.0), Some(1080.0)]), (100.0, 100.0, [0.0, 0.0], [None, None])]
        {
            assert_eq!(snap_move(&boxed(x, y), FRAME, 8.0), (shift, lines));
        }
    }

    #[test]
    fn resizes_snap_to_the_frame() {
        let v = ResolvedVideo { scale: [0.5, 0.5], ..Default::default() };
        for keep in [false, true] {
            let (scale, guides) = resize(&v, FRAME, FRAME, [1, 1], [1915.0, 1077.0], keep, 8.0);
            assert!(near(scale, [1.0, 1.0], 1e-3), "{scale:?}");
            assert!(guides.contains(&Some(1920.0)) || guides.contains(&Some(1080.0)), "{guides:?}");
        }
    }

    #[test]
    fn a_crop_handle_follows_the_pointer_and_the_picture_stays_put() {
        for v in layers() {
            for side in CORNERS.into_iter().chain(EDGES) {
                let from = handle_point(&v, LAYER, FRAME, side);
                let d = rotate(&v, [side[0] as f32 * -30.0, side[1] as f32 * -20.0]);
                let to = [from[0] + d[0], from[1] + d[1]];
                let (crop, position, _) = crop_drag(&v, LAYER, FRAME, side, to, false, 0.0);
                let after = ResolvedVideo { crop, position, ..v.clone() };
                assert!(near(handle_point(&after, LAYER, FRAME, side), to, 0.1), "{side:?} {v:?}");
                let (a, b) = (layer_to_output(&v, LAYER, FRAME), layer_to_output(&after, LAYER, FRAME));
                assert!(near(a.apply([0.5, 0.5]), b.apply([0.5, 0.5]), 0.1), "{side:?} slid: {v:?}");
            }
        }
    }

    #[test]
    fn locked_crop_handles_keep_the_shape() {
        for v in layers() {
            let before = shape(v.crop, LAYER);
            for side in CORNERS.into_iter().chain(EDGES) {
                for by in [-2000.0, -40.0, 25.0, 3000.0] {
                    let from = handle_point(&v, LAYER, FRAME, side);
                    let (crop, _, _) = crop_drag(&v, LAYER, FRAME, side, [from[0] + by, from[1] + by], true, 0.0);
                    assert!((shape(crop, LAYER) / before - 1.0).abs() < 0.02, "{side:?} {by}: {crop:?}");
                    assert!(crop.iter().all(|&c| c >= -1e-5), "{side:?} {by}: {crop:?}");
                    assert!(crop[0] + crop[1] <= MAX_CROP_SUM + 1e-4 && crop[2] + crop[3] <= MAX_CROP_SUM + 1e-4, "{side:?} {by}: {crop:?}");
                }
            }
        }
    }

    #[test]
    fn inside_follows_the_turned_box() {
        for v in layers() {
            let c = box_corners(&v, LAYER, FRAME);
            let mid = [(c[0][0] + c[2][0]) * 0.5, (c[0][1] + c[2][1]) * 0.5];
            assert!(inside(&c, mid));
            assert!(!inside(&c, [c[0][0] * 2.0 - mid[0], c[0][1] * 2.0 - mid[1]]));
        }
    }

    #[test]
    fn turns_go_the_short_way_round() {
        assert_eq!(turn(170.0, -170.0), 20.0);
        assert_eq!(turn(-170.0, 170.0), -20.0);
        assert_eq!(turn(10.0, 40.0), 30.0);
    }

    fn shape(c: Crop, layer: [f32; 2]) -> f32 {
        (1.0 - c[0] - c[1]) * layer[0] / ((1.0 - c[2] - c[3]) * layer[1])
    }

    #[test]
    fn aspect_presets_have_their_shape_and_stay_inside() {
        for layer in [[1920.0, 1080.0], [1080.0, 1920.0], [640.0, 480.0]] {
            for ratio in [16.0 / 9.0, 9.0 / 16.0, 1.0, 0.8] {
                for from in [[0.0; 4], [0.8, 0.0, 0.0, 0.7]] {
                    let c = aspect_crop(from, layer, ratio);
                    assert!((shape(c, layer) - ratio).abs() < 1e-3, "{c:?}");
                    assert!(c.iter().all(|&v| v >= -1e-5), "{c:?}");
                }
            }
        }
    }

    #[test]
    fn edges_cannot_cross() {
        let c = set_crop_edge([0.0, 0.6, 0.0, 0.0], 0, 0.9, false);
        assert!(c[0] + c[1] <= MAX_CROP_SUM + 1e-5, "{c:?}");
        assert_eq!(set_crop_edge([0.2, 0.0, 0.0, 0.0], 0, -1.0, false)[0], 0.0);
    }

    #[test]
    fn locked_edges_keep_the_shape() {
        let layer = [1920.0, 1080.0];
        let start = aspect_crop([0.0; 4], layer, 1.0);
        for edge in 0..4 {
            for v in [0.0, 0.1, 0.3, 0.5, 0.95] {
                let c = set_crop_edge(start, edge, v, true);
                assert!((shape(c, layer) - 1.0).abs() < 0.01, "edge {edge} to {v}: {c:?}");
                assert!(c.iter().all(|&x| x >= -1e-5), "edge {edge} to {v}: {c:?}");
            }
        }
    }
}
