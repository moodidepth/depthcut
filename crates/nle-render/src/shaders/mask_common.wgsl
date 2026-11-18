// Shape mask coverage, shared by correct.wgsl and mask.wgsl (prepended).
// Mirrors nle_core::color::ShapeMask::alpha.

struct Mask {
    kind: u32,      // 0 none (full coverage), 1 ellipse, 2 rectangle, 3 polygon
    count: u32,     // polygon vertices
    invert: u32,
    feather: f32,   // frame heights
    center: vec2<f32>,
    size: vec2<f32>,
    rot_cs: vec2<f32>, // cos, sin of -rotation
    aspect: f32,
    _pad: f32,
    points: array<vec4<f32>, 32>, // two vertices per entry (xy, zw), 0..1 frame coords
};

fn mask_pt(m: Mask, i: u32) -> vec2<f32> {
    let v = m.points[i / 2u];
    var p = v.xy;
    if ((i & 1u) == 1u) {
        p = v.zw;
    }
    return vec2<f32>((p.x - 0.5) * m.aspect, p.y - 0.5);
}

fn feather_alpha(d: f32, feather: f32) -> f32 {
    let f = max(feather, 1e-4);
    let t = clamp(0.5 - d / f, 0.0, 1.0);
    return t * t * (3.0 - 2.0 * t);
}

// Coverage 0..1 at frame coordinate `uv` (0..1 across and down).
fn mask_alpha(m: Mask, uv: vec2<f32>) -> f32 {
    if (m.kind == 0u) {
        return 1.0;
    }
    let p = vec2<f32>((uv.x - 0.5) * m.aspect, uv.y - 0.5);
    var d = 1.0;
    if (m.kind == 3u) {
        let n = min(m.count, 64u);
        if (n >= 3u) {
            var best = 1e9;
            var inside = false;
            var j = n - 1u;
            for (var i = 0u; i < n; i = i + 1u) {
                let a = mask_pt(m, i);
                let b = mask_pt(m, j);
                let e = b - a;
                let w = p - a;
                let t = clamp(dot(w, e) / max(dot(e, e), 1e-12), 0.0, 1.0);
                let q = w - e * t;
                best = min(best, dot(q, q));
                if ((a.y > p.y) != (b.y > p.y)) {
                    if (p.x < (b.x - a.x) * (p.y - a.y) / (b.y - a.y) + a.x) {
                        inside = !inside;
                    }
                }
                j = i;
            }
            d = sqrt(best);
            if (inside) {
                d = -d;
            }
        }
    } else {
        let c = vec2<f32>((m.center.x - 0.5) * m.aspect, m.center.y - 0.5);
        let q = p - c;
        let l = vec2<f32>(q.x * m.rot_cs.x - q.y * m.rot_cs.y, q.x * m.rot_cs.y + q.y * m.rot_cs.x);
        let hs = max(m.size, vec2<f32>(1e-4));
        if (m.kind == 1u) {
            let k = length(l / hs);
            d = (k - 1.0) * min(hs.x, hs.y);
        } else {
            let dd = abs(l) - hs;
            d = length(max(dd, vec2<f32>(0.0))) + min(max(dd.x, dd.y), 0.0);
        }
    }
    var a = feather_alpha(d, m.feather);
    if (m.invert != 0u) {
        a = 1.0 - a;
    }
    return a;
}
