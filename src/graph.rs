//! A plain-data mirror of the PipeWire graph, plus the pure logic that decides which
//! nodes and links *should* exist for a given config. Kept free of PipeWire calls so it
//! can be unit-tested.

use crate::config::{Bus, Config, Kind, Layout, Upmix, db_to_linear};
use std::collections::{HashMap, HashSet};

pub const STRIP_PREFIX: &str = "lam_strip_";
pub const BUS_PREFIX: &str = "lam_bus_";
pub const OUT_PREFIX: &str = "lam_out_";
/// Left over from an earlier surround approach; only kept so old nodes get cleaned up.
pub const SUM_PREFIX: &str = "lam_sum_";
/// Input side of a surround upmixer (PipeWire loopback owned by the engine). Its output
/// side is linked to the speakers by WirePlumber, which is what makes PipeWire upmix.
pub const UPIN_PREFIX: &str = "lam_upin_";
/// A channel's effects node (owned by the engine, between the channel and its outputs).
pub const FX_PREFIX: &str = "lam_fx_";
/// A recorder (pw-record, started by the engine) taking what an output plays.
pub const REC_PREFIX: &str = "lam_rec_";
pub fn rec_node(bus_id: &str) -> String {
    format!("{REC_PREFIX}{bus_id}")
}
/// Effects-node inputs that listen to ducking sources (not part of the audio path).
pub const SIDECHAIN_PREFIX: &str = "sidechain_";
/// Mono helpers folding centre/rear/side channels into left or right for outputs that
/// don't have those speakers (headphones, the virtual mic, stereo speakers).
pub const FOLD_PREFIX: &str = "lam_fold_";

/// Channel layouts the mixer uses internally.
pub const STEREO: &str = "FL,FR";
pub const SURROUND: &str = "FL,FR,FC,LFE,RL,RR,SL,SR";
/// Level of a channel folded into left/right (-3 dB, the usual downmix level).
const FOLD_LEVEL: f32 = std::f32::consts::FRAC_1_SQRT_2;
/// Level-meter capture streams owned by the running engine (not lingering, not in config).
pub const METER_PREFIX: &str = "lam_meter_";

pub fn strip_node(id: &str) -> String {
    format!("{STRIP_PREFIX}{id}")
}
pub fn bus_node(id: &str) -> String {
    format!("{BUS_PREFIX}{id}")
}
pub fn out_node(id: &str) -> String {
    format!("{OUT_PREFIX}{id}")
}
pub fn fold_node(bus_id: &str, right: bool) -> String {
    format!("{FOLD_PREFIX}{bus_id}_{}", if right { "r" } else { "l" })
}
pub fn fx_node(strip_id: &str) -> String {
    format!("{FX_PREFIX}{strip_id}")
}
/// Effects id (see `fx_node`) of an output's ducking stage: the channels it turns down
/// are mixed through it before reaching the output. Ids never contain dots.
pub fn bus_duck_id(bus_id: &str) -> String {
    format!("{bus_id}.duck")
}
pub fn upmix_in_node(bus_id: &str) -> String {
    format!("{UPIN_PREFIX}{bus_id}")
}
/// Name of the meter stream that measures the node `node_name`.
pub fn meter_node(node_name: &str) -> String {
    format!("{METER_PREFIX}{}", node_name.trim_start_matches("lam_"))
}
/// Nodes the engine creates from the config (and removes when the config drops them).
pub fn is_ours(name: &str) -> bool {
    [
        STRIP_PREFIX,
        BUS_PREFIX,
        OUT_PREFIX,
        SUM_PREFIX,
        FOLD_PREFIX,
    ]
    .iter()
    .any(|p| name.starts_with(p))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dir {
    In,
    Out,
}

#[derive(Debug, Clone, Default)]
pub struct Node {
    pub id: u32,
    pub name: String,
    pub class: String,
    pub description: String,
    /// A real sound card input/output (not a virtual device or app stream).
    pub hardware: bool,
    /// For app streams: the app's name and program (e.g. "Brave", "brave").
    pub app_name: String,
    pub app_binary: String,
    /// The sound card (PipeWire device id) a hardware node belongs to.
    pub card: Option<u32>,
}

impl Node {
    /// An app playing audio (as opposed to a device or a recording stream).
    pub fn is_app_playback(&self) -> bool {
        self.class == "Stream/Output/Audio"
    }
}

#[derive(Debug, Clone)]
pub struct Port {
    pub id: u32,
    pub node: u32,
    pub dir: Dir,
    pub channel: String,
    pub monitor: bool,
    pub name: String,
}

#[derive(Debug, Clone)]
pub struct Link {
    pub id: u32,
    pub out_port: u32,
    pub in_port: u32,
    pub out_node: u32,
    pub in_node: u32,
}

#[derive(Debug, Default)]
pub struct Graph {
    pub nodes: HashMap<u32, Node>,
    pub ports: HashMap<u32, Port>,
    pub links: HashMap<u32, Link>,
}

impl Graph {
    pub fn remove(&mut self, id: u32) {
        self.nodes.remove(&id);
        self.ports.remove(&id);
        self.links.remove(&id);
    }

    pub fn node_by_name(&self, name: &str) -> Option<&Node> {
        // Lowest id wins if a name is somehow duplicated, so the choice is stable.
        self.nodes
            .values()
            .filter(|n| n.name == name)
            .min_by_key(|n| n.id)
    }

    /// Ports of a node in a direction, sorted by id. `monitor` selects monitor
    /// ports (Some(true)), normal ports (Some(false)) or both (None).
    pub fn ports_of(&self, node: u32, dir: Dir, monitor: Option<bool>) -> Vec<&Port> {
        let mut v: Vec<&Port> = self
            .ports
            .values()
            .filter(|p| p.node == node && p.dir == dir && monitor.is_none_or(|m| p.monitor == m))
            .collect();
        v.sort_by_key(|p| p.id);
        v
    }
}

/// A node the mixer owns and keeps in sync with the config.
#[derive(Debug, Clone, PartialEq)]
pub struct DesiredNode {
    pub name: String,
    pub description: String,
    pub class: &'static str,
    /// Linear gain applied by the node itself.
    pub volume: f32,
    /// Balance (strips only): -1 left … 1 right.
    pub pan: f32,
    pub mute: bool,
    /// Channel layout, e.g. "FL,FR".
    pub positions: &'static str,
}

pub fn desired_nodes(cfg: &Config, g: &Graph) -> Vec<DesiredNode> {
    let mut out = Vec::new();
    for s in &cfg.strips {
        let (class, description, positions) = match s.kind {
            // Shows up as an output device apps can pick.
            Kind::Virtual => (
                "Audio/Sink",
                s.name.clone(),
                if s.layout == Layout::Surround {
                    SURROUND
                } else {
                    STEREO
                },
            ),
            // Internal: hidden from desktop sound settings.
            Kind::Physical { .. } => ("Audio/Filter", format!("Mixer strip: {}", s.name), STEREO),
        };
        out.push(DesiredNode {
            name: strip_node(&s.id),
            description,
            class,
            volume: db_to_linear(s.gain_db),
            pan: s.pan,
            mute: s.mute,
            positions,
        });
    }
    for b in &cfg.buses {
        // Every bus has an internal mixing node; that is where bus gain/mute live. It is
        // always 7.1 so surround from any strip survives until the output decides.
        out.push(DesiredNode {
            name: bus_node(&b.id),
            description: format!("Mixer bus: {}", b.name),
            class: "Audio/Filter",
            volume: db_to_linear(b.gain_db),
            pan: 0.0,
            mute: b.mute,
            positions: SURROUND,
        });
        if target_channels(b, g).is_some_and(|t| needs_fold(&t)) {
            for right in [false, true] {
                out.push(DesiredNode {
                    name: fold_node(&b.id, right),
                    description: format!(
                        "Mixer bus: {} (fold-down {})",
                        b.name,
                        if right { "R" } else { "L" }
                    ),
                    class: "Audio/Filter",
                    volume: FOLD_LEVEL,
                    pan: 0.0,
                    mute: false,
                    positions: "MONO",
                });
            }
        }
        if b.kind == Kind::Virtual {
            // What other apps (Discord, OBS) see as a microphone.
            out.push(DesiredNode {
                name: out_node(&b.id),
                description: b.name.clone(),
                class: "Audio/Source/Virtual",
                volume: 1.0,
                pan: 0.0,
                mute: false,
                positions: STEREO,
            });
        }
    }
    out
}

/// The channels a bus plays into: its device's speakers, or the virtual mic (stereo).
/// None while the device is missing.
fn target_channels(b: &Bus, g: &Graph) -> Option<Vec<String>> {
    match &b.kind {
        Kind::Virtual => Some(vec!["FL".into(), "FR".into()]),
        Kind::Physical { device } => {
            let n = g.node_by_name(device)?;
            let ch: Vec<String> = g
                .ports_of(n.id, Dir::In, None)
                .iter()
                .map(|p| p.channel.clone())
                .collect();
            (!ch.is_empty()).then_some(ch)
        }
    }
}

/// Where one channel of the (7.1) bus goes on an output with `target` channels.
#[derive(Debug, Clone, PartialEq)]
pub enum ChannelRoute {
    /// To the speaker with this channel.
    To(String),
    /// Folded into the left, right or both sides at -3 dB.
    FoldLeft,
    FoldRight,
    FoldBoth,
    /// Mono output: everything into its single channel.
    Mono,
    Drop,
}

pub fn route_channel(ch: &str, target: &[String]) -> ChannelRoute {
    let has = |c: &str| target.iter().any(|t| t == c);
    if target.len() == 1 {
        return if ch == "LFE" {
            ChannelRoute::Drop
        } else {
            ChannelRoute::Mono
        };
    }
    if has(ch) {
        return ChannelRoute::To(ch.to_string());
    }
    // Rear and side speakers stand in for each other (7.1 on 5.1 and vice versa).
    let alt = match ch {
        "RL" => "SL",
        "SL" => "RL",
        "RR" => "SR",
        "SR" => "RR",
        _ => "",
    };
    if !alt.is_empty() && has(alt) {
        return ChannelRoute::To(alt.to_string());
    }
    match ch {
        "FC" => ChannelRoute::FoldBoth,
        "RL" | "SL" => ChannelRoute::FoldLeft,
        "RR" | "SR" => ChannelRoute::FoldRight,
        // Without a subwoofer the bass is in the other channels already.
        _ => ChannelRoute::Drop,
    }
}

fn needs_fold(target: &[String]) -> bool {
    SURROUND.split(',').any(|c| {
        matches!(
            route_channel(c, target),
            ChannelRoute::FoldLeft | ChannelRoute::FoldRight | ChannelRoute::FoldBoth
        )
    })
}

/// Pair up output ports with input ports, matching audio channels where possible.
pub fn pair_ports(outs: &[&Port], ins: &[&Port]) -> Vec<(u32, u32)> {
    if outs.is_empty() || ins.is_empty() {
        return vec![];
    }
    // Mono on either side: fan out / fan in.
    if outs.len() == 1 {
        return ins.iter().map(|i| (outs[0].id, i.id)).collect();
    }
    if ins.len() == 1 {
        return outs.iter().map(|o| (o.id, ins[0].id)).collect();
    }
    let by_channel: Vec<(u32, u32)> = outs
        .iter()
        .filter_map(|o| {
            ins.iter()
                .find(|i| i.channel == o.channel)
                .map(|i| (o.id, i.id))
        })
        .collect();
    if !by_channel.is_empty() {
        return by_channel;
    }
    // Unnamed channels (e.g. pro-audio AUX0, AUX1): connect in order.
    outs.iter().zip(ins).map(|(o, i)| (o.id, i.id)).collect()
}

/// The routing the config asks for, given the devices that currently exist.
/// Returns `(output port, input port)` pairs. Anything whose device is missing
/// (e.g. unplugged headphones) is simply left out and appears once it is back.
pub fn desired_links(cfg: &Config, g: &Graph) -> HashSet<(u32, u32)> {
    let mut links = HashSet::new();
    let mut outputs = Vec::new();
    let mut connect = |from: Option<&Node>, from_monitor: bool, to: Option<&Node>| {
        if let (Some(from), Some(to)) = (from, to) {
            let outs = g.ports_of(from.id, Dir::Out, Some(from_monitor));
            // Never into an effects node's ducking inputs (they share channel names).
            let ins: Vec<&Port> = g
                .ports_of(to.id, Dir::In, None)
                .into_iter()
                .filter(|p| !p.name.starts_with(SIDECHAIN_PREFIX))
                .collect();
            links.extend(pair_ports(&outs, &ins));
        }
    };

    for s in &cfg.strips {
        let strip = g.node_by_name(&strip_node(&s.id));
        if let Kind::Physical { device } = &s.kind {
            connect(g.node_by_name(device), false, strip);
        }
        // With effects on, the channel goes through its effects node first. Until that
        // node exists the channel stays connected directly, so audio never drops out.
        let fx = g.node_by_name(&fx_node(&s.id)).filter(|_| s.fx.active());
        let (source, from_monitor) = match fx {
            Some(fx) => {
                connect(strip, true, Some(fx));
                (Some(fx), false)
            }
            None => (strip, true),
        };
        for b in cfg.sends(s) {
            // Into an output's ducking stage if it turns this channel down (until that
            // exists, straight in, so audio never drops out).
            let ducking = cfg
                .buses
                .iter()
                .find(|o| o.id == b)
                .filter(|o| o.ducks_strip(&s.id))
                .and_then(|_| g.node_by_name(&fx_node(&bus_duck_id(&b))));
            let to = ducking.or_else(|| g.node_by_name(&bus_node(&b)));
            connect(source, from_monitor, to);
        }
        // Ducking: the sources' sound goes into this channel's listening inputs.
        if let Some(fx) = fx.filter(|_| s.fx.duck.enabled) {
            outputs.extend(sidechain_links(cfg, g, fx, &s.fx.duck.sources));
        }
        connect(
            source,
            from_monitor,
            g.node_by_name(&meter_node(&strip_node(&s.id))),
        );
    }
    for b in &cfg.buses {
        let bus = g.node_by_name(&bus_node(&b.id));
        let target = match &b.kind {
            Kind::Physical { device } => g.node_by_name(device),
            Kind::Virtual => g.node_by_name(&out_node(&b.id)),
        };
        // With effects (e.g. the limiter) on, everything the output feeds comes from its
        // effects node; until that node exists, straight from the bus.
        if b.ducks()
            && let Some(duck) = g.node_by_name(&fx_node(&bus_duck_id(&b.id)))
        {
            connect(Some(duck), false, bus);
            outputs.extend(sidechain_links(cfg, g, duck, &b.fx.duck.sources));
        }
        let fx = g
            .node_by_name(&fx_node(&b.id))
            .filter(|_| b.own_fx().active());
        let (source, monitor) = match fx {
            Some(fx) => {
                connect(bus, true, Some(fx));
                (Some(fx), false)
            }
            None => (bus, true),
        };
        if let (Some(source), Some(target)) = (source, target) {
            outputs.extend(output_links(g, b, source, monitor, target));
        }
        connect(
            source,
            monitor,
            g.node_by_name(&meter_node(&bus_node(&b.id))),
        );
        // A running recording gets exactly what the output plays.
        connect(source, monitor, g.node_by_name(&rec_node(&b.id)));
    }
    links.extend(outputs);
    links
}

/// Links feeding the ducking sources' sound (after their own effects, if any) into an
/// effects node's listening inputs.
fn sidechain_links(cfg: &Config, g: &Graph, fx: &Node, sources: &[String]) -> Vec<(u32, u32)> {
    let listen: Vec<&Port> = g
        .ports_of(fx.id, Dir::In, None)
        .into_iter()
        .filter(|p| p.name.starts_with(SIDECHAIN_PREFIX))
        .collect();
    let mut links = Vec::new();
    for src in sources {
        let Some(src_strip) = cfg.strips.iter().find(|o| &o.id == src) else {
            continue;
        };
        let (node, monitor) = match g
            .node_by_name(&fx_node(src))
            .filter(|_| src_strip.fx.active())
        {
            Some(n) => (Some(n), false),
            None => (g.node_by_name(&strip_node(src)), true),
        };
        if let Some(node) = node {
            let outs: Vec<&Port> = g
                .ports_of(node.id, Dir::Out, Some(monitor))
                .into_iter()
                .filter(|p| p.channel == "FL" || p.channel == "FR" || p.channel == "MONO")
                .collect();
            links.extend(pair_ports(&outs, &listen));
        }
    }
    links
}

/// Links from a (7.1) bus to its output, adapting to the speakers the output has.
/// `source` is the bus, or its effects node (whose outputs aren't monitor ports).
fn output_links(
    g: &Graph,
    b: &Bus,
    source: &Node,
    monitor: bool,
    target: &Node,
) -> Vec<(u32, u32)> {
    let mut links = Vec::new();
    let outs = g.ports_of(source.id, Dir::Out, Some(monitor));
    let ins = g.ports_of(target.id, Dir::In, None);
    let channels: Vec<String> = ins.iter().map(|p| p.channel.clone()).collect();
    let in_port = |ch: &str| ins.iter().find(|p| p.channel == ch).map(|p| p.id);
    let has_front = in_port("FL").is_some() && in_port("FR").is_some();
    if ins.len() > 1 && !has_front {
        // Unnamed channels (pro-audio AUX0, AUX1…): front pair in order.
        let front: Vec<&Port> = outs
            .iter()
            .copied()
            .filter(|p| p.channel == "FL" || p.channel == "FR")
            .collect();
        return pair_ports(&front, &ins);
    }
    // Stereo sound spread over surround speakers: the front pair goes through the
    // upmixer (which plays on the device itself). Real surround channels go direct.
    let upmixer =
        (matches!(b.kind, Kind::Physical { .. }) && b.upmix != Upmix::Off && ins.len() > 2)
            .then(|| g.node_by_name(&upmix_in_node(&b.id)))
            .flatten();
    let first_in =
        |n: Option<&Node>| n.and_then(|n| g.ports_of(n.id, Dir::In, None).first().map(|p| p.id));
    let folding = needs_fold(&channels);
    let fold_l = g.node_by_name(&fold_node(&b.id, false)).filter(|_| folding);
    let fold_r = g.node_by_name(&fold_node(&b.id, true)).filter(|_| folding);
    for p in &outs {
        if let Some(up) = upmixer
            && (p.channel == "FL" || p.channel == "FR")
        {
            let up_in = g.ports_of(up.id, Dir::In, None);
            links.extend(
                up_in
                    .iter()
                    .find(|i| i.channel == p.channel)
                    .map(|i| (p.id, i.id)),
            );
            continue;
        }
        let dest: Vec<Option<u32>> = match route_channel(&p.channel, &channels) {
            ChannelRoute::To(ch) => vec![in_port(&ch)],
            ChannelRoute::Mono => vec![ins.first().map(|i| i.id)],
            ChannelRoute::FoldLeft => vec![first_in(fold_l)],
            ChannelRoute::FoldRight => vec![first_in(fold_r)],
            ChannelRoute::FoldBoth => vec![first_in(fold_l), first_in(fold_r)],
            ChannelRoute::Drop => vec![],
        };
        links.extend(dest.into_iter().flatten().map(|d| (p.id, d)));
    }
    for (fold, ch) in [(fold_l, "FL"), (fold_r, "FR")] {
        let out = fold.and_then(|f| g.ports_of(f.id, Dir::Out, Some(true)).first().map(|p| p.id));
        if let (Some(o), Some(i)) = (out, in_port(ch)) {
            links.push((o, i));
        }
    }
    links
}

/// Links the mixer is responsible for: between its own nodes, or between its own
/// nodes and the hardware devices named in the config. Links made by apps (e.g. an app
/// playing into a virtual strip or recording the virtual mic) are never touched.
pub fn managed_links<'a>(cfg: &Config, g: &'a Graph) -> Vec<&'a Link> {
    let devices: HashSet<&str> = cfg
        .strips
        .iter()
        .map(|s| &s.kind)
        .chain(cfg.buses.iter().map(|b| &b.kind))
        .filter_map(|k| match k {
            Kind::Physical { device } => Some(device.as_str()),
            Kind::Virtual => None,
        })
        .collect();
    let class_of = |id: u32| -> Option<(bool, bool)> {
        let n = g.nodes.get(&id)?;
        let ours = is_ours(&n.name)
            || [METER_PREFIX, UPIN_PREFIX, FX_PREFIX, REC_PREFIX]
                .iter()
                .any(|p| n.name.starts_with(p));
        Some((ours, devices.contains(n.name.as_str())))
    };
    g.links
        .values()
        .filter(|l| match (class_of(l.out_node), class_of(l.in_node)) {
            (Some((out_ours, out_dev)), Some((in_ours, in_dev))) => {
                (out_ours || in_ours) && (out_ours || out_dev) && (in_ours || in_dev)
            }
            _ => false,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Bus, Strip};

    struct Builder {
        g: Graph,
        next: u32,
    }
    impl Builder {
        fn new() -> Self {
            Builder {
                g: Graph::default(),
                next: 1,
            }
        }
        fn id(&mut self) -> u32 {
            self.next += 1;
            self.next
        }
        /// Adds a node with stereo ports; `monitor` adds monitor outputs (sinks/null nodes).
        fn node(&mut self, name: &str, ins: &[&str], outs: &[&str], monitor: bool) -> u32 {
            let id = self.id();
            self.g.nodes.insert(
                id,
                Node {
                    id,
                    name: name.into(),
                    class: String::new(),
                    description: String::new(),
                    hardware: false,
                    ..Default::default()
                },
            );
            for (chs, dir) in [(ins, Dir::In), (outs, Dir::Out)] {
                for ch in chs {
                    let pid = self.id();
                    self.g.ports.insert(
                        pid,
                        Port {
                            id: pid,
                            node: id,
                            dir,
                            channel: ch.to_string(),
                            monitor: false,
                            name: format!(
                                "{}_{ch}",
                                if dir == Dir::In { "input" } else { "output" }
                            ),
                        },
                    );
                }
            }
            if monitor {
                for ch in ins {
                    let pid = self.id();
                    self.g.ports.insert(
                        pid,
                        Port {
                            id: pid,
                            node: id,
                            dir: Dir::Out,
                            channel: ch.to_string(),
                            monitor: true,
                            name: format!("monitor_{ch}"),
                        },
                    );
                }
            }
            id
        }
        fn port(&self, node: u32, dir: Dir, ch: &str, monitor: bool) -> u32 {
            self.g
                .ports
                .values()
                .find(|p| p.node == node && p.dir == dir && p.channel == ch && p.monitor == monitor)
                .unwrap()
                .id
        }
    }

    fn cfg() -> Config {
        Config {
            strips: vec![
                Strip::new(
                    "mic",
                    "Mic",
                    Kind::Physical {
                        device: "hw_mic".into(),
                    },
                    &["b1"],
                ),
                Strip::new("music", "Music", Kind::Virtual, &["a1"]),
            ],
            buses: vec![
                Bus::new(
                    "a1",
                    "Speakers",
                    Kind::Physical {
                        device: "hw_51".into(),
                    },
                ),
                Bus::new("b1", "Virtual Mic", Kind::Virtual),
            ],
            midi: None,
            rules: vec![],
            scenes: vec![],
            eq_presets: vec![],
            cue_bus: None,
        }
    }

    const ST: &[&str] = &["FL", "FR"];

    #[test]
    fn routes_whole_chain_and_maps_channels() {
        let mut b = Builder::new();
        let mic = b.node("hw_mic", &[], &["MONO"], false);
        let sp = b.node("hw_51", &["FL", "FR", "FC", "LFE", "RL", "RR"], &[], true);
        let s_mic = b.node("lam_strip_mic", ST, &[], true);
        let s_music = b.node("lam_strip_music", ST, &[], true);
        let a1 = b.node("lam_bus_a1", ST, &[], true);
        let b1 = b.node("lam_bus_b1", ST, &[], true);
        let o1 = b.node("lam_out_b1", ST, ST, false);

        let got = desired_links(&cfg(), &b.g);
        let p = |n, d, c, m| b.port(n, d, c, m);
        let want: HashSet<(u32, u32)> = [
            // mono mic fans out to both strip channels
            (
                p(mic, Dir::Out, "MONO", false),
                p(s_mic, Dir::In, "FL", false),
            ),
            (
                p(mic, Dir::Out, "MONO", false),
                p(s_mic, Dir::In, "FR", false),
            ),
            (p(s_mic, Dir::Out, "FL", true), p(b1, Dir::In, "FL", false)),
            (p(s_mic, Dir::Out, "FR", true), p(b1, Dir::In, "FR", false)),
            (
                p(s_music, Dir::Out, "FL", true),
                p(a1, Dir::In, "FL", false),
            ),
            (
                p(s_music, Dir::Out, "FR", true),
                p(a1, Dir::In, "FR", false),
            ),
            // stereo bus goes to front L/R of the 5.1 device only
            (p(a1, Dir::Out, "FL", true), p(sp, Dir::In, "FL", false)),
            (p(a1, Dir::Out, "FR", true), p(sp, Dir::In, "FR", false)),
            (p(b1, Dir::Out, "FL", true), p(o1, Dir::In, "FL", false)),
            (p(b1, Dir::Out, "FR", true), p(o1, Dir::In, "FR", false)),
        ]
        .into();
        assert_eq!(got, want);
    }

    #[test]
    fn missing_device_just_drops_its_links() {
        let mut b = Builder::new();
        b.node("lam_strip_music", ST, &[], true);
        b.node("lam_bus_a1", ST, &[], true);
        // hw_51 unplugged
        assert_eq!(desired_links(&cfg(), &b.g).len(), 2);
    }

    const S71: &[&str] = &["FL", "FR", "FC", "LFE", "RL", "RR", "SL", "SR"];

    /// Where each bus channel ends up: "FL" → ["FL"], "FC" → ["fold_l", "fold_r"]…
    fn routes(
        b: &Builder,
        links: &HashSet<(u32, u32)>,
        bus: u32,
    ) -> std::collections::BTreeMap<String, Vec<String>> {
        let name = |port: u32| {
            let p = &b.g.ports[&port];
            let n = &b.g.nodes[&p.node].name;
            if n.starts_with("lam_fold_") || n.starts_with("lam_upin_") {
                format!(
                    "{}:{}",
                    n.trim_start_matches("lam_").replace("_a1", ""),
                    p.channel
                )
            } else {
                p.channel.clone()
            }
        };
        let mut m = std::collections::BTreeMap::new();
        for p in b.g.ports_of(bus, Dir::Out, Some(true)) {
            let mut to: Vec<String> = links
                .iter()
                .filter(|(o, _)| *o == p.id)
                .map(|(_, i)| name(*i))
                .collect();
            to.sort();
            to.retain(|t| !t.is_empty());
            m.insert(p.channel.clone(), to);
        }
        m
    }

    #[test]
    fn surround_bus_on_every_kind_of_output() {
        let v = |x: &[&str]| x.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        // 7.1 bus on 7.1, 5.1 and stereo outputs.
        for (device, expect) in [
            (
                S71,
                vec![
                    ("FC", v(&["FC"])),
                    ("LFE", v(&["LFE"])),
                    ("SL", v(&["SL"])),
                    ("RR", v(&["RR"])),
                ],
            ),
            (
                &["FL", "FR", "FC", "LFE", "RL", "RR"][..],
                vec![("SL", v(&["RL"])), ("SR", v(&["RR"])), ("FC", v(&["FC"]))],
            ),
            (
                ST,
                vec![
                    ("FL", v(&["FL"])),
                    ("FC", v(&["fold_l:MONO", "fold_r:MONO"])),
                    ("RL", v(&["fold_l:MONO"])),
                    ("SR", v(&["fold_r:MONO"])),
                    ("LFE", v(&[])),
                ],
            ),
        ] {
            let mut b = Builder::new();
            let dev = b.node("hw_51", device, &[], true);
            let bus = b.node("lam_bus_a1", S71, &[], true);
            let fold_l = b.node("lam_fold_a1_l", &["MONO"], &[], true);
            let fold_r = b.node("lam_fold_a1_r", &["MONO"], &[], true);
            let links = desired_links(&cfg(), &b.g);
            let r = routes(&b, &links, bus);
            for (ch, want) in expect {
                assert_eq!(r[ch], want, "{ch} on a {}-channel output", device.len());
            }
            let folds_used = device.len() == 2;
            let fold_to_front = links.contains(&(
                b.port(fold_l, Dir::Out, "MONO", true),
                b.port(dev, Dir::In, "FL", false),
            )) && links.contains(&(
                b.port(fold_r, Dir::Out, "MONO", true),
                b.port(dev, Dir::In, "FR", false),
            ));
            assert_eq!(fold_to_front, folds_used);
        }
    }

    #[test]
    fn fold_helpers_only_where_needed() {
        let mut b = Builder::new();
        b.node("hw_51", &["FL", "FR", "FC", "LFE", "RL", "RR"], &[], true);
        let names: Vec<String> = desired_nodes(&cfg(), &b.g)
            .into_iter()
            .map(|d| d.name)
            .collect();
        // The 5.1 speakers (a1) have every bus channel (sides go to rears); the virtual
        // mic (b1) is stereo, so it folds.
        assert!(!names.contains(&"lam_fold_a1_l".to_string()));
        assert!(
            names.contains(&"lam_fold_b1_l".to_string())
                && names.contains(&"lam_fold_b1_r".to_string())
        );
        // Virtual strips offer 7.1 by default; stereo on request.
        let mut c = cfg();
        let music = |c: &Config, g: &Graph| {
            desired_nodes(c, g)
                .into_iter()
                .find(|d| d.name == "lam_strip_music")
                .unwrap()
                .positions
        };
        assert_eq!(music(&c, &b.g), SURROUND);
        c.strips[1].layout = Layout::Stereo;
        assert_eq!(music(&c, &b.g), STEREO);
    }

    #[test]
    fn stereo_upmix_uses_the_front_pair_only() {
        let mut b = Builder::new();
        b.node("hw_51", &["FL", "FR", "FC", "LFE", "RL", "RR"], &[], true);
        let bus = b.node("lam_bus_a1", S71, &[], true);
        b.node("lam_upin_a1", ST, &[], false);
        let mut c = cfg();
        c.buses[0].upmix = Upmix::Surround;
        let r = routes(&b, &desired_links(&c, &b.g), bus);
        assert_eq!(r["FL"], vec!["upin:FL".to_string()]);
        assert_eq!(r["FR"], vec!["upin:FR".to_string()]);
        assert_eq!(
            r["FC"],
            vec!["FC".to_string()],
            "real surround still goes direct"
        );
        assert_eq!(r["SL"], vec!["RL".to_string()]);
        // Older configs with "full"/"rear" mean surround.
        let old: Config = toml::from_str(
            "[[bus]]\nid = \"a\"\nname = \"A\"\nkind = \"physical\"\ndevice = \"x\"\nupmix = \"full\"\n",
        )
        .unwrap();
        assert_eq!(old.buses[0].upmix, Upmix::Surround);
    }

    #[test]
    fn effects_node_sits_between_channel_and_outputs() {
        let mut b = Builder::new();
        let music = b.node("lam_strip_music", ST, &[], true);
        let a1 = b.node("lam_bus_a1", ST, &[], true);
        let fx = b.node("lam_fx_music", ST, ST, false);
        let mut c = cfg();
        let has = |links: &HashSet<(u32, u32)>, from: (u32, bool), to: u32| {
            links.contains(&(
                b.port(from.0, Dir::Out, "FL", from.1),
                b.port(to, Dir::In, "FL", false),
            ))
        };
        // Effects off: direct, even though an fx node is lying around.
        let links = desired_links(&c, &b.g);
        assert!(has(&links, (music, true), a1) && !has(&links, (music, true), fx));
        // Effects on: channel -> fx -> output, and no direct path.
        c.strips[1].fx.comp.enabled = true;
        let links = desired_links(&c, &b.g);
        assert!(has(&links, (music, true), fx));
        assert!(has(&links, (fx, false), a1));
        assert!(!has(&links, (music, true), a1));
    }

    #[test]
    fn output_effects_sit_before_the_speakers() {
        let mut b = Builder::new();
        let dev = b.node("hw_51", &["FL", "FR", "FC", "LFE", "RL", "RR"], &[], true);
        let bus = b.node("lam_bus_a1", S71, &[], true);
        let fx = b.node("lam_fx_a1", S71, S71, false);
        let mut c = cfg();
        c.buses[0].fx.limiter.enabled = true;
        let links = desired_links(&c, &b.g);
        let has = |from: (u32, &str, bool), to: (u32, &str)| {
            links.contains(&(
                b.port(from.0, Dir::Out, from.1, from.2),
                b.port(to.0, Dir::In, to.1, false),
            ))
        };
        assert!(has((bus, "FC", true), (fx, "FC")), "bus -> limiter");
        assert!(has((fx, "FC", false), (dev, "FC")), "limiter -> speakers");
        assert!(
            has((fx, "SL", false), (dev, "RL")),
            "and still adapted to the speakers"
        );
        assert!(
            !has((bus, "FC", true), (dev, "FC")),
            "no path around the limiter"
        );
    }

    #[test]
    fn ducking_sources_feed_the_listening_inputs() {
        let mut b = Builder::new();
        let mic = b.node("lam_strip_mic", ST, &[], true);
        let music = b.node("lam_strip_music", ST, &[], true);
        let fx = b.node("lam_fx_music", ST, ST, false);
        // The effects node's two listening inputs.
        let mut listen = vec![];
        for ch in ["FL", "FR"] {
            let pid = b.id();
            b.g.ports.insert(
                pid,
                Port {
                    id: pid,
                    node: fx,
                    dir: Dir::In,
                    channel: ch.into(),
                    monitor: false,
                    name: format!("sidechain_{ch}"),
                },
            );
            listen.push(pid);
        }
        let mut c = cfg();
        c.strips[1].fx.duck = crate::config::Duck {
            enabled: true,
            sources: vec!["mic".into()],
            ..Default::default()
        };
        let links = desired_links(&c, &b.g);
        let mic_fl = b.port(mic, Dir::Out, "FL", true);
        assert!(
            links.contains(&(mic_fl, listen[0])),
            "mic -> listening input"
        );
        // The music itself goes into the normal inputs only, never the listening ones.
        let music_fl = b.port(music, Dir::Out, "FL", true);
        let fx_in_fl =
            b.g.ports
                .values()
                .find(|p| p.node == fx && p.name == "input_FL")
                .unwrap()
                .id;
        assert!(links.contains(&(music_fl, fx_in_fl)));
        assert!(
            !links
                .iter()
                .any(|(o, i)| *o == music_fl && listen.contains(i))
        );
        // Without ducking, nothing goes to the listening inputs.
        c.strips[1].fx.duck.enabled = false;
        c.strips[1].fx.comp.enabled = true;
        assert!(
            !desired_links(&c, &b.g)
                .iter()
                .any(|(_, i)| listen.contains(i))
        );
    }

    #[test]
    fn output_ducking_turns_down_only_the_other_channels_there() {
        let mut b = Builder::new();
        let mic = b.node("lam_strip_mic", ST, &[], true);
        let music = b.node("lam_strip_music", ST, &[], true);
        let a1 = b.node("lam_bus_a1", ST, &[], true);
        let b1 = b.node("lam_bus_b1", ST, &[], true);
        let duck = b.node("lam_fx_b1.duck", ST, ST, false);
        let mut listen = vec![];
        for ch in ["FL", "FR"] {
            let pid = b.id();
            b.g.ports.insert(
                pid,
                Port {
                    id: pid,
                    node: duck,
                    dir: Dir::In,
                    channel: ch.into(),
                    monitor: false,
                    name: format!("sidechain_{ch}"),
                },
            );
            listen.push(pid);
        }
        let mut c = cfg();
        c.strips[1].buses.push("b1".into());
        c.buses[1].fx.duck = crate::config::Duck {
            enabled: true,
            sources: vec!["mic".into()],
            ..Default::default()
        };
        let links = desired_links(&c, &b.g);
        let input = |node| {
            b.g.ports
                .values()
                .find(|p| p.node == node && p.dir == Dir::In && p.channel == "FL" && !p.name.starts_with("sidechain_"))
                .unwrap()
                .id
        };
        let (mic_fl, music_fl) = (
            b.port(mic, Dir::Out, "FL", true),
            b.port(music, Dir::Out, "FL", true),
        );
        // Music reaches b1 through the ducking stage, and a1 directly (not ducked there).
        assert!(links.contains(&(music_fl, input(duck))));
        assert!(!links.contains(&(music_fl, input(b1))));
        assert!(links.contains(&(music_fl, input(a1))));
        // The mic goes straight into b1 and is what the ducking listens to.
        assert!(links.contains(&(mic_fl, input(b1))));
        assert!(links.contains(&(mic_fl, listen[0])));
        assert!(!links.contains(&(mic_fl, input(duck))));
        // The ducking stage feeds the output.
        assert!(links.contains(&(b.port(duck, Dir::Out, "FL", false), input(b1))));
        // Switched off: music goes straight in again.
        c.buses[1].fx.duck.enabled = false;
        assert!(desired_links(&c, &b.g).contains(&(music_fl, input(b1))));
    }

    #[test]
    fn app_links_are_not_managed() {
        let mut b = Builder::new();
        let app = b.node("spotify-stream", &[], ST, false);
        let s_music = b.node("lam_strip_music", ST, &[], true);
        let a1 = b.node("lam_bus_a1", ST, &[], true);
        let obs = b.node("obs-capture", ST, &[], false);
        let mut add = |out_node, out_port, in_node, in_port| {
            let id = 1000 + b.g.links.len() as u32;
            b.g.links.insert(
                id,
                Link {
                    id,
                    out_port,
                    in_port,
                    out_node,
                    in_node,
                },
            );
            id
        };
        let app_link = add(app, 0, s_music, 0);
        let ours = add(s_music, 0, a1, 0);
        let obs_link = add(s_music, 0, obs, 0);
        let managed: Vec<u32> = managed_links(&cfg(), &b.g).iter().map(|l| l.id).collect();
        assert_eq!(managed, vec![ours]);
        assert!(!managed.contains(&app_link) && !managed.contains(&obs_link));
    }
}
