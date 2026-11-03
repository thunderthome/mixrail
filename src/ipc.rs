//! Engine <-> GUI protocol: newline-delimited JSON over a Unix socket in the user's
//! runtime directory. The engine is the only owner of the mixer state; GUIs send
//! commands and receive the full state whenever it changes, plus level meters.

use crate::config::{Action, Config};
use anyhow::{Context, Result, bail};
use log::{debug, warn};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::mpsc::{SyncSender, TrySendError, sync_channel};

pub fn socket_path() -> PathBuf {
    dirs::runtime_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("mixrail.sock")
}

/// GUI -> engine.
#[allow(clippy::enum_variant_names)]
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum Command {
    SetGain {
        id: String,
        db: f32,
    },
    SetMute {
        id: String,
        mute: bool,
    },
    /// Cue a strip on or off.
    SetCue {
        id: String,
        on: bool,
    },
    /// Which output plays cued channels (None = cue off).
    SetCueBus {
        bus: Option<String>,
    },
    /// Balance of a strip, -1 (left) … 1 (right).
    SetPan {
        id: String,
        pan: f32,
    },
    SetRoute {
        strip: String,
        bus: String,
        on: bool,
    },
    /// Change a channel's effects (live while dragging a control).
    SetEffects {
        id: String,
        fx: crate::config::Effects,
    },
    /// Replace the whole layout (adding, editing or removing strips/buses).
    SetConfig {
        config: Config,
    },
    /// Assign the next control touched on the MIDI controller to this function.
    MidiLearn {
        learn: LearnTarget,
    },
    CancelLearn,
    /// Remove the MIDI mapping for this function.
    RemoveBinding {
        learn: LearnTarget,
    },
    /// Move an app's audio to a strip now, and with `remember` also from now on.
    RouteApp {
        app: String,
        strip: String,
        remember: bool,
    },
    /// Remove the rule for an app.
    ForgetApp {
        app: String,
    },
    /// Save the current mix as a scene (replacing one with the same name).
    SaveScene {
        name: String,
    },
    RecallScene {
        name: String,
    },
    DeleteScene {
        name: String,
    },
    /// Save an EQ preset (replacing one of the user's with the same name).
    SaveEqPreset {
        preset: crate::config::EqPreset,
    },
    DeleteEqPreset {
        name: String,
    },
    /// Record what an output plays to a file (in the Music folder).
    StartRecording {
        bus: String,
    },
    StopRecording {
        bus: String,
    },
    /// Play a short sound on each speaker of an output device in turn.
    TestSpeakers {
        device: String,
    },
    /// Sent by the tray icon right after connecting: it isn't a window (needs no meters).
    Tray,
    /// Bring the open mixer window(s) to the front.
    RaiseWindow,
    /// Switch the sound card of a device to another profile (e.g. stereo -> 5.1).
    /// The device gets a new name; outputs and inputs using it follow automatically.
    SetProfile {
        device: String,
        index: u32,
    },
}

/// An app that is currently playing audio.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AppInfo {
    /// The app's name (what rules match on).
    pub name: String,
    /// Strip ids its streams currently play into.
    pub strips: Vec<String>,
    /// Devices it plays into that are not strips (e.g. straight to headphones).
    pub other: Vec<String>,
    /// Number of audio streams (a browser has one per tab).
    pub streams: usize,
}

/// A mixer function a MIDI control can be mapped to.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LearnTarget {
    /// Strip or bus id.
    pub target: String,
    pub action: Action,
}

/// Engine -> GUI.
#[allow(clippy::large_enum_variant)] // one State per change; boxing buys nothing here
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    State {
        config: Config,
        devices: Vec<Device>,
        midi_connected: bool,
        /// MIDI controllers currently plugged in (names usable as `[midi] device`).
        #[serde(default)]
        midi_ports: Vec<String>,
        /// Set while waiting for a control to be touched for MIDI learn.
        #[serde(default)]
        learning: Option<LearnTarget>,
        /// Apps currently playing audio.
        #[serde(default)]
        apps: Vec<AppInfo>,
        /// Outputs being recorded.
        #[serde(default)]
        recordings: Vec<RecordingInfo>,
        /// How many mixer windows are open.
        #[serde(default)]
        windows: usize,
    },
    /// Peak level per channel (linear) for each strip/bus id.
    Meters {
        levels: HashMap<String, [f32; 2]>,
        /// What the effects are doing, per strip with effects on.
        #[serde(default)]
        fx: HashMap<String, FxStatus>,
    },
    Error {
        message: String,
    },
    /// Progress of a speaker test: the channel sounding now, None when done.
    SpeakerTest {
        device: String,
        channel: Option<String>,
    },
    /// Come to the front (the tray icon was clicked).
    Raise,
}

/// A running recording.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RecordingInfo {
    pub bus: String,
    pub path: String,
    /// Start time, seconds since 1970.
    pub started: u64,
}

/// Live effects readout for the GUI.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct FxStatus {
    /// Input level before the fader, dBFS.
    pub level_db: f32,
    pub gate_open: bool,
    /// Compressor gain reduction, dB.
    pub reduction_db: f32,
    /// Limiter gain reduction, dB.
    #[serde(default)]
    pub limit_db: f32,
    /// Ducking: how far it's turned down now, and how loud its sources are.
    #[serde(default)]
    pub duck_db: f32,
    #[serde(default)]
    pub duck_source_db: f32,
    /// How much the de-esser is turning sharp sounds down (dB).
    #[serde(default)]
    pub deess_db: f32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Device {
    /// PipeWire node.name, what goes into `device = "..."`.
    pub name: String,
    pub description: String,
    /// true for outputs (speakers, headphones), false for inputs (mics).
    pub output: bool,
    /// Its audio channels, e.g. ["FL", "FR", "FC", "LFE", "RL", "RR"].
    #[serde(default)]
    pub channels: Vec<String>,
    /// The sound card it belongs to; the device's name changes when the card switches
    /// profile, the card stays.
    #[serde(default)]
    pub card: Option<u32>,
    /// The card's profiles (e.g. "Analog Stereo Output", "Analog Surround 5.1 Output").
    #[serde(default)]
    pub profiles: Vec<Profile>,
    /// The card's current profile index.
    #[serde(default)]
    pub profile: Option<u32>,
}

/// One way a sound card can be set up.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Profile {
    pub index: u32,
    pub description: String,
    /// False when the card says it can't be used now (e.g. nothing plugged in).
    pub available: bool,
    /// It has an output (speakers/headphones).
    pub outputs: bool,
    /// It has an input (microphone/line-in).
    pub inputs: bool,
}

/// What the server threads hand to the engine's main loop.
pub enum Incoming {
    Connected(u64, SyncSender<String>),
    Command(u64, Command),
    Disconnected(u64),
}

/// Start listening. Fails if another engine is already serving the socket.
pub fn serve(send: impl Fn(Incoming) + Send + Clone + 'static) -> Result<()> {
    let path = socket_path();
    if UnixStream::connect(&path).is_ok() {
        bail!("another mixrail engine is already running");
    }
    let _ = std::fs::remove_file(&path);
    let listener =
        UnixListener::bind(&path).with_context(|| format!("binding {}", path.display()))?;
    std::thread::Builder::new()
        .name("ipc-accept".into())
        .spawn(move || {
            for (client, stream) in (1u64..).zip(listener.incoming()) {
                match stream {
                    Ok(stream) => spawn_client(client, stream, send.clone()),
                    Err(e) => warn!("ipc accept: {e}"),
                }
            }
        })?;
    Ok(())
}

fn spawn_client(client: u64, stream: UnixStream, send: impl Fn(Incoming) + Send + 'static) {
    let Ok(mut writer) = stream.try_clone() else {
        return;
    };
    // Bounded: if a GUI stops reading we drop its messages instead of growing forever.
    let (tx, rx) = sync_channel::<String>(256);
    std::thread::spawn(move || {
        for line in rx {
            if writer.write_all(line.as_bytes()).is_err() || writer.write_all(b"\n").is_err() {
                break;
            }
        }
    });
    send(Incoming::Connected(client, tx));
    std::thread::spawn(move || {
        for line in BufReader::new(stream).lines() {
            let Ok(line) = line else { break };
            match serde_json::from_str::<Command>(&line) {
                Ok(cmd) => send(Incoming::Command(client, cmd)),
                Err(e) => debug!("ipc: bad command {line:?}: {e}"),
            }
        }
        send(Incoming::Disconnected(client));
    });
}

/// Connected GUI clients, owned by the engine.
#[derive(Default)]
pub struct Clients(HashMap<u64, (SyncSender<String>, bool)>);

impl Clients {
    pub fn add(&mut self, id: u64, tx: SyncSender<String>) {
        self.0.insert(id, (tx, true));
    }
    pub fn remove(&mut self, id: u64) {
        self.0.remove(&id);
    }
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
    /// Mark a client as not being a window (the tray icon).
    pub fn set_window(&mut self, id: u64, window: bool) {
        if let Some(c) = self.0.get_mut(&id) {
            c.1 = window;
        }
    }
    /// Number of mixer windows connected.
    pub fn windows(&self) -> usize {
        self.0.values().filter(|c| c.1).count()
    }
    pub fn send_to(&mut self, id: u64, ev: &Event) {
        if let (Some((tx, _)), Ok(line)) = (self.0.get(&id), serde_json::to_string(ev)) {
            let _ = tx.try_send(line);
        }
    }
    pub fn broadcast(&mut self, ev: &Event) {
        let Ok(line) = serde_json::to_string(ev) else {
            return;
        };
        self.0.retain(|_, (tx, _)| {
            !matches!(
                tx.try_send(line.clone()),
                Err(TrySendError::Disconnected(_))
            )
        });
    }
}
