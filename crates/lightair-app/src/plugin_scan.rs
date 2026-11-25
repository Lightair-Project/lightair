//! Finding VST3 plugins without risking Lightair: a bundle with no moduleinfo.json has to be
//! loaded to learn what's in it, and that happens in a helper process (this same binary, run with
//! `--probe-vst3 <bundle>`), so a plugin that crashes or hangs on load is only skipped.

use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use lightair_obs::plugins::{PluginInfo, vst3};

const PROBE_FLAG: &str = "--probe-vst3";

/// The installed VST3 effects (cached next to the preferences until a bundle changes).
pub fn vst3() -> Vec<PluginInfo> {
    let cache = crate::prefs::path().with_file_name("vst3-plugins.json");
    vst3::scan(Some(&cache), &probe_in_helper)
}

fn probe_in_helper(bundle: &Path) -> Result<Vec<PluginInfo>, String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let mut child = Command::new(exe)
        .arg(PROBE_FLAG)
        .arg(bundle)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| e.to_string())?;
    let started = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if started.elapsed() > Duration::from_secs(20) => {
                let _ = child.kill();
                return Err("it took too long to load".into());
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(20)),
            Err(e) => return Err(e.to_string()),
        }
    }
    let out = child.wait_with_output().map_err(|e| e.to_string())?;
    if !out.status.success() {
        return Err(format!("it crashed while loading ({})", out.status));
    }
    // plugins may print while loading: the list is the last line
    let text = String::from_utf8_lossy(&out.stdout);
    let line = text.lines().rev().find(|l| l.starts_with('[')).ok_or("no answer")?;
    serde_json::from_str(line).map_err(|e| e.to_string())
}

/// When this process is a probe: lists the bundle's effects as JSON on stdout and returns the
/// exit code.
pub fn probe_main() -> Option<i32> {
    let mut args = std::env::args().skip(1);
    if args.next().as_deref() != Some(PROBE_FLAG) {
        return None;
    }
    let bundle = args.next()?;
    Some(match vst3::probe(Path::new(&bundle)) {
        Ok(list) => {
            println!("{}", serde_json::to_string(&list).unwrap_or_else(|_| "[]".into()));
            0
        }
        Err(why) => {
            eprintln!("{why}");
            1
        }
    })
}
