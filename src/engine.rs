//! The live side: watches the PipeWire graph and continuously reconciles it with the config.
//!
//! The approach is "desired state": every tick we compute which nodes and links the config
//! asks for and fix any difference. That is what makes it self-healing — a replugged
//! device, a crashed app or a restarted PipeWire all converge back to the right routing.
//!
//! Nodes and links are created with `object.linger`, so audio keeps flowing even if the
//! mixer process itself stops. On restart the mixer adopts what is already there.

use crate::config::{self, Action, Config};
use crate::config::{Binding, Control, Kind, Mode, Rule, Upmix};
use crate::graph::{self, DesiredNode, Dir, Graph, Link, Node, Port};
use crate::ipc::{
    self, AppInfo, Clients, Command, Device, Event, Incoming, LearnTarget, RecordingInfo,
};
use crate::meter::Meter;
use crate::midi::{Controller, MidiMsg};
use anyhow::Result;
use log::{debug, info, warn};
use pipewire as pw;
use pw::properties::properties;
use pw::spa;
use pw::types::ObjectType;
use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;
use std::rc::{Rc, Weak};
use std::time::{Duration, Instant, SystemTime};

/// How long to wait for a requested object to show up before asking again.
const PENDING_TIMEOUT: Duration = Duration::from_secs(3);
const TICK: Duration = Duration::from_millis(100);
const CONFIG_POLL: Duration = Duration::from_millis(500);
const MIDI_POLL: Duration = Duration::from_secs(1);
/// Changes from MIDI or the desktop are written to the config after this quiet period.
const SAVE_DELAY: Duration = Duration::from_secs(1);

struct BoundNode {
    id: u32,
    proxy: pw::node::Node,
    _listener: pw::node::NodeListener,
    /// Last (volume, balance, mute) we sent.
    applied: Option<(f32, f32, bool)>,
    /// True once the node has reported back what we last sent. Until then its reports
    /// are stale and must not be mistaken for someone else changing the volume.
    synced: bool,
}

/// A sound card, watched for its profiles (stereo, surround, ...).
struct Card {
    /// PipeWire device.name, e.g. "alsa_card.pci-0000_36_00.4".
    name: String,
    proxy: pw::device::Device,
    _listener: pw::device::DeviceListener,
    profiles: BTreeMap<u32, ipc::Profile>,
    current: Option<u32>,
}

pub struct Engine {
    cfg: Config,
    cfg_path: Option<PathBuf>,
    cfg_mtime: Option<SystemTime>,
    last_cfg_check: Instant,

    graph: Graph,
    /// Registry globals of our own nodes, needed to bind proxies to them.
    our_globals: HashMap<u32, pw::registry::GlobalObject<pw::properties::PropertiesBox>>,
    bound: HashMap<String, BoundNode>,
    pending_nodes: HashMap<String, Instant>,
    pending_links: HashMap<(u32, u32), Instant>,
    /// Objects we asked PipeWire to remove that haven't disappeared yet.
    destroying: HashSet<u32>,
    dirty: bool,
    save_at: Option<Instant>,

    controller: Option<Controller>,
    last_midi_poll: Option<Instant>,
    midi_tx: pw::channel::Sender<MidiMsg>,
    /// Feedback values last sent, keyed by (channel, control).
    feedback_sent: HashMap<(Option<u8>, Control), u16>,
    /// Times at which to resend all LEDs: a freshly plugged-in controller ignores
    /// messages for a moment, so the states sent right at connect can get lost.
    led_resends: Vec<Instant>,

    /// Connected GUIs.
    clients: Clients,
    /// The state GUIs see changed and should be resent.
    state_dirty: bool,
    devices: Vec<Device>,
    midi_was_connected: bool,
    midi_ports: Vec<String>,
    /// PipeWire's "default" metadata, used to tell WirePlumber where an app should play.
    metadata_global: Option<pw::registry::GlobalObject<pw::properties::PropertiesBox>>,
    metadata: Option<pw::metadata::Metadata>,
    /// App streams the rules have already been applied to (each is handled once, so
    /// moving a stream by hand afterwards is respected).
    handled_streams: HashSet<u32>,
    /// The rules `handled_streams` was computed for; a rule change re-applies them.
    applied_rules: Vec<Rule>,
    apps: Vec<AppInfo>,
    /// MIDI learn in progress, and when it gives up.
    learning: Option<(LearnTarget, Instant)>,
    /// Level meters by meter node name, only while a GUI is connected.
    meters: HashMap<String, (String, Meter)>,

    /// Surround upmixers by bus id (PipeWire loopbacks running in this process).
    upmixers: HashMap<String, Upmixer>,
    upmix_started: HashMap<String, Instant>,
    /// Effects nodes by strip id.
    fx_nodes: HashMap<String, crate::fx::FxNode>,
    /// Running recordings by bus id.
    recordings: HashMap<String, Recording>,
    /// Sound cards by PipeWire id.
    cards: HashMap<u32, Card>,
    /// Which card (and its profile at the time) each hardware device seen so far belongs
    /// to, by device name, so a device that disappears because its card switched profile
    /// can be followed.
    device_cards: HashMap<String, (u32, Option<u32>)>,
    /// An upmixer restart was postponed; check again on the next tick.
    retry_upmix: bool,
    /// Progress reports from a running speaker test (its own thread).
    speaker_test_tx: pw::channel::Sender<(String, Option<String>)>,
    speaker_test_running: bool,

    me: Weak<RefCell<Engine>>,
    context: pw::context::ContextRc,
    core: pw::core::CoreRc,
    registry: pw::registry::RegistryRc,
}

impl Engine {
    fn on_global(&mut self, g: &pw::registry::GlobalObject<&spa::utils::dict::DictRef>) {
        let Some(props) = g.props else { return };
        let get = |k: &str| props.get(k).unwrap_or_default().to_string();
        let num = |k: &str| props.get(k).and_then(|v| v.parse::<u32>().ok());
        match g.type_ {
            ObjectType::Node => {
                let node = Node {
                    id: g.id,
                    name: get("node.name"),
                    class: get("media.class"),
                    description: get("node.description"),
                    hardware: props.get("device.id").is_some()
                        && matches!(
                            props.get("media.class"),
                            Some("Audio/Sink" | "Audio/Source")
                        ),
                    app_name: get("application.name"),
                    app_binary: get("application.process.binary"),
                    card: num("device.id"),
                };
                if graph::is_ours(&node.name) {
                    self.our_globals.insert(g.id, g.to_owned());
                }
                self.graph.nodes.insert(g.id, node);
            }
            ObjectType::Port => {
                let (Some(node), Some(dir)) = (num("node.id"), props.get("port.direction")) else {
                    return;
                };
                let name = get("port.name");
                self.graph.ports.insert(
                    g.id,
                    Port {
                        id: g.id,
                        node,
                        dir: if dir == "in" { Dir::In } else { Dir::Out },
                        channel: get("audio.channel"),
                        monitor: props.get("port.monitor") == Some("true")
                            || name.starts_with("monitor_"),
                        name: name.clone(),
                    },
                );
            }
            ObjectType::Link => {
                if let (Some(out_port), Some(in_port), Some(out_node), Some(in_node)) = (
                    num("link.output.port"),
                    num("link.input.port"),
                    num("link.output.node"),
                    num("link.input.node"),
                ) {
                    self.pending_links.remove(&(out_port, in_port));
                    self.graph.links.insert(
                        g.id,
                        Link {
                            id: g.id,
                            out_port,
                            in_port,
                            out_node,
                            in_node,
                        },
                    );
                }
            }
            ObjectType::Device if props.get("media.class") == Some("Audio/Device") => {
                self.bind_card(&g.to_owned(), get("device.name"));
            }
            ObjectType::Metadata if props.get("metadata.name") == Some("default") => {
                self.metadata_global = Some(g.to_owned());
                self.metadata = None;
            }
            _ => return,
        }
        self.dirty = true;
    }

    fn on_global_remove(&mut self, id: u32) {
        self.graph.remove(id);
        self.our_globals.remove(&id);
        self.bound.retain(|_, b| b.id != id);
        self.destroying.remove(&id);
        self.handled_streams.remove(&id);
        if self.cards.remove(&id).is_some() {
            self.state_dirty = true;
        }
        if self.metadata_global.as_ref().is_some_and(|g| g.id == id) {
            self.metadata_global = None;
            self.metadata = None;
        }
        self.dirty = true;
    }

    fn tick(&mut self) {
        if self.last_cfg_check.elapsed() >= CONFIG_POLL {
            self.last_cfg_check = Instant::now();
            self.reload_config_if_changed();
        }
        // Retry anything that timed out even when nothing else changed.
        let now = Instant::now();
        let before = self.pending_nodes.len() + self.pending_links.len();
        self.pending_nodes
            .retain(|_, t| now.duration_since(*t) < PENDING_TIMEOUT);
        self.pending_links
            .retain(|_, t| now.duration_since(*t) < PENDING_TIMEOUT);
        if self.pending_nodes.len() + self.pending_links.len() != before {
            self.dirty = true;
        }
        if std::mem::take(&mut self.retry_upmix) {
            self.dirty = true;
        }
        if self.dirty {
            self.dirty = false;
            self.reconcile();
        }
        if self.last_midi_poll.is_none_or(|t| t.elapsed() >= MIDI_POLL) {
            self.last_midi_poll = Some(Instant::now());
            self.poll_midi();
            // Only worth scanning while someone could pick a controller.
            if self.clients.windows() > 0 {
                let ports = crate::midi::list_devices();
                if ports != self.midi_ports {
                    self.midi_ports = ports;
                    self.state_dirty = true;
                }
            }
        }
        if self
            .learning
            .as_ref()
            .is_some_and(|(_, until)| Instant::now() >= *until)
        {
            info!("MIDI learn timed out");
            self.learning = None;
            self.state_dirty = true;
        }
        let before = self.led_resends.len();
        self.led_resends.retain(|t| Instant::now() < *t);
        if self.led_resends.len() != before {
            self.refresh_feedback(true);
        }
        self.check_recordings();
        if self.save_at.is_some_and(|t| Instant::now() >= t) {
            self.flush_save();
        }
        let midi_connected = self.controller.as_ref().is_some_and(|c| c.is_connected());
        if midi_connected != self.midi_was_connected {
            self.midi_was_connected = midi_connected;
            self.state_dirty = true;
        }
        if self.state_dirty && !self.clients.is_empty() {
            self.state_dirty = false;
            let ev = self.state_event();
            self.clients.broadcast(&ev);
        }
    }

    fn bind_card(
        &mut self,
        g: &pw::registry::GlobalObject<pw::properties::PropertiesBox>,
        name: String,
    ) {
        let proxy = match self.registry.bind::<pw::device::Device, _>(g) {
            Ok(p) => p,
            Err(e) => {
                warn!("cannot watch sound card {name}: {e}");
                return;
            }
        };
        let me = self.me.clone();
        let id = g.id;
        let listener = proxy
            .add_listener_local()
            .info({
                let me = me.clone();
                // A changed param is only announced, by toggling its SERIAL flag; ask for it.
                let serials = RefCell::new(HashMap::new());
                move |i| {
                    use spa::param::{ParamInfoFlags, ParamType};
                    let mut serials = serials.borrow_mut();
                    let changed: Vec<ParamType> = i
                        .params()
                        .iter()
                        .filter(|p| p.id() == ParamType::Profile || p.id() == ParamType::EnumProfile)
                        .filter(|p| {
                            let serial = p.flags().contains(ParamInfoFlags::SERIAL);
                            serials.insert(p.id().as_raw(), serial).is_some_and(|old| old != serial)
                        })
                        .map(|p| p.id())
                        .collect();
                    if changed.is_empty() {
                        return;
                    }
                    if let Some(e) = me.upgrade()
                        && let Ok(e) = e.try_borrow()
                        && let Some(c) = e.cards.get(&id)
                    {
                        for t in changed {
                            c.proxy.enum_params(0, Some(t), 0, u32::MAX);
                        }
                    }
                }
            })
            .param(move |_seq, kind, _index, _next, pod| {
                let Some(p) = pod.and_then(parse_profile) else {
                    return;
                };
                if let Some(e) = me.upgrade()
                    && let Ok(mut e) = e.try_borrow_mut()
                {
                    e.on_card_profile(id, kind, p);
                }
            })
            .register();
        proxy.subscribe_params(&[
            spa::param::ParamType::EnumProfile,
            spa::param::ParamType::Profile,
        ]);
        self.cards.insert(
            id,
            Card {
                name,
                proxy,
                _listener: listener,
                profiles: BTreeMap::new(),
                current: None,
            },
        );
    }

    fn on_card_profile(&mut self, card: u32, kind: spa::param::ParamType, p: ipc::Profile) {
        let Some(c) = self.cards.get_mut(&card) else { return };
        if kind == spa::param::ParamType::Profile {
            if c.current != Some(p.index) {
                info!("{}: profile is now \"{}\"", c.name, p.description);
                c.current = Some(p.index);
            }
        } else if kind == spa::param::ParamType::EnumProfile {
            c.profiles.insert(p.index, p);
        }
        self.dirty = true; // refresh device list
    }

    fn set_profile(&mut self, device: &str, index: u32) {
        let Some(card) = self
            .graph
            .node_by_name(device)
            .and_then(|n| n.card)
            .and_then(|c| self.cards.get(&c))
        else {
            warn!("no sound card found for {device}");
            return;
        };
        match profile_pod(index) {
            Ok(bytes) => {
                let Some(pod) = spa::pod::Pod::from_bytes(&bytes) else { return };
                info!("switching {} to profile {index}", card.name);
                card.proxy
                    .set_param(spa::param::ParamType::Profile, 0, pod);
            }
            Err(e) => warn!("profile switch: {e}"),
        }
    }

    /// Physical strips/buses whose device vanished because its sound card switched
    /// profile (the device gets a new name) move to the card's new device.
    fn follow_devices(&mut self) {
        for n in self.graph.nodes.values() {
            if let (true, Some(c)) = (n.hardware, n.card) {
                let profile = self.cards.get(&c).and_then(|c| c.current);
                self.device_cards.insert(n.name.clone(), (c, profile));
            }
        }
        let mut moves = Vec::new();
        let mut check = |id: &str, kind: &Kind, output: bool| {
            let Kind::Physical { device } = kind else { return };
            if self.graph.node_by_name(device).is_some() {
                return;
            }
            let card = match self.device_cards.get(device) {
                // Seen before: only follow once the card really switched profile
                // (not while it's still appearing, or after it was unplugged).
                Some(&(card, profile)) => {
                    let now = self.cards.get(&card).and_then(|c| c.current);
                    (now.is_some() && now != profile).then_some(card)
                }
                None => alsa_card_name(device).and_then(|name| {
                    self.cards.iter().find(|(_, c)| c.name == name).map(|(id, _)| *id)
                }),
            };
            let Some(card) = card else { return };
            if let Some(new) = card_device(&self.graph, card, output) {
                moves.push((id.to_string(), new));
            }
        };
        for s in &self.cfg.strips {
            check(&s.id, &s.kind, false);
        }
        for b in &self.cfg.buses {
            check(&b.id, &b.kind, true);
        }
        if moves.is_empty() {
            return;
        }
        for (id, new) in moves {
            info!("{id}: its sound card changed profile, now using {new}");
            let kind = self
                .cfg
                .strips
                .iter_mut()
                .find(|s| s.id == id)
                .map(|s| &mut s.kind)
                .or_else(|| {
                    self.cfg
                        .buses
                        .iter_mut()
                        .find(|b| b.id == id)
                        .map(|b| &mut b.kind)
                });
            if let Some(Kind::Physical { device }) = kind {
                *device = new;
            }
        }
        self.state_changed();
    }

    fn state_event(&self) -> Event {
        Event::State {
            config: self.cfg.clone(),
            devices: self.devices.clone(),
            midi_connected: self.midi_was_connected,
            midi_ports: self.midi_ports.clone(),
            learning: self.learning.as_ref().map(|(l, _)| l.clone()),
            apps: self.apps.clone(),
            recordings: self.recordings.values().map(|r| r.info.clone()).collect(),
            windows: self.clients.windows(),
        }
    }

    fn refresh_devices(&mut self) {
        let mut devices: Vec<Device> = self
            .graph
            .nodes
            .values()
            .filter(|n| n.hardware)
            .map(|n| {
                let output = n.class == "Audio/Sink";
                let dir = if output { Dir::In } else { Dir::Out };
                let card = n.card.and_then(|c| self.cards.get(&c));
                Device {
                    name: n.name.clone(),
                    description: n.description.clone(),
                    output,
                    channels: self
                        .graph
                        .ports_of(n.id, dir, Some(false))
                        .iter()
                        .map(|p| p.channel.clone())
                        .collect(),
                    card: n.card,
                    profiles: card
                        .map(|c| c.profiles.values().cloned().collect())
                        .unwrap_or_default(),
                    profile: card.and_then(|c| c.current),
                }
            })
            .collect();
        devices.sort_by(|a, b| a.description.cmp(&b.description));
        if devices != self.devices {
            self.devices = devices;
            self.state_dirty = true;
        }
    }

    fn on_ipc(&mut self, msg: Incoming) {
        match msg {
            Incoming::Connected(id, tx) => {
                debug!("GUI {id} connected");
                self.clients.add(id, tx);
                self.state_dirty = true; // window count
                let ev = self.state_event();
                self.clients.send_to(id, &ev);
                self.dirty = true; // create meters
            }
            Incoming::Disconnected(id) => {
                debug!("GUI {id} disconnected");
                self.clients.remove(id);
                self.state_dirty = true; // window count
                self.dirty = true; // drop meters if nobody watches
            }
            Incoming::Command(client, cmd) => {
                match cmd {
                    Command::SetGain { id, db } => {
                        if let Some((gain, _)) = self.cfg.level_mut(&id) {
                            *gain = db.clamp(config::MIN_GAIN_DB, config::MAX_GAIN_DB);
                        }
                    }
                    Command::SetMute { id, mute } => {
                        if let Some((_, m)) = self.cfg.level_mut(&id) {
                            *m = mute;
                        }
                    }
                    Command::SetRoute { strip, bus, on } => self.cfg.set_routed(&strip, &bus, on),
                    Command::SetCue { id, on } => {
                        if let Some(s) = self.cfg.strips.iter_mut().find(|s| s.id == id) {
                            s.cue = on;
                        }
                    }
                    Command::SetCueBus { bus } => {
                        if bus
                            .as_ref()
                            .is_none_or(|b| self.cfg.buses.iter().any(|x| &x.id == b))
                        {
                            self.cfg.cue_bus = bus;
                        }
                    }
                    Command::SetPan { id, pan } => {
                        if let Some(s) = self.cfg.strips.iter_mut().find(|s| s.id == id) {
                            s.pan = pan.clamp(-1.0, 1.0);
                        }
                    }
                    Command::MidiLearn { learn } => {
                        if self.cfg.midi.is_none() {
                            let message = "Choose a MIDI controller first".to_string();
                            self.clients.send_to(client, &Event::Error { message });
                            return;
                        }
                        info!(
                            "MIDI learn: waiting for a control for {} {:?}",
                            learn.target, learn.action
                        );
                        self.learning = Some((learn, Instant::now() + Duration::from_secs(20)));
                        self.state_dirty = true;
                        return;
                    }
                    Command::RouteApp {
                        app,
                        strip,
                        remember,
                    } => {
                        if !self.cfg.strips.iter().any(|s| s.id == strip) {
                            return;
                        }
                        let streams: Vec<u32> = self
                            .graph
                            .nodes
                            .values()
                            .filter(|n| n.is_app_playback() && app_key(n) == app)
                            .map(|n| n.id)
                            .collect();
                        info!("moving {app} ({} streams) to {strip}", streams.len());
                        for id in streams {
                            self.move_stream(id, &graph::strip_node(&strip));
                        }
                        if remember {
                            self.cfg.rules.retain(|r| r.app != app);
                            self.cfg.rules.push(Rule { app, strip });
                            // Already moved; don't re-apply to these streams.
                            self.applied_rules = self.cfg.rules.clone();
                        }
                    }
                    Command::ForgetApp { app } => {
                        self.cfg.rules.retain(|r| r.app != app);
                        self.applied_rules = self.cfg.rules.clone();
                    }
                    Command::SaveScene { name } => {
                        let name = name.trim().to_string();
                        if name.is_empty() {
                            return;
                        }
                        info!("scene \"{name}\" saved");
                        self.cfg.save_scene(&name);
                    }
                    Command::RecallScene { name } => {
                        if !self.cfg.recall_scene(&name) {
                            return;
                        }
                        info!("scene \"{name}\" recalled");
                    }
                    Command::SetEffects { id, fx } => {
                        if let Some(current) = self.cfg.effects_mut(&id) {
                            *current = fx;
                        }
                    }
                    Command::Tray => {
                        self.clients.set_window(client, false);
                        self.state_dirty = true;
                        self.dirty = true; // no meters for it
                    }
                    Command::RaiseWindow => self.clients.broadcast(&Event::Raise),
                    Command::SetProfile { device, index } => {
                        self.set_profile(&device, index);
                    }
                    Command::StartRecording { bus } => {
                        if let Err(e) = self.start_recording(&bus) {
                            let message = format!("Couldn't start recording: {e:#}");
                            self.clients.send_to(client, &Event::Error { message });
                        }
                        self.state_dirty = true;
                        self.dirty = true;
                        return;
                    }
                    Command::StopRecording { bus } => {
                        self.stop_recording(&bus);
                        self.state_dirty = true;
                        return;
                    }
                    Command::TestSpeakers { device } => {
                        self.start_speaker_test(client, &device);
                        return;
                    }
                    Command::SaveEqPreset { preset } => {
                        let name = preset.name.clone();
                        if !self.cfg.save_eq_preset(preset) {
                            let message = format!(
                                "\"{name}\" is the name of a built-in preset; pick another name"
                            );
                            self.clients.send_to(client, &Event::Error { message });
                            return;
                        }
                        info!("EQ preset \"{name}\" saved");
                    }
                    Command::DeleteEqPreset { name } => {
                        self.cfg.eq_presets.retain(|p| p.name != name);
                        info!("EQ preset \"{name}\" deleted");
                    }
                    Command::DeleteScene { name } => {
                        info!("scene \"{name}\" deleted");
                        self.cfg.delete_scene(&name);
                    }
                    Command::CancelLearn => {
                        self.learning = None;
                        self.state_dirty = true;
                        return;
                    }
                    Command::RemoveBinding { learn } => {
                        if let Some(midi) = &mut self.cfg.midi {
                            midi.bindings.retain(|b| {
                                !(b.target == learn.target && b.action == learn.action)
                            });
                        }
                    }
                    Command::SetConfig { config } => match config.validate() {
                        Ok(()) => {
                            info!("layout changed from the GUI");
                            self.cfg = config;
                            self.last_midi_poll = None;
                            self.refresh_feedback(true);
                        }
                        Err(e) => {
                            let message = format!("{e:#}");
                            self.clients.send_to(client, &Event::Error { message });
                            return;
                        }
                    },
                }
                self.state_changed();
                self.dirty = false;
                self.reconcile();
            }
        }
    }

    /// Run a surround upmixer for every speaker output that wants one, while its speakers
    /// are connected. PipeWire unloads an upmixer by itself when its speakers disappear;
    /// a new one is started when they come back.
    fn sync_upmixers(&mut self) {
        let want: HashMap<String, (String, Upmix, String)> = self
            .cfg
            .buses
            .iter()
            .filter_map(|b| match (&b.kind, b.upmix) {
                (_, Upmix::Off) | (Kind::Virtual, _) => None,
                (Kind::Physical { device }, mode) => {
                    Some((b.id.clone(), (device.clone(), mode, b.name.clone())))
                }
            })
            .collect();
        let mut stopping = Vec::new();
        self.upmixers.retain(|id, u| {
            if !u.is_alive() {
                info!("speakers for {id} went away; surround upmix resumes when they're back");
                return false;
            }
            let keep = want
                .get(id)
                .is_some_and(|(device, mode, _)| *device == u.device && *mode == u.mode);
            if !keep {
                stopping.push(id.clone());
            }
            keep
        });
        // Their nodes (and links) disappear with the module; don't remove those links again.
        for id in stopping {
            if let Some(n) = self.graph.node_by_name(&graph::upmix_in_node(&id)) {
                self.destroying.insert(n.id);
            }
        }
        for (id, (device, mode, name)) in want {
            if self.upmixers.contains_key(&id) {
                continue;
            }
            // Only while the speakers are there: PipeWire unloads an upmixer that has
            // nothing to play on. Their reappearance triggers another reconcile.
            // And only on surround speakers (a card can be switched to stereo).
            let Some(node) = self.graph.node_by_name(&device) else {
                continue;
            };
            if self.graph.ports_of(node.id, Dir::In, Some(false)).len() <= 2 {
                continue;
            }
            // Don't restart in a tight loop if PipeWire keeps unloading it.
            if self
                .upmix_started
                .get(&id)
                .is_some_and(|t| t.elapsed() < Duration::from_secs(2))
            {
                self.retry_upmix = true;
                continue;
            }
            self.upmix_started.insert(id.clone(), Instant::now());
            match Upmixer::start(&self.context, &id, &name, &device, mode) {
                Some(u) => {
                    info!("surround upmix ({mode:?}) for {id} on {device}");
                    self.upmixers.insert(id, u);
                }
                None => warn!("could not start the surround upmixer for {id}"),
            }
        }
    }

    /// Run an effects node for every channel with effects switched on, and keep its
    /// settings (and the fader it compensates for) up to date.
    fn sync_fx(&mut self) {
        let desired = graph::desired_nodes(&self.cfg, &self.graph);
        let mut want: HashMap<String, (Vec<String>, crate::config::Effects, f32, String)> = self
            .cfg
            .strips
            .iter()
            .filter(|s| s.fx.active())
            .filter_map(|s| {
                let name = graph::strip_node(&s.id);
                let d = desired.iter().find(|d| d.name == name)?;
                let channels = d.positions.split(',').map(str::to_string).collect();
                Some((
                    s.id.clone(),
                    (channels, s.fx.clone(), s.gain_db, s.name.clone()),
                ))
            })
            .collect();
        // Outputs too (they are always 7.1 inside).
        let surround: Vec<String> = graph::SURROUND.split(',').map(str::to_string).collect();
        for b in &self.cfg.buses {
            let own = b.own_fx();
            if own.active() {
                want.insert(
                    b.id.clone(),
                    (surround.clone(), own, b.gain_db, b.name.clone()),
                );
            }
            // The channels this output turns down are mixed through here first.
            if b.ducks() {
                want.insert(
                    graph::bus_duck_id(&b.id),
                    (surround.clone(), b.duck_fx(), 0.0, format!("{} (ducking)", b.name)),
                );
            }
        }
        let mut stopping = Vec::new();
        self.fx_nodes.retain(|id, n| {
            let keep = want.get(id).is_some_and(|(ch, ..)| *ch == n.channels);
            if !keep {
                stopping.push(id.clone());
            }
            keep
        });
        // Its links vanish with it; don't remove them a second time.
        for id in stopping {
            if let Some(n) = self.graph.node_by_name(&graph::fx_node(&id)) {
                self.destroying.insert(n.id);
            }
        }
        for (id, (channels, fx, gain_db, name)) in want {
            if let Some(n) = self.fx_nodes.get(&id) {
                n.set(&fx, gain_db);
                continue;
            }
            let node_name = graph::fx_node(&id);
            match crate::fx::FxNode::new(
                &self.core,
                &node_name,
                &format!("Mixer effects: {name}"),
                &channels,
                fx,
                gain_db,
            ) {
                Ok(n) => {
                    info!("effects on for {id}");
                    self.fx_nodes.insert(id, n);
                }
                Err(e) => warn!("could not start effects for {id}: {e:#}"),
            }
        }
    }

    /// Create/remove level meters so there is one per strip and bus while a GUI watches.
    fn sync_meters(&mut self) {
        let want: HashMap<String, String> = if self.clients.windows() == 0 {
            HashMap::new()
        } else {
            self.cfg
                .strips
                .iter()
                .map(|s| (graph::strip_node(&s.id), s.id.clone()))
                .chain(
                    self.cfg
                        .buses
                        .iter()
                        .map(|b| (graph::bus_node(&b.id), b.id.clone())),
                )
                .map(|(node, id)| (graph::meter_node(&node), id))
                .collect()
        };
        self.meters
            .retain(|name, (id, _)| want.get(name) == Some(id));
        for (name, id) in want {
            if self.meters.contains_key(&name) {
                continue;
            }
            match Meter::new(&self.core, &name, &format!("Level meter: {id}")) {
                Ok(m) => {
                    self.meters.insert(name, (id, m));
                }
                Err(e) => warn!("cannot create level meter for {id}: {e}"),
            }
        }
    }

    /// Start recording what an output plays, into the Music folder as FLAC.
    fn start_recording(&mut self, bus_id: &str) -> Result<()> {
        if self.recordings.contains_key(bus_id) {
            return Ok(());
        }
        let Some(bus) = self.cfg.buses.iter().find(|b| b.id == bus_id) else {
            anyhow::bail!("no output {bus_id:?}");
        };
        let dir = dirs::audio_dir()
            .or_else(|| dirs::home_dir().map(|h| h.join("Music")))
            .unwrap_or_else(std::env::temp_dir)
            .join("Mixrail");
        std::fs::create_dir_all(&dir)?;
        let stamp = std::process::Command::new("date")
            .arg("+%Y-%m-%d %H.%M.%S")
            .output()
            .ok()
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .unwrap_or_default();
        let clean = |s: &str| s.replace(['/', '"', '\\'], "");
        let path = dir.join(format!("{} {stamp}.flac", clean(&bus.name)));
        let props = format!(
            r#"{{ node.name = "{}" node.description = "Recording: {}" node.autoconnect = false node.dont-reconnect = true media.role = Production }}"#,
            graph::rec_node(bus_id),
            clean(&bus.name)
        );
        let child = std::process::Command::new("pw-record")
            .args([
                "--format",
                "s24",
                "--channels",
                "2",
                "--channel-map",
                "FL,FR",
                "-P",
                &props,
            ])
            .arg(&path)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .map_err(|e| anyhow::anyhow!("can't run pw-record (part of PipeWire's tools): {e}"))?;
        let started = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        info!("recording {bus_id} to {}", path.display());
        let info = RecordingInfo {
            bus: bus_id.to_string(),
            path: path.display().to_string(),
            started,
        };
        self.recordings
            .insert(bus_id.to_string(), Recording { child, info });
        Ok(())
    }

    /// Stop a recording; the recorder finishes the file on its own (not in our loop).
    fn stop_recording(&mut self, bus_id: &str) {
        if let Some(mut r) = self.recordings.remove(bus_id) {
            info!("recording of {bus_id} stopped: {}", r.info.path);
            // SIGINT lets pw-record close the file properly.
            let _ = std::process::Command::new("kill")
                .args(["-INT", &r.child.id().to_string()])
                .status();
            std::thread::spawn(move || {
                let _ = r.child.wait();
            });
        }
    }

    /// Notice recorders that stopped by themselves (e.g. a full disk).
    fn check_recordings(&mut self) {
        let ended: Vec<String> = self
            .recordings
            .iter_mut()
            .filter_map(|(id, r)| matches!(r.child.try_wait(), Ok(Some(_))).then(|| id.clone()))
            .collect();
        for id in ended {
            if let Some(r) = self.recordings.remove(&id) {
                warn!("recording of {id} ended unexpectedly: {}", r.info.path);
                let message = format!(
                    "The recording of an output stopped unexpectedly: {}",
                    r.info.path
                );
                self.clients.broadcast(&Event::Error { message });
                self.state_dirty = true;
            }
        }
    }

    /// Play a short noise burst on each speaker of a physical output, straight to the
    /// device (so it checks the speakers and their wiring, not the mix).
    fn start_speaker_test(&mut self, client: u64, device: &str) {
        // Any playback device will do (not only sound cards: also e.g. Bluetooth or
        // network speakers).
        let channels: Vec<String> = self
            .graph
            .node_by_name(device)
            .filter(|n| n.class == "Audio/Sink")
            .map(|n| {
                self.graph
                    .ports_of(n.id, Dir::In, None)
                    .iter()
                    .map(|p| p.channel.clone())
                    .collect()
            })
            .unwrap_or_default();
        if channels.is_empty() {
            let message = "That output's device isn't connected".to_string();
            self.clients.send_to(client, &Event::Error { message });
            return;
        }
        if self.speaker_test_running {
            return;
        }
        // The usual test order: front left, front right, centre, sub, rears, sides.
        let order = ["FL", "FR", "FC", "LFE", "RL", "RR", "SL", "SR"];
        let mut channels = channels;
        channels.sort_by_key(|c| order.iter().position(|o| o == c).unwrap_or(order.len()));
        self.speaker_test_running = true;
        let device = device.to_string();
        let tx = self.speaker_test_tx.clone();
        info!("speaker test on {device} ({})", channels.join(" "));
        std::thread::spawn(move || {
            for (i, ch) in channels.iter().enumerate() {
                let _ = tx.send((device.clone(), Some(ch.clone())));
                if let Err(e) = play_test_burst(&device, &channels, i) {
                    warn!("speaker test: {e:#}");
                    break;
                }
            }
            let _ = tx.send((device, None));
        });
    }

    fn send_meters(&mut self) {
        if self.clients.windows() == 0 || self.meters.is_empty() {
            return;
        }
        let levels = self
            .meters
            .values()
            .map(|(id, m)| (id.clone(), m.take()))
            .collect();
        let fx = self
            .fx_nodes
            .iter()
            .map(|(id, n)| {
                let s = n.stats();
                let status = ipc::FxStatus {
                    level_db: s.level_db,
                    gate_open: s.gate_open,
                    reduction_db: s.reduction_db,
                    limit_db: s.limit_db,
                    duck_db: s.duck_db,
                    duck_source_db: s.duck_source_db,
                    deess_db: s.deess_db,
                };
                (id.clone(), status)
            })
            .collect();
        self.clients.broadcast(&Event::Meters { levels, fx });
    }

    /// Something changed the mixer state (MIDI, desktop volume control): apply it,
    /// update LEDs and schedule saving it to the config file.
    fn state_changed(&mut self) {
        self.dirty = true;
        self.state_dirty = true;
        self.save_at = Some(Instant::now() + SAVE_DELAY);
        self.refresh_feedback(false);
    }

    pub fn flush_save(&mut self) {
        self.save_at = None;
        let Some(path) = &self.cfg_path else { return };
        match self.cfg.save(path) {
            // Remember our own write so it isn't treated as an outside edit.
            Ok(()) => self.cfg_mtime = std::fs::metadata(path).and_then(|m| m.modified()).ok(),
            Err(e) => warn!("could not save config: {e:#}"),
        }
    }

    fn poll_midi(&mut self) {
        let Some(device) = self.cfg.midi.as_ref().map(|m| m.device.clone()) else {
            self.controller = None;
            return;
        };
        if self.controller.as_ref().is_none_or(|c| c.device != device) {
            self.controller = Some(Controller::new(device));
        }
        let tx = self.midi_tx.clone();
        let controller = self.controller.as_mut().expect("set above");
        if controller.poll(move |cc| {
            let _ = tx.send(cc);
        }) {
            self.refresh_feedback(true);
            let now = Instant::now();
            self.led_resends = vec![now + Duration::from_secs(1), now + Duration::from_secs(3)];
        }
    }

    fn on_midi(&mut self, ev: MidiMsg) {
        if self.learning.is_some() {
            self.learn(ev);
            return;
        }
        let Some(midi) = &self.cfg.midi else { return };
        let actions: Vec<_> = midi
            .bindings
            .iter()
            .filter(|b| b.matches(ev.control, ev.channel))
            .map(|b| (b.target.clone(), b.action.clone(), b.mode, b.channel))
            .collect();
        if actions.is_empty() {
            debug!(
                "unmapped MIDI {:?} (channel {}) = {}",
                ev.control, ev.channel, ev.value
            );
            return;
        }
        let pressed = ev.value > 0;
        for (target, action, mode, channel) in actions {
            let latching = mode == Some(Mode::Latching);
            match action {
                Action::Gain => {
                    let Some((gain, _)) = self.cfg.level_mut(&target) else {
                        continue;
                    };
                    match mode {
                        Some(Mode::Relative | Mode::RelativeOffset) => {
                            let v = ev.value as i32;
                            let steps = if mode == Some(Mode::Relative) {
                                if v >= 64 { v - 128 } else { v }
                            } else {
                                v - 64
                            };
                            *gain = config::round_db(
                                (*gain + steps as f32 * 0.5)
                                    .clamp(config::MIN_GAIN_DB, config::MAX_GAIN_DB),
                            );
                        }
                        _ => {
                            *gain = config::control_to_db(ev.value, ev.control.max());
                            // The fader is already there: don't send its own position back
                            // (a motorised fader would fight the hand moving it).
                            let fb = gain_feedback(*gain, ev.control);
                            self.feedback_sent.insert((channel, ev.control), fb);
                        }
                    }
                }
                Action::Mute => {
                    if let Some((_, mute)) = self.cfg.level_mut(&target) {
                        // Momentary buttons toggle on press; latching ones say on/off themselves.
                        let new = match (latching, pressed) {
                            (true, p) => p,
                            (false, true) => !*mute,
                            (false, false) => continue,
                        };
                        if new != *mute {
                            *mute = new;
                            info!("{target}: {}", if new { "muted" } else { "unmuted" });
                        }
                    }
                }
                Action::Route { bus } => {
                    let current = self.cfg.is_routed(&target, &bus);
                    let new = match (latching, pressed) {
                        (true, p) => p,
                        (false, true) => !current,
                        (false, false) => continue,
                    };
                    if new != current {
                        self.cfg.set_routed(&target, &bus, new);
                        info!("{target} -> {bus}: {}", if new { "on" } else { "off" });
                    }
                }
                Action::Scene if pressed && self.cfg.recall_scene(&target) => {
                    info!("scene \"{target}\" recalled");
                }
                Action::Scene => {}
                Action::Cue => {
                    if let Some(s) = self.cfg.strips.iter_mut().find(|s| s.id == target) {
                        let new = match (latching, pressed) {
                            (true, p) => p,
                            (false, true) => !s.cue,
                            (false, false) => continue,
                        };
                        s.cue = new;
                    }
                }
                Action::Pan => {
                    let Some(s) = self.cfg.strips.iter_mut().find(|s| s.id == target) else {
                        continue;
                    };
                    let pan = match mode {
                        Some(Mode::Relative | Mode::RelativeOffset) => {
                            let v = ev.value as i32;
                            let steps = if mode == Some(Mode::Relative) {
                                if v >= 64 { v - 128 } else { v }
                            } else {
                                v - 64
                            };
                            s.pan + steps as f32 / 32.0
                        }
                        _ => {
                            let pos = ev.value as f32 / ev.control.max() as f32;
                            self.feedback_sent
                                .insert((channel, ev.control), knob_feedback(pos, ev.control));
                            pos * 2.0 - 1.0
                        }
                    };
                    // Snap to the middle, which a hardware knob can't hit exactly.
                    s.pan = if pan.abs() < 0.03 {
                        0.0
                    } else {
                        (pan.clamp(-1.0, 1.0) * 100.0).round() / 100.0
                    };
                }
                Action::Gate | Action::Comp => {
                    let is_gate = action == Action::Gate;
                    let Some(s) = self.cfg.strips.iter_mut().find(|s| s.id == target) else {
                        continue;
                    };
                    let current = if is_gate {
                        s.fx.gate.knob()
                    } else {
                        s.fx.comp.knob()
                    };
                    let pos = match mode {
                        Some(Mode::Relative | Mode::RelativeOffset) => {
                            let v = ev.value as i32;
                            let steps = if mode == Some(Mode::Relative) {
                                if v >= 64 { v - 128 } else { v }
                            } else {
                                v - 64
                            };
                            (current + steps as f32 / 64.0).clamp(0.0, 1.0)
                        }
                        _ => {
                            let pos = ev.value as f32 / ev.control.max() as f32;
                            // The knob is already there: don't send it its own position.
                            self.feedback_sent
                                .insert((channel, ev.control), knob_feedback(pos, ev.control));
                            pos
                        }
                    };
                    if is_gate {
                        s.fx.gate.set_knob(pos);
                    } else {
                        s.fx.comp.set_knob(pos);
                    }
                }
            }
        }
        self.state_changed();
        // Apply immediately rather than on the next tick, so faders feel responsive.
        self.dirty = false;
        self.reconcile();
    }

    /// Turn the control that just sent `ev` into a mapping for the function being learned.
    fn learn(&mut self, ev: MidiMsg) {
        let Some((lt, _)) = &self.learning else {
            return;
        };
        let usable = if lt.action.is_continuous() {
            // Faders and knobs send CC or pitch bend; a note is a button, not a fader.
            !matches!(ev.control, Control::Note(_))
        } else {
            // Buttons: wait for the press (ignore releases), CC or note.
            ev.value > 0 && ev.control != Control::PitchBend
        };
        if !usable {
            return;
        }
        let (lt, _) = self.learning.take().expect("checked above");
        let Some(midi) = &mut self.cfg.midi else {
            return;
        };
        // One control does one thing, and one thing is done by one control.
        midi.bindings.retain(|b| {
            let same_function = b.target == lt.target && b.action == lt.action;
            !b.matches(ev.control, ev.channel) && !same_function
        });
        let binding = Binding {
            control: ev.control,
            channel: Some(ev.channel),
            target: lt.target,
            action: lt.action,
            mode: None,
        };
        info!(
            "MIDI learn: {} -> {} {:?}",
            binding.control_label(),
            binding.target,
            binding.action
        );
        // Forget what this control showed, so it gets its new state.
        self.feedback_sent
            .remove(&(binding.channel, binding.control));
        midi.bindings.push(binding);
        self.state_changed();
    }

    /// Send controls their current state: button LEDs for mute/send/scene, and the
    /// position for faders (motorised faders and LED rings follow changes made
    /// elsewhere). With `force`, resend everything (after a reconnect the controller is
    /// in an unknown state).
    fn refresh_feedback(&mut self, force: bool) {
        let Some(midi) = self.cfg.midi.as_ref().filter(|m| m.feedback) else {
            return;
        };
        let Some(ctl) = self.controller.as_mut().filter(|c| c.is_connected()) else {
            return;
        };
        for b in &midi.bindings {
            let on = |on: bool| if on { b.control.max().min(127) } else { 0 };
            let value = match &b.action {
                Action::Gain => {
                    // Endless knobs have no position to send.
                    if matches!(b.mode, Some(Mode::Relative | Mode::RelativeOffset)) {
                        continue;
                    }
                    let Some((gain, _)) = self.cfg.level(&b.target) else {
                        continue;
                    };
                    gain_feedback(gain, b.control)
                }
                Action::Mute => on(self.cfg.level(&b.target).is_some_and(|(_, m)| m)),
                Action::Route { bus } => on(self.cfg.is_routed(&b.target, bus)),
                Action::Scene => on(self.cfg.scene_active(&b.target)),
                Action::Cue => on(self.cfg.strips.iter().any(|s| s.id == b.target && s.cue)),
                Action::Pan => {
                    if matches!(b.mode, Some(Mode::Relative | Mode::RelativeOffset)) {
                        continue;
                    }
                    let Some(s) = self.cfg.strips.iter().find(|s| s.id == b.target) else {
                        continue;
                    };
                    knob_feedback((s.pan + 1.0) / 2.0, b.control)
                }
                Action::Gate | Action::Comp => {
                    if matches!(b.mode, Some(Mode::Relative | Mode::RelativeOffset)) {
                        continue;
                    }
                    let Some(s) = self.cfg.strips.iter().find(|s| s.id == b.target) else {
                        continue;
                    };
                    let pos = if b.action == Action::Gate {
                        s.fx.gate.knob()
                    } else {
                        s.fx.comp.knob()
                    };
                    knob_feedback(pos, b.control)
                }
            };
            let before = self.feedback_sent.insert((b.channel, b.control), value);
            if force || before != Some(value) {
                ctl.send_feedback(b.channel, b.control, value);
            }
        }
        // A control whose mapping was removed shouldn't stay lit.
        let mapped: HashSet<(Option<u8>, Control)> = midi
            .bindings
            .iter()
            .map(|b| (b.channel, b.control))
            .collect();
        self.feedback_sent.retain(|&(channel, control), value| {
            if mapped.contains(&(channel, control)) {
                return true;
            }
            if *value > 0 && !matches!(control, Control::PitchBend) {
                ctl.send_feedback(channel, control, 0);
            }
            false
        });
    }

    /// The node reported its volume/mute. If that differs from what we set, someone else
    /// changed it (e.g. the desktop volume slider): adopt it so faders, LEDs and the
    /// config stay in step.
    fn on_node_props(&mut self, name: &str, volume: f32, mute: bool) {
        let Some(b) = self.bound.get_mut(name) else {
            return;
        };
        let Some((want_vol, _, want_mute)) = b.applied else {
            return;
        };
        let same = (volume - want_vol).abs() < 1e-3 && mute == want_mute;
        if !b.synced {
            b.synced = same;
            return;
        }
        if same {
            return;
        }
        let id = name
            .strip_prefix(graph::STRIP_PREFIX)
            .or_else(|| name.strip_prefix(graph::BUS_PREFIX));
        let Some(id) = id.map(str::to_string) else {
            return;
        };
        if let Some((gain, m)) = self.cfg.level_mut(&id) {
            let db = config::linear_to_db(volume);
            debug!("{id}: changed outside the mixer to {db:.1} dB, mute {mute}");
            *gain = (db * 10.0).round() / 10.0;
            *m = mute;
            b.applied = Some((volume, b.applied.map_or(0.0, |a| a.1), mute));
            self.state_changed();
        }
    }

    fn reload_config_if_changed(&mut self) {
        let Some(path) = &self.cfg_path else { return };
        let mtime = std::fs::metadata(path).and_then(|m| m.modified()).ok();
        if mtime == self.cfg_mtime {
            return;
        }
        self.cfg_mtime = mtime;
        match Config::load(path) {
            Ok(cfg) if cfg != self.cfg => {
                info!("config changed, applying");
                self.cfg = cfg;
                self.dirty = true;
                self.state_dirty = true;
                self.last_midi_poll = None;
                self.refresh_feedback(true);
            }
            Ok(_) => {}
            // Keep running with the last good config rather than tearing anything down.
            Err(e) => warn!("ignoring invalid config: {e:#}"),
        }
    }

    pub fn reconcile(&mut self) {
        self.refresh_devices();
        self.follow_devices();
        self.sync_meters();
        self.sync_upmixers();
        self.sync_fx();
        let desired = graph::desired_nodes(&self.cfg, &self.graph);
        let desired_names: HashSet<&str> = desired.iter().map(|d| d.name.as_str()).collect();

        // Remove our nodes that the config no longer has, or whose type changed.
        for node in self.graph.nodes.values() {
            if !graph::is_ours(&node.name) {
                continue;
            }
            let keep = desired
                .iter()
                .any(|d| d.name == node.name && matches(d, node, &self.graph));
            // Only remove duplicates/outdated ones; the canonical node is matched below.
            if !keep || self.graph.node_by_name(&node.name).map(|n| n.id) != Some(node.id) {
                if desired_names.contains(node.name.as_str()) {
                    info!("recreating {} (settings changed)", node.name);
                } else {
                    info!("removing {}", node.name);
                }
                if self.destroying.insert(node.id) {
                    self.registry.destroy_global(node.id);
                }
            }
        }
        // A node's links go away with it; don't remove or create links on such nodes.

        for d in &desired {
            match self.graph.node_by_name(&d.name) {
                Some(n) if matches(d, n, &self.graph) => {
                    let id = n.id;
                    self.pending_nodes.remove(&d.name);
                    self.ensure_volume(d, id);
                }
                Some(_) => {} // being destroyed above; will be recreated once it's gone
                None => {
                    if !self.pending_nodes.contains_key(&d.name) {
                        self.create_node(d);
                    }
                }
            }
        }

        // Links.
        let want = graph::desired_links(&self.cfg, &self.graph);
        let mut have = HashSet::new();
        let mut remove = Vec::new();
        for l in graph::managed_links(&self.cfg, &self.graph) {
            if self.destroying.contains(&l.id) || self.doomed(l.out_port) || self.doomed(l.in_port)
            {
                continue;
            }
            if want.contains(&(l.out_port, l.in_port)) && have.insert((l.out_port, l.in_port)) {
                continue;
            }
            remove.push(l.id);
        }
        for id in remove {
            debug!("removing link {id}");
            self.destroying.insert(id);
            self.registry.destroy_global(id);
        }
        for &(out_port, in_port) in want.difference(&have) {
            if self.pending_links.contains_key(&(out_port, in_port))
                || self.doomed(out_port)
                || self.doomed(in_port)
            {
                continue;
            }
            self.create_link(out_port, in_port);
        }

        self.apply_app_rules();
        self.refresh_apps();
    }

    /// Send new app streams to the strip their app's rule names.
    fn apply_app_rules(&mut self) {
        if self.cfg.rules != self.applied_rules {
            self.applied_rules = self.cfg.rules.clone();
            self.handled_streams.clear();
        }
        let pending: Vec<(u32, String)> = self
            .graph
            .nodes
            .values()
            .filter(|n| n.is_app_playback() && !self.handled_streams.contains(&n.id))
            .map(|n| {
                let rule = self
                    .cfg
                    .rules
                    .iter()
                    .find(|r| r.matches(&n.app_name, &n.app_binary));
                (
                    n.id,
                    rule.map(|r| graph::strip_node(&r.strip))
                        .unwrap_or_default(),
                )
            })
            .collect();
        for (stream, target) in pending {
            if target.is_empty() {
                self.handled_streams.insert(stream);
                continue;
            }
            // Wait until the strip exists (e.g. right after it was added).
            let Some(target_id) = self.graph.node_by_name(&target).map(|n| n.id) else {
                continue;
            };
            let already = self
                .graph
                .links
                .values()
                .any(|l| l.out_node == stream && l.in_node == target_id);
            if !already && !self.move_stream(stream, &target) {
                continue; // metadata not available yet; retry next time
            }
            self.handled_streams.insert(stream);
        }
    }

    /// Ask WirePlumber to (re)connect an app stream to `target_node`.
    fn move_stream(&mut self, stream: u32, target_node: &str) -> bool {
        if self.metadata.is_none() {
            let Some(global) = &self.metadata_global else {
                return false;
            };
            match self.registry.bind::<pw::metadata::Metadata, _>(global) {
                Ok(m) => self.metadata = Some(m),
                Err(e) => {
                    warn!("cannot use PipeWire metadata: {e}");
                    return false;
                }
            }
        }
        let name = self
            .graph
            .nodes
            .get(&stream)
            .map(app_key)
            .unwrap_or_default();
        debug!("moving stream {stream} ({name}) to {target_node}");
        let md = self.metadata.as_ref().expect("bound above");
        md.set_property(stream, "target.object", Some("Spa:Id"), Some(target_node));
        true
    }

    /// Which apps are playing, and into what — for the GUI.
    fn refresh_apps(&mut self) {
        let mut by_app: std::collections::BTreeMap<String, AppInfo> = Default::default();
        for n in self.graph.nodes.values().filter(|n| n.is_app_playback()) {
            let key = app_key(n);
            if key.is_empty() {
                continue;
            }
            let info = by_app.entry(key.clone()).or_insert_with(|| AppInfo {
                name: key,
                strips: vec![],
                other: vec![],
                streams: 0,
            });
            info.streams += 1;
            for l in self.graph.links.values().filter(|l| l.out_node == n.id) {
                let Some(dest) = self.graph.nodes.get(&l.in_node) else {
                    continue;
                };
                if let Some(id) = dest.name.strip_prefix(graph::STRIP_PREFIX) {
                    if !info.strips.iter().any(|s| s == id) {
                        info.strips.push(id.to_string());
                    }
                } else if !info.other.contains(&dest.description) {
                    info.other.push(dest.description.clone());
                }
            }
        }
        let apps: Vec<AppInfo> = by_app.into_values().collect();
        if apps != self.apps {
            self.apps = apps;
            self.state_dirty = true;
        }
    }

    /// True if a port is gone or belongs to a node that is being removed.
    fn doomed(&self, port: u32) -> bool {
        self.graph
            .ports
            .get(&port)
            .is_none_or(|p| self.destroying.contains(&p.node))
    }

    fn create_node(&mut self, d: &DesiredNode) {
        info!("creating {} ({}, \"{}\")", d.name, d.class, d.description);
        let props = properties! {
            "factory.name" => "support.null-audio-sink",
            "node.name" => d.name.as_str(),
            "node.description" => d.description.as_str(),
            "media.class" => d.class,
            "audio.position" => d.positions,
            // Make the node's volume also apply to what it passes on (verified necessary:
            // by default a sink's monitor ignores its volume).
            "monitor.channel-volumes" => "true",
            "node.virtual" => "true",
            // The config owns volume and routing; stop WirePlumber restoring old saved values.
            "state.restore-props" => "false",
            "state.restore-target" => "false",
            "object.linger" => "true",
            "lam.managed" => "true",
        };
        match self.core.create_object::<pw::node::Node>("adapter", &props) {
            // The proxy can go: with object.linger the node lives on in PipeWire,
            // and we bind to it again when its global appears.
            Ok(_proxy) => {
                self.pending_nodes.insert(d.name.clone(), Instant::now());
            }
            Err(e) => warn!("failed to create {}: {e}", d.name),
        }
    }

    fn create_link(&mut self, out_port: u32, in_port: u32) {
        let (Some(op), Some(ip)) = (
            self.graph.ports.get(&out_port),
            self.graph.ports.get(&in_port),
        ) else {
            return;
        };
        debug!("linking {out_port} -> {in_port}");
        let props = properties! {
            "link.output.node" => op.node.to_string(),
            "link.output.port" => out_port.to_string(),
            "link.input.node" => ip.node.to_string(),
            "link.input.port" => in_port.to_string(),
            "object.linger" => "true",
            "lam.managed" => "true",
        };
        match self
            .core
            .create_object::<pw::link::Link>("link-factory", &props)
        {
            Ok(_proxy) => {
                self.pending_links
                    .insert((out_port, in_port), Instant::now());
            }
            Err(e) => warn!("failed to link {out_port} -> {in_port}: {e}"),
        }
    }

    fn ensure_volume(&mut self, d: &DesiredNode, id: u32) {
        if self.bound.get(&d.name).is_none_or(|b| b.id != id) {
            let Some(global) = self.our_globals.get(&id) else {
                return;
            };
            match self.registry.bind::<pw::node::Node, _>(global) {
                Ok(proxy) => {
                    let me = self.me.clone();
                    let name = d.name.clone();
                    let listener = proxy
                        .add_listener_local()
                        .param(move |_seq, id, _index, _next, pod| {
                            if id != spa::param::ParamType::Props {
                                return;
                            }
                            let Some((volume, mute)) = pod.and_then(parse_volume) else {
                                return;
                            };
                            if let Some(e) = me.upgrade()
                                && let Ok(mut e) = e.try_borrow_mut()
                            {
                                e.on_node_props(&name, volume, mute);
                            }
                        })
                        .register();
                    proxy.subscribe_params(&[spa::param::ParamType::Props]);
                    self.bound.insert(
                        d.name.clone(),
                        BoundNode {
                            id,
                            proxy,
                            _listener: listener,
                            applied: None,
                            synced: false,
                        },
                    );
                }
                Err(e) => {
                    warn!("cannot bind {}: {e}", d.name);
                    return;
                }
            }
        }
        let b = self.bound.get_mut(&d.name).expect("bound above");
        let want = (d.volume, d.pan, d.mute);
        if b.applied == Some(want) {
            return;
        }
        let positions: Vec<&str> = d.positions.split(',').collect();
        let volumes: Vec<f32> = config::pan_gains(d.pan, &positions)
            .iter()
            .map(|g| g * d.volume)
            .collect();
        match volume_pod(&volumes, d.mute) {
            Ok(bytes) => {
                let pod = spa::pod::Pod::from_bytes(&bytes).expect("valid pod");
                b.proxy.set_param(spa::param::ParamType::Props, 0, pod);
                b.applied = Some(want);
                b.synced = false;
                debug!("{}: volume {:.3} mute {}", d.name, d.volume, d.mute);
            }
            Err(e) => warn!("building volume for {}: {e}", d.name),
        }
    }
}

/// A running pw-record child and what the GUI shows about it.
struct Recording {
    child: std::process::Child,
    info: RecordingInfo,
}

/// PipeWire's loopback module turning a stereo bus into surround: its input
/// (`lam_upin_<bus>`) is fed by the bus, its stereo output is played on the speakers by
/// WirePlumber, and PipeWire's channel mixer upmixes on the way in.
struct Upmixer {
    device: String,
    mode: Upmix,
    module: std::ptr::NonNull<pw::sys::pw_impl_module>,
    /// Cleared by PipeWire's "destroy" event when the module unloads itself.
    alive: Box<std::cell::Cell<bool>>,
    _hook: Box<pw::spa::sys::spa_hook>,
}

static UPMIXER_EVENTS: pw::sys::pw_impl_module_events = pw::sys::pw_impl_module_events {
    version: pw::sys::PW_VERSION_IMPL_MODULE_EVENTS,
    destroy: Some(upmixer_destroyed),
    free: None,
    initialized: None,
    registered: None,
};

unsafe extern "C" fn upmixer_destroyed(data: *mut std::ffi::c_void) {
    // SAFETY: `data` is the `alive` cell of the Upmixer that registered this listener;
    // it lives as long as the Upmixer, which outlives the module or is told it died.
    unsafe { (*(data as *const std::cell::Cell<bool>)).set(false) };
}

impl Upmixer {
    fn start(
        context: &pw::context::ContextRc,
        bus_id: &str,
        bus_name: &str,
        device: &str,
        mode: Upmix,
    ) -> Option<Self> {
        let quote = |s: &str| s.replace(['"', '\\'], "");
        let method = if mode == Upmix::All { "simple" } else { "psd" };
        let args = format!(
            r#"{{
                node.description = "Mixer surround: {name}"
                audio.position = [ FL FR ]
                capture.props = {{
                    node.name = "{upin}"
                    node.autoconnect = false
                    node.dont-reconnect = true
                    state.restore-props = false
                    state.restore-target = false
                }}
                playback.props = {{
                    node.name = "lam_upmix_{id}"
                    target.object = "{device}"
                    # Never fall back to another output (e.g. the default, which may be
                    # one of our own strips): wait for these speakers instead.
                    node.dont-fallback = true
                    state.restore-props = false
                    state.restore-target = false
                    channelmix.upmix = true
                    channelmix.upmix-method = {method}
                    channelmix.lfe-cutoff = 120
                    channelmix.fc-cutoff = 12000
                    channelmix.rear-delay = 12.0
                }}
            }}"#,
            name = quote(bus_name),
            upin = crate::graph::upmix_in_node(bus_id),
            id = bus_id,
            device = quote(device),
        );
        let name = std::ffi::CString::new("libpipewire-module-loopback").ok()?;
        let args = std::ffi::CString::new(args).ok()?;
        // SAFETY: the context outlives the module (upmixers are dropped before the
        // context at shutdown, see `run`), and the strings live across the call.
        let module = unsafe {
            pw::sys::pw_context_load_module(
                context.as_raw_ptr(),
                name.as_ptr(),
                args.as_ptr(),
                std::ptr::null_mut(),
            )
        };
        let module = std::ptr::NonNull::new(module)?;
        let alive = Box::new(std::cell::Cell::new(true));
        // SAFETY: a zeroed spa_hook is its valid initial state.
        let mut hook: Box<pw::spa::sys::spa_hook> = Box::new(unsafe { std::mem::zeroed() });
        // SAFETY: module is live; hook and alive are boxed (stable addresses) and owned by
        // the Upmixer for at least as long as the module can emit events to them.
        unsafe {
            pw::sys::pw_impl_module_add_listener(
                module.as_ptr(),
                &mut *hook,
                &UPMIXER_EVENTS,
                &*alive as *const std::cell::Cell<bool> as *mut std::ffi::c_void,
            );
        }
        Some(Upmixer {
            device: device.to_string(),
            mode,
            module,
            alive,
            _hook: hook,
        })
    }

    fn is_alive(&self) -> bool {
        self.alive.get()
    }
}

impl Drop for Upmixer {
    fn drop(&mut self) {
        // If PipeWire already unloaded it, it's gone: destroying it again would be a
        // double free.
        if self.alive.get() {
            // SAFETY: created by pw_context_load_module and still alive.
            unsafe { pw::sys::pw_impl_module_destroy(self.module.as_ptr()) };
        }
    }
}

/// Play ~1 s of soft noise on channel `index` of `device` (all other channels silent).
fn play_test_burst(device: &str, channels: &[String], index: usize) -> Result<()> {
    const RATE: usize = 48000;
    let n = channels.len();
    let len = RATE * 9 / 10;
    let fade = RATE / 50;
    let mut data = Vec::with_capacity(len * n * 2);
    let mut seed: u32 = 0x1234_5678;
    for i in 0..len {
        // White noise at about -18 dBFS with short fades, so there are no clicks.
        seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        let white = (seed >> 16) as f32 / 32768.0 - 1.0;
        let env = (i.min(len - 1 - i) as f32 / fade as f32).min(1.0);
        let sample = (white * 0.125 * env * 32767.0) as i16;
        for c in 0..n {
            let v = if c == index { sample } else { 0 };
            data.extend_from_slice(&v.to_le_bytes());
        }
    }
    let path = std::env::temp_dir().join(format!("lam-speaker-test-{}.raw", std::process::id()));
    std::fs::write(&path, &data)?;
    let status = std::process::Command::new("pw-cat")
        .args([
            "--playback",
            "--raw",
            "--target",
            device,
            "--rate",
            "48000",
            "--format",
            "s16",
        ])
        .args([
            "--channels",
            &n.to_string(),
            "--channel-map",
            &channels.join(","),
        ])
        .args(["-P", "{ node.dont-fallback = true media.role = Test }"])
        .arg(&path)
        .status();
    let _ = std::fs::remove_file(&path);
    match status {
        Ok(s) if s.success() => Ok(()),
        Ok(s) => anyhow::bail!("pw-cat exited with {s}"),
        Err(e) => anyhow::bail!("can't run pw-cat (part of PipeWire's tools): {e}"),
    }
}

/// Value to send a knob so it shows position `pos` (0..1).
fn knob_feedback(pos: f32, control: Control) -> u16 {
    (pos.clamp(0.0, 1.0) * control.max() as f32).round() as u16
}

/// Value to send a fader/knob so it shows `gain_db`.
fn gain_feedback(gain_db: f32, control: Control) -> u16 {
    (config::db_to_pos(gain_db) * control.max() as f32).round() as u16
}

/// The name an app is known by: what it calls itself, else its program name.
fn app_key(n: &Node) -> String {
    if n.app_name.is_empty() {
        n.app_binary.clone()
    } else {
        n.app_name.clone()
    }
}

/// Whether an existing node already has the settings we want (else it gets recreated).
fn matches(d: &DesiredNode, n: &Node, g: &Graph) -> bool {
    // An empty description means PipeWire didn't report one; don't recreate over that.
    let same_text =
        d.class == n.class && (n.description.is_empty() || d.description == n.description);
    // Channel layout (e.g. stereo -> 7.1): compare input port counts once they're known.
    let ports = g.ports_of(n.id, Dir::In, None).len();
    let same_layout = ports == 0 || ports == d.positions.split(',').count();
    same_text && same_layout
}

/// Read (volume, mute) out of a node's Props param.
fn parse_volume(pod: &spa::pod::Pod) -> Option<(f32, bool)> {
    use spa::pod::{Value, ValueArray, deserialize::PodDeserializer};
    let (_, Value::Object(obj)) = PodDeserializer::deserialize_any_from(pod.as_bytes()).ok()?
    else {
        return None;
    };
    let mut volume = None;
    let mut mute = None;
    for p in obj.properties {
        match (p.key, p.value) {
            (spa::sys::SPA_PROP_channelVolumes, Value::ValueArray(ValueArray::Float(v))) => {
                volume = v.into_iter().reduce(f32::max);
            }
            (spa::sys::SPA_PROP_mute, Value::Bool(m)) => mute = Some(m),
            _ => {}
        }
    }
    Some((volume?, mute?))
}

/// ALSA device names are "alsa_output.<card>.<profile>" and the card is
/// "alsa_card.<card>", so a device not seen this run can still be traced to its card.
fn alsa_card_name(device: &str) -> Option<String> {
    let rest = device
        .strip_prefix("alsa_output.")
        .or_else(|| device.strip_prefix("alsa_input."))?;
    Some(format!("alsa_card.{}", rest.rsplit_once('.')?.0))
}

/// The card's single output (or input) device, if it has exactly one.
fn card_device(g: &Graph, card: u32, output: bool) -> Option<String> {
    let class = if output { "Audio/Sink" } else { "Audio/Source" };
    let mut found = g
        .nodes
        .values()
        .filter(|n| n.hardware && n.card == Some(card) && n.class == class);
    let first = found.next()?;
    found.next().is_none().then(|| first.name.clone())
}

/// Read a sound card profile (from EnumProfile or Profile).
fn parse_profile(pod: &spa::pod::Pod) -> Option<ipc::Profile> {
    use spa::pod::{Value, deserialize::PodDeserializer};
    let (_, Value::Object(obj)) = PodDeserializer::deserialize_any_from(pod.as_bytes()).ok()?
    else {
        return None;
    };
    profile_from_props(obj.properties.into_iter().map(|p| (p.key, p.value)))
}

fn profile_from_props(
    props: impl Iterator<Item = (u32, spa::pod::Value)>,
) -> Option<ipc::Profile> {
    use spa::pod::Value;
    fn has(v: &Value, text: &str) -> bool {
        match v {
            Value::String(s) => s == text,
            Value::Struct(items) => items.iter().any(|i| has(i, text)),
            _ => false,
        }
    }
    let mut p = ipc::Profile {
        index: u32::MAX,
        description: String::new(),
        available: true,
        outputs: false,
        inputs: false,
    };
    for (key, value) in props {
        match (key, value) {
            (spa::sys::SPA_PARAM_PROFILE_index, Value::Int(i)) => p.index = i as u32,
            (spa::sys::SPA_PARAM_PROFILE_description, Value::String(d)) => p.description = d,
            // SPA_PARAM_AVAILABILITY_no
            (spa::sys::SPA_PARAM_PROFILE_available, Value::Id(a)) => p.available = a.0 != 1,
            (spa::sys::SPA_PARAM_PROFILE_classes, v) => {
                p.outputs = has(&v, "Audio/Sink");
                p.inputs = has(&v, "Audio/Source");
            }
            _ => {}
        }
    }
    (p.index != u32::MAX).then_some(p)
}

/// Asks a card to switch profile, and WirePlumber to remember it.
fn profile_pod(index: u32) -> Result<Vec<u8>> {
    use spa::pod::{Object, Property, Value, serialize::PodSerializer};
    let obj = Value::Object(Object {
        type_: spa::sys::SPA_TYPE_OBJECT_ParamProfile,
        id: spa::sys::SPA_PARAM_Profile,
        properties: vec![
            Property::new(spa::sys::SPA_PARAM_PROFILE_index, Value::Int(index as i32)),
            Property::new(spa::sys::SPA_PARAM_PROFILE_save, Value::Bool(true)),
        ],
    });
    let (cursor, _) = PodSerializer::serialize(std::io::Cursor::new(Vec::new()), &obj)
        .map_err(|e| anyhow::anyhow!("{e:?}"))?;
    Ok(cursor.into_inner())
}

/// Props with one volume per channel.
fn volume_pod(volumes: &[f32], mute: bool) -> Result<Vec<u8>> {
    use spa::pod::{Object, Property, Value, ValueArray, serialize::PodSerializer};
    let obj = Value::Object(Object {
        type_: spa::sys::SPA_TYPE_OBJECT_Props,
        id: spa::sys::SPA_PARAM_Props,
        properties: vec![
            Property::new(
                spa::sys::SPA_PROP_channelVolumes,
                Value::ValueArray(ValueArray::Float(volumes.to_vec())),
            ),
            Property::new(spa::sys::SPA_PROP_mute, Value::Bool(mute)),
        ],
    });
    let (cursor, _) = PodSerializer::serialize(std::io::Cursor::new(Vec::new()), &obj)
        .map_err(|e| anyhow::anyhow!("{e:?}"))?;
    Ok(cursor.into_inner())
}

/// Connect to PipeWire and run the engine until `stop_after_settle` (if given) elapses,
/// or until SIGINT/SIGTERM.
pub fn run(
    cfg: Config,
    cfg_path: Option<PathBuf>,
    stop_after_settle: Option<Duration>,
    serve_gui: bool,
) -> Result<()> {
    pw::init();
    let mainloop = pw::main_loop::MainLoopRc::new(None)?;
    let context = pw::context::ContextRc::new(&mainloop, None)?;
    let core = context.connect_rc(None)?;
    let registry = core.get_registry_rc()?;

    let cfg_mtime = cfg_path
        .as_ref()
        .and_then(|p| std::fs::metadata(p).and_then(|m| m.modified()).ok());
    let (midi_tx, midi_rx) = pw::channel::channel::<MidiMsg>();
    let (speaker_test_tx, speaker_test_rx) = pw::channel::channel::<(String, Option<String>)>();
    let engine = Rc::new_cyclic(|me| {
        RefCell::new(Engine {
            cfg,
            cfg_path,
            cfg_mtime,
            last_cfg_check: Instant::now(),
            graph: Graph::default(),
            our_globals: HashMap::new(),
            bound: HashMap::new(),
            pending_nodes: HashMap::new(),
            pending_links: HashMap::new(),
            destroying: HashSet::new(),
            dirty: false,
            save_at: None,
            controller: None,
            last_midi_poll: None,
            midi_tx,
            feedback_sent: HashMap::new(),
            led_resends: Vec::new(),
            clients: Clients::default(),
            state_dirty: false,
            devices: Vec::new(),
            midi_was_connected: false,
            midi_ports: Vec::new(),
            metadata_global: None,
            metadata: None,
            handled_streams: HashSet::new(),
            applied_rules: Vec::new(),
            apps: Vec::new(),
            learning: None,
            meters: HashMap::new(),
            upmixers: HashMap::new(),
            upmix_started: HashMap::new(),
            fx_nodes: HashMap::new(),
            recordings: HashMap::new(),
            cards: HashMap::new(),
            device_cards: HashMap::new(),
            retry_upmix: false,
            speaker_test_tx,
            speaker_test_running: false,
            me: me.clone(),
            context: context.clone(),
            core: core.clone(),
            registry: registry.clone(),
        })
    });

    let _reg_listener = registry
        .add_listener_local()
        .global({
            let e = engine.clone();
            move |g| e.borrow_mut().on_global(g)
        })
        .global_remove({
            let e = engine.clone();
            move |id| e.borrow_mut().on_global_remove(id)
        })
        .register();

    // If PipeWire goes away we exit with an error; systemd restarts us and we re-adopt.
    let _core_listener = core
        .add_listener_local()
        .error({
            let ml = mainloop.clone();
            move |id, _seq, res, msg| {
                warn!("pipewire error on object {id}: {msg} ({res})");
                if id == pw::core::PW_ID_CORE {
                    ml.quit();
                }
            }
        })
        .register();

    // MIDI arrives on its own thread; hand it to the main loop.
    let _midi_rx = midi_rx.attach(mainloop.loop_(), {
        let e = engine.clone();
        move |msg| e.borrow_mut().on_midi(msg)
    });

    // GUI connections, also from their own threads.
    let (ipc_tx, ipc_rx) = pw::channel::channel::<Incoming>();
    let _speaker_test_rx = speaker_test_rx.attach(mainloop.loop_(), {
        let e = engine.clone();
        move |(device, channel)| {
            let mut e = e.borrow_mut();
            e.speaker_test_running = channel.is_some();
            e.clients.broadcast(&Event::SpeakerTest { device, channel });
        }
    });
    if serve_gui {
        let ipc_tx = std::sync::Arc::new(std::sync::Mutex::new(ipc_tx));
        ipc::serve(move |msg| {
            if let Ok(tx) = ipc_tx.lock() {
                let _ = tx.send(msg);
            }
        })?;
    }
    let _ipc_rx = ipc_rx.attach(mainloop.loop_(), {
        let e = engine.clone();
        move |msg| e.borrow_mut().on_ipc(msg)
    });
    let meter_timer = mainloop.loop_().add_timer({
        let e = engine.clone();
        move |_| e.borrow_mut().send_meters()
    });
    let meter_interval = Duration::from_millis(33);
    meter_timer
        .update_timer(Some(meter_interval), Some(meter_interval))
        .into_result()?;

    let started = Instant::now();
    let timer = mainloop.loop_().add_timer({
        let e = engine.clone();
        let ml = mainloop.clone();
        move |_| {
            e.borrow_mut().tick();
            if stop_after_settle.is_some_and(|d| started.elapsed() >= d) {
                ml.quit();
            }
        }
    });
    timer.update_timer(Some(TICK), Some(TICK)).into_result()?;

    let quit = |ml: pw::main_loop::MainLoopRc| move || ml.quit();
    let _sigint = mainloop
        .loop_()
        .add_signal_local(pw::loop_::Signal::INT, quit(mainloop.clone()));
    let _sigterm = mainloop
        .loop_()
        .add_signal_local(pw::loop_::Signal::TERM, quit(mainloop.clone()));

    info!("mixer engine running");
    mainloop.run();
    // Modules must go before the PipeWire context does.
    engine.borrow_mut().upmixers.clear();
    engine.borrow_mut().fx_nodes.clear();
    let buses: Vec<String> = engine.borrow().recordings.keys().cloned().collect();
    for bus in buses {
        engine.borrow_mut().stop_recording(&bus);
    }
    if engine.borrow().save_at.is_some() {
        engine.borrow_mut().flush_save();
    }
    info!("mixer engine stopped (virtual devices and routing stay active)");
    Ok(())
}

/// Snapshot of hardware audio devices, for picking `device = "..."` values.
pub fn list_devices() -> Result<Vec<(String, String, String)>> {
    pw::init();
    let mainloop = pw::main_loop::MainLoopRc::new(None)?;
    let context = pw::context::ContextRc::new(&mainloop, None)?;
    let core = context.connect_rc(None)?;
    let registry = core.get_registry_rc()?;

    let found = Rc::new(RefCell::new(Vec::new()));
    let _l = registry
        .add_listener_local()
        .global({
            let found = found.clone();
            move |g| {
                let Some(p) = g.props else { return };
                let class = p.get("media.class").unwrap_or_default();
                let is_hw = p.get("device.id").is_some()
                    || p.get("object.path").is_some_and(|s| s.starts_with("alsa:"));
                if g.type_ == ObjectType::Node
                    && is_hw
                    && (class == "Audio/Sink" || class == "Audio/Source")
                {
                    found.borrow_mut().push((
                        class.to_string(),
                        p.get("node.name").unwrap_or_default().to_string(),
                        p.get("node.description").unwrap_or_default().to_string(),
                    ));
                }
            }
        })
        .register();

    let done = core.sync(0)?;
    let _c = core
        .add_listener_local()
        .done({
            let ml = mainloop.clone();
            move |id, seq| {
                if id == pw::core::PW_ID_CORE && seq == done {
                    ml.quit();
                }
            }
        })
        .register();
    mainloop.run();
    let mut v = found.take();
    v.sort();
    Ok(v)
}

#[cfg(test)]
mod tests {
    use super::*;
    use spa::pod::Value;

    #[test]
    fn reads_profiles() {
        // What an ALSA card reports for a 5.1 profile (classes nested as PipeWire does).
        let classes = Value::Struct(vec![
            Value::Int(1),
            Value::Struct(vec![
                Value::String("Audio/Sink".into()),
                Value::Int(1),
                Value::String("card.profile.devices".into()),
            ]),
        ]);
        let p = profile_from_props(
            [
                (spa::sys::SPA_PARAM_PROFILE_index, Value::Int(16)),
                (
                    spa::sys::SPA_PARAM_PROFILE_description,
                    Value::String("Digital Surround 5.1 (IEC958/AC3) Output".into()),
                ),
                (
                    spa::sys::SPA_PARAM_PROFILE_available,
                    Value::Id(spa::utils::Id(2)),
                ),
                (spa::sys::SPA_PARAM_PROFILE_classes, classes),
            ]
            .into_iter(),
        )
        .unwrap();
        assert_eq!(p.index, 16);
        assert!(p.available && p.outputs && !p.inputs);

        let off = profile_from_props(
            [
                (spa::sys::SPA_PARAM_PROFILE_index, Value::Int(0)),
                (
                    spa::sys::SPA_PARAM_PROFILE_available,
                    Value::Id(spa::utils::Id(1)),
                ),
            ]
            .into_iter(),
        )
        .unwrap();
        assert!(!off.available && !off.outputs);
        assert!(profile_from_props(std::iter::empty()).is_none());

        // The switch request round-trips through the parser.
        let bytes = profile_pod(5).unwrap();
        let pod = spa::pod::Pod::from_bytes(&bytes).unwrap();
        assert_eq!(parse_profile(pod).unwrap().index, 5);
    }

    #[test]
    fn traces_devices_to_cards() {
        assert_eq!(
            alsa_card_name("alsa_output.pci-0000_36_00.4.iec958-ac3-surround-51").as_deref(),
            Some("alsa_card.pci-0000_36_00.4")
        );
        assert_eq!(
            alsa_card_name("alsa_input.usb-Mic_123-00.analog-stereo").as_deref(),
            Some("alsa_card.usb-Mic_123-00")
        );
        assert_eq!(alsa_card_name("bluez_output.AA_BB.1"), None);

        let mut g = Graph::default();
        let mut add = |id, name: &str, class: &str, card| {
            g.nodes.insert(
                id,
                Node {
                    id,
                    name: name.into(),
                    class: class.into(),
                    hardware: true,
                    card: Some(card),
                    ..Default::default()
                },
            );
        };
        add(1, "alsa_output.a.analog-stereo", "Audio/Sink", 10);
        add(2, "alsa_input.a.analog-stereo", "Audio/Source", 10);
        add(3, "alsa_output.b.hdmi-stereo", "Audio/Sink", 20);
        add(4, "alsa_output.b.hdmi-stereo-extra1", "Audio/Sink", 20);
        assert_eq!(
            card_device(&g, 10, true).as_deref(),
            Some("alsa_output.a.analog-stereo")
        );
        assert_eq!(
            card_device(&g, 10, false).as_deref(),
            Some("alsa_input.a.analog-stereo")
        );
        // Two outputs: ambiguous, don't guess.
        assert_eq!(card_device(&g, 20, true), None);
        assert_eq!(card_device(&g, 30, true), None);
    }
}
