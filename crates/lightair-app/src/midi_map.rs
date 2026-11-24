//! MIDI mappings (docs/13, "Built-in replacements"): a controller's fader, knob, button or pad
//! drives a track's volume or mute, a scene, a soundboard pad or any shortcut action
//! (Settings › MIDI). What a control shows comes back to the device as it changes here, so motor
//! faders move with Lightair's faders and buttons light for what's on (feedback is sent to the
//! output port with the input's name). Mappings are this computer's (its devices), so they're
//! preferences, not workspace settings.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use lightair_model::Id;
use serde::{Deserialize, Serialize};

use crate::shortcuts::GlobalAction;

/// What a control drives.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, Default)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum MidiTarget {
    /// Not chosen yet.
    #[default]
    None,
    /// A track's fader (a control's 0–127 over the fader's range).
    Volume { track: Id },
    /// A track's mute (each press toggles it).
    Mute { track: Id },
    /// Puts a scene on air.
    Scene { scene: Id },
    /// Plays a soundboard pad.
    Pad { pad: Id },
    /// Any shortcut action (record, go live, clip, …).
    Action { action: GlobalAction },
}

/// One control and what it drives.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MidiMap {
    /// The device's port name ("" any).
    pub device: String,
    /// "cc" or "note".
    pub kind: String,
    /// 1–16 (0 any).
    pub channel: u8,
    pub number: u8,
    pub target: MidiTarget,
}

impl MidiMap {
    pub fn matches(&self, m: &Incoming) -> bool {
        (self.device.is_empty() || self.device == m.device)
            && self.kind == m.kind
            && (self.channel == 0 || self.channel == m.channel)
            && self.number == m.number
    }

    /// "nanoKONTROL2 · CC 0 (channel 1)".
    pub fn control(&self) -> String {
        let what = if self.kind == "note" { "Note" } else { "CC" };
        let dev = if self.device.is_empty() { "Any device".to_string() } else { self.device.clone() };
        format!("{dev} · {what} {} (channel {})", self.number, self.channel.max(1))
    }
}

/// A message from a device: note presses and control changes.
#[derive(Debug, Clone, PartialEq)]
pub struct Incoming {
    pub device: String,
    pub kind: &'static str,
    pub channel: u8,
    pub number: u8,
    pub value: u8,
}

/// Opens every MIDI input while `wanted`, and sends feedback to their outputs.
pub struct MidiHub {
    rx: Receiver<Incoming>,
    wanted: Arc<AtomicBool>,
    outputs: Mutex<HashMap<String, midir::MidiOutputConnection>>,
}

impl MidiHub {
    pub fn start() -> MidiHub {
        let (tx, rx) = channel();
        let wanted = Arc::new(AtomicBool::new(false));
        let w = wanted.clone();
        std::thread::Builder::new().name("lightair-midi-map".into()).spawn(move || watch(w, tx)).ok();
        MidiHub { rx, wanted, outputs: Mutex::new(HashMap::new()) }
    }

    /// Whether anything listens (mappings exist, or one is being learned).
    pub fn set_wanted(&self, on: bool) {
        self.wanted.store(on, Ordering::Relaxed);
        if !on {
            self.outputs.lock().map(|mut o| o.clear()).ok();
        }
    }

    pub fn next(&self) -> Option<Incoming> {
        self.rx.try_recv().ok()
    }

    /// Sends `bytes` to the output named `device` (opened the first time).
    pub fn send(&self, device: &str, bytes: &[u8]) {
        let Ok(mut outputs) = self.outputs.lock() else { return };
        if !outputs.contains_key(device) {
            let Ok(out) = midir::MidiOutput::new("Lightair") else { return };
            let Some(port) = out.ports().into_iter().find(|p| out.port_name(p).is_ok_and(|n| n == device)) else { return };
            let Ok(conn) = out.connect(&port, "lightair-feedback") else { return };
            outputs.insert(device.to_string(), conn);
        }
        if let Some(conn) = outputs.get_mut(device) {
            let _ = conn.send(bytes);
        }
    }

    /// Lights or moves a control: `value` 0–127 as the map's kind (a note off when 0).
    pub fn feedback(&self, map: &MidiMap, device: &str, value: u8) {
        let ch = map.channel.clamp(1, 16) - 1;
        let bytes = if map.kind == "note" { [0x90 | ch, map.number, value] } else { [0xB0 | ch, map.number, value] };
        self.send(device, &bytes);
    }
}

fn watch(wanted: Arc<AtomicBool>, tx: Sender<Incoming>) {
    let mut open: HashMap<String, midir::MidiInputConnection<()>> = HashMap::new();
    loop {
        std::thread::sleep(Duration::from_secs(2));
        if !wanted.load(Ordering::Relaxed) {
            open.clear();
            continue;
        }
        let Ok(probe) = midir::MidiInput::new("Lightair") else { continue };
        let names: Vec<(String, midir::MidiInputPort)> =
            probe.ports().into_iter().filter_map(|p| Some((probe.port_name(&p).ok()?, p))).collect();
        open.retain(|name, _| names.iter().any(|(n, _)| n == name));
        for (name, port) in names {
            if open.contains_key(&name) {
                continue;
            }
            let Ok(input) = midir::MidiInput::new("Lightair") else { continue };
            let (tx, device) = (tx.clone(), name.clone());
            let conn = input.connect(
                &port,
                "lightair-mappings",
                move |_, bytes, _| {
                    if let Some((kind, channel, number, value)) = message(bytes) {
                        let _ = tx.send(Incoming { device: device.clone(), kind, channel, number, value });
                    }
                },
                (),
            );
            if let Ok(conn) = conn {
                open.insert(name, conn);
            }
        }
    }
}

/// Note presses (and releases, as value 0) and control changes: (kind, channel 1–16, number, value).
fn message(bytes: &[u8]) -> Option<(&'static str, u8, u8, u8)> {
    let (&status, number, value) = (bytes.first()?, *bytes.get(1)?, *bytes.get(2)?);
    let channel = (status & 0x0f) + 1;
    match status & 0xf0 {
        0x90 => Some(("note", channel, number, value)),
        0x80 => Some(("note", channel, number, 0)),
        0xb0 => Some(("cc", channel, number, value)),
        _ => None,
    }
}
