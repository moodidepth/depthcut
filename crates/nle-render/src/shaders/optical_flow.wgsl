// Dense optical flow: multi-scale pyramidal block matching + Lucas-Kanade.
//
// Produces per-pixel 2D screen-space motion vectors `v` such that
// B(x + v) ≈ A(x) (Frame[N] → Frame[N+1]), stored in an `rg16float`
// texture (rgba16float on adapters without rg16float storage support —
// the renderer rewrites the storage format below before compiling).
//
// Passes (all share one binding interface):
//   luma       A, B (full res, RGB)      → level 0 (r = luma A, g = luma B)
//   downsample level k                  → level k+1 (half size)
//   block_init coarsest level           → initial flow (±SEARCH px SSD search)
//   upsample   flow at level k+1        → flow at level k (×2)
//   lk_iter    level k + flow           → refined flow (one LK Gauss-Newton step)
// Flow is in pixels of the level it lives on.

struct Flow {
    swap: u32,      // 1: estimate B → A (backward flow) in the luma pass
    radius: i32,    // LK window radius
    search: i32,    // block-matching search radius (coarsest level)
    _pad: u32,
};

@group(0) @binding(0) var img_a: texture_2d<f32>;
@group(0) @binding(1) var img_b: texture_2d<f32>;
@group(0) @binding(2) var flow_in: texture_2d<f32>;
@group(0) @binding(3) var samp: sampler;
@group(0) @binding(4) var<uniform> u: Flow;
@group(0) @binding(5) var dst: texture_storage_2d<rg16float, write>;

const LUMA = vec3<f32>(0.2126, 0.7152, 0.0722);

fn out_dims() -> vec2<i32> {
    return vec2<i32>(textureDimensions(dst));
}

fn inside(id: vec3<u32>) -> bool {
    let d = textureDimensions(dst);
    return id.x < d.x && id.y < d.y;
}

fn uv_of(id: vec3<u32>) -> vec2<f32> {
    return (vec2<f32>(id.xy) + vec2<f32>(0.5)) / vec2<f32>(textureDimensions(dst));
}

// Pyramid level texel (r = A, g = B), clamped.
fn lvl(p: vec2<i32>) -> vec2<f32> {
    let d = vec2<i32>(textureDimensions(img_a)) - vec2<i32>(1);
    return textureLoad(img_a, clamp(p, vec2<i32>(0), d), 0).rg;
}

// Bilinear luma of B at a fractional pixel position of the level.
fn lb_at(p: vec2<f32>) -> f32 {
    let uv = p / vec2<f32>(textureDimensions(img_a));
    return textureSampleLevel(img_a, samp, uv, 0.0).g;
}

@compute @workgroup_size(8, 8, 1)
fn luma(@builtin(global_invocation_id) id: vec3<u32>) {
    if (!inside(id)) {
        return;
    }
    let uv = uv_of(id);
    let a = dot(textureSampleLevel(img_a, samp, uv, 0.0).rgb, LUMA);
    let b = dot(textureSampleLevel(img_b, samp, uv, 0.0).rgb, LUMA);
    var v = vec2<f32>(a, b);
    if (u.swap == 1u) {
        v = vec2<f32>(b, a);
    }
    textureStore(dst, vec2<i32>(id.xy), vec4<f32>(v, 0.0, 1.0));
}

@compute @workgroup_size(8, 8, 1)
fn downsample(@builtin(global_invocation_id) id: vec3<u32>) {
    if (!inside(id)) {
        return;
    }
    // bilinear tap at the shared corner of 2×2 finer texels = box filter
    let v = textureSampleLevel(img_a, samp, uv_of(id), 0.0).rg;
    textureStore(dst, vec2<i32>(id.xy), vec4<f32>(v, 0.0, 1.0));
}

@compute @workgroup_size(8, 8, 1)
fn block_init(@builtin(global_invocation_id) id: vec3<u32>) {
    if (!inside(id)) {
        return;
    }
    let p = vec2<i32>(id.xy);
    var best = vec2<i32>(0);
    var best_cost = 1e9;
    for (var dy = -u.search; dy <= u.search; dy = dy + 1) {
        for (var dx = -u.search; dx <= u.search; dx = dx + 1) {
            let d = vec2<i32>(dx, dy);
            var cost = 0.0;
            for (var y = -2; y <= 2; y = y + 1) {
                for (var x = -2; x <= 2; x = x + 1) {
                    let o = vec2<i32>(x, y);
                    let e = lvl(p + o).r - lvl(p + o + d).g;
                    cost = cost + e * e;
                }
            }
            // small bias towards zero motion (static HUD, flat areas)
            cost = cost * (1.0 + 0.02 * f32(dx * dx + dy * dy));
            if (cost < best_cost) {
                best_cost = cost;
                best = d;
            }
        }
    }
    textureStore(dst, p, vec4<f32>(vec2<f32>(best), 0.0, 1.0));
}

@compute @workgroup_size(8, 8, 1)
fn upsample(@builtin(global_invocation_id) id: vec3<u32>) {
    if (!inside(id)) {
        return;
    }
    let v = textureSampleLevel(flow_in, samp, uv_of(id), 0.0).xy * 2.0;
    textureStore(dst, vec2<i32>(id.xy), vec4<f32>(v, 0.0, 1.0));
}

@compute @workgroup_size(8, 8, 1)
fn lk_iter(@builtin(global_invocation_id) id: vec3<u32>) {
    if (!inside(id)) {
        return;
    }
    let p = vec2<i32>(id.xy);
    let fd = vec2<i32>(textureDimensions(flow_in)) - vec2<i32>(1);
    // light 3×3 smoothing of the incoming estimate
    var v = vec2<f32>(0.0);
    for (var y = -1; y <= 1; y = y + 1) {
        for (var x = -1; x <= 1; x = x + 1) {
            let w = select(0.0625, 0.125, x == 0 || y == 0);
            v = v + textureLoad(flow_in, clamp(p + vec2<i32>(x, y), vec2<i32>(0), fd), 0).xy * select(w, 0.25, x == 0 && y == 0);
        }
    }
    var gxx = 0.0;
    var gxy = 0.0;
    var gyy = 0.0;
    var bx = 0.0;
    var by = 0.0;
    let r = u.radius;
    let sigma2 = f32(r * r) * 0.5 + 0.5;
    let pf = vec2<f32>(p) + vec2<f32>(0.5);
    for (var y = -r; y <= r; y = y + 1) {
        for (var x = -r; x <= r; x = x + 1) {
            let o = vec2<i32>(x, y);
            let q = p + o;
            let ix = (lvl(q + vec2<i32>(1, 0)).r - lvl(q - vec2<i32>(1, 0)).r) * 0.5;
            let iy = (lvl(q + vec2<i32>(0, 1)).r - lvl(q - vec2<i32>(0, 1)).r) * 0.5;
            let it = lb_at(pf + vec2<f32>(o) + v) - lvl(q).r;
            let w = exp(-f32(x * x + y * y) / (2.0 * sigma2));
            gxx = gxx + w * ix * ix;
            gxy = gxy + w * ix * iy;
            gyy = gyy + w * iy * iy;
            bx = bx + w * ix * it;
            by = by + w * iy * it;
        }
    }
    // Tikhonov regularisation keeps flat / aperture-problem areas stable
    let lambda = 1e-4;
    let a = gxx + lambda;
    let d = gyy + lambda;
    let det = a * d - gxy * gxy;
    var delta = vec2<f32>(0.0);
    if (det > 1e-9) {
        delta = -vec2<f32>(d * bx - gxy * by, a * by - gxy * bx) / det;
    }
    let len = length(delta);
    if (len > 2.0) {
        delta = delta * (2.0 / len);
    }
    textureStore(dst, p, vec4<f32>(v + delta, 0.0, 1.0));
}
