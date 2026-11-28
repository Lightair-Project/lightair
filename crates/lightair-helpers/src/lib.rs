//! Running package helpers (tier 4, plugins.md §4.8, §4.11, §4.12).
//!
//! A sandboxed helper is started on macOS through `lightair-sandbox` with responsibility
//! disclaimed, so it inherits none of Lightair's privacy permissions (camera, microphone, screen
//! recording, Accessibility). It runs under a deny-by-default seatbelt profile written from its
//! grants ([`profile::macos`]): its package folder to read, its data folder to read and write,
//! and a proxy [`proxy::Lane`] as its only network. Anything powerful (the screen, the camera,
//! chat, Lightair's methods) it asks Lightair for over its protocol, where the caller checks
//! the permission. A full-access helper starts as a plain program.
//!
//! On Windows a sandboxed helper runs in an AppContainer of its own, at low integrity, in a job
//! object ([`windows`]): its package folder to read, its data folder to read and write, and its
//! proxy lane reached through a bridge in the same AppContainer as its only network.
//!
//! The helper's stdin and stdout are pipes for its protocol; stderr is its log.

pub mod grants;
pub mod profile;
pub mod proxy;
mod spawn;
#[cfg(windows)]
pub mod windows;

use std::io;
use std::path::PathBuf;

pub use crossbeam_channel::Sender;

pub use crossbeam_channel::Receiver;
pub use grants::{Device, Grants, Picked};
pub use proxy::{Event, Lane, Rules};
pub use spawn::Child;

/// A channel for a helper's network events (what [`launch`] reports into).
pub fn events() -> (Sender<Event>, Receiver<Event>) {
    crossbeam_channel::unbounded()
}

/// A helper to start.
#[derive(Clone, Debug)]
pub struct Spec {
    /// Its package id (for logs, events and the lane's name).
    pub id: String,
    /// The program, inside `package_dir`.
    pub entry: PathBuf,
    pub args: Vec<String>,
    /// The installed package's folder (read-only to it).
    pub package_dir: PathBuf,
    /// Its own data folder (`<data>/PackageData/<id>/`), its working folder and home.
    pub data_dir: PathBuf,
    pub grants: Grants,
    /// More environment for it (never secrets: those go over the protocol).
    pub env: Vec<(String, String)>,
}

/// A running helper. Dropping it kills the helper and closes its lane.
pub struct Helper {
    pub child: Child,
    lane: Option<Lane>,
    profile_file: Option<PathBuf>,
    #[cfg(windows)]
    sandbox: Option<windows::Sandbox>,
}

impl Helper {
    /// Its proxy port, when it has network.
    pub fn proxy_port(&self) -> Option<u16> {
        self.lane.as_ref().map(Lane::port)
    }

    /// CPU seconds and peak memory (MB) of all its processes so far, where the sandbox counts
    /// them (Windows' job object).
    pub fn usage(&self) -> Option<(f64, f64)> {
        #[cfg(windows)]
        if let Some(s) = &self.sandbox {
            return s.usage();
        }
        None
    }

    /// Its AppContainer's SID (Windows), for logs.
    pub fn container(&self) -> Option<String> {
        #[cfg(windows)]
        if let Some(s) = &self.sandbox {
            return Some(s.sid_string());
        }
        None
    }
}

impl Drop for Helper {
    fn drop(&mut self) {
        self.child.kill();
        if let Some(f) = &self.profile_file {
            let _ = std::fs::remove_file(f);
        }
    }
}

/// Starts a helper. `launcher` is `lightair-sandbox` beside Lightair's executable: the sandbox
/// launcher on macOS, the network bridge on Windows.
/// Network attempts are sent to `events`.
pub fn launch(spec: &Spec, launcher: &std::path::Path, events: Sender<Event>) -> io::Result<Helper> {
    std::fs::create_dir_all(spec.data_dir.join("tmp"))?;
    // The sandbox matches real paths: a folder reached through a link (`/var` is `/private/var`
    // on macOS) is written into the profile as where it really is.
    let real = |p: &std::path::Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    let mut resolved = spec.clone();
    resolved.entry = real(&spec.entry);
    resolved.package_dir = real(&spec.package_dir);
    resolved.data_dir = real(&spec.data_dir);
    for picked in &mut resolved.grants.picked {
        picked.path = real(&picked.path);
    }
    let spec = &resolved;
    if !spec.entry.starts_with(&spec.package_dir) {
        return Err(io::Error::other("the helper's program isn't inside its package"));
    }
    // a package's files don't carry permissions: its program is made executable here
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&spec.entry, std::fs::Permissions::from_mode(0o755))?;
    }

    if spec.grants.full_access {
        let env = full_access_env(spec);
        let child = spawn::spawn(&spec.entry, &spec.args, &spec.data_dir, &env)?;
        return Ok(Helper {
            child,
            lane: None,
            profile_file: None,
            #[cfg(windows)]
            sandbox: None,
        });
    }

    sandboxed(spec, launcher, events)
}

#[cfg(target_os = "macos")]
fn sandboxed(spec: &Spec, launcher: &std::path::Path, events: Sender<Event>) -> io::Result<Helper> {
    let lane =
        if spec.grants.uses_network() { Some(Lane::open(&spec.id, Rules::from(&spec.grants), events)?) } else { None };
    let profile = profile::macos(spec, lane.as_ref().map(Lane::port));
    let dir = std::env::temp_dir().join("lightair-sandbox");
    std::fs::create_dir_all(&dir)?;
    let profile_file = dir.join(format!("{}-{}.sb", sanitize(&spec.id), std::process::id()));
    std::fs::write(&profile_file, profile)?;

    let mut env = base_env(spec);
    if let Some(lane) = &lane {
        let url = lane.url();
        for k in ["HTTPS_PROXY", "HTTP_PROXY", "ALL_PROXY", "https_proxy", "http_proxy", "all_proxy"] {
            env.push((k.into(), url.clone()));
        }
        env.push(("NO_PROXY".into(), String::new()));
    }
    let mut args = vec![profile_file.to_string_lossy().into_owned(), spec.entry.to_string_lossy().into_owned()];
    args.extend(spec.args.iter().cloned());
    let child = spawn::spawn(launcher, &args, &spec.data_dir, &env)?;
    Ok(Helper { child, lane, profile_file: Some(profile_file) })
}

#[cfg(windows)]
fn sandboxed(spec: &Spec, launcher: &std::path::Path, events: Sender<Event>) -> io::Result<Helper> {
    windows::launch(spec, launcher, events)
}

#[cfg(not(any(target_os = "macos", windows)))]
fn sandboxed(_spec: &Spec, _launcher: &std::path::Path, _events: Sender<Event>) -> io::Result<Helper> {
    Err(io::Error::new(io::ErrorKind::Unsupported, "sandboxed helpers aren't available on this platform yet"))
}

/// A sandboxed helper's environment: nothing of Lightair's own, its data folder as home.
#[cfg(target_os = "macos")]
fn base_env(spec: &Spec) -> Vec<(String, String)> {
    let data = spec.data_dir.to_string_lossy().into_owned();
    let mut env = vec![
        ("HOME".to_string(), data.clone()),
        ("TMPDIR".to_string(), format!("{data}/tmp/")),
        ("PATH".to_string(), "/usr/bin:/bin".to_string()),
        ("LANG".to_string(), std::env::var("LANG").unwrap_or_else(|_| "en_US.UTF-8".into())),
        ("LIGHTAIR_PACKAGE".to_string(), spec.id.clone()),
    ];
    env.extend(spec.env.iter().cloned());
    env
}

/// A full-access helper runs as a normal program: the user's own environment, plus ours.
fn full_access_env(spec: &Spec) -> Vec<(String, String)> {
    let mut env: Vec<(String, String)> = std::env::vars().collect();
    env.push(("LIGHTAIR_PACKAGE".to_string(), spec.id.clone()));
    env.extend(spec.env.iter().cloned());
    env
}

#[cfg(target_os = "macos")]
fn sanitize(id: &str) -> String {
    id.chars().map(|c| if c.is_ascii_alphanumeric() || c == '.' || c == '-' { c } else { '_' }).collect()
}
