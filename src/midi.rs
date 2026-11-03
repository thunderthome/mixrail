//! MIDI controller connection with hotplug: finds the controller by port name, reconnects
//! when it is replugged, and sends feedback (button LEDs, motorised faders).

use crate::config::Control;
use log::{info, warn};
use midir::{Ignore, MidiInput, MidiInputConnection, MidiOutput, MidiOutputConnection};
use std::time::SystemTime;

const CLIENT: &str = "mixrail";

/// A message from a control on the controller.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MidiMsg {
    /// MIDI channel 1-16.
    pub channel: u8,
    pub control: Control,
    /// 0..=control.max(). For notes: the velocity, 0 when released.
    pub value: u16,
}

/// Decode the messages controls send: Control Change, Note On/Off and Pitch Bend.
pub fn parse(msg: &[u8]) -> Option<MidiMsg> {
    let [status, a, b] = *msg else { return None };
    let channel = (status & 0x0F) + 1;
    let (control, value) = match status & 0xF0 {
        0xB0 => (Control::Cc(a), b as u16),
        0x90 => (Control::Note(a), b as u16), // velocity 0 means released
        0x80 => (Control::Note(a), 0),
        0xE0 => (
            Control::PitchBend,
            (a as u16 & 0x7F) | ((b as u16 & 0x7F) << 7),
        ),
        _ => return None,
    };
    Some(MidiMsg {
        channel,
        control,
        value,
    })
}

/// Encode a feedback message: the same kind of message the control sends.
pub fn encode(channel: Option<u8>, control: Control, value: u16) -> [u8; 3] {
    let ch = channel.unwrap_or(1).clamp(1, 16) - 1;
    let v7 = value.min(127) as u8;
    match control {
        Control::Cc(n) => [0xB0 | ch, n, v7],
        Control::Note(n) => [0x90 | ch, n, v7],
        Control::PitchBend => {
            let v = value.min(16383);
            [0xE0 | ch, (v & 0x7F) as u8, (v >> 7) as u8]
        }
    }
}

/// Names of the MIDI controllers currently available (the ALSA client names,
/// e.g. "nanoKONTROL2"), excluding system and our own ports.
pub fn list_devices() -> Vec<String> {
    let Ok(mi) = MidiInput::new(CLIENT) else {
        return vec![];
    };
    let mut names: Vec<String> = mi
        .ports()
        .iter()
        .filter_map(|p| mi.port_name(p).ok())
        // midir names ports "<client>:<port> <client id>:<port id>".
        .filter_map(|n| n.split(':').next().map(str::to_string))
        .filter(|c| c != CLIENT && c != "Midi Through" && !c.starts_with("PipeWire"))
        .collect();
    names.sort();
    names.dedup();
    names
}

pub struct Controller {
    /// Substring of the port name to look for.
    pub device: String,
    port_name: Option<String>,
    input: Option<MidiInputConnection<()>>,
    output: Option<MidiOutputConnection>,
    /// Modification time of /dev/snd. It changes whenever any sound/MIDI device is
    /// plugged or unplugged, which catches a replug faster than our poll interval
    /// (the port name is identical afterwards, but our old connection is dead).
    dev_snd_mtime: Option<SystemTime>,
}

fn dev_snd_mtime() -> Option<SystemTime> {
    std::fs::metadata("/dev/snd")
        .and_then(|m| m.modified())
        .ok()
}

impl Controller {
    pub fn new(device: String) -> Self {
        Controller {
            device,
            port_name: None,
            input: None,
            output: None,
            dev_snd_mtime: dev_snd_mtime(),
        }
    }

    pub fn is_connected(&self) -> bool {
        self.input.is_some()
    }

    /// Connect if the controller is present, drop the connection if it vanished.
    /// Returns true when a (re)connection was just made, so the caller can refresh LEDs.
    pub fn poll(&mut self, on_msg: impl Fn(MidiMsg) + Send + 'static) -> bool {
        let present = match MidiInput::new(CLIENT) {
            Ok(mi) => mi
                .ports()
                .iter()
                .filter_map(|p| mi.port_name(p).ok())
                .find(|n| n.contains(&self.device)),
            Err(e) => {
                warn!("MIDI unavailable: {e}");
                return false;
            }
        };
        let mtime = dev_snd_mtime();
        let devices_changed = mtime != self.dev_snd_mtime;
        self.dev_snd_mtime = mtime;
        match (&self.port_name, present) {
            (Some(cur), Some(now)) if *cur == now && !devices_changed => false,
            (Some(cur), None) => {
                info!("MIDI controller disconnected: {cur}");
                self.disconnect();
                false
            }
            (_, Some(now)) => {
                self.disconnect();
                self.connect(&now, on_msg)
            }
            (None, None) => false,
        }
    }

    fn disconnect(&mut self) {
        self.input = None;
        self.output = None;
        self.port_name = None;
    }

    fn connect(&mut self, port_name: &str, on_msg: impl Fn(MidiMsg) + Send + 'static) -> bool {
        let mut mi = match MidiInput::new(CLIENT) {
            Ok(mi) => mi,
            Err(e) => {
                warn!("MIDI input: {e}");
                return false;
            }
        };
        mi.ignore(Ignore::All);
        let Some(port) = mi
            .ports()
            .into_iter()
            .find(|p| mi.port_name(p).ok().as_deref() == Some(port_name))
        else {
            return false;
        };
        let input = mi.connect(
            &port,
            "control-in",
            move |_ts, msg, _| {
                if let Some(m) = parse(msg) {
                    on_msg(m);
                }
            },
            (),
        );
        match input {
            Ok(conn) => self.input = Some(conn),
            Err(e) => {
                warn!("cannot open MIDI input {port_name}: {e}");
                return false;
            }
        }
        // LEDs are optional: some controllers have no output port.
        self.output = MidiOutput::new(CLIENT).ok().and_then(|mo| {
            let port = mo
                .ports()
                .into_iter()
                .find(|p| mo.port_name(p).is_ok_and(|n| n.contains(&self.device)))?;
            mo.connect(&port, "led-out").ok()
        });
        info!(
            "MIDI controller connected: {port_name}{}",
            if self.output.is_some() {
                " (with feedback)"
            } else {
                ""
            }
        );
        self.port_name = Some(port_name.to_string());
        true
    }

    /// Send a control its state: lights a button LED (most controllers light a button
    /// when they receive the message it sends) or moves a motorised fader / LED ring.
    pub fn send_feedback(&mut self, channel: Option<u8>, control: Control, value: u16) {
        if let Some(out) = &mut self.output
            && let Err(e) = out.send(&encode(channel, control, value))
        {
            warn!("MIDI feedback send failed: {e}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_common_controller_messages() {
        assert_eq!(
            parse(&[0xB0, 7, 100]),
            Some(MidiMsg {
                channel: 1,
                control: Control::Cc(7),
                value: 100
            })
        );
        assert_eq!(parse(&[0x92, 36, 90]).unwrap().control, Control::Note(36));
        assert_eq!(parse(&[0x92, 36, 90]).unwrap().channel, 3);
        assert_eq!(
            parse(&[0x80, 36, 64]).unwrap().value,
            0,
            "note off = released"
        );
        let pb = parse(&[0xE1, 0x7F, 0x7F]).unwrap();
        assert_eq!(
            (pb.control, pb.channel, pb.value),
            (Control::PitchBend, 2, 16383)
        );
        assert_eq!(parse(&[0xF8]), None, "clock ticks are ignored");
        assert_eq!(parse(&[0xC0, 1, 0]), None, "program change is ignored");
    }

    #[test]
    fn feedback_roundtrips() {
        for (ch, control, value) in [
            (Some(1), Control::Cc(48), 127),
            (Some(10), Control::Note(16), 0),
            (Some(3), Control::PitchBend, 12345),
        ] {
            let m = parse(&encode(ch, control, value)).unwrap();
            assert_eq!((Some(m.channel), m.control, m.value), (ch, control, value));
        }
    }
}
