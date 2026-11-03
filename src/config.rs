//! The mixer layout: which input strips and output buses exist, and how they are routed.
//!
//! Everything here is user-editable. Nothing in the engine assumes a fixed number of
//! strips or buses — the config is the single source of truth.

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// Gain range a fader can take, in dB.
pub const MIN_GAIN_DB: f32 = -60.0;
pub const MAX_GAIN_DB: f32 = 12.0;

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Config {
    #[serde(default, rename = "strip")]
    pub strips: Vec<Strip>,
    #[serde(default, rename = "bus")]
    pub buses: Vec<Bus>,
    /// MIDI controller mapping (optional).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub midi: Option<Midi>,
    /// Which app plays into which strip.
    #[serde(default, rename = "rule", skip_serializing_if = "Vec::is_empty")]
    pub rules: Vec<Rule>,
    /// Saved mixes to switch between.
    #[serde(default, rename = "scene", skip_serializing_if = "Vec::is_empty")]
    pub scenes: Vec<Scene>,
    /// EQ presets the user saved (shared by all channels).
    #[serde(default, rename = "eq_preset", skip_serializing_if = "Vec::is_empty")]
    pub eq_presets: Vec<EqPreset>,
    /// The output that plays cued channels (usually headphones).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cue_bus: Option<String>,
}

/// A saved EQ: its points and the low cut.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EqPreset {
    pub name: String,
    #[serde(default)]
    pub low_cut: bool,
    #[serde(default = "default_low_cut_hz")]
    pub low_cut_hz: f32,
    #[serde(default)]
    pub bands: Vec<EqBand>,
}

fn default_low_cut_hz() -> f32 {
    80.0
}

impl EqPreset {
    /// Save a channel's current EQ and low cut.
    pub fn from_effects(name: &str, fx: &Effects) -> Self {
        EqPreset {
            name: name.trim().to_string(),
            low_cut: fx.low_cut,
            low_cut_hz: fx.low_cut_hz,
            bands: fx.eq.bands.clone(),
        }
    }

    /// Put this preset on a channel (switching its EQ on).
    pub fn apply(&self, fx: &mut Effects) {
        fx.eq = Eq {
            enabled: true,
            bands: self.bands.clone(),
        };
        fx.low_cut = self.low_cut;
        fx.low_cut_hz = self.low_cut_hz;
    }
}

/// A saved mix: gain, mute and sends of every strip and bus. Recalling it doesn't add or
/// remove channels, so apps keep playing where they are.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Scene {
    pub name: String,
    /// Settings per strip/bus id. Ids that no longer exist are ignored.
    #[serde(default)]
    pub levels: std::collections::BTreeMap<String, SceneLevel>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SceneLevel {
    pub gain_db: f32,
    pub mute: bool,
    /// Sends (strips only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub buses: Option<Vec<String>>,
}

/// Send every audio stream of an app to a strip, including ones it opens later
/// (e.g. each browser tab).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Rule {
    /// App name as the app reports it (e.g. "Brave", "Spotify"), or its program name.
    /// Case-insensitive; `*` matches anything (e.g. "brave*").
    pub app: String,
    /// Id of the (virtual) strip it goes to.
    pub strip: String,
}

impl Rule {
    pub fn matches(&self, app_name: &str, binary: &str) -> bool {
        [app_name, binary]
            .iter()
            .any(|v| !v.is_empty() && glob_match(&self.app.to_lowercase(), &v.to_lowercase()))
    }
}

/// Minimal glob: `*` matches any run of characters.
fn glob_match(pattern: &str, text: &str) -> bool {
    match pattern.split_once('*') {
        None => pattern == text,
        Some((head, rest)) => {
            let Some(mut text) = text.strip_prefix(head) else {
                return false;
            };
            if rest.is_empty() {
                return true;
            }
            loop {
                if glob_match(rest, text) {
                    return true;
                }
                let mut chars = text.chars();
                if chars.next().is_none() {
                    return false;
                }
                text = chars.as_str();
            }
        }
    }
}

/// A MIDI controller and what its knobs, faders and buttons do.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Midi {
    /// Part of the controller's MIDI port name, e.g. "nanoKONTROL2".
    pub device: String,
    /// Send LED feedback to the controller (turn off for controllers without LEDs).
    #[serde(default = "yes", skip_serializing_if = "is_true")]
    pub feedback: bool,
    #[serde(default, rename = "binding")]
    pub bindings: Vec<Binding>,
}

/// One control on the controller bound to one action.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(try_from = "BindingToml", into = "BindingToml")]
pub struct Binding {
    /// What the control sends.
    pub control: Control,
    /// MIDI channel 1-16; None accepts any channel.
    pub channel: Option<u8>,
    /// Id of the strip or bus this control acts on (scene name for scene buttons).
    pub target: String,
    pub action: Action,
    /// How the control behaves; None means the usual for its action.
    pub mode: Option<Mode>,
}

/// The kind of MIDI message a control sends. Controllers differ: most knobs and faders
/// send Control Change (CC), many buttons and pads send notes, and Mackie-style
/// controllers (often with motorised faders) send high-resolution pitch bend.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Control {
    Cc(u8),
    Note(u8),
    /// One fader per MIDI channel, 14-bit.
    PitchBend,
}

impl Control {
    /// Largest value the control sends.
    pub fn max(self) -> u16 {
        match self {
            Control::PitchBend => 16383,
            _ => 127,
        }
    }
}

/// How a control behaves.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Mode {
    /// Faders/knobs: the position sets the gain (default).
    Absolute,
    /// Endless knobs sending +n/-n as 1..63 / 127..65.
    Relative,
    /// Endless knobs sending 64+n / 64-n.
    RelativeOffset,
    /// Buttons: every press toggles (default).
    Momentary,
    /// Buttons that send on/off themselves: on means muted/sent, off means not.
    Latching,
}

impl Binding {
    pub fn matches(&self, control: Control, channel: u8) -> bool {
        self.control == control && self.channel.is_none_or(|c| c == channel)
    }

    /// "CC 7", "Note 36 · ch 2", "Pitch bend · ch 1".
    pub fn control_label(&self) -> String {
        let base = match self.control {
            Control::Cc(n) => format!("CC {n}"),
            Control::Note(n) => format!("Note {n}"),
            Control::PitchBend => "Pitch bend".to_string(),
        };
        match self.channel {
            Some(ch) if ch != 1 || self.control == Control::PitchBend => {
                format!("{base} · ch {ch}")
            }
            _ => base,
        }
    }
}

/// How a binding looks in the config file: exactly one of `cc = N`, `note = N` or
/// `pitchbend = true`, plus the optional `channel` and `mode`.
#[derive(Serialize, Deserialize)]
struct BindingToml {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    cc: Option<u8>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    note: Option<u8>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pitchbend: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    channel: Option<u8>,
    target: String,
    #[serde(flatten)]
    action: Action,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    mode: Option<Mode>,
}

impl TryFrom<BindingToml> for Binding {
    type Error = String;
    fn try_from(t: BindingToml) -> Result<Self, String> {
        let control = match (t.cc, t.note, t.pitchbend) {
            (Some(n), None, false) => Control::Cc(n),
            (None, Some(n), false) => Control::Note(n),
            (None, None, true) => Control::PitchBend,
            _ => {
                return Err(format!(
                    "MIDI binding for {:?} needs exactly one of `cc`, `note` or `pitchbend = true`",
                    t.target
                ));
            }
        };
        Ok(Binding {
            control,
            channel: t.channel,
            target: t.target,
            action: t.action,
            mode: t.mode,
        })
    }
}

impl From<Binding> for BindingToml {
    fn from(b: Binding) -> Self {
        let (cc, note, pitchbend) = match b.control {
            Control::Cc(n) => (Some(n), None, false),
            Control::Note(n) => (None, Some(n), false),
            Control::PitchBend => (None, None, true),
        };
        BindingToml {
            cc,
            note,
            pitchbend,
            channel: b.channel,
            target: b.target,
            action: b.action,
            mode: b.mode,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "lowercase")]
pub enum Action {
    /// A fader or knob setting the gain.
    Gain,
    /// A button toggling mute; its LED lights while muted.
    Mute,
    /// A button toggling whether a strip is sent to `bus`; its LED lights while it is.
    Route { bus: String },
    /// A button recalling the scene named by the binding's `target`; its LED lights
    /// while the mix matches that scene.
    Scene,
    /// A knob for the gate (0 = off), like the GATE knob on a channel.
    Gate,
    /// A knob for the compressor (0 = off), like the COMP knob on a channel.
    Comp,
    /// A knob for the balance (middle = centre).
    Pan,
    /// A button cueing the channel; its LED lights while cued.
    Cue,
}

impl Action {
    /// Faders and knobs (as opposed to buttons).
    pub fn is_continuous(&self) -> bool {
        matches!(
            self,
            Action::Gain | Action::Gate | Action::Comp | Action::Pan
        )
    }
}

/// An input channel: either a virtual device apps can play into, or a physical input (mic, line-in).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Strip {
    /// Short unique identifier, used in node names (`lam_strip_<id>`). Letters, digits, `-`, `_`.
    pub id: String,
    /// Display name, shown in the sound settings of the desktop and in apps.
    pub name: String,
    #[serde(flatten)]
    pub kind: Kind,
    #[serde(default)]
    pub gain_db: f32,
    #[serde(default)]
    pub mute: bool,
    /// Ids of the buses this strip is sent to (the A1/A2/B1 buttons).
    #[serde(default)]
    pub buses: Vec<String>,
    /// Channels a virtual strip offers apps (physical strips follow their device).
    #[serde(default, skip_serializing_if = "Layout::is_default")]
    pub layout: Layout,
    /// Low cut, gate, EQ and compressor.
    #[serde(default, skip_serializing_if = "Effects::is_default")]
    pub fx: Effects,
    /// Balance: -1 = left only, 0 = centre, 1 = right only.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub pan: f32,
    /// Cued: while any channel is cued, the cue output plays only the cued channels.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub cue: bool,
}

fn is_zero(v: &f32) -> bool {
    *v == 0.0
}

/// Per-channel gains for a balance setting: turning towards one side lowers only the
/// other side, so the centre position leaves the level unchanged. Centre and LFE
/// channels are never changed.
pub fn pan_gains(pan: f32, positions: &[&str]) -> Vec<f32> {
    let pan = pan.clamp(-1.0, 1.0);
    let (left, right) = (
        if pan > 0.0 { 1.0 - pan } else { 1.0 },
        if pan < 0.0 { 1.0 + pan } else { 1.0 },
    );
    positions
        .iter()
        .map(|p| match *p {
            "FL" | "RL" | "SL" => left,
            "FR" | "RR" | "SR" => right,
            _ => 1.0,
        })
        .collect()
}

/// A channel's effects, processed in this order: low cut, noise suppression, mono, gate,
/// EQ, de-esser, compressor, ducking, limiter, delay.
/// Each keeps its settings while switched off.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Effects {
    /// Remove rumble below `low_cut_hz` (desk bumps, fans, breath pops).
    pub low_cut: bool,
    pub low_cut_hz: f32,
    pub gate: Gate,
    pub eq: Eq,
    pub comp: Comp,
    pub limiter: Limiter,
    pub noise: NoiseSuppression,
    pub deess: DeEsser,
    pub duck: Duck,
    /// Mix left and right together (e.g. a mic on one side, or one-ear listening).
    pub mono: bool,
    /// Hold the sound back this long (up to 1 s), e.g. to line it up with video.
    pub delay_ms: f32,
}

/// Ducking: turn this channel down while other channels are active (e.g. music while
/// you talk or someone speaks on Discord).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Duck {
    pub enabled: bool,
    /// Ids of the strips whose sound makes this one duck.
    pub sources: Vec<String>,
    /// How far it turns down.
    pub amount_db: f32,
    /// How loud a source must be to count as active.
    pub threshold_db: f32,
    pub attack_ms: f32,
    /// Stays down this long after the source goes quiet (so it doesn't pump between words).
    pub hold_ms: f32,
    pub release_ms: f32,
}

impl Default for Duck {
    fn default() -> Self {
        Duck {
            enabled: false,
            sources: vec![],
            amount_db: 12.0,
            threshold_db: -40.0,
            attack_ms: 30.0,
            hold_ms: 400.0,
            release_ms: 600.0,
        }
    }
}

/// Noise suppression (RNNoise): removes fans, keyboards, hum and the like while keeping
/// voices. Adds 10 ms of delay to the channel while on.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct NoiseSuppression {
    pub enabled: bool,
    /// 1 = fully suppressed; lower mixes some of the original back in.
    pub amount: f32,
}

impl Default for NoiseSuppression {
    fn default() -> Self {
        NoiseSuppression {
            enabled: false,
            amount: 1.0,
        }
    }
}

/// De-esser: turns down only the sharp high sounds ("s", "sh", "t") when they get loud,
/// leaving the rest of the voice alone.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct DeEsser {
    pub enabled: bool,
    /// Sounds above this frequency count as sharp (and are what gets turned down).
    pub freq: f32,
    /// How loud the sharp part may get before it's turned down.
    pub threshold_db: f32,
    /// The most it turns them down.
    pub amount_db: f32,
}

impl Default for DeEsser {
    fn default() -> Self {
        DeEsser {
            enabled: false,
            freq: 5500.0,
            threshold_db: -30.0,
            amount_db: 10.0,
        }
    }
}

/// Limiter: the level never goes above the ceiling. Mostly for outputs, to protect ears
/// and streams from sudden loud peaks. Comes last, and judges the actual output level.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Limiter {
    pub enabled: bool,
    /// The highest level let through (dBFS).
    pub ceiling_db: f32,
    /// How fast it lets go after a peak.
    pub release_ms: f32,
}

impl Default for Limiter {
    fn default() -> Self {
        Limiter {
            enabled: false,
            ceiling_db: -1.0,
            release_ms: 100.0,
        }
    }
}

impl Default for Effects {
    fn default() -> Self {
        Effects {
            low_cut: false,
            low_cut_hz: 80.0,
            gate: Gate::default(),
            eq: Eq::default(),
            comp: Comp::default(),
            limiter: Limiter::default(),
            noise: NoiseSuppression::default(),
            deess: DeEsser::default(),
            duck: Duck::default(),
            mono: false,
            delay_ms: 0.0,
        }
    }
}

impl Effects {
    pub fn is_default(&self) -> bool {
        *self == Effects::default()
    }
    /// Whether any processing is switched on.
    pub fn active(&self) -> bool {
        self.low_cut
            || self.noise.enabled
            || self.deess.enabled
            || self.gate.enabled
            || self.eq.enabled
            || self.comp.enabled
            || self.limiter.enabled
            || (self.duck.enabled && !self.duck.sources.is_empty())
            || self.mono
            || self.delay_ms > 0.0
    }
}

/// Noise gate: silences the channel while its level is below the threshold
/// (e.g. a mic between sentences).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Gate {
    pub enabled: bool,
    pub threshold_db: f32,
    /// How fast it opens.
    pub attack_ms: f32,
    /// How long it stays open after the sound drops (so word endings aren't cut).
    pub hold_ms: f32,
    /// How fast it fades out after the hold.
    pub release_ms: f32,
    /// How far it turns down when closed.
    pub range_db: f32,
}

impl Default for Gate {
    fn default() -> Self {
        Gate {
            enabled: false,
            threshold_db: -45.0,
            attack_ms: 1.0,
            hold_ms: 150.0,
            release_ms: 150.0,
            range_db: 80.0,
        }
    }
}

/// Lowest gate threshold the knob reaches.
pub const GATE_KNOB_MIN_DB: f32 = -80.0;
/// Lowest compressor threshold the knob reaches.
pub const COMP_KNOB_MIN_DB: f32 = -40.0;

impl Gate {
    /// The one-knob view: 0 = off, then turning right raises the threshold
    /// (more is cut away), up to 0 dB at 1.
    pub fn knob(&self) -> f32 {
        if !self.enabled {
            return 0.0;
        }
        ((self.threshold_db - GATE_KNOB_MIN_DB) / -GATE_KNOB_MIN_DB).clamp(0.001, 1.0)
    }
    pub fn set_knob(&mut self, pos: f32) {
        self.enabled = pos > 0.0;
        if self.enabled {
            self.threshold_db = round_db(GATE_KNOB_MIN_DB + pos.min(1.0) * -GATE_KNOB_MIN_DB);
        }
    }
}

/// Parametric equalizer: the points the user placed on the EQ graph, each a shelf or a
/// bell. No points means flat.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Eq {
    pub enabled: bool,
    pub bands: Vec<EqBand>,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct EqBand {
    pub kind: BandKind,
    pub freq: f32,
    pub gain_db: f32,
    /// Width: low = broad, high = narrow.
    pub q: f32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum BandKind {
    /// Raises/lowers everything below its frequency.
    LowShelf,
    /// Raises/lowers around its frequency.
    Bell,
    /// Raises/lowers everything above its frequency.
    HighShelf,
}

pub const EQ_MIN_HZ: f32 = 20.0;
pub const EQ_MAX_HZ: f32 = 20000.0;
pub const EQ_MAX_DB: f32 = 15.0;
pub const EQ_MIN_Q: f32 = 0.3;
pub const EQ_MAX_Q: f32 = 10.0;
/// Most points an EQ can have (the audio thread reserves room for this many).
pub const EQ_MAX_BANDS: usize = 16;

impl EqBand {
    pub const fn new(kind: BandKind, freq: f32, gain_db: f32, q: f32) -> Self {
        EqBand {
            kind,
            freq,
            gain_db,
            q,
        }
    }

    /// A new point placed by clicking on the graph: shelves at the edges, bells between.
    pub fn at(freq: f32, gain_db: f32) -> Self {
        let kind = if freq < 60.0 {
            BandKind::LowShelf
        } else if freq > 10000.0 {
            BandKind::HighShelf
        } else {
            BandKind::Bell
        };
        let q = if kind == BandKind::Bell { 1.0 } else { 0.7 };
        EqBand::new(
            kind,
            freq.clamp(EQ_MIN_HZ, EQ_MAX_HZ),
            gain_db.clamp(-EQ_MAX_DB, EQ_MAX_DB),
            q,
        )
    }
}

impl Eq {
    pub const PRESET_NAMES: [&'static str; 6] =
        ["Flat", "Voice", "Podcast", "Warm", "Bright", "Bass boost"];

    /// A preset: the EQ points and the low cut it suggests.
    pub fn preset(name: &str) -> (Eq, Option<f32>) {
        use BandKind::*;
        let b = EqBand::new;
        let (bands, low_cut) = match name {
            "Voice" => (
                vec![
                    b(LowShelf, 120.0, -2.0, 0.7),
                    b(Bell, 300.0, -3.0, 1.2),
                    b(Bell, 3500.0, 4.0, 1.0),
                    b(HighShelf, 9000.0, 2.0, 0.7),
                ],
                Some(90.0),
            ),
            "Podcast" => (
                vec![
                    b(LowShelf, 110.0, 2.0, 0.7),
                    b(Bell, 350.0, -4.0, 1.4),
                    b(Bell, 3000.0, 3.0, 1.0),
                    b(HighShelf, 10000.0, 1.0, 0.7),
                ],
                Some(80.0),
            ),
            "Warm" => (
                vec![
                    b(LowShelf, 150.0, 3.0, 0.7),
                    b(Bell, 400.0, 1.5, 1.0),
                    b(HighShelf, 6000.0, -3.0, 0.7),
                ],
                None,
            ),
            "Bright" => (
                vec![
                    b(Bell, 250.0, -1.0, 1.0),
                    b(Bell, 3500.0, 2.5, 1.0),
                    b(HighShelf, 8000.0, 5.0, 0.7),
                ],
                None,
            ),
            "Bass boost" => (
                vec![b(LowShelf, 90.0, 7.0, 0.7), b(Bell, 250.0, 2.0, 1.2)],
                None,
            ),
            _ => (vec![], None),
        };
        (
            Eq {
                enabled: true,
                bands,
            },
            low_cut,
        )
    }
}

/// Compressor: turns down loud moments so the level is more even.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Comp {
    pub enabled: bool,
    pub threshold_db: f32,
    /// 1 = off, 4 = firm, 20 = limiting.
    pub ratio: f32,
    /// Gain added afterwards to make up for the turned-down peaks.
    pub makeup_db: f32,
    /// How fast it reacts to a loud moment.
    pub attack_ms: f32,
    /// How fast it lets go afterwards.
    pub release_ms: f32,
    /// How gently it eases in around the threshold (0 = hard).
    pub knee_db: f32,
}

impl Default for Comp {
    fn default() -> Self {
        Comp {
            enabled: false,
            threshold_db: -20.0,
            ratio: 3.0,
            makeup_db: 0.0,
            attack_ms: 10.0,
            release_ms: 150.0,
            knee_db: 6.0,
        }
    }
}

impl Comp {
    /// The one-knob view: 0 = off, then turning right lowers the threshold
    /// (more compression), down to COMP_KNOB_MIN_DB at 1.
    pub fn knob(&self) -> f32 {
        if !self.enabled {
            return 0.0;
        }
        (self.threshold_db / COMP_KNOB_MIN_DB).clamp(0.001, 1.0)
    }
    pub fn set_knob(&mut self, pos: f32) {
        self.enabled = pos > 0.0;
        if self.enabled {
            self.threshold_db = round_db(pos.min(1.0) * COMP_KNOB_MIN_DB);
        }
    }
}

/// The channels a virtual input offers to apps.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Layout {
    /// 7.1: games and films that output surround keep it; stereo apps play on the
    /// front left/right (PipeWire doesn't spread them by itself).
    #[default]
    Surround,
    /// Plain stereo.
    Stereo,
}

impl Layout {
    pub fn is_default(&self) -> bool {
        *self == Layout::default()
    }
}

/// An output: either a physical device (headphones, speakers) or a virtual mic other apps can record.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Bus {
    pub id: String,
    pub name: String,
    #[serde(flatten)]
    pub kind: Kind,
    #[serde(default)]
    pub gain_db: f32,
    #[serde(default)]
    pub mute: bool,
    /// How stereo is spread over a surround speaker set (physical outputs only).
    #[serde(default, skip_serializing_if = "Upmix::is_off")]
    pub upmix: Upmix,
    /// Limiter, EQ and compressor on the whole output. `fx.duck` on an output turns
    /// down the *other* channels in this output while its sources are active (the
    /// sources themselves and every other output are left alone).
    #[serde(default, skip_serializing_if = "Effects::is_default")]
    pub fx: Effects,
}

impl Bus {
    /// The effects on the whole output (ducking works per channel, see `ducks`).
    pub fn own_fx(&self) -> Effects {
        let mut fx = self.fx.clone();
        fx.duck.enabled = false;
        fx
    }
    /// Whether this output turns its other channels down while its sources are active.
    pub fn ducks(&self) -> bool {
        self.fx.duck.enabled && !self.fx.duck.sources.is_empty()
    }
    /// Whether `strip` is turned down here (sent here and not one of the sources).
    pub fn ducks_strip(&self, strip: &str) -> bool {
        self.ducks() && !self.fx.duck.sources.iter().any(|s| s == strip)
    }
    /// The ducking effects for the ducked channels' submix.
    pub fn duck_fx(&self) -> Effects {
        Effects {
            duck: self.fx.duck.clone(),
            ..Effects::default()
        }
    }
}

/// How a stereo mix plays on an output with more than two speakers.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Upmix {
    /// Front left/right only.
    #[default]
    Off,
    /// Real surround (passive surround decoding): the centre gets what left and right
    /// share, the rears get what differs between them (slightly delayed), and the
    /// subwoofer gets only the bass.
    #[serde(alias = "full", alias = "rear")]
    Surround,
    /// The same sound on every speaker (the subwoofer still gets only the bass).
    All,
}

impl Upmix {
    pub fn is_off(&self) -> bool {
        *self == Upmix::Off
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum Kind {
    /// A device created by the mixer.
    Virtual,
    /// A hardware device, identified by its PipeWire `node.name`
    /// (see `mixrail devices`).
    Physical { device: String },
}

impl Config {
    pub fn default_path() -> PathBuf {
        dirs::config_dir()
            .unwrap_or_else(|| PathBuf::from("~/.config"))
            .join("mixrail")
            .join("config.toml")
    }

    /// Before the app was called Mixrail its settings lived in
    /// `~/.config/linux-audio-mixer`; move them over once.
    pub fn move_from_old_location() {
        let Some(dir) = dirs::config_dir() else { return };
        let (old, new) = (dir.join("linux-audio-mixer"), dir.join("mixrail"));
        if old.join("config.toml").exists() && !new.exists() {
            match std::fs::rename(&old, &new) {
                Ok(()) => log::info!("moved settings from {} to {}", old.display(), new.display()),
                Err(e) => log::warn!("could not move {} to {}: {e}", old.display(), new.display()),
            }
        }
    }

    pub fn load(path: &Path) -> Result<Self> {
        let text =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        let cfg: Config =
            toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
        cfg.validate()?;
        Ok(cfg)
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let text = toml::to_string_pretty(self)?;
        // Write atomically so a crash never leaves a half-written config behind.
        let tmp = path.with_extension("toml.tmp");
        std::fs::write(&tmp, text)?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    }

    pub fn validate(&self) -> Result<()> {
        let mut seen = HashSet::new();
        for id in self
            .strips
            .iter()
            .map(|s| &s.id)
            .chain(self.buses.iter().map(|b| &b.id))
        {
            if id.is_empty()
                || !id
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
            {
                bail!("invalid id {id:?}: use only letters, digits, '-' and '_'");
            }
            if !seen.insert(id) {
                bail!("id {id:?} is used more than once (strip and bus ids must all be unique)");
            }
        }
        let bus_ids: HashSet<_> = self.buses.iter().map(|b| &b.id).collect();
        for s in &self.strips {
            for b in &s.buses {
                if !bus_ids.contains(b) {
                    bail!("strip {:?} is routed to unknown bus {b:?}", s.id);
                }
            }
        }
        if let Some(cue) = &self.cue_bus
            && !bus_ids.contains(cue)
        {
            bail!("the cue output {cue:?} doesn't exist");
        }
        let mut names = HashSet::new();
        for p in &self.eq_presets {
            if p.name.trim().is_empty() || !names.insert(p.name.as_str()) {
                bail!(
                    "EQ preset names must be filled in and unique ({:?})",
                    p.name
                );
            }
            if p.bands.len() > EQ_MAX_BANDS {
                bail!("EQ preset {:?} has more than {EQ_MAX_BANDS} points", p.name);
            }
        }
        for b in &self.buses {
            if b.fx.eq.bands.len() > EQ_MAX_BANDS {
                bail!(
                    "output {:?}: an EQ can have at most {EQ_MAX_BANDS} points",
                    b.id
                );
            }
        }
        for s in &self.strips {
            if s.fx.eq.bands.len() > EQ_MAX_BANDS {
                bail!(
                    "strip {:?}: an EQ can have at most {EQ_MAX_BANDS} points",
                    s.id
                );
            }
        }
        for s in &self.strips {
            for src in &s.fx.duck.sources {
                if src == &s.id || !self.strips.iter().any(|o| &o.id == src) {
                    bail!("strip {:?} ducks for unknown or own channel {src:?}", s.id);
                }
            }
        }
        for b in &self.buses {
            for src in &b.fx.duck.sources {
                if !self.strips.iter().any(|o| &o.id == src) {
                    bail!("output {:?} ducks for unknown channel {src:?}", b.id);
                }
            }
        }
        for r in &self.rules {
            if r.app.trim().is_empty() {
                bail!("an app rule has no app name");
            }
            match self.strips.iter().find(|s| s.id == r.strip) {
                Some(s) if s.kind == Kind::Virtual => {}
                Some(_) => bail!(
                    "app rule {:?}: apps can only play into virtual strips",
                    r.app
                ),
                None => bail!("app rule {:?} points at unknown strip {:?}", r.app, r.strip),
            }
        }
        if let Some(midi) = &self.midi {
            for b in &midi.bindings {
                let what = b.control_label();
                let number_ok = match b.control {
                    Control::Cc(n) | Control::Note(n) => n <= 127,
                    Control::PitchBend => true,
                };
                if !number_ok || b.channel.is_some_and(|c| !(1..=16).contains(&c)) {
                    bail!("MIDI binding {what}: numbers must be 0-127 and channel 1-16");
                }
                if b.control == Control::PitchBend && b.channel.is_none() {
                    bail!(
                        "MIDI binding {what}: pitch bend needs a channel (one fader per channel)"
                    );
                }
                let mode_ok = match (&b.action, b.mode) {
                    (_, None) => true,
                    (Action::Gain | Action::Gate | Action::Comp | Action::Pan, Some(m)) => {
                        matches!(m, Mode::Absolute | Mode::Relative | Mode::RelativeOffset)
                    }
                    (Action::Scene, Some(m)) => m == Mode::Momentary,
                    (_, Some(m)) => matches!(m, Mode::Momentary | Mode::Latching),
                };
                if !mode_ok {
                    bail!(
                        "MIDI binding {what}: mode {:?} doesn't fit a {:?} control",
                        b.mode,
                        b.action
                    );
                }
                if b.action == Action::Scene {
                    if !self.scenes.iter().any(|s| s.name == b.target) {
                        bail!("MIDI binding {} recalls unknown scene {:?}", what, b.target);
                    }
                    continue;
                }
                let is_strip = self.strips.iter().any(|s| s.id == b.target);
                let is_bus = bus_ids.contains(&b.target);
                if !is_strip && !is_bus {
                    bail!(
                        "MIDI binding {} targets unknown strip/bus {:?}",
                        what,
                        b.target
                    );
                }
                if matches!(
                    b.action,
                    Action::Gate | Action::Comp | Action::Pan | Action::Cue
                ) && !is_strip
                {
                    bail!(
                        "MIDI binding {}: gate/compressor knobs control a strip",
                        what
                    );
                }
                if let Action::Route { bus } = &b.action
                    && (!is_strip || !bus_ids.contains(bus))
                {
                    bail!(
                        "MIDI binding {}: route needs a strip target and an existing bus",
                        what
                    );
                }
            }
        }
        Ok(())
    }

    /// A generic starting layout built from the machine's current default devices.
    pub fn starter(default_sink: Option<String>, default_source: Option<String>) -> Self {
        let mut buses = Vec::new();
        let mut strips = Vec::new();
        let out_buses: Vec<String> = if default_sink.is_some() {
            vec!["a1".into()]
        } else {
            vec![]
        };
        if let Some(dev) = default_sink {
            buses.push(Bus::new(
                "a1",
                "Main Output",
                Kind::Physical { device: dev },
            ));
        }
        buses.push(Bus::new("b1", "Virtual Mic", Kind::Virtual));
        if let Some(dev) = default_source {
            strips.push(Strip::new(
                "mic",
                "Microphone",
                Kind::Physical { device: dev },
                &["b1"],
            ));
        }
        let outs: Vec<&str> = out_buses.iter().map(String::as_str).collect();
        strips.push(Strip::new("system", "System", Kind::Virtual, &outs));
        strips.push(Strip::new("music", "Music", Kind::Virtual, &outs));
        strips.push(Strip::new("chat", "Chat", Kind::Virtual, &outs));
        Config {
            strips,
            buses,
            midi: None,
            rules: vec![],
            scenes: vec![],
            eq_presets: vec![],
            cue_bus: None,
        }
    }

    /// Gain and mute of a strip or bus.
    pub fn level(&self, id: &str) -> Option<(f32, bool)> {
        self.strips
            .iter()
            .find(|s| s.id == id)
            .map(|s| (s.gain_db, s.mute))
            .or_else(|| {
                self.buses
                    .iter()
                    .find(|b| b.id == id)
                    .map(|b| (b.gain_db, b.mute))
            })
    }

    /// Mutable gain and mute of a strip or bus.
    pub fn level_mut(&mut self, id: &str) -> Option<(&mut f32, &mut bool)> {
        if let Some(s) = self.strips.iter_mut().find(|s| s.id == id) {
            return Some((&mut s.gain_db, &mut s.mute));
        }
        self.buses
            .iter_mut()
            .find(|b| b.id == id)
            .map(|b| (&mut b.gain_db, &mut b.mute))
    }

    /// Remove a strip and any MIDI bindings that control it.
    pub fn remove_strip(&mut self, id: &str) {
        self.strips.retain(|s| s.id != id);
        for s in &mut self.strips {
            s.fx.duck.sources.retain(|src| src != id);
        }
        for b in &mut self.buses {
            b.fx.duck.sources.retain(|src| src != id);
        }
        self.rules.retain(|r| r.strip != id);
        for scene in &mut self.scenes {
            scene.levels.remove(id);
        }
        if let Some(midi) = &mut self.midi {
            midi.bindings
                .retain(|b| b.target != id || b.action == Action::Scene);
        }
    }

    /// Remove a bus, every send to it, and any MIDI bindings that refer to it.
    pub fn remove_bus(&mut self, id: &str) {
        self.buses.retain(|b| b.id != id);
        if self.cue_bus.as_deref() == Some(id) {
            self.cue_bus = None;
        }
        for s in &mut self.strips {
            s.buses.retain(|b| b != id);
        }
        for scene in &mut self.scenes {
            scene.levels.remove(id);
            for level in scene.levels.values_mut() {
                if let Some(buses) = &mut level.buses {
                    buses.retain(|b| b != id);
                }
            }
        }
        if let Some(midi) = &mut self.midi {
            midi.bindings.retain(|b| {
                (b.target != id || b.action == Action::Scene)
                    && !matches!(&b.action, Action::Route { bus } if bus == id)
            });
        }
    }

    /// The current mix as a scene.
    pub fn snapshot(&self, name: &str) -> Scene {
        let strips = self.strips.iter().map(|s| {
            let level = SceneLevel {
                gain_db: s.gain_db,
                mute: s.mute,
                buses: Some(s.buses.clone()),
            };
            (s.id.clone(), level)
        });
        let buses = self.buses.iter().map(|b| {
            (
                b.id.clone(),
                SceneLevel {
                    gain_db: b.gain_db,
                    mute: b.mute,
                    buses: None,
                },
            )
        });
        Scene {
            name: name.to_string(),
            levels: strips.chain(buses).collect(),
        }
    }

    /// Save an EQ preset, replacing one of the user's with the same name. Built-in
    /// preset names are reserved; returns false for those.
    pub fn save_eq_preset(&mut self, preset: EqPreset) -> bool {
        if preset.name.is_empty()
            || Eq::PRESET_NAMES
                .iter()
                .any(|n| n.eq_ignore_ascii_case(&preset.name))
        {
            return false;
        }
        match self.eq_presets.iter_mut().find(|p| p.name == preset.name) {
            Some(existing) => *existing = preset,
            None => self.eq_presets.push(preset),
        }
        true
    }

    /// Save the current mix under `name`, replacing a scene with that name.
    pub fn save_scene(&mut self, name: &str) {
        let scene = self.snapshot(name);
        match self.scenes.iter_mut().find(|s| s.name == name) {
            Some(existing) => *existing = scene,
            None => self.scenes.push(scene),
        }
    }

    pub fn delete_scene(&mut self, name: &str) {
        self.scenes.retain(|s| s.name != name);
        if let Some(midi) = &mut self.midi {
            midi.bindings
                .retain(|b| !(b.action == Action::Scene && b.target == name));
        }
    }

    /// Apply a saved mix. Returns false if there is no such scene.
    pub fn recall_scene(&mut self, name: &str) -> bool {
        let Some(scene) = self.scenes.iter().find(|s| s.name == name).cloned() else {
            return false;
        };
        let bus_ids: Vec<String> = self.buses.iter().map(|b| b.id.clone()).collect();
        for s in &mut self.strips {
            if let Some(l) = scene.levels.get(&s.id) {
                s.gain_db = l.gain_db;
                s.mute = l.mute;
                if let Some(buses) = &l.buses {
                    s.buses = buses
                        .iter()
                        .filter(|b| bus_ids.contains(b))
                        .cloned()
                        .collect();
                }
            }
        }
        for b in &mut self.buses {
            if let Some(l) = scene.levels.get(&b.id) {
                b.gain_db = l.gain_db;
                b.mute = l.mute;
            }
        }
        true
    }

    /// Whether the current mix is (still) the given scene.
    pub fn scene_active(&self, name: &str) -> bool {
        let Some(scene) = self.scenes.iter().find(|s| s.name == name) else {
            return false;
        };
        let same = |gain: f32, mute: bool, l: &SceneLevel| {
            (gain - l.gain_db).abs() < 0.05 && mute == l.mute
        };
        self.strips.iter().all(|s| {
            scene.levels.get(&s.id).is_none_or(|l| {
                same(s.gain_db, s.mute, l)
                    && l.buses.as_ref().is_none_or(|b| {
                        let mut a = b.clone();
                        let mut c = s.buses.clone();
                        a.sort();
                        c.sort();
                        a == c
                    })
            })
        }) && self.buses.iter().all(|b| {
            scene
                .levels
                .get(&b.id)
                .is_none_or(|l| same(b.gain_db, b.mute, l))
        })
    }

    /// The output currently playing cued channels, if anything is cued.
    pub fn cueing(&self) -> Option<&str> {
        self.cue_bus
            .as_deref()
            .filter(|_| self.strips.iter().any(|s| s.cue))
    }

    /// Where a strip is actually sent right now: its sends, except that while cueing the
    /// cue output takes only the cued strips.
    pub fn sends(&self, s: &Strip) -> Vec<String> {
        match self.cueing() {
            None => s.buses.clone(),
            Some(cue) => {
                let mut out: Vec<String> = s.buses.iter().filter(|b| *b != cue).cloned().collect();
                if s.cue {
                    out.push(cue.to_string());
                }
                out
            }
        }
    }

    pub fn effects_mut(&mut self, id: &str) -> Option<&mut Effects> {
        if let Some(s) = self.strips.iter_mut().find(|s| s.id == id) {
            return Some(&mut s.fx);
        }
        self.buses
            .iter_mut()
            .find(|b| b.id == id)
            .map(|b| &mut b.fx)
    }

    pub fn is_routed(&self, strip: &str, bus: &str) -> bool {
        self.strips
            .iter()
            .any(|s| s.id == strip && s.buses.iter().any(|b| b == bus))
    }

    /// Turn a strip→bus send on or off.
    pub fn set_routed(&mut self, strip: &str, bus: &str, on: bool) {
        if let Some(s) = self.strips.iter_mut().find(|s| s.id == strip) {
            s.buses.retain(|b| b != bus);
            if on {
                s.buses.push(bus.to_string());
            }
        }
    }
}

fn yes() -> bool {
    true
}
fn is_true(b: &bool) -> bool {
    *b
}

/// A MIDI fader/knob value (0..=max) to dB, on the same curve as the GUI faders:
/// bottom is off, 100/127 of the travel is 0 dB, top is +12 dB.
pub fn control_to_db(value: u16, max: u16) -> f32 {
    round_db(pos_to_db(value.min(max) as f32 / max.max(1) as f32))
}

/// 0.1 dB steps are finer than anyone can hear and keep the config file readable.
pub fn round_db(db: f32) -> f32 {
    (db * 10.0).round() / 10.0
}

/// Fader position 0.0..=1.0 for a gain in dB, on the same curve as MIDI faders
/// (0 dB sits at 100/127 of the travel). Also used to draw level meters.
pub fn db_to_pos(db: f32) -> f32 {
    const ZERO: f32 = 100.0 / 127.0;
    if db <= MIN_GAIN_DB {
        0.0
    } else if db <= 0.0 {
        (db - MIN_GAIN_DB) / -MIN_GAIN_DB * ZERO
    } else {
        ZERO + db.min(MAX_GAIN_DB) / MAX_GAIN_DB * (1.0 - ZERO)
    }
}

/// Inverse of [`db_to_pos`].
pub fn pos_to_db(pos: f32) -> f32 {
    const ZERO: f32 = 100.0 / 127.0;
    let pos = pos.clamp(0.0, 1.0);
    if pos <= ZERO {
        MIN_GAIN_DB + pos / ZERO * -MIN_GAIN_DB
    } else {
        (pos - ZERO) / (1.0 - ZERO) * MAX_GAIN_DB
    }
}

/// Make a valid, unused id from a display name ("Game Chat" -> "game_chat").
pub fn make_id(name: &str, taken: &[&str]) -> String {
    let base: String = name
        .to_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect::<String>()
        .split('_')
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("_");
    let base = if base.is_empty() {
        "channel".to_string()
    } else {
        base
    };
    let mut id = base.clone();
    let mut n = 2;
    while taken.contains(&id.as_str()) {
        id = format!("{base}_{n}");
        n += 1;
    }
    id
}

/// Linear amplitude (as PipeWire reports it) back to dB.
pub fn linear_to_db(v: f32) -> f32 {
    if v <= 0.0 {
        MIN_GAIN_DB
    } else {
        (20.0 * v.log10()).clamp(MIN_GAIN_DB, MAX_GAIN_DB)
    }
}

impl Strip {
    pub fn new(id: &str, name: &str, kind: Kind, buses: &[&str]) -> Self {
        Strip {
            id: id.into(),
            name: name.into(),
            kind,
            gain_db: 0.0,
            mute: false,
            buses: buses.iter().map(|s| s.to_string()).collect(),
            layout: Layout::default(),
            fx: Effects::default(),
            pan: 0.0,
            cue: false,
        }
    }
}

impl Bus {
    pub fn new(id: &str, name: &str, kind: Kind) -> Self {
        Bus {
            id: id.into(),
            name: name.into(),
            kind,
            gain_db: 0.0,
            mute: false,
            upmix: Upmix::Off,
            fx: Effects::default(),
        }
    }
}

/// Name of a speaker layout from the channels a device has, e.g. "5.1".
pub fn speaker_layout(channels: &[String]) -> String {
    let has = |c: &str| channels.iter().any(|x| x == c);
    let lfe = if has("LFE") { ".1" } else { ".0" };
    let (fronts, center, rears, sides) = (
        has("FL") && has("FR"),
        has("FC"),
        has("RL") && has("RR"),
        has("SL") && has("SR"),
    );
    match (channels.len(), fronts, center, rears, sides) {
        (1, ..) => "Mono".into(),
        (_, true, false, false, false) => format!("Stereo (2{lfe})"),
        (_, true, false, true, false) | (_, true, false, false, true) => format!("Quad (4{lfe})"),
        (_, true, true, true, false) | (_, true, true, false, true) => format!("5{lfe}"),
        (_, true, true, true, true) => format!("7{lfe}"),
        (n, ..) => format!("{n} channels"),
    }
}
/// Convert a fader position in dB to the linear amplitude PipeWire expects.
pub fn db_to_linear(db: f32) -> f32 {
    if db <= MIN_GAIN_DB {
        0.0
    } else {
        10f32.powf(db.min(MAX_GAIN_DB) / 20.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_and_validate() {
        let cfg = Config::starter(Some("hw_out".into()), Some("hw_in".into()));
        let text = toml::to_string_pretty(&cfg).unwrap();
        let back: Config = toml::from_str(&text).unwrap();
        assert_eq!(cfg, back);
        back.validate().unwrap();
    }

    #[test]
    fn rejects_unknown_bus_and_duplicates() {
        let mut cfg = Config::starter(None, None);
        cfg.strips[0].buses.push("nope".into());
        assert!(cfg.validate().is_err());

        let mut cfg = Config::starter(None, None);
        cfg.strips[1].id = cfg.strips[0].id.clone();
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn fader_curve() {
        assert_eq!(control_to_db(0, 127), MIN_GAIN_DB);
        assert_eq!(control_to_db(100, 127), 0.0);
        assert_eq!(control_to_db(127, 127), MAX_GAIN_DB);
        assert!(control_to_db(50, 127) < control_to_db(51, 127));
        assert!((linear_to_db(db_to_linear(-6.0)) + 6.0).abs() < 1e-4);
        // A 14-bit pitch-bend fader lands on the same 0 dB point.
        assert_eq!(control_to_db(12900, 16383), 0.0);
        assert_eq!(control_to_db(16383, 16383), MAX_GAIN_DB);
    }

    #[test]
    fn fader_position_roundtrip() {
        for db in [-60.0, -30.0, -6.0, 0.0, 6.0, 12.0] {
            assert!((pos_to_db(db_to_pos(db)) - db).abs() < 1e-3, "{db}");
        }
        // GUI and MIDI faders agree on where 0 dB is
        assert!((db_to_pos(0.0) - 100.0 / 127.0).abs() < 1e-6);
        assert_eq!(control_to_db(100, 127), pos_to_db(100.0 / 127.0));
    }

    #[test]
    fn removing_keeps_config_valid() {
        let mut cfg = Config::starter(Some("hw".into()), Some("mic".into()));
        cfg.midi = Some(Midi {
            device: "x".into(),
            feedback: true,
            bindings: vec![
                Binding {
                    control: Control::Cc(0),
                    channel: None,
                    target: "a1".into(),
                    action: Action::Gain,
                    mode: None,
                },
                Binding {
                    control: Control::Cc(1),
                    channel: None,
                    target: "music".into(),
                    action: Action::Route { bus: "a1".into() },
                    mode: None,
                },
                Binding {
                    control: Control::Cc(2),
                    channel: None,
                    target: "chat".into(),
                    action: Action::Mute,
                    mode: None,
                },
            ],
        });
        cfg.validate().unwrap();
        cfg.remove_bus("a1");
        cfg.validate().unwrap();
        assert_eq!(cfg.midi.as_ref().unwrap().bindings.len(), 1);
        cfg.remove_strip("chat");
        cfg.validate().unwrap();
        assert!(cfg.midi.as_ref().unwrap().bindings.is_empty());
    }

    #[test]
    fn scenes_save_recall_and_survive_edits() {
        let mut cfg = Config::starter(Some("hw".into()), Some("mic".into()));
        cfg.save_scene("Gaming");
        assert!(cfg.scene_active("Gaming"));

        // Change the mix: no longer the scene.
        cfg.strips[1].gain_db = -20.0;
        cfg.strips[1].buses.clear();
        cfg.buses[0].mute = true;
        assert!(!cfg.scene_active("Gaming"));
        cfg.save_scene("Quiet");

        assert!(cfg.recall_scene("Gaming"));
        assert!(cfg.scene_active("Gaming") && !cfg.scene_active("Quiet"));
        assert_eq!(cfg.strips[1].gain_db, 0.0);
        assert_eq!(cfg.strips[1].buses, vec!["a1".to_string()]);
        assert!(!cfg.buses[0].mute);
        assert!(!cfg.recall_scene("nope"));

        // Saved to TOML and back.
        cfg.midi = Some(Midi {
            device: "x".into(),
            feedback: true,
            bindings: vec![Binding {
                control: Control::Cc(9),
                channel: None,
                target: "Quiet".into(),
                action: Action::Scene,
                mode: None,
            }],
        });
        cfg.validate().unwrap();
        let back: Config = toml::from_str(&toml::to_string_pretty(&cfg).unwrap()).unwrap();
        assert_eq!(back, cfg);

        // Removing a bus cleans it out of scenes; deleting a scene drops its MIDI button.
        cfg.remove_bus("a1");
        assert!(cfg.scenes.iter().all(|s| !s.levels.contains_key("a1")));
        assert!(cfg.recall_scene("Gaming"));
        cfg.validate().unwrap();
        cfg.delete_scene("Quiet");
        assert!(cfg.midi.as_ref().unwrap().bindings.is_empty());
        cfg.validate().unwrap();
    }

    #[test]
    fn midi_control_kinds() {
        let text = r#"
            [[strip]]
            id = "music"
            name = "Music"
            kind = "virtual"

            [midi]
            device = "X-Touch"

            [[midi.binding]]
            pitchbend = true
            channel = 1
            target = "music"
            action = "gain"

            [[midi.binding]]
            note = 16
            target = "music"
            action = "mute"
            mode = "latching"

            [[midi.binding]]
            cc = 16
            target = "music"
            action = "gain"
            mode = "relative"
        "#;
        let cfg: Config = toml::from_str(text).unwrap();
        cfg.validate().unwrap();
        let b = &cfg.midi.as_ref().unwrap().bindings;
        assert_eq!(b[0].control, Control::PitchBend);
        assert_eq!(
            (b[1].control, b[1].mode),
            (Control::Note(16), Some(Mode::Latching))
        );
        assert_eq!(b[2].mode, Some(Mode::Relative));
        assert_eq!(b[0].control_label(), "Pitch bend · ch 1");
        assert!(b[1].matches(Control::Note(16), 5) && !b[1].matches(Control::Cc(16), 1));
        let back: Config = toml::from_str(&toml::to_string_pretty(&cfg).unwrap()).unwrap();
        assert_eq!(back, cfg);

        // Mistakes are caught with a clear message.
        let mut bad = cfg.clone();
        bad.midi.as_mut().unwrap().bindings[0].channel = None;
        assert!(
            bad.validate()
                .unwrap_err()
                .to_string()
                .contains("pitch bend needs a channel")
        );
        let mut bad = cfg.clone();
        bad.midi.as_mut().unwrap().bindings[1].mode = Some(Mode::Relative);
        assert!(bad.validate().is_err());
        assert!(
            toml::from_str::<Config>(&text.replace("pitchbend = true", "pitchbend = true\ncc = 3"))
                .is_err()
        );
    }

    #[test]
    fn speaker_layout_names() {
        let l = |c: &[&str]| speaker_layout(&c.iter().map(|s| s.to_string()).collect::<Vec<_>>());
        assert_eq!(l(&["MONO"]), "Mono");
        assert_eq!(l(&["FL", "FR"]), "Stereo (2.0)");
        assert_eq!(l(&["FL", "FR", "LFE"]), "Stereo (2.1)");
        assert_eq!(l(&["FL", "FR", "RL", "RR"]), "Quad (4.0)");
        assert_eq!(l(&["FL", "FR", "FC", "LFE", "RL", "RR"]), "5.1");
        assert_eq!(l(&["FL", "FR", "FC", "LFE", "SL", "SR"]), "5.1");
        assert_eq!(l(&["FL", "FR", "FC", "LFE", "RL", "RR", "SL", "SR"]), "7.1");
        assert_eq!(l(&["AUX0", "AUX1", "AUX2"]), "3 channels");
    }

    #[test]
    fn effects_in_config() {
        let text = r#"
            [[strip]]
            id = "mic"
            name = "Mic"
            kind = "virtual"

            [strip.fx]
            low_cut = true

            [strip.fx.gate]
            enabled = true
            threshold_db = -50.0

            [strip.fx.comp]
            enabled = true
        "#;
        let cfg: Config = toml::from_str(text).unwrap();
        let fx = cfg.strips[0].fx.clone();
        assert!(fx.low_cut && fx.gate.enabled && fx.comp.enabled && !fx.eq.enabled);
        assert_eq!(fx.gate.threshold_db, -50.0);
        assert_eq!(fx.comp.ratio, 3.0, "unset values take their defaults");
        assert!(fx.active());
        let back: Config = toml::from_str(&toml::to_string_pretty(&cfg).unwrap()).unwrap();
        assert_eq!(back, cfg);
        // Strips without effects don't get an [fx] section at all.
        let plain = toml::to_string_pretty(&Config::starter(None, None)).unwrap();
        assert!(!plain.contains("fx"));
    }

    #[test]
    fn knobs_switch_on_and_set_threshold() {
        let mut g = Gate::default();
        assert_eq!(g.knob(), 0.0);
        g.set_knob(0.5);
        assert!(g.enabled && g.threshold_db == -40.0);
        assert!((g.knob() - 0.5).abs() < 1e-6);
        g.set_knob(0.0);
        assert!(!g.enabled && g.knob() == 0.0);
        assert_eq!(g.threshold_db, -40.0, "settings kept while off");

        let mut c = Comp::default();
        c.set_knob(1.0);
        assert!(c.enabled && c.threshold_db == COMP_KNOB_MIN_DB);
        c.set_knob(0.25);
        assert_eq!(c.threshold_db, -10.0);
        assert!((c.knob() - 0.25).abs() < 1e-6);
    }

    #[test]
    fn eq_presets_and_points() {
        for name in Eq::PRESET_NAMES {
            let (eq, low_cut) = Eq::preset(name);
            assert!(eq.enabled, "{name}");
            assert!(eq.bands.len() <= EQ_MAX_BANDS);
            assert!(eq.bands.iter().all(|b| b.gain_db.abs() <= EQ_MAX_DB));
            assert!(low_cut.is_none_or(|hz| (20.0..=300.0).contains(&hz)));
        }
        assert!(Eq::preset("Flat").0.bands.is_empty());
        // Clicked points: shelves near the edges, bells in between, clamped to range.
        assert_eq!(EqBand::at(40.0, 3.0).kind, BandKind::LowShelf);
        assert_eq!(EqBand::at(1000.0, 3.0).kind, BandKind::Bell);
        assert_eq!(EqBand::at(15000.0, 30.0).gain_db, EQ_MAX_DB);
        let fx = Effects {
            eq: Eq::preset("Voice").0,
            ..Default::default()
        };
        let back: Effects = toml::from_str(&toml::to_string_pretty(&fx).unwrap()).unwrap();
        assert_eq!(back, fx);
        let mut too_many = Config::starter(None, None);
        too_many.strips[0].fx.eq.bands = vec![EqBand::at(1000.0, 1.0); EQ_MAX_BANDS + 1];
        assert!(too_many.validate().is_err());
    }

    #[test]
    fn own_eq_presets() {
        let mut cfg = Config::starter(None, None);
        let mut fx = Effects::default();
        fx.low_cut = true;
        fx.low_cut_hz = 110.0;
        fx.eq.bands = vec![EqBand::at(3000.0, 4.0), EqBand::at(200.0, -2.0)];
        assert!(cfg.save_eq_preset(EqPreset::from_effects(" My mic ", &fx)));
        assert_eq!(cfg.eq_presets[0].name, "My mic", "name trimmed");
        // Same name replaces; built-in names are reserved.
        fx.eq.bands.pop();
        assert!(cfg.save_eq_preset(EqPreset::from_effects("My mic", &fx)));
        assert_eq!(
            (cfg.eq_presets.len(), cfg.eq_presets[0].bands.len()),
            (1, 1)
        );
        assert!(!cfg.save_eq_preset(EqPreset::from_effects("voice", &fx)));
        cfg.validate().unwrap();
        // Applying puts the points and low cut on a channel and switches the EQ on.
        let mut other = Effects::default();
        cfg.eq_presets[0].apply(&mut other);
        assert!(
            other.eq.enabled
                && other.low_cut
                && other.low_cut_hz == 110.0
                && other.eq.bands.len() == 1
        );
        // Saved in the config file and back.
        let back: Config = toml::from_str(&toml::to_string_pretty(&cfg).unwrap()).unwrap();
        assert_eq!(back, cfg);
        // Duplicate names in a hand-edited file are caught.
        cfg.eq_presets.push(cfg.eq_presets[0].clone());
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn ducking_sources_are_checked_and_cleaned_up() {
        let mut cfg = Config::starter(Some("hw".into()), Some("mic".into()));
        let music = cfg.strips.iter().position(|s| s.id == "music").unwrap();
        cfg.strips[music].fx.duck = Duck {
            enabled: true,
            sources: vec!["mic".into(), "chat".into()],
            ..Duck::default()
        };
        assert!(cfg.strips[music].fx.active());
        cfg.validate().unwrap();
        let back: Config = toml::from_str(&toml::to_string_pretty(&cfg).unwrap()).unwrap();
        assert_eq!(back, cfg);
        cfg.strips[music].fx.duck.sources.push("music".into());
        assert!(cfg.validate().is_err(), "can't duck for itself");
        cfg.strips[music].fx.duck.sources.pop();
        cfg.remove_strip("chat");
        assert_eq!(
            cfg.strips
                .iter()
                .find(|s| s.id == "music")
                .unwrap()
                .fx
                .duck
                .sources,
            vec!["mic".to_string()]
        );
        cfg.validate().unwrap();
    }

    #[test]
    fn balance_lowers_only_the_other_side() {
        let st = ["FL", "FR"];
        assert_eq!(pan_gains(0.0, &st), vec![1.0, 1.0]);
        assert_eq!(pan_gains(0.5, &st), vec![0.5, 1.0]);
        assert_eq!(pan_gains(-1.0, &st), vec![1.0, 0.0]);
        let s71 = ["FL", "FR", "FC", "LFE", "RL", "RR", "SL", "SR"];
        assert_eq!(
            pan_gains(-0.25, &s71),
            vec![1.0, 0.75, 1.0, 1.0, 1.0, 0.75, 1.0, 0.75]
        );
        assert_eq!(pan_gains(5.0, &st), vec![0.0, 1.0], "clamped");
    }

    #[test]
    fn cue_takes_over_the_cue_output_only() {
        let mut cfg = Config::starter(Some("hw".into()), Some("mic".into()));
        cfg.buses.push(Bus::new(
            "a2",
            "Speakers",
            Kind::Physical {
                device: "hw2".into(),
            },
        ));
        for s in &mut cfg.strips {
            s.buses = vec!["a1".into(), "a2".into()];
        }
        cfg.cue_bus = Some("a1".into());
        cfg.validate().unwrap();
        let sends = |c: &Config, id: &str| c.sends(c.strips.iter().find(|s| s.id == id).unwrap());
        // Nothing cued: normal sends.
        assert_eq!(sends(&cfg, "music"), vec!["a1", "a2"]);
        // Music cued: a1 plays only music; the speakers keep everything.
        cfg.strips.iter_mut().find(|s| s.id == "music").unwrap().cue = true;
        assert_eq!(cfg.cueing(), Some("a1"));
        assert_eq!(sends(&cfg, "music"), vec!["a2", "a1"]);
        assert_eq!(sends(&cfg, "chat"), vec!["a2"]);
        // A cued channel reaches the cue output even if it isn't normally sent there.
        cfg.strips
            .iter_mut()
            .find(|s| s.id == "music")
            .unwrap()
            .buses = vec!["a2".into()];
        assert!(sends(&cfg, "music").contains(&"a1".to_string()));
        // Removing the cue output switches cueing off.
        cfg.remove_bus("a1");
        assert_eq!(cfg.cueing(), None);
        cfg.validate().unwrap();
    }

    #[test]
    fn app_rules() {
        let r = Rule {
            app: "Brave".into(),
            strip: "web".into(),
        };
        assert!(r.matches("Brave", ""));
        assert!(r.matches("", "brave"));
        assert!(!r.matches("Brave Search Helper", ""));
        let r = Rule {
            app: "brave*".into(),
            strip: "web".into(),
        };
        assert!(r.matches("Brave Search Helper", ""));
        assert!(
            Rule {
                app: "*cord".into(),
                strip: "x".into()
            }
            .matches("Discord", "")
        );
        assert!(
            !Rule {
                app: "*cord".into(),
                strip: "x".into()
            }
            .matches("Discordia", "")
        );

        let mut cfg = Config::starter(Some("hw".into()), Some("mic".into()));
        cfg.rules.push(Rule {
            app: "Spotify".into(),
            strip: "music".into(),
        });
        cfg.validate().unwrap();
        cfg.rules.push(Rule {
            app: "x".into(),
            strip: "mic".into(),
        });
        assert!(cfg.validate().is_err(), "physical strips can't take apps");
        cfg.rules.pop();
        cfg.remove_strip("music");
        assert!(cfg.rules.is_empty());
        cfg.validate().unwrap();
    }

    #[test]
    fn ids_from_names() {
        assert_eq!(make_id("Game Chat!", &[]), "game_chat");
        assert_eq!(make_id("Music", &["music"]), "music_2");
        assert_eq!(make_id("???", &[]), "channel");
    }

    #[test]
    fn midi_section_parses_and_validates() {
        let text = r#"
            [[strip]]
            id = "music"
            name = "Music"
            kind = "virtual"
            buses = ["a1"]

            [[bus]]
            id = "a1"
            name = "Out"
            kind = "physical"
            device = "hw"

            [midi]
            device = "nanoKONTROL2"

            [[midi.binding]]
            cc = 0
            target = "music"
            action = "gain"

            [[midi.binding]]
            cc = 48
            target = "a1"
            action = "mute"

            [[midi.binding]]
            cc = 32
            channel = 1
            target = "music"
            action = "route"
            bus = "a1"
        "#;
        let cfg: Config = toml::from_str(text).unwrap();
        cfg.validate().unwrap();
        let midi = cfg.midi.as_ref().unwrap();
        assert_eq!(midi.bindings[2].action, Action::Route { bus: "a1".into() });
        // survives a save/load roundtrip
        let back: Config = toml::from_str(&toml::to_string_pretty(&cfg).unwrap()).unwrap();
        assert_eq!(cfg, back);

        let mut bad = cfg.clone();
        bad.midi.as_mut().unwrap().bindings[0].target = "nope".into();
        assert!(bad.validate().is_err());
    }

    #[test]
    fn gain_conversion() {
        assert_eq!(db_to_linear(0.0), 1.0);
        assert!((db_to_linear(-6.0) - 0.501).abs() < 0.001);
        assert_eq!(db_to_linear(MIN_GAIN_DB), 0.0);
        assert!((db_to_linear(100.0) - db_to_linear(MAX_GAIN_DB)).abs() < 1e-6);
    }
}
