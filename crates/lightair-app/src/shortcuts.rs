//! Shortcuts that work while Lightair isn't in front (docs/05-interface-shell.md, "Global
//! shortcuts"). macOS: Carbon's hot keys, delivered whatever app is in front, without the
//! Accessibility or Input Monitoring permission that watching every key would need. Windows:
//! `RegisterHotKey`, on a thread of its own with a message loop. Shortcuts are stored the same
//! way on both ("cmd-shift-r"); on Windows "cmd" means Ctrl.

use serde::{Deserialize, Serialize};

/// What a global shortcut does.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum GlobalAction {
    ToggleRecording,
    ToggleLive,
    /// Go live to every included destination (does nothing when already live).
    StartAll,
    /// End every destination.
    StopAll,
    /// Start or stop the *n*th destination in Outputs' order (1-based).
    Destination(u8),
    Take,
    /// Start or stop a Studio take (docs/plans/screen-studio.md).
    StudioTake,
    AddMarker,
    SaveClip,
    MuteMics,
    /// Zooms a scene's screen in (following the pointer) or back out, where its zoom is Toggle.
    FollowZoom,
    Scene(u8),
    /// An automation's hotkey (its place in `automation_keys`).
    Automation(u16),
}

impl GlobalAction {
    /// The label with a destination's name where the action is about one ("… destination 2
    /// (YouTube)"), for the Settings list.
    pub fn label_in(self, destinations: &[lightair_model::Destination]) -> String {
        match self {
            GlobalAction::Destination(n) => match destinations.get((n as usize).saturating_sub(1)) {
                Some(d) => format!("Start or stop destination {n} ({})", d.name),
                None => format!("Start or stop destination {n} (none yet)"),
            },
            other => other.label(),
        }
    }

    pub const ALL: &'static [GlobalAction] = &[
        GlobalAction::ToggleRecording,
        GlobalAction::ToggleLive,
        GlobalAction::StartAll,
        GlobalAction::StopAll,
        GlobalAction::Destination(1),
        GlobalAction::Destination(2),
        GlobalAction::Destination(3),
        GlobalAction::Destination(4),
        GlobalAction::Take,
        GlobalAction::StudioTake,
        GlobalAction::AddMarker,
        GlobalAction::SaveClip,
        GlobalAction::MuteMics,
        GlobalAction::FollowZoom,
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
            GlobalAction::StartAll => "Start every destination".into(),
            GlobalAction::StopAll => "Stop every destination".into(),
            GlobalAction::Destination(n) => format!("Start or stop destination {n}"),
            GlobalAction::Take => "Take (cued scene on air)".into(),
            GlobalAction::StudioTake => "Start or stop a Studio take".into(),
            GlobalAction::AddMarker => "Add marker".into(),
            GlobalAction::SaveClip => "Save clip".into(),
            GlobalAction::MuteMics => "Mute or unmute microphones".into(),
            GlobalAction::FollowZoom => "Screen zoom in or out (Toggle zoom)".into(),
            GlobalAction::Scene(n) => format!("Scene {n} on air"),
            GlobalAction::Automation(_) => "An automation".into(),
        }
    }
}

/// The shortcuts from Settings last registered, and automations' hotkeys (automation id, keys).
static BASE: std::sync::Mutex<Vec<(GlobalAction, String)>> = std::sync::Mutex::new(Vec::new());
static AUTOMATIONS: std::sync::Mutex<Vec<(String, String)>> = std::sync::Mutex::new(Vec::new());

/// Registers Settings' shortcuts and the automations' hotkeys together: the ones the system
/// refused.
pub fn register_all(base: &[(GlobalAction, String)]) -> Vec<GlobalAction> {
    *BASE.lock().unwrap() = base.to_vec();
    let mut all = base.to_vec();
    for (i, (_, keys)) in AUTOMATIONS.lock().unwrap().iter().enumerate() {
        // a key already taken by Settings' shortcuts stays theirs
        if !base.iter().any(|(_, k)| k == keys) {
            all.push((GlobalAction::Automation(i as u16), keys.clone()));
        }
    }
    register(&all)
}

/// The automations' hotkeys changed: registered again with Settings' shortcuts.
pub fn set_automation_keys(keys: Vec<(String, String)>) {
    if *AUTOMATIONS.lock().unwrap() == keys {
        return;
    }
    *AUTOMATIONS.lock().unwrap() = keys;
    let base = BASE.lock().unwrap().clone();
    register_all(&base);
}

/// The automation a hotkey belongs to.
pub fn automation_for(i: u16) -> Option<String> {
    AUTOMATIONS.lock().unwrap().get(i as usize).map(|(id, _)| id.clone())
}

#[cfg(target_os = "macos")]
pub use imp::{display, parse, register, take_pressed};
#[cfg(windows)]
pub use win::{display, parse, register, take_pressed};

#[cfg(target_os = "macos")]
mod imp {
    use super::GlobalAction;
    use std::collections::HashMap;
    use std::ffi::c_void;
    use std::sync::{Mutex, OnceLock};

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

}

#[cfg(windows)]
mod win {
    use super::GlobalAction;
    use std::collections::HashMap;
    use std::sync::mpsc;
    use std::sync::{Mutex, OnceLock};

    use windows_sys::Win32::System::Threading::GetCurrentThreadId;
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
        MOD_ALT, MOD_CONTROL, MOD_NOREPEAT, MOD_SHIFT, RegisterHotKey, UnregisterHotKey,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::{GetMessageW, MSG, PostThreadMessageW, WM_APP, WM_HOTKEY};

    /// "cmd-shift-r" as Windows shows it: "Ctrl+Shift+R".
    pub fn display(keys: &str) -> String {
        let parts: Vec<&str> = keys.split('-').collect();
        let (key, mods) = parts.split_last().map_or(("", &[][..]), |(k, m)| (*k, m));
        let mut out = Vec::new();
        if mods.iter().any(|m| *m == "cmd" || *m == "ctrl") {
            out.push("Ctrl".to_string());
        }
        if mods.contains(&"alt") {
            out.push("Alt".into());
        }
        if mods.contains(&"shift") {
            out.push("Shift".into());
        }
        out.push(match key {
            "space" => "Space".to_string(),
            k => k.to_uppercase(),
        });
        out.join("+")
    }

    /// The virtual key and modifier flags for "cmd-shift-r". Needs Ctrl or Alt unless the key is
    /// a function key, so plain typing elsewhere is left alone.
    pub fn parse(keys: &str) -> Option<(u32, u32)> {
        let parts: Vec<&str> = keys.split('-').collect();
        let (key, mods) = parts.split_last()?;
        let mut mask = 0;
        for m in mods {
            mask |= match *m {
                "cmd" | "ctrl" => MOD_CONTROL,
                "shift" => MOD_SHIFT,
                "alt" => MOD_ALT,
                _ => return None,
            };
        }
        let mut chars = key.chars();
        let code = match (chars.next(), chars.next()) {
            (Some(c), None) if c.is_ascii_lowercase() => c.to_ascii_uppercase() as u32,
            (Some(c), None) if c.is_ascii_digit() => c as u32,
            _ if *key == "space" => 0x20,
            _ if key.starts_with('f') => {
                key[1..].parse::<u32>().ok().filter(|n| (1..=12).contains(n)).map(|n| 0x70 + n - 1)?
            }
            _ => return None,
        };
        let function = key.starts_with('f') && key.len() > 1;
        (function || mask & (MOD_CONTROL | MOD_ALT) != 0).then_some((code, mask))
    }

    #[derive(Default)]
    struct State {
        /// The hot key thread and its id, once started.
        thread: Option<(u32, mpsc::Sender<Request>)>,
        pressed: Vec<GlobalAction>,
    }

    /// New bindings for the hot key thread, and where it answers with the refused ones.
    struct Request {
        bindings: Vec<(GlobalAction, String)>,
        reply: mpsc::Sender<Vec<GlobalAction>>,
    }

    fn state() -> &'static Mutex<State> {
        static STATE: OnceLock<Mutex<State>> = OnceLock::new();
        STATE.get_or_init(Default::default)
    }

    /// The hot key thread: owns the registrations; WM_APP means "new bindings waiting".
    fn run(requests: mpsc::Receiver<Request>, started: mpsc::Sender<u32>) {
        let _ = started.send(unsafe { GetCurrentThreadId() });
        let mut keys: HashMap<i32, GlobalAction> = HashMap::new();
        let mut msg: MSG = unsafe { std::mem::zeroed() };
        while unsafe { GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) } > 0 {
            match msg.message {
                WM_HOTKEY => {
                    if let Some(action) = keys.get(&(msg.wParam as i32)).copied() {
                        state().lock().unwrap().pressed.push(action);
                    }
                }
                WM_APP => {
                    while let Ok(req) = requests.try_recv() {
                        for id in keys.keys() {
                            unsafe { UnregisterHotKey(std::ptr::null_mut(), *id) };
                        }
                        keys.clear();
                        let mut refused = Vec::new();
                        for (i, (action, binding)) in req.bindings.iter().enumerate() {
                            let id = i as i32 + 1;
                            let ok = parse(binding).is_some_and(|(vk, mods)| unsafe {
                                RegisterHotKey(std::ptr::null_mut(), id, mods | MOD_NOREPEAT, vk) != 0
                            });
                            if ok {
                                keys.insert(id, *action);
                            } else {
                                refused.push(*action);
                            }
                        }
                        let _ = req.reply.send(refused);
                    }
                }
                _ => {}
            }
        }
    }

    /// Registers these shortcuts in place of any before. Returns the ones Windows refused
    /// (another app has them).
    pub fn register(bindings: &[(GlobalAction, String)]) -> Vec<GlobalAction> {
        let (thread, tx) = {
            let mut s = state().lock().unwrap();
            if s.thread.is_none() {
                let (tx, rx) = mpsc::channel();
                let (started_tx, started_rx) = mpsc::channel();
                if std::thread::Builder::new()
                    .name("lightair-shortcuts".into())
                    .spawn(move || run(rx, started_tx))
                    .is_err()
                {
                    return bindings.iter().map(|(a, _)| *a).collect();
                }
                let Ok(id) = started_rx.recv() else { return bindings.iter().map(|(a, _)| *a).collect() };
                s.thread = Some((id, tx));
            }
            s.thread.clone().unwrap()
        };
        let (reply, answer) = mpsc::channel();
        let _ = tx.send(Request { bindings: bindings.to_vec(), reply });
        // the thread may not have its message queue yet: post until it's taken
        for _ in 0..50 {
            if unsafe { PostThreadMessageW(thread, WM_APP, 0, 0) } != 0 {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        answer
            .recv_timeout(std::time::Duration::from_secs(2))
            .unwrap_or_else(|_| bindings.iter().map(|(a, _)| *a).collect())
    }

    /// Shortcuts pressed since the last call.
    pub fn take_pressed() -> Vec<GlobalAction> {
        std::mem::take(&mut state().lock().unwrap().pressed)
    }
}

#[cfg(all(test, target_os = "macos"))]
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
    #[ignore = "changes this Mac: registers a real global shortcut; cargo xtask test --machine"]
    fn macos_takes_a_registration() {
        // An unlikely combination, registered and released again.
        let refused = register(&[(GlobalAction::Scene(5), "ctrl-alt-cmd-f12".into())]);
        assert!(refused.is_empty(), "macOS refused the shortcut");
        assert!(register(&[]).is_empty());
    }
}

#[cfg(all(test, windows))]
mod windows_tests {
    use super::*;

    #[test]
    fn keys_parse_and_read_like_windows() {
        // Ctrl (0x2) + Shift (0x4), virtual key 'R'
        assert_eq!(parse("cmd-shift-r"), Some((0x52, 0x6)));
        assert_eq!(parse("f5"), Some((0x74, 0)));
        assert_eq!(parse("r"), None);
        assert_eq!(display("cmd-shift-r"), "Ctrl+Shift+R");
    }
}

#[cfg(test)]
mod label_tests {
    use super::GlobalAction;

    fn dest(name: &str) -> lightair_model::Destination {
        lightair_model::Destination {
            id: lightair_model::Id::new(),
            name: name.into(),
            server: "rtmp://example.com/live".into(),
            enabled: true,
            canvas: None,
            platform: None,
            encoding: None,
            audio_mix: None,
            vod_mix: None,
            chat: None,
            options: Default::default(),
        }
    }

    #[test]
    fn a_destination_shortcut_is_labelled_with_the_destination() {
        let list = [dest("Twitch"), dest("YouTube")];
        assert_eq!(GlobalAction::Destination(2).label_in(&list), "Start or stop destination 2 (YouTube)");
        assert_eq!(GlobalAction::Destination(3).label_in(&list), "Start or stop destination 3 (none yet)");
        assert_eq!(GlobalAction::StartAll.label_in(&list), "Start every destination");
    }

    #[test]
    fn every_action_has_its_own_label() {
        let mut labels: Vec<String> = GlobalAction::ALL.iter().map(|a| a.label()).collect();
        labels.sort();
        let before = labels.len();
        labels.dedup();
        assert_eq!(before, labels.len(), "no two shortcuts read the same");
    }
}
