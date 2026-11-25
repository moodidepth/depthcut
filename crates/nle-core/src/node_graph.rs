//! Node compositor: a clip's colour and compositing as a graph, after the
//! primary grade (in the spirit of Resolve's node tree and a small Fusion).
//!
//! A clip without a graph runs its correction nodes in series, as before.
//! With one, the graph decides the order and the wiring:
//!
//! * **Source**: the clip's picture after the primary grade.
//! * **Corrector**: one of [`ColorParams::corrections`](crate::clip::ColorParams)
//!   by id, so the existing node editor, keyframes, qualifiers and masks
//!   keep working on it.
//! * **Effect**: any built-in effect (keyframable parameters; no mask, no
//!   motion blur: there is no motion field inside the graph).
//! * **Media In**: another asset's picture at the same clip-local time (plus
//!   an offset), fitted into the frame. Not graded.
//! * **Solid**: a flat colour (alpha included).
//! * **Merge**: foreground over background with position, scale, rotation,
//!   opacity and a blend mode (the timeline compositor's maths).
//! * **Parallel mixer**: each input's change from the Source, summed.
//! * **Layer mixer**: inputs stacked bottom (port 0) to top with a blend mode.
//! * **Output**: what the rest of the clip pipeline (effects, transform) sees.
//!
//! Correctors that no graph node refers to run in series right before the
//! Output, so a node added from the list is never silently lost. An
//! unconnected input is transparent black; an unconnected Output passes the
//! Source through.

use crate::clip::BlendMode;
use crate::color::{new_node_id, ColorCorrection};
use crate::effects::{Effect, EffectKind, ResolvedEffect};
use crate::ids::AssetId;
use crate::keyframe::Param;
use crate::time::Time;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Mixer inputs are capped (each is one GPU pass).
pub const MAX_MIXER_INPUTS: usize = 8;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MergeParams {
    pub blend: BlendMode,
    pub opacity: Param,
    /// Offset of the foreground's centre in frame widths / heights.
    pub position_x: Param,
    pub position_y: Param,
    pub scale: Param,
    /// Degrees, clockwise.
    pub rotation: Param,
}

impl Default for MergeParams {
    fn default() -> Self {
        MergeParams {
            blend: BlendMode::Normal,
            opacity: Param::new(1.0),
            position_x: Param::new(0.0),
            position_y: Param::new(0.0),
            scale: Param::new(1.0),
            rotation: Param::new(0.0),
        }
    }
}

impl MergeParams {
    pub fn params_mut(&mut self) -> [(&'static str, &mut Param); 5] {
        [("Opacity", &mut self.opacity), ("Position X", &mut self.position_x), ("Position Y", &mut self.position_y), ("Scale", &mut self.scale), ("Rotation", &mut self.rotation)]
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum NodeKind {
    Source,
    Output,
    Corrector { node: u64 },
    Effect { effect: Effect },
    MediaIn { asset: AssetId, offset: Time },
    Solid { color: [f32; 4] },
    Merge(MergeParams),
    Parallel,
    Layer { blend: BlendMode },
}

impl NodeKind {
    pub fn label(&self) -> &'static str {
        match self {
            NodeKind::Source => "Source",
            NodeKind::Output => "Output",
            NodeKind::Corrector { .. } => "Corrector",
            NodeKind::Effect { effect } => effect.kind.label(),
            NodeKind::MediaIn { .. } => "Media In",
            NodeKind::Solid { .. } => "Solid",
            NodeKind::Merge(_) => "Merge",
            NodeKind::Parallel => "Parallel Mixer",
            NodeKind::Layer { .. } => "Layer Mixer",
        }
    }

    fn is_mixer(&self) -> bool {
        matches!(self, NodeKind::Parallel | NodeKind::Layer { .. })
    }

    /// Fixed input count (mixers: see [`NodeGraph::input_count`]).
    fn fixed_inputs(&self) -> usize {
        match self {
            NodeKind::Source | NodeKind::MediaIn { .. } | NodeKind::Solid { .. } => 0,
            NodeKind::Output | NodeKind::Corrector { .. } | NodeKind::Effect { .. } => 1,
            NodeKind::Merge(_) => 2,
            NodeKind::Parallel | NodeKind::Layer { .. } => MAX_MIXER_INPUTS,
        }
    }

    /// Name of input `port`.
    pub fn port_name(&self, port: usize) -> String {
        match self {
            NodeKind::Merge(_) => if port == 0 { "Background" } else { "Foreground" }.into(),
            NodeKind::Parallel | NodeKind::Layer { .. } => format!("In {}", port + 1),
            _ => "In".into(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct GraphNode {
    pub id: u64,
    pub kind: NodeKind,
    /// Position in the node editor (points).
    #[serde(default)]
    pub pos: [f32; 2],
    #[serde(default = "crate::clip::default_true")]
    pub enabled: bool,
}

/// `from`'s output feeds input `port` of `to`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Link {
    pub from: u64,
    pub to: u64,
    pub port: u8,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct NodeGraph {
    pub nodes: Vec<GraphNode>,
    pub links: Vec<Link>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GraphError {
    /// The link would feed a node's output back into itself.
    Cycle,
    /// No such node or input.
    BadPort,
}

impl GraphError {
    pub fn message(self) -> &'static str {
        match self {
            GraphError::Cycle => "That link would make a loop",
            GraphError::BadPort => "That input does not exist",
        }
    }
}

impl NodeGraph {
    /// Source → each correction → Output: what a clip without a graph does.
    pub fn from_stack(corrections: &[ColorCorrection]) -> NodeGraph {
        let mut g = NodeGraph { nodes: Vec::new(), links: Vec::new() };
        let src = g.add(NodeKind::Source, [0.0, 0.0]);
        let mut prev = src;
        for (i, c) in corrections.iter().enumerate() {
            let n = g.add(NodeKind::Corrector { node: c.id }, [170.0 * (i + 1) as f32, 0.0]);
            g.links.push(Link { from: prev, to: n, port: 0 });
            prev = n;
        }
        let out = g.add(NodeKind::Output, [170.0 * (corrections.len() + 1) as f32, 0.0]);
        g.links.push(Link { from: prev, to: out, port: 0 });
        g
    }

    pub fn add(&mut self, kind: NodeKind, pos: [f32; 2]) -> u64 {
        let id = new_node_id();
        self.nodes.push(GraphNode { id, kind, pos, enabled: true });
        id
    }

    pub fn node(&self, id: u64) -> Option<&GraphNode> {
        self.nodes.iter().find(|n| n.id == id)
    }

    pub fn node_mut(&mut self, id: u64) -> Option<&mut GraphNode> {
        self.nodes.iter_mut().find(|n| n.id == id)
    }

    pub fn source_id(&self) -> Option<u64> {
        self.nodes.iter().find(|n| n.kind == NodeKind::Source).map(|n| n.id)
    }

    pub fn output_id(&self) -> Option<u64> {
        self.nodes.iter().find(|n| n.kind == NodeKind::Output).map(|n| n.id)
    }

    /// Inputs shown on `id`: mixers get one free slot after the last used
    /// one (up to [`MAX_MIXER_INPUTS`]).
    pub fn input_count(&self, id: u64) -> usize {
        let Some(n) = self.node(id) else { return 0 };
        if n.kind.is_mixer() {
            let used = self.links.iter().filter(|l| l.to == id).map(|l| l.port as usize + 1).max().unwrap_or(0);
            (used + 1).clamp(2, MAX_MIXER_INPUTS)
        } else {
            n.kind.fixed_inputs()
        }
    }

    /// What feeds input `port` of `to`.
    pub fn input(&self, to: u64, port: usize) -> Option<u64> {
        self.links.iter().find(|l| l.to == to && l.port as usize == port).map(|l| l.from)
    }

    /// Whether `target` is upstream of (or is) `node`.
    fn feeds(&self, target: u64, node: u64) -> bool {
        let mut stack = vec![node];
        let mut seen = Vec::new();
        while let Some(n) = stack.pop() {
            if n == target {
                return true;
            }
            if seen.contains(&n) {
                continue;
            }
            seen.push(n);
            stack.extend(self.links.iter().filter(|l| l.to == n).map(|l| l.from));
        }
        false
    }

    /// Wire `from` into input `port` of `to`, replacing what was there.
    pub fn connect(&mut self, from: u64, to: u64, port: usize) -> Result<(), GraphError> {
        let (Some(_), Some(t)) = (self.node(from), self.node(to)) else { return Err(GraphError::BadPort) };
        if port >= t.kind.fixed_inputs() || self.node(from).is_some_and(|n| n.kind == NodeKind::Output) {
            return Err(GraphError::BadPort);
        }
        if self.feeds(to, from) {
            return Err(GraphError::Cycle);
        }
        self.disconnect(to, port);
        self.links.push(Link { from, to, port: port as u8 });
        Ok(())
    }

    pub fn disconnect(&mut self, to: u64, port: usize) {
        self.links.retain(|l| !(l.to == to && l.port as usize == port));
    }

    /// Remove a node (never the Source or the Output) and its links. A
    /// single-input node is bridged: what fed it feeds what it fed.
    /// Returns the corrector id it referred to, for the caller to drop.
    pub fn remove(&mut self, id: u64) -> Option<NodeKind> {
        let i = self.nodes.iter().position(|n| n.id == id)?;
        if matches!(self.nodes[i].kind, NodeKind::Source | NodeKind::Output) {
            return None;
        }
        let upstream = (self.nodes[i].kind.fixed_inputs() == 1).then(|| self.input(id, 0)).flatten();
        let outs: Vec<Link> = self.links.iter().filter(|l| l.from == id).copied().collect();
        self.links.retain(|l| l.from != id && l.to != id);
        if let Some(u) = upstream {
            for l in outs {
                self.links.push(Link { from: u, ..l });
            }
        }
        Some(self.nodes.remove(i).kind)
    }

    /// Whether the graph holds nothing but correctors (it then changes the
    /// picture only through them).
    pub fn only_correctors(&self) -> bool {
        self.nodes.iter().all(|n| matches!(n.kind, NodeKind::Source | NodeKind::Output | NodeKind::Corrector { .. }))
    }

    /// Correctors the graph refers to.
    pub fn corrector_ids(&self) -> Vec<u64> {
        self.nodes
            .iter()
            .filter_map(|n| match n.kind {
                NodeKind::Corrector { node } => Some(node),
                _ => None,
            })
            .collect()
    }

    /// Point corrector references at new ids (after a grade copy).
    pub fn remap_correctors(&mut self, map: &HashMap<u64, u64>) {
        for n in &mut self.nodes {
            if let NodeKind::Corrector { node } = &mut n.kind {
                if let Some(new) = map.get(node) {
                    *node = *new;
                }
            }
        }
    }

    /// Every keyframable parameter in the graph (effect and merge nodes).
    pub fn for_each_param_mut(&mut self, mut f: impl FnMut(&mut Param)) {
        for n in &mut self.nodes {
            match &mut n.kind {
                NodeKind::Effect { effect } => effect.params.iter_mut().for_each(&mut f),
                NodeKind::Merge(m) => m.params_mut().into_iter().for_each(|(_, p)| f(p)),
                _ => {}
            }
        }
    }

    /// Media the graph pulls in.
    pub fn media(&self) -> Vec<AssetId> {
        self.nodes
            .iter()
            .filter_map(|n| match n.kind {
                NodeKind::MediaIn { asset, .. } if n.enabled => Some(asset),
                _ => None,
            })
            .collect()
    }

    /// The render steps at clip-local time `t`. `corrections` are the
    /// clip's enabled correction nodes already resolved at `t`.
    pub fn resolve(&self, corrections: &[ColorCorrection], t: Time) -> ResolvedGraph {
        let mut r = Resolver { g: self, corrections, t, steps: vec![Step { op: Op::Source, inputs: Vec::new() }], memo: HashMap::new(), visiting: Vec::new() };
        let mut out = match self.output_id().and_then(|o| self.input(o, 0)) {
            Some(from) => r.visit(from),
            None => Some(0),
        };
        // correctors the graph does not place run last, in list order
        let placed = self.corrector_ids();
        for c in corrections.iter().filter(|c| !placed.contains(&c.id)) {
            if let Some(o) = out {
                out = Some(r.push(Op::Correct(c.clone()), vec![Some(o)]));
            }
        }
        ResolvedGraph { steps: r.steps, output: out }
    }
}

struct Resolver<'a> {
    g: &'a NodeGraph,
    corrections: &'a [ColorCorrection],
    t: Time,
    steps: Vec<Step>,
    memo: HashMap<u64, Option<usize>>,
    visiting: Vec<u64>,
}

impl Resolver<'_> {
    fn push(&mut self, op: Op, inputs: Vec<Option<usize>>) -> usize {
        self.steps.push(Step { op, inputs });
        self.steps.len() - 1
    }

    /// Step index of node `id`'s output (`None`: transparent).
    fn visit(&mut self, id: u64) -> Option<usize> {
        if let Some(s) = self.memo.get(&id) {
            return *s;
        }
        if self.visiting.contains(&id) {
            return None; // a loop in a hand-edited file: cut it here
        }
        let node = self.g.node(id)?;
        self.visiting.push(id);
        let inputs: Vec<Option<usize>> = (0..self.g.input_count(id)).map(|p| self.g.input(id, p).and_then(|from| self.visit(from))).collect();
        self.visiting.pop();
        let s = self.step(node, inputs);
        self.memo.insert(id, s);
        s
    }

    /// The step for `node` (pass-throughs resolve to their input's step).
    fn step(&mut self, node: &GraphNode, inputs: Vec<Option<usize>>) -> Option<usize> {
        let first = inputs.first().copied().flatten();
        if !node.enabled {
            return first; // bypassed
        }
        let t = self.t;
        let op = match &node.kind {
            NodeKind::Source => return Some(0),
            NodeKind::Output => return first,
            NodeKind::Corrector { node } => match self.corrections.iter().find(|c| c.id == *node) {
                Some(c) if first.is_some() => Op::Correct(c.clone()),
                _ => return first, // disabled, at zero mix, deleted, or nothing in
            },
            NodeKind::Effect { effect } => {
                let mut e = effect.resolve(t);
                e.mask = None;
                if first.is_none() || !effect.enabled || e.is_noop() || e.kind == EffectKind::MotionBlur {
                    return first;
                }
                Op::Effect(e)
            }
            NodeKind::MediaIn { asset, offset } => Op::Media { asset: *asset, source_time: (t + *offset).max(Time::ZERO) },
            NodeKind::Solid { color } => Op::Solid(*color),
            NodeKind::Merge(m) => {
                let opacity = m.opacity.at_f32(t).clamp(0.0, 1.0);
                if inputs.get(1).copied().flatten().is_none() || opacity <= 0.0 {
                    return first;
                }
                Op::Merge(ResolvedMerge {
                    blend: m.blend,
                    opacity,
                    position: [m.position_x.at_f32(t), m.position_y.at_f32(t)],
                    scale: m.scale.at_f32(t),
                    rotation: m.rotation.at_f32(t),
                })
            }
            NodeKind::Parallel => {
                let ins: Vec<Option<usize>> = inputs.into_iter().filter(Option::is_some).collect();
                return match ins.len() {
                    0 => None,
                    1 => ins[0],
                    // the Source is the base every change is measured from
                    _ => Some(self.push(Op::Parallel, std::iter::once(Some(0)).chain(ins).collect())),
                };
            }
            NodeKind::Layer { blend } => {
                let ins: Vec<Option<usize>> = inputs.into_iter().filter(Option::is_some).collect();
                return match ins.len() {
                    0 => None,
                    1 => ins[0],
                    _ => Some(self.push(Op::Layer(*blend), ins)),
                };
            }
        };
        Some(self.push(op, inputs))
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct ResolvedMerge {
    pub blend: BlendMode,
    pub opacity: f32,
    pub position: [f32; 2],
    pub scale: f32,
    pub rotation: f32,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Op {
    /// The graph input (always step 0).
    Source,
    Correct(ColorCorrection),
    Effect(ResolvedEffect),
    Media {
        asset: AssetId,
        source_time: Time,
    },
    Solid([f32; 4]),
    /// Input 0 background (`None`: transparent), input 1 foreground.
    Merge(ResolvedMerge),
    /// Input 0 is the Source (the base); out = base + Σ (input − base).
    Parallel,
    /// Inputs stacked bottom to top.
    Layer(BlendMode),
}

#[derive(Clone, Debug, PartialEq)]
pub struct Step {
    pub op: Op,
    /// Step indices (always earlier steps); `None` = transparent.
    pub inputs: Vec<Option<usize>>,
}

/// A graph flattened for one frame: steps in dependency order.
#[derive(Clone, Debug, PartialEq)]
pub struct ResolvedGraph {
    pub steps: Vec<Step>,
    /// The step that is the result (`None`: transparent).
    pub output: Option<usize>,
}

impl ResolvedGraph {
    /// Media `(asset, source_time)` the steps read.
    pub fn media(&self) -> impl Iterator<Item = (AssetId, Time)> + '_ {
        self.steps.iter().filter_map(|s| match s.op {
            Op::Media { asset, source_time } => Some((asset, source_time)),
            _ => None,
        })
    }

    /// Whether the graph leaves the picture as it is.
    pub fn is_identity(&self) -> bool {
        self.output == Some(0)
    }

    /// For each step, the last step reading it (`usize::MAX` for the
    /// result): the renderer releases targets once they are no longer read.
    pub fn last_use(&self) -> Vec<usize> {
        let mut last: Vec<usize> = (0..self.steps.len()).collect();
        for (i, s) in self.steps.iter().enumerate() {
            for j in s.inputs.iter().flatten() {
                last[*j] = last[*j].max(i);
            }
        }
        if let Some(o) = self.output {
            last[o] = usize::MAX;
        }
        last
    }
}

/// Effects that make sense inside the graph (not the AI mattes, which read
/// per-clip matte files, nor motion blur, which needs a motion field).
pub fn graph_effects() -> Vec<EffectKind> {
    EffectKind::ALL.iter().copied().filter(|k| !matches!(k, EffectKind::MotionBlur | EffectKind::PersonMatte | EffectKind::DepthMap | EffectKind::GamingReframe916)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cc(name: &str, exposure: f32) -> ColorCorrection {
        ColorCorrection { exposure, ..ColorCorrection::named(name) }
    }

    #[test]
    fn stack_graph_matches_the_serial_chain() {
        let cs = vec![cc("a", 0.5), cc("b", -0.2)];
        let g = NodeGraph::from_stack(&cs);
        let r = g.resolve(&cs, Time::ZERO);
        assert_eq!(r.steps.len(), 3);
        assert!(matches!(&r.steps[0].op, Op::Source));
        assert!(matches!(&r.steps[1].op, Op::Correct(c) if c.name == "a"));
        assert_eq!(r.steps[1].inputs, vec![Some(0)]);
        assert!(matches!(&r.steps[2].op, Op::Correct(c) if c.name == "b"));
        assert_eq!(r.steps[2].inputs, vec![Some(1)]);
        assert_eq!(r.output, Some(2));
        assert!(!r.is_identity());
        assert!(NodeGraph::from_stack(&[]).resolve(&[], Time::ZERO).is_identity());
        // a graph with no Output link passes the Source through
        let mut g = NodeGraph::from_stack(&[]);
        let out = g.output_id().unwrap();
        g.disconnect(out, 0);
        assert!(g.resolve(&[], Time::ZERO).is_identity());
    }

    #[test]
    fn connect_rejects_loops_and_bad_ports() {
        let cs = vec![cc("a", 0.5), cc("b", -0.2)];
        let mut g = NodeGraph::from_stack(&cs);
        let a = g.nodes[1].id;
        let b = g.nodes[2].id;
        assert_eq!(g.connect(b, a, 0), Err(GraphError::Cycle));
        assert_eq!(g.connect(a, a, 0), Err(GraphError::Cycle));
        assert_eq!(g.connect(a, b, 1), Err(GraphError::BadPort));
        let out = g.output_id().unwrap();
        assert_eq!(g.connect(out, b, 0), Err(GraphError::BadPort));
        let src = g.source_id().unwrap();
        assert_eq!(g.connect(a, src, 0), Err(GraphError::BadPort));
        // rewiring replaces the old link
        g.connect(src, b, 0).unwrap();
        assert_eq!(g.input(b, 0), Some(src));
        assert_eq!(g.links.iter().filter(|l| l.to == b).count(), 1);
    }

    #[test]
    fn parallel_merge_and_unplaced_correctors() {
        let cs = vec![cc("a", 0.5), cc("b", -0.2), cc("late", 0.1)];
        let mut g = NodeGraph { nodes: Vec::new(), links: Vec::new() };
        let src = g.add(NodeKind::Source, [0.0; 2]);
        let a = g.add(NodeKind::Corrector { node: cs[0].id }, [0.0; 2]);
        let b = g.add(NodeKind::Corrector { node: cs[1].id }, [0.0; 2]);
        let par = g.add(NodeKind::Parallel, [0.0; 2]);
        let media = g.add(NodeKind::MediaIn { asset: AssetId(7), offset: Time::from_secs(1) }, [0.0; 2]);
        let merge = g.add(NodeKind::Merge(MergeParams::default()), [0.0; 2]);
        let out = g.add(NodeKind::Output, [0.0; 2]);
        g.connect(src, a, 0).unwrap();
        g.connect(src, b, 0).unwrap();
        g.connect(a, par, 0).unwrap();
        assert_eq!(g.input_count(par), 2);
        g.connect(b, par, 1).unwrap();
        assert_eq!(g.input_count(par), 3, "a free slot after the last used one");
        g.connect(par, merge, 0).unwrap();
        g.connect(media, merge, 1).unwrap();
        g.connect(merge, out, 0).unwrap();
        let r = g.resolve(&cs, Time::from_secs(2));
        assert_eq!(r.media().collect::<Vec<_>>(), vec![(AssetId(7), Time::from_secs(3))]);
        // Source, a, b, parallel (base + a + b), media, merge, then the unplaced corrector
        assert!(matches!(r.steps[3].op, Op::Parallel));
        assert_eq!(r.steps[3].inputs, vec![Some(0), Some(1), Some(2)]);
        assert!(matches!(r.steps[5].op, Op::Merge(_)));
        assert_eq!(r.steps[5].inputs, vec![Some(3), Some(4)]);
        assert!(matches!(&r.steps[6].op, Op::Correct(c) if c.name == "late"));
        assert_eq!(r.steps.len(), 7);
        assert_eq!(r.output, Some(6));
        let last = r.last_use();
        assert_eq!(last[0], 3, "the parallel mixer reads the Source");
        assert_eq!(last[3], 5);
        assert_eq!(last[6], usize::MAX);

        // a disabled or missing corrector passes its input through
        let r = g.resolve(&cs[1..], Time::ZERO);
        assert!(matches!(&r.steps[1].op, Op::Correct(c) if c.name == "b"));
        assert_eq!(r.steps[2].inputs, vec![Some(0), Some(0), Some(1)]);
        // a merge without a foreground is its background
        g.disconnect(merge, 1);
        let r = g.resolve(&cs[..2], Time::ZERO);
        assert_eq!(r.output, Some(3));
        // removing a single-input node bridges it
        g.remove(a);
        assert_eq!(g.input(par, 0), Some(src));
        assert!(g.remove(src).is_none(), "the Source stays");
        // an unconnected chain is transparent
        let lone = g.add(NodeKind::Effect { effect: Effect::new(EffectKind::Vignette) }, [0.0; 2]);
        g.connect(lone, out, 0).unwrap();
        assert_eq!(g.resolve(&cs, Time::ZERO).output, None);
    }

    #[test]
    fn serde_round_trip_and_remap() {
        let cs = vec![cc("a", 0.5)];
        let mut g = NodeGraph::from_stack(&cs);
        let mut e = Effect::new(EffectKind::Vignette);
        e.params[0] = Param::new(0.8);
        g.add(NodeKind::Effect { effect: e }, [10.0, 20.0]);
        g.add(NodeKind::Layer { blend: BlendMode::Screen }, [0.0; 2]);
        let json = serde_json::to_string(&g).unwrap();
        let back: NodeGraph = serde_json::from_str(&json).unwrap();
        assert_eq!(back, g);
        let mut map = HashMap::new();
        map.insert(cs[0].id, 42);
        g.remap_correctors(&map);
        assert_eq!(g.corrector_ids(), vec![42]);
        let mut n = 0;
        g.for_each_param_mut(|_| n += 1);
        assert_eq!(n, EffectKind::Vignette.params().len());
        assert!(graph_effects().contains(&EffectKind::GaussianBlur) && !graph_effects().contains(&EffectKind::MotionBlur));
    }
}
