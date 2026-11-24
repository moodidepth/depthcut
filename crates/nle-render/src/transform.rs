//! Layer placement math shared by the compositor and the viewer gizmos.
//!
//! A layer of native size `lw × lh` is first fitted into the frame
//! (aspect-preserving, like FCP's "Fit"), then transformed about its anchor:
//! `out = C + P + A + R·S·(q − A)`, where `q` is the layer-local point in
//! fitted output pixels relative to the layer centre. A flip mirrors the
//! picture inside its crop, so a cropped layer stays where it is.

use nle_core::eval::ResolvedVideo;

/// Row-major 2×3 affine matrix.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Affine {
    pub m: [[f32; 3]; 2],
}

impl Affine {
    pub fn apply(&self, p: [f32; 2]) -> [f32; 2] {
        [self.m[0][0] * p[0] + self.m[0][1] * p[1] + self.m[0][2], self.m[1][0] * p[0] + self.m[1][1] * p[1] + self.m[1][2]]
    }

    pub fn inverse(&self) -> Option<Affine> {
        let [[a, b, c], [d, e, f]] = self.m;
        let det = a * e - b * d;
        if det.abs() < 1e-12 {
            return None;
        }
        let ia = e / det;
        let ib = -b / det;
        let id = -d / det;
        let ie = a / det;
        Some(Affine { m: [[ia, ib, -(ia * c + ib * f)], [id, ie, -(id * c + ie * f)]] })
    }
}

/// Fitted size of a `lw × lh` layer inside a `w × h` frame.
pub fn fit_size(lw: f32, lh: f32, w: f32, h: f32) -> [f32; 2] {
    let s = (w / lw.max(1.0)).min(h / lh.max(1.0));
    [lw * s, lh * s]
}

/// Map from layer UV (0..1) to output pixels.
pub fn layer_to_output(v: &ResolvedVideo, layer: [f32; 2], frame: [f32; 2]) -> Affine {
    let [fw, fh] = fit_size(layer[0], layer[1], frame[0], frame[1]);
    let (sin, cos) = v.rotation.to_radians().sin_cos();
    let (sx, sy) = (v.scale[0], v.scale[1]);
    let anchor = [v.anchor[0] * fw, v.anchor[1] * fh];
    let centre = [frame[0] * 0.5 + v.position[0] * frame[0], frame[1] * 0.5 + v.position[1] * frame[1]];
    // q = (uv - 0.5) * fitted ; out = centre + A + R S (q - A)
    // R S = [[cos*sx, -sin*sy], [sin*sx, cos*sy]]
    let rs = [[cos * sx, -sin * sy], [sin * sx, cos * sy]];
    // uv → q; flipped, uv first mirrors about the crop's centre
    let axis = |size: f32, flip: bool, near: f32, far: f32| if flip { (-size, size * (0.5 + near - far)) } else { (size, -0.5 * size) };
    let (ux, ox) = axis(fw, v.flip[0], v.crop[0], v.crop[1]);
    let (uy, oy) = axis(fh, v.flip[1], v.crop[2], v.crop[3]);
    let q_m = [[ux, 0.0, ox], [0.0, uy, oy]];
    let t = [centre[0] + anchor[0] - (rs[0][0] * anchor[0] + rs[0][1] * anchor[1]), centre[1] + anchor[1] - (rs[1][0] * anchor[0] + rs[1][1] * anchor[1])];
    let mut m = [[0.0f32; 3]; 2];
    for r in 0..2 {
        m[r][0] = rs[r][0] * q_m[0][0] + rs[r][1] * q_m[1][0];
        m[r][1] = rs[r][0] * q_m[0][1] + rs[r][1] * q_m[1][1];
        m[r][2] = rs[r][0] * q_m[0][2] + rs[r][1] * q_m[1][2] + t[r];
    }
    Affine { m }
}

/// The output-space corners of a layer's picture (TL, TR, BR, BL of the
/// picture, so a flip swaps which side they show on), for gizmos.
pub fn layer_corners(v: &ResolvedVideo, layer: [f32; 2], frame: [f32; 2]) -> [[f32; 2]; 4] {
    let a = layer_to_output(v, layer, frame);
    let (l, r, t, b) = (v.crop[0], 1.0 - v.crop[1], v.crop[2], 1.0 - v.crop[3]);
    [a.apply([l, t]), a.apply([r, t]), a.apply([r, b]), a.apply([l, b])]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: [f32; 2], b: [f32; 2]) -> bool {
        (a[0] - b[0]).abs() < 1e-3 && (a[1] - b[1]).abs() < 1e-3
    }

    #[test]
    fn identity_fills_frame() {
        let v = ResolvedVideo::default();
        let a = layer_to_output(&v, [1920.0, 1080.0], [1920.0, 1080.0]);
        assert!(close(a.apply([0.0, 0.0]), [0.0, 0.0]));
        assert!(close(a.apply([1.0, 1.0]), [1920.0, 1080.0]));
        let inv = a.inverse().unwrap();
        assert!(close(inv.apply([960.0, 540.0]), [0.5, 0.5]));
    }

    #[test]
    fn letterbox_scale_rotate_position() {
        let mut v = ResolvedVideo::default();
        // 4:3 source in 16:9 frame is pillarboxed
        let a = layer_to_output(&v, [1440.0, 1080.0], [1920.0, 1080.0]);
        assert!(close(a.apply([0.0, 0.0]), [240.0, 0.0]));
        v.scale = [0.5, 0.5];
        v.position = [0.25, 0.0];
        let a = layer_to_output(&v, [1920.0, 1080.0], [1920.0, 1080.0]);
        assert!(close(a.apply([0.5, 0.5]), [960.0 + 480.0, 540.0]));
        assert!(close(a.apply([0.0, 0.0]), [960.0 + 480.0 - 480.0, 540.0 - 270.0]));
        v.rotation = 90.0;
        v.position = [0.0, 0.0];
        v.scale = [1.0, 1.0];
        let a = layer_to_output(&v, [1920.0, 1080.0], [1920.0, 1080.0]);
        // top-left corner rotates clockwise (y down) around the centre
        assert!(close(a.apply([0.0, 0.0]), [960.0 + 540.0, 540.0 - 960.0]));
        let inv = a.inverse().unwrap();
        assert!(close(inv.apply(a.apply([0.3, 0.7])), [0.3, 0.7]));
    }

    #[test]
    fn flip_keeps_the_cropped_box_in_place() {
        let mut v = ResolvedVideo { crop: [0.4, 0.1, 0.05, 0.3], rotation: 25.0, scale: [0.7, 1.3], ..Default::default() };
        let (layer, frame) = ([1440.0, 1080.0], [1920.0, 1080.0]);
        let [tl, tr, br, bl] = layer_corners(&v, layer, frame);
        for (flip, want) in [([true, false], [tr, tl, bl, br]), ([false, true], [bl, br, tr, tl]), ([true, true], [br, bl, tl, tr])] {
            v.flip = flip;
            let got = layer_corners(&v, layer, frame);
            assert!(got.iter().zip(want).all(|(a, b)| (a[0] - b[0]).abs() < 0.01 && (a[1] - b[1]).abs() < 0.01), "{flip:?}: {got:?}");
        }
    }
}
