//! Node compositor on the GPU: a serial graph matches the serial corrector
//! chain, the parallel mixer sums each branch's change, Merge places a
//! foreground with its transform, the Layer mixer blends, and Media In
//! fits another asset into the frame. Skipped when no GPU adapter exists.

use nle_core::clip::BlendMode;
use nle_core::color::{ColorCorrection, WorkingSpace};
use nle_core::eval::{Composition, Layer, LayerSource, RenderNode, ResolvedColor};
use nle_core::node_graph::{MergeParams, NodeGraph, NodeKind};
use nle_core::{AssetId, ClipId, FrameRate, Param, Time};
use nle_media::{PixelFormat, VideoFrame};
use nle_render::colormath::{self, Rgb};
use nle_render::{FrameSource, GpuContext, RenderOptions, Renderer};

const W: u32 = 64;
const H: u32 = 32;
const TOL: f32 = 3.0 / 255.0;
/// Media In test asset: a 16×16 square of this colour.
const MEDIA_RGB: [u8; 3] = [40, 200, 90];

struct Square;

impl FrameSource for Square {
    fn frame_rate(&self, _: AssetId) -> FrameRate {
        FrameRate::FPS_30
    }
    fn frame(&mut self, _: AssetId, index: i64) -> Option<VideoFrame> {
        let px = [MEDIA_RGB[0], MEDIA_RGB[1], MEDIA_RGB[2], 255];
        let data = px.repeat(16 * 16);
        Some(VideoFrame::new(16, 16, PixelFormat::Rgba8, data, Time::from_frames(index, FrameRate::FPS_30)))
    }
}

fn gpu() -> Option<GpuContext> {
    match GpuContext::new_headless() {
        Ok(g) => Some(g),
        Err(e) => {
            eprintln!("skipping GPU node graph test: {e}");
            None
        }
    }
}

/// A solid layer graded by `nodes` through `graph` (`None`: serial).
fn layer(rgb: Rgb, nodes: Vec<ColorCorrection>, graph: Option<&NodeGraph>) -> Layer {
    let mut l = Layer::new(ClipId(1), 0, LayerSource::Solid([rgb[0], rgb[1], rgb[2], 1.0]));
    l.color = ResolvedColor { enabled: true, graph: graph.map(|g| g.resolve(&nodes, Time::ZERO)), nodes, ..Default::default() };
    l
}

fn render(r: &mut Renderer, l: Layer) -> Vec<u8> {
    let comp = Composition::new(Time::ZERO, vec![RenderNode::Layer(l)]);
    r.render(&comp, &mut Square, RenderOptions::default());
    r.read_display()
}

fn px(img: &[u8], x: u32, y: u32) -> Rgb {
    let i = ((y * W + x) * 4) as usize;
    [img[i], img[i + 1], img[i + 2]].map(|v| v as f32 / 255.0)
}

fn close(gpu: Rgb, cpu: Rgb, tol: f32, what: &str) {
    let cpu = cpu.map(|v| v.clamp(0.0, 1.0));
    let ok = gpu.iter().zip(cpu).all(|(a, b)| (a - b).abs() <= tol);
    assert!(ok, "{what}: GPU {gpu:?} vs expected {cpu:?}");
}

fn graph_with_ends() -> (NodeGraph, u64, u64) {
    let mut g = NodeGraph { nodes: Vec::new(), links: Vec::new() };
    let src = g.add(NodeKind::Source, [0.0; 2]);
    let out = g.add(NodeKind::Output, [0.0; 2]);
    (g, src, out)
}

#[test]
fn gpu_serial_graph_matches_the_serial_chain_and_parallel_sums_changes() {
    let Some(gpu) = gpu() else { return };
    let mut r = Renderer::new(gpu, W, H);
    let rgb = [0.6, 0.4, 0.3];
    let a = ColorCorrection { exposure: 0.4, ..ColorCorrection::named("a") };
    let b = ColorCorrection { saturation: 0.3, ..ColorCorrection::named("b") };
    let nodes = vec![a.clone(), b.clone()];

    let serial = render(&mut r, layer(rgb, nodes.clone(), None));
    let graph = render(&mut r, layer(rgb, nodes.clone(), Some(&NodeGraph::from_stack(&nodes))));
    close(px(&graph, W / 2, H / 2), px(&serial, W / 2, H / 2), 1.0 / 255.0, "serial graph vs serial chain");

    // a and b side by side from the Source
    let (mut g, src, out) = graph_with_ends();
    let na = g.add(NodeKind::Corrector { node: a.id }, [0.0; 2]);
    let nb = g.add(NodeKind::Corrector { node: b.id }, [0.0; 2]);
    let par = g.add(NodeKind::Parallel, [0.0; 2]);
    g.connect(src, na, 0).unwrap();
    g.connect(src, nb, 0).unwrap();
    g.connect(na, par, 0).unwrap();
    g.connect(nb, par, 1).unwrap();
    g.connect(par, out, 0).unwrap();
    let img = render(&mut r, layer(rgb, nodes, Some(&g)));
    let wa = colormath::correct_node(rgb, &a, WorkingSpace::Rec709Gamma);
    let wb = colormath::correct_node(rgb, &b, WorkingSpace::Rec709Gamma);
    let want = [0, 1, 2].map(|i| rgb[i] + (wa[i] - rgb[i]) + (wb[i] - rgb[i]));
    close(px(&img, W / 2, H / 2), want, TOL, "parallel mixer");
}

#[test]
fn gpu_merge_places_the_foreground_and_layer_mixer_blends() {
    let Some(gpu) = gpu() else { return };
    let mut r = Renderer::new(gpu, W, H);
    let rgb = [0.6, 0.4, 0.3];

    // a red solid, half size, pushed into the right half
    let (mut g, src, out) = graph_with_ends();
    let red = g.add(NodeKind::Solid { color: [1.0, 0.0, 0.0, 1.0] }, [0.0; 2]);
    let m = MergeParams { scale: Param::new(0.5), position_x: Param::new(0.25), ..MergeParams::default() };
    let merge = g.add(NodeKind::Merge(m), [0.0; 2]);
    g.connect(src, merge, 0).unwrap();
    g.connect(red, merge, 1).unwrap();
    g.connect(merge, out, 0).unwrap();
    let img = render(&mut r, layer(rgb, Vec::new(), Some(&g)));
    close(px(&img, W * 3 / 4, H / 2), [1.0, 0.0, 0.0], TOL, "foreground centre");
    close(px(&img, W / 4, H / 2), rgb, TOL, "left half is the background");
    close(px(&img, W * 3 / 4, 2), rgb, TOL, "above the half-size foreground");

    // Source multiplied by mid grey
    let (mut g, src, out) = graph_with_ends();
    let grey = g.add(NodeKind::Solid { color: [0.5, 0.5, 0.5, 1.0] }, [0.0; 2]);
    let mix = g.add(NodeKind::Layer { blend: BlendMode::Multiply }, [0.0; 2]);
    g.connect(src, mix, 0).unwrap();
    g.connect(grey, mix, 1).unwrap();
    g.connect(mix, out, 0).unwrap();
    let img = render(&mut r, layer(rgb, Vec::new(), Some(&g)));
    close(px(&img, W / 2, H / 2), rgb.map(|v| v * 0.5), TOL, "layer mixer multiply");

    // an unconnected Output passes the Source; a transparent chain shows nothing
    let (g, _, _) = graph_with_ends();
    let img = render(&mut r, layer(rgb, Vec::new(), Some(&g)));
    close(px(&img, W / 2, H / 2), rgb, TOL, "unconnected output");
}

#[test]
fn gpu_media_in_is_fitted_into_the_frame() {
    let Some(gpu) = gpu() else { return };
    let mut r = Renderer::new(gpu, W, H);
    let rgb = [0.6, 0.4, 0.3];
    let (mut g, src, out) = graph_with_ends();
    let media = g.add(NodeKind::MediaIn { asset: AssetId(9), offset: Time::ZERO }, [0.0; 2]);
    let merge = g.add(NodeKind::Merge(MergeParams::default()), [0.0; 2]);
    g.connect(src, merge, 0).unwrap();
    g.connect(media, merge, 1).unwrap();
    g.connect(merge, out, 0).unwrap();
    let l = layer(rgb, Vec::new(), Some(&g));
    let comp = Composition::new(Time::ZERO, vec![RenderNode::Layer(l)]);
    assert_eq!(comp.media_requests(), vec![(AssetId(9), Time::ZERO)], "the graph's media is requested");
    let img = render(&mut r, comp.layers()[0].clone());
    // the 16×16 square fills the centre 32×32 of the 64×32 frame
    close(px(&img, W / 2, H / 2), MEDIA_RGB.map(|v| v as f32 / 255.0), 4.0 / 255.0, "media centre");
    close(px(&img, 4, H / 2), rgb, TOL, "pillarbox shows the background");
}
