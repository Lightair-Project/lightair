//! Shortcuts that work while Lightair isn't in front (docs/05-interface-shell.md, "Global
//! shortcuts"), through Carbon's hot keys: macOS delivers them to Lightair whatever app is in
//! front, without the Accessibility or Input Monitoring permission that watching every key
//! would need. Register and handle them on the main thread.

use std::collections::HashMap;
use std::ffi::c_void;
use std::sync::{Mutex, OnceLock};

use serde::{Deserialize, Serialize};

/// What a global shortcut does.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum GlobalAction {
    ToggleRecording,
    ToggleLive,
    Take,
    AddMarker,
    SaveClip,
    MuteMics,
    Scene(u8),
}

impl GlobalAction {
    pub const ALL: &'static [GlobalAction] = &[
        GlobalAction::ToggleRecording,
        GlobalAction::ToggleLive,
        GlobalAction::Take,
        GlobalAction::AddMarker,
        GlobalAction::SaveClip,
        GlobalAction::MuteMics,
        GlobalAction::Scene(1),
        GlobalAction::Scene(2),
        GlobalAction::Scene(3),
        GlobalAction::Scene(4),
        GlobalAction::Scene(5),
    ];

    pub fn label(self) -> String {
        match self {
            GlobalAction::ToggleRecording => "Start or stop recording".into(),
            GlobalAction::ToggleLive => "Go live or end".into(),
            GlobalAction::Take => "Take (cued scene on air)".into(),
            GlobalAction::AddMarker => "Add marker".into(),
            GlobalAction::SaveClip => "Save clip".into(),
            GlobalAction::MuteMics => "Mute or unmute microphones".into(),
            GlobalAction::Scene(n) => format!("Scene {n} on air"),
        }
    }
}

/// "cmd-shift-r" as macOS shows it: "⇧⌘R".
pub fn display(keys: &str) -> String {
    let parts: Vec<&str> = keys.split('-').collect();
    let (key, mods) = parts.split_last().map_or(("", &[][..]), |(k, m)| (*k, m));
    let mut out = String::new();
    for (m, sym) in [("ctrl", "⌃"), ("alt", "⌥"), ("shift", "⇧"), ("cmd", "⌘")] {
        if mods.contains(&m) {
            out.push_str(sym);
        }
    }
    out.push_str(&match key {
        "space" => "Space".to_string(),
        k => k.to_uppercase(),
    });
    out
}

/// Carbon's key code and modifier mask for "cmd-shift-r". Needs a modifier (⌘ ⌃ ⌥) unless the
/// key is a function key, so plain typing elsewhere is left alone.
pub fn parse(keys: &str) -> Option<(u32, u32)> {
    let parts: Vec<&str> = keys.split('-').collect();
    let (key, mods) = parts.split_last()?;
    let mut mask = 0;
    for m in mods {
        mask |= match *m {
            "cmd" => 0x0100,
            "shift" => 0x0200,
            "alt" => 0x0800,
            "ctrl" => 0x1000,
            _ => return None,
        };
    }
    let code = key_code(key)?;
    let function = key.starts_with('f') && key.len() > 1 && key[1..].parse::<u8>().is_ok();
    (function || mask & (0x0100 | 0x0800 | 0x1000) != 0).then_some((code, mask))
}

/// macOS virtual key codes (US layout positions).
fn key_code(key: &str) -> Option<u32> {
    const LETTERS: [(char, u32); 26] = [
        ('a', 0x00),
        ('b', 0x0B),
        ('c', 0x08),
        ('d', 0x02),
        ('e', 0x0E),
        ('f', 0x03),
        ('g', 0x05),
        ('h', 0x04),
        ('i', 0x22),
        ('j', 0x26),
        ('k', 0x28),
        ('l', 0x25),
        ('m', 0x2E),
        ('n', 0x2D),
        ('o', 0x1F),
        ('p', 0x23),
        ('q', 0x0C),
        ('r', 0x0F),
        ('s', 0x01),
        ('t', 0x11),
        ('u', 0x20),
        ('v', 0x09),
        ('w', 0x0D),
        ('x', 0x07),
        ('y', 0x10),
        ('z', 0x06),
    ];
    const DIGITS: [u32; 10] = [0x1D, 0x12, 0x13, 0x14, 0x15, 0x17, 0x16, 0x1A, 0x1C, 0x19];
    const FKEYS: [u32; 12] = [0x7A, 0x78, 0x63, 0x76, 0x60, 0x61, 0x62, 0x64, 0x65, 0x6D, 0x67, 0x6F];
    let mut chars = key.chars();
    match (chars.next(), chars.next()) {
        (Some(c), None) if c.is_ascii_lowercase() => LETTERS.iter().find(|(l, _)| *l == c).map(|(_, k)| *k),
        (Some(c), None) if c.is_ascii_digit() => Some(DIGITS[c as usize - '0' as usize]),
        _ if key == "space" => Some(0x31),
        _ if key.starts_with('f') => {
            key[1..].parse::<usize>().ok().filter(|n| (1..=12).contains(n)).map(|n| FKEYS[n - 1])
        }
        _ => None,
    }
}

// ---------------------------------------------------------------- Carbon

#[repr(C)]
struct EventTypeSpec {
    event_class: u32,
    event_kind: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct EventHotKeyId {
    signature: u32,
    id: u32,
}

type EventHandler = extern "C" fn(*mut c_void, *mut c_void, *mut c_void) -> i32;

#[link(name = "Carbon", kind = "framework")]
unsafe extern "C" {
    fn GetApplicationEventTarget() -> *mut c_void;
    fn InstallEventHandler(
        target: *mut c_void,
        handler: EventHandler,
        count: u32,
        types: *const EventTypeSpec,
        user_data: *mut c_void,
        out: *mut *mut c_void,
    ) -> i32;
    fn RegisterEventHotKey(
        code: u32,
        modifiers: u32,
        id: EventHotKeyId,
        target: *mut c_void,
        options: u32,
        out: *mut *mut c_void,
    ) -> i32;
    fn UnregisterEventHotKey(hotkey: *mut c_void) -> i32;
    fn GetEventParameter(
        event: *mut c_void,
        name: u32,
        desired_type: u32,
        actual_type: *mut u32,
        size: usize,
        actual_size: *mut usize,
        data: *mut c_void,
    ) -> i32;
}

const fn four(code: &[u8; 4]) -> u32 {
    u32::from_be_bytes(*code)
}

const SIGNATURE: u32 = four(b"LtAr");

#[derive(Default)]
struct State {
    installed: bool,
    /// Registered hot keys: Carbon's handle (as an address) and the action, by id.
    keys: HashMap<u32, (usize, GlobalAction)>,
    pressed: Vec<GlobalAction>,
}

fn state() -> &'static Mutex<State> {
    static STATE: OnceLock<Mutex<State>> = OnceLock::new();
    STATE.get_or_init(Default::default)
}

extern "C" fn on_hot_key(_: *mut c_void, event: *mut c_void, _: *mut c_void) -> i32 {
    let mut id = EventHotKeyId::default();
    let ok = unsafe {
        GetEventParameter(
            event,
            four(b"----"),
            four(b"hkid"),
            std::ptr::null_mut(),
            std::mem::size_of::<EventHotKeyId>(),
            std::ptr::null_mut(),
            &mut id as *mut _ as *mut c_void,
        )
    } == 0;
    if ok && id.signature == SIGNATURE {
        let mut s = state().lock().unwrap();
        if let Some((_, action)) = s.keys.get(&id.id).copied() {
            s.pressed.push(action);
        }
    }
    0
}

/// Registers these shortcuts in place of any before (on the main thread). Returns the ones
/// macOS refused (another app has them).
pub fn register(bindings: &[(GlobalAction, String)]) -> Vec<GlobalAction> {
    let mut s = state().lock().unwrap();
    unsafe {
        if !s.installed {
            let spec = EventTypeSpec { event_class: four(b"keyb"), event_kind: 5 };
            let rc = InstallEventHandler(
                GetApplicationEventTarget(),
                on_hot_key,
                1,
                &spec,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            );
            if rc != 0 {
                tracing::warn!(rc, "couldn't listen for global shortcuts");
                return bindings.iter().map(|(a, _)| *a).collect();
            }
            s.installed = true;
        }
        for (_, (handle, _)) in s.keys.drain() {
            UnregisterEventHotKey(handle as *mut c_void);
        }
    }
    let mut refused = Vec::new();
    for (i, (action, keys)) in bindings.iter().enumerate() {
        let Some((code, modifiers)) = parse(keys) else {
            refused.push(*action);
            continue;
        };
        let id = i as u32 + 1;
        let mut handle: *mut c_void = std::ptr::null_mut();
        let rc = unsafe {
            RegisterEventHotKey(
                code,
                modifiers,
                EventHotKeyId { signature: SIGNATURE, id },
                GetApplicationEventTarget(),
                0,
                &mut handle,
            )
        };
        if rc == 0 && !handle.is_null() {
            s.keys.insert(id, (handle as usize, *action));
        } else {
            refused.push(*action);
        }
    }
    refused
}

/// Shortcuts pressed since the last call.
pub fn take_pressed() -> Vec<GlobalAction> {
    std::mem::take(&mut state().lock().unwrap().pressed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_parse_and_read_like_macos() {
        assert_eq!(parse("cmd-shift-r"), Some((0x0F, 0x0300)));
        assert_eq!(parse("ctrl-alt-1"), Some((0x12, 0x1800)));
        assert_eq!(parse("f5"), Some((0x60, 0)), "function keys alone are fine");
        assert_eq!(parse("r"), None, "a plain letter would eat typing everywhere");
        assert_eq!(parse("shift-r"), None);
        assert_eq!(display("cmd-shift-r"), "⇧⌘R");
        assert_eq!(display("ctrl-alt-space"), "⌃⌥Space");
    }

    #[test]
    fn macos_takes_a_registration() {
        // An unlikely combination, registered and released again.
        let refused = register(&[(GlobalAction::Scene(5), "ctrl-alt-cmd-f12".into())]);
        assert!(refused.is_empty(), "macOS refused the shortcut");
        assert!(register(&[]).is_empty());
    }
}
