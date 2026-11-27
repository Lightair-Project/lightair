//! What a helper was granted, read from its permission strings (catalog-formats.md §6), plus the
//! folders the user picked for it. Only granted permissions are passed in: the ones the user
//! took back in the Packages panel aren't.

use std::path::PathBuf;

/// A folder or file the user picked for a helper in Lightair's file picker.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Picked {
    pub path: PathBuf,
    /// Writing was granted too (`fs.write:picked`), not only reading.
    pub write: bool,
}

/// A kind of device a helper may open.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Device {
    /// HID devices (control panels, game controllers), optionally one vendor's.
    Hid(Option<u16>),
    /// USB devices, optionally one vendor's.
    Usb(Option<u16>),
    Midi,
    Serial,
    Bluetooth,
}

/// Everything a helper may reach outside its own folders.
#[derive(Clone, Debug, Default)]
pub struct Grants {
    /// `network:*`: any internet host.
    pub any_host: bool,
    /// `network:<host>`: these hosts (`*.example.com` covers its subdomains).
    pub hosts: Vec<String>,
    /// `network:localhost:<port>`: an app's local server.
    pub localhost_ports: Vec<u16>,
    /// `network.lan`: devices on the home network (private addresses).
    pub lan: bool,
    /// `network.lan.discovery`: Bonjour/mDNS.
    pub lan_discovery: bool,
    /// `network.listen:<port>`: other apps may connect to it (on the LAN too when `lan`).
    pub listen: Vec<u16>,
    /// `ipc:<name>`: another app's socket (an absolute path, or `tmp:<name>` in the temporary folder).
    pub ipc: Vec<String>,
    /// `automation:<app id>`: Apple Events to that app.
    pub automation: Vec<String>,
    /// Folders and files the user picked (`fs.read:picked`, `fs.write:picked`).
    pub picked: Vec<Picked>,
    pub devices: Vec<Device>,
    /// `full-access` (or `full-access:<platform>` for this platform): no sandbox at all.
    pub full_access: bool,
    /// The helper's signature allows JIT (Bun, V8, .NET): its sandbox allows generating code.
    pub jit: bool,
    /// Permissions Lightair serves over the helper protocol (`screen.capture`, `camera`, `mic`,
    /// `chat.read`, `api.call`, `secrets`…); nothing to open in the sandbox.
    pub served: Vec<String>,
    /// Permissions this Lightair doesn't know; ignored (nothing opens for them).
    pub unknown: Vec<String>,
}

/// This platform's name in `full-access:<platform>`.
pub const PLATFORM: &str = if cfg!(target_os = "macos") {
    "macos"
} else if cfg!(target_os = "windows") {
    "windows"
} else {
    "linux"
};

const SERVED: &[&str] = &[
    "screen.capture",
    "camera",
    "mic",
    "chat.read",
    "chat.write",
    "api.call",
    "secrets",
    "project.read",
    "project.write",
    "outputs.control",
    "notifications",
    "audio.levels",
    "hotkeys",
];

impl Grants {
    /// Reads granted permission strings.
    pub fn from_permissions<'a>(granted: impl IntoIterator<Item = &'a str>) -> Grants {
        let mut g = Grants::default();
        for p in granted {
            let p = p.trim();
            if p == "network:*" {
                g.any_host = true;
            } else if let Some(port) = p.strip_prefix("network:localhost:") {
                match port.parse() {
                    Ok(port) => g.localhost_ports.push(port),
                    Err(_) => g.unknown.push(p.to_string()),
                }
            } else if let Some(host) = p.strip_prefix("network:") {
                if valid_host_rule(host) {
                    g.hosts.push(host.to_ascii_lowercase());
                } else {
                    g.unknown.push(p.to_string());
                }
            } else if p == "network.lan" {
                g.lan = true;
            } else if p == "network.lan.discovery" {
                g.lan_discovery = true;
            } else if let Some(port) = p.strip_prefix("network.listen:") {
                match port.parse() {
                    Ok(port) => g.listen.push(port),
                    Err(_) => g.unknown.push(p.to_string()),
                }
            } else if let Some(name) = p.strip_prefix("ipc:") {
                g.ipc.push(name.to_string());
            } else if let Some(app) = p.strip_prefix("automation:") {
                g.automation.push(app.to_string());
            } else if p == "fs.read:picked" || p == "fs.write:picked" {
                // The folders themselves come from the picker (`Grants::pick`).
            } else if let Some(d) = device(p) {
                g.devices.push(d);
            } else if p == "full-access" {
                g.full_access = true;
            } else if let Some(platform) = p.strip_prefix("full-access:") {
                if platform == PLATFORM {
                    g.full_access = true;
                }
            } else if SERVED.contains(&p) {
                g.served.push(p.to_string());
            } else {
                g.unknown.push(p.to_string());
            }
        }
        g
    }

    /// Adds a folder or file the user picked.
    pub fn pick(&mut self, path: PathBuf, write: bool) {
        self.picked.push(Picked { path, write });
    }

    /// Whether the helper reaches any network at all (so it gets a proxy lane).
    pub fn uses_network(&self) -> bool {
        self.any_host || !self.hosts.is_empty() || !self.localhost_ports.is_empty() || self.lan
    }
}

fn device(p: &str) -> Option<Device> {
    let vendor = |rest: &str| u16::from_str_radix(rest.trim_start_matches("0x"), 16).ok();
    Some(match p {
        "device.hid" => Device::Hid(None),
        "device.usb" => Device::Usb(None),
        "device.midi" => Device::Midi,
        "device.serial" => Device::Serial,
        "device.bluetooth" => Device::Bluetooth,
        _ => {
            if let Some(v) = p.strip_prefix("device.hid:") {
                Device::Hid(Some(vendor(v)?))
            } else {
                Device::Usb(Some(vendor(p.strip_prefix("device.usb:")?)?))
            }
        }
    })
}

/// `example.com`, `api.example.com` or `*.example.com`: letters, digits, `-` and `.`, no ports.
fn valid_host_rule(h: &str) -> bool {
    let h = h.strip_prefix("*.").unwrap_or(h);
    !h.is_empty()
        && h.len() <= 253
        && h.contains('.')
        && h.split('.').all(|l| !l.is_empty() && l.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'))
}
