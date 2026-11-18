// Colour-space helpers shared by grade.wgsl and correct.wgsl (prepended to
// both). Index order matches nle_core::color::ColorSpace / WorkingSpace;
// colormath.rs mirrors every function for the tests.

const LUMA = vec3<f32>(0.2126, 0.7152, 0.0722);

const CS_REC709: u32 = 0u;
const CS_SRGB: u32 = 1u;
const CS_LINEAR: u32 = 2u;
const CS_PQ: u32 = 3u;
const CS_HLG: u32 = 4u;
const CS_LOGC3: u32 = 5u;
const CS_SLOG3: u32 = 6u;
const CS_VLOG: u32 = 7u;

const WS_GAMMA: u32 = 0u;
const WS_ACESCCT: u32 = 1u;

// Gamut matrices to linear Rec.709 (WGSL mat3x3 constructors are column-major,
// so each is written as its transpose: rows below are the source columns).
fn m_rec2020_to_709() -> mat3x3<f32> {
    return transpose(mat3x3<f32>(
        vec3<f32>(1.660491, -0.587641, -0.072850),
        vec3<f32>(-0.124550, 1.132900, -0.008349),
        vec3<f32>(-0.018151, -0.100579, 1.118730)));
}
fn m_awg3_to_709() -> mat3x3<f32> {
    return transpose(mat3x3<f32>(
        vec3<f32>(1.617523, -0.537287, -0.080237),
        vec3<f32>(-0.070573, 1.334613, -0.264040),
        vec3<f32>(-0.021102, -0.226954, 1.248056)));
}
fn m_sgamut3cine_to_709() -> mat3x3<f32> {
    return transpose(mat3x3<f32>(
        vec3<f32>(1.626947, -0.540139, -0.086809),
        vec3<f32>(-0.178516, 1.417941, -0.239425),
        vec3<f32>(-0.044436, -0.195920, 1.240356)));
}
fn m_vgamut_to_709() -> mat3x3<f32> {
    return transpose(mat3x3<f32>(
        vec3<f32>(1.806576, -0.695697, -0.110879),
        vec3<f32>(-0.170090, 1.305955, -0.135865),
        vec3<f32>(-0.025206, -0.154468, 1.179674)));
}
fn m_709_to_ap1() -> mat3x3<f32> {
    return transpose(mat3x3<f32>(
        vec3<f32>(0.613097, 0.339523, 0.047379),
        vec3<f32>(0.070194, 0.916354, 0.013452),
        vec3<f32>(0.020616, 0.109570, 0.869815)));
}
fn m_ap1_to_709() -> mat3x3<f32> {
    return transpose(mat3x3<f32>(
        vec3<f32>(1.705051, -0.621792, -0.083259),
        vec3<f32>(-0.130256, 1.140805, -0.010548),
        vec3<f32>(-0.024003, -0.128969, 1.152972)));
}

fn srgb_decode1(v: f32) -> f32 {
    if (v <= 0.04045) {
        return v / 12.92;
    }
    return pow((v + 0.055) / 1.055, 2.4);
}

fn pq_decode1(v: f32) -> f32 {
    // SMPTE ST 2084 EOTF, scaled so 203 nits (reference white) = 1.0
    let p = pow(clamp(v, 0.0, 1.0), 1.0 / 78.84375);
    let y = pow(max(p - 0.8359375, 0.0) / (18.8515625 - 18.6875 * p), 1.0 / 0.1593017578125);
    return y * 10000.0 / 203.0;
}

fn hlg_decode1(v: f32) -> f32 {
    // inverse OETF, scaled so reference white (75 % signal) = 1.0
    var e: f32;
    if (v <= 0.5) {
        e = v * v / 3.0;
    } else {
        e = (exp((v - 0.55991073) / 0.17883277) + 0.28466892) / 12.0;
    }
    return e / 0.26496256;
}

fn logc3_decode1(t: f32) -> f32 {
    if (t > 0.149658) {
        return (pow(10.0, (t - 0.385537) / 0.247190) - 0.052272) / 5.555556;
    }
    return (t - 0.092809) / 5.367655;
}

fn slog3_decode1(t: f32) -> f32 {
    if (t >= 171.2102946929 / 1023.0) {
        return pow(10.0, (t * 1023.0 - 420.0) / 261.5) * 0.19 - 0.01;
    }
    return (t * 1023.0 - 95.0) * 0.01125 / (171.2102946929 - 95.0);
}

fn vlog_decode1(t: f32) -> f32 {
    if (t < 0.181) {
        return (t - 0.125) / 5.6;
    }
    return pow(10.0, (t - 0.598206) / 0.241514) - 0.00873;
}

// Decoded code values of `space` → linear light, Rec.709 primaries
// (scene-referred for log, 1.0 = reference white for HDR).
fn to_linear709(c: vec3<f32>, space: u32) -> vec3<f32> {
    switch space {
        case 1u: {
            return vec3<f32>(srgb_decode1(c.r), srgb_decode1(c.g), srgb_decode1(c.b));
        }
        case 2u: {
            return c;
        }
        case 3u: {
            return m_rec2020_to_709() * vec3<f32>(pq_decode1(c.r), pq_decode1(c.g), pq_decode1(c.b));
        }
        case 4u: {
            return m_rec2020_to_709() * vec3<f32>(hlg_decode1(c.r), hlg_decode1(c.g), hlg_decode1(c.b));
        }
        case 5u: {
            return m_awg3_to_709() * vec3<f32>(logc3_decode1(c.r), logc3_decode1(c.g), logc3_decode1(c.b));
        }
        case 6u: {
            return m_sgamut3cine_to_709() * vec3<f32>(slog3_decode1(c.r), slog3_decode1(c.g), slog3_decode1(c.b));
        }
        case 7u: {
            return m_vgamut_to_709() * vec3<f32>(vlog_decode1(c.r), vlog_decode1(c.g), vlog_decode1(c.b));
        }
        default: {
            return pow(max(c, vec3<f32>(0.0)), vec3<f32>(2.4));
        }
    }
}

// Highlight roll-off for scene-referred / HDR sources: values above the knee
// compress smoothly towards 1.0 (hue kept by scaling on max(R, G, B)).
const KNEE: f32 = 0.8;
fn tonemap(lin: vec3<f32>) -> vec3<f32> {
    let c = max(lin, vec3<f32>(0.0));
    let m = max(c.r, max(c.g, c.b));
    if (m <= KNEE) {
        return c;
    }
    let mm = KNEE + (1.0 - KNEE) * (1.0 - exp(-(m - KNEE) / (1.0 - KNEE)));
    return c * (mm / m);
}

fn acescct_enc1(x: f32) -> f32 {
    if (x <= 0.0078125) {
        return 10.540237 * x + 0.072905534;
    }
    return (log2(x) + 9.72) / 17.52;
}

fn acescct_dec1(y: f32) -> f32 {
    if (y <= 0.15525114) {
        return (y - 0.072905534) / 10.540237;
    }
    return exp2(y * 17.52 - 9.72);
}

// Linear Rec.709 → working encoding.
fn to_working(lin: vec3<f32>, ws: u32) -> vec3<f32> {
    if (ws == WS_ACESCCT) {
        let a = m_709_to_ap1() * lin;
        return vec3<f32>(acescct_enc1(a.r), acescct_enc1(a.g), acescct_enc1(a.b));
    }
    return pow(max(lin, vec3<f32>(0.0)), vec3<f32>(1.0 / 2.4));
}

// Working encoding → linear Rec.709.
fn from_working(c: vec3<f32>, ws: u32) -> vec3<f32> {
    if (ws == WS_ACESCCT) {
        return m_ap1_to_709() * vec3<f32>(acescct_dec1(c.r), acescct_dec1(c.g), acescct_dec1(c.b));
    }
    return pow(max(c, vec3<f32>(0.0)), vec3<f32>(2.4));
}

fn working_pivot(ws: u32) -> f32 {
    if (ws == WS_ACESCCT) {
        return 0.41358840;
    }
    return 0.5;
}

fn display_encode(lin: vec3<f32>) -> vec3<f32> {
    return pow(max(lin, vec3<f32>(0.0)), vec3<f32>(1.0 / 2.4));
}

fn display_decode(c: vec3<f32>) -> vec3<f32> {
    return pow(max(c, vec3<f32>(0.0)), vec3<f32>(2.4));
}

// Lift / gamma / gain wheels (rgb + master), as the primary has always done.
fn wheels(c_in: vec3<f32>, lift_w: vec4<f32>, gamma_w: vec4<f32>, gain_w: vec4<f32>) -> vec3<f32> {
    let lift = (lift_w.rgb + vec3<f32>(lift_w.a)) * 0.25;
    let gam = (gamma_w.rgb + vec3<f32>(gamma_w.a)) * 0.5;
    let gain = (gain_w.rgb + vec3<f32>(gain_w.a)) * 0.5;
    var c = c_in * (vec3<f32>(1.0) + gain);
    c = c + lift * (vec3<f32>(1.0) - c);
    return pow(max(c, vec3<f32>(0.0)), vec3<f32>(1.0) / max(vec3<f32>(1.0) + gam, vec3<f32>(0.05)));
}

fn white_balance(lin: vec3<f32>, temperature: f32, tint: f32) -> vec3<f32> {
    return lin * vec3<f32>(1.0 + 0.2 * temperature, 1.0 - 0.2 * tint, 1.0 - 0.2 * temperature);
}
