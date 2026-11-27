//! A helper's permissions become exactly the holes its sandbox opens.

use std::path::PathBuf;

use lightair_helpers::{Device, Grants, Spec, profile};

#[test]
fn permission_strings_become_grants() {
    let g = Grants::from_permissions([
        "network:api.example.com",
        "network:localhost:8001",
        "network.lan",
        "network.listen:1455",
        "ipc:tmp:discord-ipc-0",
        "automation:com.apple.Music",
        "device.hid:0fd9",
        "device.midi",
        "screen.capture",
    ]);
    assert_eq!(g.hosts, ["api.example.com"]);
    assert_eq!(g.localhost_ports, [8001]);
    assert!(g.lan && !g.any_host && !g.full_access);
    assert_eq!(g.listen, [1455]);
    assert_eq!(g.devices, [Device::Hid(Some(0x0fd9)), Device::Midi]);
    assert_eq!(g.served, ["screen.capture"], "screen capture is Lightair's to serve, not a hole");
}

#[test]
fn full_access_for_another_platform_doesnt_apply_here() {
    let other = if cfg!(windows) { "full-access:macos" } else { "full-access:windows" };
    assert!(!Grants::from_permissions([other]).full_access);
    assert!(Grants::from_permissions(["full-access"]).full_access);
}

#[test]
fn a_permission_lightair_doesnt_know_opens_nothing() {
    let g = Grants::from_permissions(["keylogger", "network:not a host"]);
    assert!(g.hosts.is_empty() && !g.uses_network(), "{g:?}");
    assert_eq!(g.unknown.len(), 2);
}

fn spec(grants: Grants) -> Spec {
    Spec {
        id: "io.test.h".into(),
        entry: PathBuf::from("/pkg/io.test.h/1.0.0/bin/h"),
        args: vec![],
        package_dir: PathBuf::from("/pkg/io.test.h/1.0.0"),
        data_dir: PathBuf::from("/data/io.test.h"),
        grants,
        env: vec![],
    }
}

#[test]
fn the_profile_lets_the_network_out_only_to_its_proxy_lane() {
    let p = profile::macos(&spec(Grants::from_permissions(["network:*"])), Some(40123));
    assert!(p.contains("(deny default)"), "{p}");
    assert!(p.contains(r#"(remote ip "localhost:40123")"#), "{p}");
    assert!(!p.contains(r#"(remote ip "*"#), "the profile opens the network beyond the proxy: {p}");
}

#[test]
fn a_helper_without_network_gets_no_socket_at_all() {
    let p = profile::macos(&spec(Grants::default()), None);
    assert!(!p.contains("network-outbound") && !p.contains("system-socket"), "{p}");
}

#[test]
fn generating_code_is_allowed_only_for_a_jit_runtime() {
    assert!(!profile::macos(&spec(Grants::default()), None).contains("dynamic-code-generation"));
    let jit = Grants { jit: true, ..Grants::default() };
    assert!(profile::macos(&spec(jit), None).contains("dynamic-code-generation"));
}

#[test]
fn a_picked_folder_is_read_only_unless_writing_was_granted() {
    let mut g = Grants::default();
    g.pick(PathBuf::from("/Users/me/Games/logs"), false);
    let p = profile::macos(&spec(g), None);
    let line = p.lines().find(|l| l.contains(r#""/Users/me/Games/logs")"#)).unwrap_or_default();
    assert!(line.contains("file-read*") && !line.contains("file-write*"), "{p}");
}

#[test]
fn a_path_with_a_quote_cant_break_out_of_the_profile() {
    let mut s = spec(Grants::default());
    s.data_dir = PathBuf::from("/data/x\")(allow default)(\"");
    let p = profile::macos(&s, None);
    // the quote stays escaped inside the string, so no rule of its own starts there
    assert!(p.contains(r#"(subpath "/data/x\")(allow default)(\""))"#), "{p}");
    assert!(!p.lines().any(|l| l.trim_start().starts_with("(allow default)")), "{p}");
}

#[test]
fn an_ipc_name_cant_climb_out_of_the_temporary_folder() {
    assert_eq!(profile::ipc_path("tmp:../../etc/passwd"), None);
    assert_eq!(profile::ipc_path("relative/socket"), None);
    assert!(profile::ipc_path("tmp:discord-ipc-0").is_some_and(|p| p.ends_with("/discord-ipc-0")));
}
