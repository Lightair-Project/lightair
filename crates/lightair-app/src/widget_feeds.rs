//! Live data for the widgets that show what happens on this computer (docs/13, "Built-in
//! replacements"): the Input overlay's keys, mouse buttons and game controller (`input`), live
//! Captions from the speech helper (`captions`), and the Audio visualiser's spectrum of the
//! program (`spectrum`). Each runs only while a widget of its kind exists, and sends to the
//! widget pages on this computer only.

use std::sync::Arc;
use std::time::{Duration, Instant};

use lightair_engine::Engine;
use lightair_widgets::Widgets;
use serde_json::{Value, json};

pub fn start(widgets: Widgets, engine: Arc<Engine>) {
    {
        let w = widgets.clone();
        std::thread::Builder::new().name("lightair-input-feed".into()).spawn(move || input(w)).ok();
    }
    {
        let w = widgets.clone();
        std::thread::Builder::new().name("lightair-captions-feed".into()).spawn(move || captions(w)).ok();
    }
    std::thread::Builder::new().name("lightair-spectrum-feed".into()).spawn(move || spectrum(widgets, engine)).ok();
}

fn now_ms() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as i64)
}

/// A widget of `kind`'s settings, for each one there is.
fn settings_of(w: &Widgets, kind: &str) -> Vec<serde_json::Map<String, Value>> {
    w.list().into_iter().filter(|(_, k)| k == kind).filter_map(|(id, _)| w.fields(&id)).collect()
}

// ---------------------------------------------------------------------------------------------
// Input overlay

/// A key's name on a keycap, from the platform's key code (`None`: a modifier alone).
fn key_label(code: u16) -> Option<&'static str> {
    #[cfg(target_os = "macos")]
    let name = match code {
        0 => "A", 1 => "S", 2 => "D", 3 => "F", 4 => "H", 5 => "G", 6 => "Z", 7 => "X", 8 => "C", 9 => "V",
        11 => "B", 12 => "Q", 13 => "W", 14 => "E", 15 => "R", 16 => "Y", 17 => "T", 18 => "1", 19 => "2",
        20 => "3", 21 => "4", 22 => "6", 23 => "5", 24 => "=", 25 => "9", 26 => "7", 27 => "-", 28 => "8",
        29 => "0", 30 => "]", 31 => "O", 32 => "U", 33 => "[", 34 => "I", 35 => "P", 36 => "Return", 37 => "L",
        38 => "J", 39 => "'", 40 => "K", 41 => ";", 42 => "\\", 43 => ",", 44 => "/", 45 => "N", 46 => "M",
        47 => ".", 48 => "Tab", 49 => "Space", 50 => "`", 51 => "⌫", 53 => "Esc", 76 => "Enter", 96 => "F5",
        97 => "F6", 98 => "F7", 99 => "F3", 100 => "F8", 101 => "F9", 103 => "F11", 105 => "F13", 107 => "F14",
        109 => "F10", 111 => "F12", 113 => "F15", 115 => "Home", 116 => "PgUp", 117 => "⌦", 118 => "F4",
        119 => "End", 120 => "F2", 121 => "PgDn", 122 => "F1", 123 => "←", 124 => "→", 125 => "↓", 126 => "↑",
        _ => return None,
    };
    #[cfg(not(target_os = "macos"))]
    let name = match code {
        0x08 => "⌫", 0x09 => "Tab", 0x0D => "Enter", 0x1B => "Esc", 0x20 => "Space", 0x21 => "PgUp", 0x22 => "PgDn",
        0x23 => "End", 0x24 => "Home", 0x25 => "←", 0x26 => "↑", 0x27 => "→", 0x28 => "↓", 0x2E => "Del",
        0x30 => "0", 0x31 => "1", 0x32 => "2", 0x33 => "3", 0x34 => "4", 0x35 => "5", 0x36 => "6", 0x37 => "7",
        0x38 => "8", 0x39 => "9", 0x41 => "A", 0x42 => "B", 0x43 => "C", 0x44 => "D", 0x45 => "E", 0x46 => "F",
        0x47 => "G", 0x48 => "H", 0x49 => "I", 0x4A => "J", 0x4B => "K", 0x4C => "L", 0x4D => "M", 0x4E => "N",
        0x4F => "O", 0x50 => "P", 0x51 => "Q", 0x52 => "R", 0x53 => "S", 0x54 => "T", 0x55 => "U", 0x56 => "V",
        0x57 => "W", 0x58 => "X", 0x59 => "Y", 0x5A => "Z", 0x70 => "F1", 0x71 => "F2", 0x72 => "F3", 0x73 => "F4",
        0x74 => "F5", 0x75 => "F6", 0x76 => "F7", 0x77 => "F8", 0x78 => "F9", 0x79 => "F10", 0x7A => "F11",
        0x7B => "F12", 0xBA => ";", 0xBB => "=", 0xBC => ",", 0xBD => "-", 0xBE => ".", 0xBF => "/", 0xC0 => "`",
        0xDB => "[", 0xDC => "\\", 0xDD => "]", 0xDE => "'",
        _ => return None,
    };
    Some(name)
}

/// The modifiers held, as keycaps.
fn modifier_labels(m: u16) -> Vec<&'static str> {
    use lightair_platform::input::modifiers::*;
    let mut out = Vec::new();
    if cfg!(target_os = "macos") {
        if m & CONTROL != 0 {
            out.push("⌃");
        }
        if m & ALT != 0 {
            out.push("⌥");
        }
        if m & SHIFT != 0 {
            out.push("⇧");
        }
        if m & COMMAND != 0 {
            out.push("⌘");
        }
    } else {
        if m & CONTROL != 0 {
            out.push("Ctrl");
        }
        if m & COMMAND != 0 {
            out.push("Win");
        }
        if m & ALT != 0 {
            out.push("Alt");
        }
        if m & SHIFT != 0 {
            out.push("Shift");
        }
    }
    out
}

/// A key shown alone (not typing): function, arrow and editing keys.
fn shown_alone(label: &str) -> bool {
    label.len() > 1 || matches!(label, "←" | "→" | "↑" | "↓" | "⌫" | "⌦")
}

fn input(w: Widgets) {
    use lightair_platform::input::{Event, Recorder, modifiers};
    let mut recorder: Option<Recorder> = None;
    let mut pads: Option<gilrs::Gilrs> = None;
    let mut recent: Vec<Value> = Vec::new();
    let mut mouse = [false; 3];
    let mut last_sent = Value::Null;
    loop {
        let wanted = settings_of(&w, "input-overlay");
        if wanted.is_empty() {
            recorder = None;
            pads = None;
            std::thread::sleep(Duration::from_secs(1));
            continue;
        }
        // plain typing only when an overlay asks for it
        let typing = wanted.iter().any(|f| f.get("shortcutsOnly").and_then(Value::as_bool) == Some(false));
        let keys_or_mouse = wanted.iter().any(|f| f.get("showKeys").and_then(Value::as_bool) != Some(false) || f.get("showMouse").and_then(Value::as_bool) != Some(false));
        let pad_wanted = wanted.iter().any(|f| f.get("showPad").and_then(Value::as_bool) != Some(false));
        if keys_or_mouse && recorder.is_none() {
            if !lightair_platform::input::can_listen() {
                lightair_platform::input::request_listen();
            }
            recorder = Some(Recorder::start(10));
        }
        if pad_wanted && pads.is_none() {
            pads = gilrs::Gilrs::new().ok();
        }
        if let Some(r) = &recorder {
            for s in r.take() {
                match s.event {
                    Event::Down { button, .. } if (button as usize) < 3 => mouse[button as usize] = true,
                    Event::Up { button, .. } if (button as usize) < 3 => mouse[button as usize] = false,
                    Event::KeyDown { code, modifiers: m } => {
                        let Some(label) = key_label(code) else { continue };
                        let shortcut = m & (modifiers::COMMAND | modifiers::CONTROL | modifiers::ALT) != 0;
                        if !(typing || shortcut || shown_alone(label)) {
                            continue;
                        }
                        let mut keys: Vec<&str> = modifier_labels(m);
                        keys.push(label);
                        recent.push(json!({"keys": keys, "at": now_ms()}));
                        if recent.len() > 8 {
                            recent.remove(0);
                        }
                    }
                    _ => {}
                }
            }
        }
        let mut pad_list = Vec::new();
        if let Some(g) = pads.as_mut() {
            while g.next_event().is_some() {}
            for (_, pad) in g.gamepads() {
                use gilrs::{Axis, Button};
                let b = |x: Button| pad.is_pressed(x);
                let a = |x: Axis| pad.value(x);
                let trigger = |x: Button| pad.button_data(x).map_or(0.0, |d| d.value());
                pad_list.push(json!({
                    "name": pad.name(),
                    "buttons": {
                        "south": b(Button::South), "east": b(Button::East), "west": b(Button::West), "north": b(Button::North),
                        "lb": b(Button::LeftTrigger), "rb": b(Button::RightTrigger),
                        "select": b(Button::Select), "start": b(Button::Start),
                        "up": b(Button::DPadUp), "down": b(Button::DPadDown), "left": b(Button::DPadLeft), "right": b(Button::DPadRight),
                    },
                    "lt": trigger(Button::LeftTrigger2), "rt": trigger(Button::RightTrigger2),
                    "lx": a(Axis::LeftStickX), "ly": -a(Axis::LeftStickY), "rx": a(Axis::RightStickX), "ry": -a(Axis::RightStickY),
                }));
            }
        }
        let now = now_ms();
        recent.retain(|c| now - c["at"].as_i64().unwrap_or(0) < 10_000);
        let data = json!({
            "recent": recent,
            "mouse": {"left": mouse[0], "right": mouse[1], "middle": mouse[2]},
            "pads": pad_list,
            "listening": recorder.as_ref().is_some_and(|r| r.listening),
        });
        if data != last_sent {
            w.set_data("input", data.clone());
            last_sent = data;
        }
        std::thread::sleep(Duration::from_millis(30));
    }
}

// ---------------------------------------------------------------------------------------------
// Captions

fn captions(w: Widgets) {
    use std::io::BufRead;
    loop {
        std::thread::sleep(Duration::from_secs(1));
        let wanted = settings_of(&w, "captions");
        if !cfg!(target_os = "macos") || wanted.is_empty() {
            continue;
        }
        let Some(helper) = std::env::current_exe().ok().and_then(|e| Some(e.parent()?.join("lightair-speech"))).filter(|p| p.exists())
        else {
            w.set_data("captions", json!({"lines": [], "partial": "", "error": "This build has no speech helper", "at": now_ms()}));
            std::thread::sleep(Duration::from_secs(30));
            continue;
        };
        // the first captions widget's choices: what's said in, and what to translate it to
        let field = |k: &str| wanted[0].get(k).and_then(Value::as_str).unwrap_or("").trim().to_string();
        let (language, translate) = (field("language"), field("translateTo"));
        let mut cmd = std::process::Command::new(&helper);
        cmd.arg(if language.is_empty() { "-" } else { language.as_str() }).arg("").arg(&translate);
        let child = cmd.stdin(std::process::Stdio::piped()).stdout(std::process::Stdio::piped()).spawn();
        let Ok(mut child) = child else { continue };
        let Some(out) = child.stdout.take() else { continue };
        let (tx, rx) = std::sync::mpsc::channel::<String>();
        std::thread::spawn(move || {
            for line in std::io::BufReader::new(out).lines().map_while(Result::ok) {
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
        let mut lines: Vec<Value> = Vec::new();
        let mut partial = String::new();
        let mut error = String::new();
        let started = (language.clone(), translate.clone());
        let mut checked = Instant::now();
        loop {
            match rx.recv_timeout(Duration::from_millis(500)) {
                Ok(line) => {
                    if let Some(text) = line.strip_prefix("TEXT ") {
                        partial = text.to_string();
                    } else if line == "DONE" {
                        if !partial.is_empty() {
                            lines.push(json!({"text": std::mem::take(&mut partial)}));
                            if lines.len() > 12 {
                                lines.remove(0);
                            }
                        }
                    } else if let Some(t) = line.strip_prefix("TRANSLATION ") {
                        if let Some(last) = lines.last_mut() {
                            last["translation"] = json!(t);
                        }
                    } else if let Some(why) = line.strip_prefix("ERROR ") {
                        error = why.to_string();
                        tracing::warn!(%why, "captions");
                    }
                    w.set_data("captions", json!({"lines": lines, "partial": partial, "error": error, "at": now_ms()}));
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                Err(_) => break,
            }
            // stop with the last widget; start again when its languages change
            if checked.elapsed() > Duration::from_secs(1) {
                checked = Instant::now();
                let now = settings_of(&w, "captions");
                let langs = now.first().map(|f| {
                    let g = |k: &str| f.get(k).and_then(Value::as_str).unwrap_or("").trim().to_string();
                    (g("language"), g("translateTo"))
                });
                if langs.as_ref() != Some(&started) {
                    break;
                }
            }
        }
        let _ = child.kill();
        let _ = child.wait();
    }
}

// ---------------------------------------------------------------------------------------------
// Audio visualiser

/// The spectrum's bands (log-spaced from 40 Hz to 7.5 kHz).
const BANDS: usize = 48;
const FFT: usize = 1024;

/// In-place radix-2 FFT of (re, im).
fn fft(re: &mut [f32], im: &mut [f32]) {
    let n = re.len();
    let mut j = 0;
    for i in 1..n {
        let mut bit = n >> 1;
        while j & bit != 0 {
            j ^= bit;
            bit >>= 1;
        }
        j |= bit;
        if i < j {
            re.swap(i, j);
            im.swap(i, j);
        }
    }
    let mut len = 2;
    while len <= n {
        let ang = -2.0 * std::f32::consts::PI / len as f32;
        for start in (0..n).step_by(len) {
            for k in 0..len / 2 {
                let (s, c) = (ang * k as f32).sin_cos();
                let (a, b) = (start + k, start + k + len / 2);
                let (tr, ti) = (re[b] * c - im[b] * s, re[b] * s + im[b] * c);
                re[b] = re[a] - tr;
                im[b] = im[a] - ti;
                re[a] += tr;
                im[a] += ti;
            }
        }
        len <<= 1;
    }
}

/// Bands (0..1) of the last [`FFT`] samples at `rate`.
fn bands(samples: &[f32], rate: f32) -> Vec<f32> {
    let mut re: Vec<f32> = samples
        .iter()
        .enumerate()
        .map(|(i, s)| s * (0.5 - 0.5 * (2.0 * std::f32::consts::PI * i as f32 / (FFT as f32 - 1.0)).cos()))
        .collect();
    let mut im = vec![0.0; FFT];
    fft(&mut re, &mut im);
    let mag: Vec<f32> = (0..FFT / 2).map(|k| (re[k] * re[k] + im[k] * im[k]).sqrt() / (FFT as f32 / 4.0)).collect();
    let (lo, hi) = (40.0f32, 7500.0f32);
    (0..BANDS)
        .map(|b| {
            let f0 = lo * (hi / lo).powf(b as f32 / BANDS as f32);
            let f1 = lo * (hi / lo).powf((b + 1) as f32 / BANDS as f32);
            let (k0, k1) = ((f0 / rate * FFT as f32) as usize, ((f1 / rate * FFT as f32) as usize).max((f0 / rate * FFT as f32) as usize + 1));
            let peak = mag[k0.min(FFT / 2 - 1)..k1.min(FFT / 2)].iter().cloned().fold(0.0f32, f32::max);
            let db = 20.0 * peak.max(1e-9).log10();
            ((db + 65.0) / 55.0).clamp(0.0, 1.0)
        })
        .collect()
}

fn spectrum(w: Widgets, engine: Arc<Engine>) {
    let rate = lightair_engine::audio_host::LISTEN_RATE as f32;
    loop {
        if !w.has_kind("visualiser") {
            std::thread::sleep(Duration::from_secs(1));
            continue;
        }
        let Some(mut ring) = engine.call(|e| e.reconciler.listen_program()) else {
            std::thread::sleep(Duration::from_secs(2));
            continue;
        };
        let mut window: Vec<f32> = vec![0.0; FFT];
        let mut quiet_sent = false;
        while w.has_kind("visualiser") {
            std::thread::sleep(Duration::from_millis(33));
            let n = ring.slots();
            if let Ok(chunk) = ring.read_chunk(n) {
                let (a, b) = chunk.as_slices();
                for s in a.iter().chain(b) {
                    window.push(*s as f32 / 32768.0);
                }
                chunk.commit_all();
            }
            if window.len() > FFT {
                window.drain(..window.len() - FFT);
            }
            let b = bands(&window, rate);
            let silent = b.iter().all(|v| *v <= 0.0);
            if silent && quiet_sent {
                continue;
            }
            quiet_sent = silent;
            w.set_data("spectrum", json!({"bands": b}));
        }
    }
}
