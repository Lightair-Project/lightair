//! Where PiAir comes from and how it's started (plugins.md §5).
//!
//! In order:
//! - `LIGHTAIR_PIAIR` (a path, or `bun` to run the piair checkout's source): development;
//! - the installed package `app.lightair.piair`: started as a sandboxed helper
//!   (`lightair-helpers`), with its own data folder and only the permissions its manifest
//!   declares;
//! - a `piair` beside Lightair's executable (builds from before PiAir became a package);
//! - the piair checkout's build beside this repo (development).
//!
//! Started any way, PiAir speaks its protocol over stdin and stdout; stderr is its log.

use std::io::{Read, Write};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};

use lightair_packages::catalog::Target;
use lightair_packages::{Installed, Kind, Tier};

/// PiAir's package id.
pub const PACKAGE: &str = "app.lightair.piair";

/// A running PiAir.
pub enum Proc {
    Plain(Child),
    Sandboxed(lightair_helpers::Helper),
}

impl Proc {
    /// Whether it has ended.
    pub fn try_wait(&mut self) -> bool {
        match self {
            Proc::Plain(c) => c.try_wait().ok().flatten().is_some(),
            Proc::Sandboxed(h) => h.child.try_wait().is_some(),
        }
    }

    pub fn wait(&mut self) {
        match self {
            Proc::Plain(c) => {
                let _ = c.wait();
            }
            Proc::Sandboxed(h) => {
                let _ = h.child.wait();
            }
        }
    }

    pub fn kill(&mut self) {
        match self {
            Proc::Plain(c) => {
                let _ = c.kill();
            }
            Proc::Sandboxed(h) => h.child.kill(),
        }
    }
}

/// A started PiAir and its pipes.
pub struct Spawned {
    pub proc: Proc,
    pub stdin: Box<dyn Write + Send>,
    pub stdout: Box<dyn Read + Send>,
    pub stderr: Box<dyn Read + Send>,
    /// The folder to give it as `data_dir` (it keeps its memory and settings in `PiAir/` there).
    pub data_dir: PathBuf,
    /// What it may do when it runs as the installed package (its granted permissions); `None`:
    /// a bundled or development PiAir, which has everything.
    pub granted: Option<Vec<String>>,
}

/// The installed PiAir this Lightair can run (the newest usable version), if any.
pub fn installed() -> Option<Installed> {
    let mut found: Vec<Installed> = crate::packages::installed()
        .iter()
        .filter(|i| i.id().as_str() == PACKAGE && i.manifest.check_supported_up_to(Tier::Native).is_empty())
        .filter(|i| entry_of(i).is_some())
        .cloned()
        .collect();
    found.sort_by(|a, b| a.version().cmp(b.version()));
    found.pop()
}

/// Its program for this computer.
fn entry_of(i: &Installed) -> Option<PathBuf> {
    let target = Target::current();
    i.manifest
        .contents
        .iter()
        .filter(|c| c.kind == Kind::Helper)
        .find_map(|c| c.entry.get(target.as_str()))
        .map(|rel| i.content_path(rel))
        .filter(|p| p.exists())
}

/// Whether PiAir can be started at all (for the co-host's "isn't installed" state).
pub fn available() -> bool {
    dev_command().is_some() || installed().is_some() || bundled().is_some()
}

/// Starts PiAir.
pub fn spawn() -> Result<Spawned, String> {
    if let Some(cmd) = dev_command() {
        return plain(&cmd, lightair_platform::paths::data_dir());
    }
    if let Some(i) = installed() {
        return sandboxed(&i);
    }
    if let Some(cmd) = bundled() {
        return plain(&cmd, lightair_platform::paths::data_dir());
    }
    if let Some(cmd) = checkout_build() {
        return plain(&cmd, lightair_platform::paths::data_dir());
    }
    Err("PiAir isn't installed.".into())
}

fn checkout() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../piair")
}

fn dev_command() -> Option<Vec<String>> {
    let v = std::env::var("LIGHTAIR_PIAIR").ok()?;
    if v == "bun" {
        let main = checkout().join("src/main.ts");
        return main.exists().then(|| vec!["bun".into(), main.to_string_lossy().into_owned()]);
    }
    Some(vec![v])
}

fn bundled() -> Option<Vec<String>> {
    let exe = if cfg!(windows) { "piair.exe" } else { "piair" };
    let p = std::env::current_exe().ok()?.parent()?.join(exe);
    p.exists().then(|| vec![p.to_string_lossy().into_owned()])
}

fn checkout_build() -> Option<Vec<String>> {
    if !cfg!(debug_assertions) {
        return None;
    }
    let exe = if cfg!(windows) { "piair.exe" } else { "piair" };
    let target = match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => "darwin-arm64",
        ("macos", _) => "darwin-x64",
        _ => "windows-x64",
    };
    let built = checkout().join("dist").join(target).join(exe);
    built.exists().then(|| vec![built.to_string_lossy().into_owned()])
}

fn plain(cmd: &[String], data_dir: PathBuf) -> Result<Spawned, String> {
    let mut c = Command::new(&cmd[0]);
    c.args(&cmd[1..]).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        c.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    let mut child = c.spawn().map_err(|e| format!("PiAir couldn't start: {e}"))?;
    let (Some(stdin), Some(stdout), Some(stderr)) = (child.stdin.take(), child.stdout.take(), child.stderr.take())
    else {
        return Err("PiAir couldn't start: no pipes".into());
    };
    Ok(Spawned {
        proc: Proc::Plain(child),
        stdin: Box::new(stdin),
        stdout: Box::new(stdout),
        stderr: Box::new(stderr),
        data_dir,
        granted: None,
    })
}

/// PiAir's own data folder as a package (`<data>/PackageData/app.lightair.piair/`).
pub fn package_data_dir() -> PathBuf {
    lightair_platform::paths::data_dir().join("PackageData").join(PACKAGE)
}

fn sandboxed(i: &Installed) -> Result<Spawned, String> {
    let entry = entry_of(i).ok_or("PiAir's package has no program for this computer.")?;
    let data_dir = package_data_dir();
    move_old_data(&data_dir);
    let granted = crate::packages::granted_permissions(i);
    let mut grants = lightair_helpers::Grants::from_permissions(granted.iter().map(String::as_str));
    let held = granted.clone();
    // Bun compiles JavaScript as it runs; its notarized signature allows that.
    grants.jit = true;
    let spec = lightair_helpers::Spec {
        id: PACKAGE.into(),
        entry,
        args: Vec::new(),
        package_dir: i.path.clone(),
        data_dir: data_dir.clone(),
        grants,
        env: Vec::new(),
    };
    let launcher = std::env::current_exe()
        .ok()
        .and_then(|e| e.parent().map(|d| d.join("lightair-sandbox")))
        .filter(|p| p.exists())
        .ok_or("Lightair's sandbox launcher is missing from this build.")?;
    let (tx, rx) = lightair_helpers::events();
    let name = i.manifest.name.clone();
    std::thread::Builder::new()
        .name("piair-network".into())
        .spawn(move || {
            for e in rx {
                crate::plugin_activity::helper_event(&name, &e);
            }
        })
        .map_err(|e| e.to_string())?;
    let mut helper =
        lightair_helpers::launch(&spec, &launcher, tx).map_err(|e| format!("PiAir couldn't start: {e}"))?;
    let (Some(stdin), Some(stdout), Some(stderr)) =
        (helper.child.stdin.take(), helper.child.stdout.take(), helper.child.stderr.take())
    else {
        return Err("PiAir couldn't start: no pipes".into());
    };
    Ok(Spawned {
        proc: Proc::Sandboxed(helper),
        stdin: Box::new(stdin),
        stdout: Box::new(stdout),
        stderr: Box::new(stderr),
        data_dir,
        granted: Some(held),
    })
}

/// Once: PiAir's memory and settings from before it was a package move into its own folder.
/// `lightair.json` (AI drives, allowed apps) is Lightair's and stays.
fn move_old_data(data_dir: &std::path::Path) {
    let old = lightair_platform::paths::data_dir().join("PiAir");
    let new = data_dir.join("PiAir");
    if !old.is_dir() || new.exists() {
        return;
    }
    if std::fs::create_dir_all(&new).is_err() {
        return;
    }
    let Ok(entries) = std::fs::read_dir(&old) else { return };
    for e in entries.flatten() {
        if e.file_name() == "lightair.json" {
            continue;
        }
        if let Err(err) = std::fs::rename(e.path(), new.join(e.file_name())) {
            tracing::warn!(%err, "couldn't move {} into PiAir's package folder", e.path().display());
        }
    }
    tracing::info!("moved PiAir's memory and settings into {}", new.display());
}
