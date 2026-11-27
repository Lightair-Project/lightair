//! The macOS seatbelt profile for one helper, written from its grants. Deny by default; what the
//! helper-sandbox spike proved a runtime like Bun needs, then one rule per granted hole
//! (plugins.md §4.11). Network goes only to the helper's own proxy lane.

use std::fmt::Write as _;
use std::path::Path;

use crate::Spec;
use crate::grants::Device;

/// The profile's text.
pub fn macos(spec: &Spec, proxy_port: Option<u16>) -> String {
    let mut p = String::from("(version 1)\n(deny default)\n(import \"system.sb\")\n");
    let _ = writeln!(p, "(allow process-exec (literal {}))", q(&spec.entry));
    p.push_str("(allow process-fork)\n(allow file-read-metadata)\n(allow sysctl-read)\n");

    // The folders on the way to its program, package, data and picked folders: listed, not read.
    let mut parents: Vec<String> = Vec::new();
    let mut add_parents = |path: &Path| {
        let mut cur = path.parent();
        while let Some(dir) = cur {
            let s = q(dir);
            if !parents.contains(&s) {
                parents.push(s);
            }
            cur = dir.parent();
        }
    };
    add_parents(&spec.entry);
    add_parents(&spec.package_dir);
    add_parents(&spec.data_dir);
    for picked in &spec.grants.picked {
        add_parents(&picked.path);
    }
    if !parents.is_empty() {
        let _ = writeln!(
            p,
            "(allow file-read* {})",
            parents.iter().map(|d| format!("(literal {d})")).collect::<Vec<_>>().join(" ")
        );
    }

    let _ = writeln!(p, "(allow file-read* (subpath {}))", q(&spec.package_dir));
    let _ = writeln!(p, "(allow file-read* file-write* (subpath {}))", q(&spec.data_dir));
    for picked in &spec.grants.picked {
        let rule = if picked.path.is_dir() { "subpath" } else { "literal" };
        let ops = if picked.write { "file-read* file-write*" } else { "file-read*" };
        let _ = writeln!(p, "(allow {ops} ({rule} {}))", q(&picked.path));
    }

    if spec.grants.jit {
        p.push_str("(allow dynamic-code-generation)\n");
    }

    let g = &spec.grants;
    if proxy_port.is_some() || !g.listen.is_empty() || g.lan_discovery || !g.ipc.is_empty() {
        p.push_str("(allow system-socket)\n");
    }
    if let Some(port) = proxy_port {
        let _ = writeln!(p, "(allow network-outbound (remote ip \"localhost:{port}\"))");
    }
    for port in &g.listen {
        let host = if g.lan { "*" } else { "localhost" };
        let _ = writeln!(p, "(allow network-bind (local ip \"{host}:{port}\"))");
        let _ = writeln!(p, "(allow network-inbound (local ip \"{host}:{port}\"))");
    }
    if g.lan_discovery {
        // Bonjour goes through the system's mDNS service. It also answers ordinary name lookups,
        // which is why it's a permission of its own.
        p.push_str("(allow network-outbound (literal \"/private/var/run/mDNSResponder\"))\n");
    }
    for name in &g.ipc {
        if let Some(path) = ipc_path(name) {
            let _ = writeln!(p, "(allow network-outbound (literal {}))", q(Path::new(&path)));
            let _ = writeln!(p, "(allow file-read* file-write* (literal {}))", q(Path::new(&path)));
        }
    }
    if !g.automation.is_empty() {
        p.push_str("(allow mach-lookup (global-name \"com.apple.coreservices.appleevents\"))\n");
        for app in &g.automation {
            let _ = writeln!(p, "(allow appleevent-send (appleevent-destination {}))", qs(app));
        }
    }
    for d in &g.devices {
        // Seatbelt can't tell vendors apart: a vendor-limited permission opens its whole class here.
        match d {
            Device::Hid(_) => p.push_str("(allow iokit-open (iokit-user-client-class \"IOHIDLibUserClient\"))\n"),
            Device::Usb(_) => p.push_str(
                "(allow iokit-open (iokit-user-client-class \"IOUSBDeviceUserClientV2\") (iokit-user-client-class \"IOUSBInterfaceUserClientV3\") (iokit-user-client-class \"AppleUSBHostDeviceUserClient\") (iokit-user-client-class \"AppleUSBHostInterfaceUserClient\"))\n",
            ),
            Device::Midi => p.push_str("(allow mach-lookup (global-name \"com.apple.midiserver\") (global-name \"com.apple.midiserver.io\"))\n"),
            Device::Serial => p.push_str("(allow file-read* file-write* (regex #\"^/dev/cu\\.\") (regex #\"^/dev/tty\\.\"))\n"),
            Device::Bluetooth => p.push_str("(allow mach-lookup (global-name \"com.apple.bluetoothd\") (global-name \"com.apple.server.bluetooth\") (global-name \"com.apple.server.bluetooth.le.att.xpc\"))\n"),
        }
    }
    p
}

/// Where `ipc:<name>` points: an absolute path, or `tmp:<name>` in the user's temporary folder.
pub fn ipc_path(name: &str) -> Option<String> {
    if let Some(rest) = name.strip_prefix("tmp:") {
        if rest.contains('/') || rest.contains("..") || rest.is_empty() {
            return None;
        }
        let tmp = std::env::var("TMPDIR").unwrap_or_else(|_| "/tmp/".into());
        return Some(format!("{}/{rest}", tmp.trim_end_matches('/')));
    }
    if name.starts_with('/') && !name.contains("..") {
        return Some(name.to_string());
    }
    None
}

/// A path as a profile string literal.
fn q(path: &Path) -> String {
    qs(&path.to_string_lossy())
}

fn qs(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if (c as u32) < 0x20 => {}
            c => out.push(c),
        }
    }
    out.push('"');
    out
}
