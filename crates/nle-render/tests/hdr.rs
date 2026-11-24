//! HDR packing on the GPU: SDR reference white lands at 203 nits in PQ
//! (code ≈ 0.5807) and at 75 % in HLG, in BT.2020 with the BT.2020 luma
//! matrix; an over-white HDR source keeps its highlights above reference
//! white instead of being tone-mapped to it. Skipped without a GPU adapter.

use nle_core::color::{ColorSpace, WorkingSpace};
use nle_core::eval::{Composition, Layer, LayerSource, RenderNode, ResolvedColor};
use nle_core::{AssetId, ClipId, FrameRate, Time};
use nle_media::VideoFrame;
use nle_render::colormath::{hlg_encode, pq_encode};
use nle_render::{FrameSource, GpuContext, HdrOutput, PackFormat, RenderOptions, Renderer};

const W: u32 = 16;
const H: u32 = 8;

struct NoFrames;

impl FrameSource for NoFrames {
    fn frame_rate(&self, _: AssetId) -> FrameRate {
        FrameRate::FPS_30
    }
    fn frame(&mut self, _: AssetId, _: i64) -> Option<VideoFrame> {
        None
    }
}

fn solid(rgb: [f32; 3], color: ResolvedColor) -> Composition {
    let mut l = Layer::new(ClipId(1), 0, LayerSource::Solid([rgb[0], rgb[1], rgb[2], 1.0]));
    l.color = color;
    Composition::new(Time::ZERO, vec![RenderNode::Layer(l)])
}

/// First luma and first (Cb, Cr) of a P010 frame, as 10-bit codes.
fn p010_codes(frame: &[u8]) -> (u16, u16, u16) {
    let word = |i: usize| u16::from_le_bytes([frame[i * 2], frame[i * 2 + 1]]) >> 6;
    let uv = (W * H) as usize;
    (word(0), word(uv), word(uv + 1))
}

fn rgba64_first(frame: &[u8]) -> [f32; 3] {
    [0, 1, 2].map(|c| u16::from_le_bytes([frame[c * 2], frame[c * 2 + 1]]) as f32 / 65535.0)
}

#[test]
fn reference_white_lands_at_the_hdr_reference_levels() {
    let gpu = match GpuContext::new_headless() {
        Ok(g) => g,
        Err(e) => {
            eprintln!("skipping HDR test: {e}");
            return;
        }
    };
    let mut r = Renderer::new(gpu, W, H);
    let white = solid([1.0, 1.0, 1.0], ResolvedColor::default());
    let pack = |r: &mut Renderer, comp: &Composition, hdr, fmt| r.render_packed(comp, &mut NoFrames, RenderOptions { hdr, ..Default::default() }, fmt);

    let sdr = p010_codes(&pack(&mut r, &white, HdrOutput::Sdr, PackFormat::P010));
    assert_eq!(sdr, (940, 512, 512), "SDR white");
    let pq = p010_codes(&pack(&mut r, &white, HdrOutput::Pq { peak_nits: 1000.0 }, PackFormat::P010));
    let want = (64.0 + 876.0 * pq_encode(203.0 / 10_000.0)).round() as u16;
    assert!(pq.0.abs_diff(want) <= 1 && pq.0.abs_diff(573) <= 1, "PQ white {pq:?}, want {want}");
    assert!(pq.1.abs_diff(512) <= 1 && pq.2.abs_diff(512) <= 1, "white stays neutral {pq:?}");
    let hlg = p010_codes(&pack(&mut r, &white, HdrOutput::Hlg, PackFormat::P010));
    assert!(hlg.0.abs_diff(721) <= 1, "HLG white {hlg:?}");

    // pure red: the BT.2020 matrix and primaries (709 red is inside 2020, so
    // it is less saturated there: R' < 1, G', B' > 0)
    let red = p010_codes(&pack(&mut r, &solid([1.0, 0.0, 0.0], ResolvedColor::default()), HdrOutput::Pq { peak_nits: 1000.0 }, PackFormat::P010));
    let lin = [0.6274f32, 0.0691, 0.0164].map(|v| pq_encode(v * 203.0 / 10_000.0));
    let y = 0.2627 * lin[0] + 0.6780 * lin[1] + 0.0593 * lin[2];
    let want_y = (64.0 + 876.0 * y).round() as u16;
    let want_cr = (512.0 + 896.0 * (lin[0] - y) / 1.4746).round() as u16;
    assert!(red.0.abs_diff(want_y) <= 2 && red.2.abs_diff(want_cr) <= 2, "PQ red {red:?} vs Y {want_y} Cr {want_cr}");

    // RGBA64 (ProRes / DNxHR) carries the same signal
    let m = rgba64_first(&pack(&mut r, &white, HdrOutput::Hlg, PackFormat::Rgba64 { alpha: false, linear: false }));
    assert!(m.iter().all(|v| (v - hlg_encode(0.264_962_56)).abs() < 2e-3), "HLG RGBA64 {m:?}");

    // a PQ source at 1000 nits: SDR export rolls it off to white, HDR keeps it bright
    let bright = ResolvedColor { enabled: true, input_space: ColorSpace::Rec2100Pq, working: WorkingSpace::Rec709Gamma, ..Default::default() };
    let src = solid([0.75, 0.75, 0.75], bright);
    let sdr = rgba64_first(&pack(&mut r, &src, HdrOutput::Sdr, PackFormat::Rgba64 { alpha: false, linear: false }));
    assert!(sdr[0] <= 1.0 && sdr[0] > 0.95, "SDR roll-off {sdr:?}");
    let hdr = rgba64_first(&pack(&mut r, &src, HdrOutput::Pq { peak_nits: 1000.0 }, PackFormat::Rgba64 { alpha: false, linear: false }));
    assert!(hdr[0] > pq_encode(203.0 / 10_000.0) + 0.05, "HDR keeps the highlight above reference white: {hdr:?}");
}
