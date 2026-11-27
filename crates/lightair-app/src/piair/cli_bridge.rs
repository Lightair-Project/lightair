//! The streamer's AI apps for a sandboxed PiAir (plugins.md §5): PiAir can use the streamer's
//! own command-line apps as its brain (codex, claude, agy, grok), but in Lightair's sandbox it
//! can't find, read or start programs. So Lightair does it for the co-host's PiAir only, and only
//! for those four apps: it finds them on the user's PATH, reads their model caches, and starts
//! them outside the sandbox with a scrubbed environment in a fresh scratch folder, streaming
//! their output back (`cli_out`, `cli_exit`).
//!
//! Calls: `cli.which {name}`, `cli.catalog {name}`, `cli.spawn {name, args, config_env, scratch}`,
//! `cli.stdin {id, data, end}`, `cli.kill {id}`.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use serde_json::{Value, json};

/// The apps PiAir may have started.
const APPS: [&str; 4] = ["codex", "claude", "agy", "grok"];

type Send = Arc<dyn Fn(Value) + std::marker::Send + Sync>;

struct Running {
    child: Child,
    stdin: Option<ChildStdin>,
}

fn running() -> &'static Mutex<HashMap<u64, Running>> {
    static R: OnceLock<Mutex<HashMap<u64, Running>>> = OnceLock::new();
    R.get_or_init(Default::default)
}

static NEXT: AtomicU64 = AtomicU64::new(1);

type Answer = Result<Value, (String, String)>;

fn refuse(code: &str, message: impl Into<String>) -> (String, String) {
    (code.to_string(), message.into())
}

/// Answers one `cli.*` call from the co-host's PiAir; `send` delivers the streamed messages.
pub fn call(method: &str, params: &Value, send: Send) -> Answer {
    let name = || -> Result<&str, (String, String)> {
        let n = params.get("name").and_then(Value::as_str).unwrap_or("");
        APPS.iter().copied().find(|a| *a == n).ok_or_else(|| {
            refuse("not_allowed", format!("Lightair starts only codex, claude, agy and grok for PiAir, not \"{n}\"."))
        })
    };
    match method {
        "cli.which" => Ok(json!({"path": which(name()?).map(|p| p.to_string_lossy().into_owned())})),
        "cli.catalog" => Ok(json!({"text": catalog(name()?)})),
        "cli.spawn" => spawn(name()?, params, send),
        "cli.stdin" => {
            let id = params.get("id").and_then(Value::as_u64).unwrap_or(0);
            let mut all = running().lock().unwrap_or_else(|e| e.into_inner());
            let Some(r) = all.get_mut(&id) else { return Ok(json!({})) };
            if let (Some(stdin), Some(data)) = (r.stdin.as_mut(), params.get("data").and_then(Value::as_str))
                && !data.is_empty()
            {
                let _ = stdin.write_all(data.as_bytes()).and_then(|_| stdin.flush());
            }
            if params.get("end").and_then(Value::as_bool) == Some(true) {
                r.stdin = None;
            }
            Ok(json!({}))
        }
        "cli.kill" => {
            let id = params.get("id").and_then(Value::as_u64).unwrap_or(0);
            if let Some(r) = running().lock().unwrap_or_else(|e| e.into_inner()).get_mut(&id) {
                let _ = r.child.kill();
            }
            Ok(json!({}))
        }
        _ => Err(refuse("unknown", format!("no method {method}"))),
    }
}

/// Ends every app PiAir started (PiAir stopped or went away).
pub fn kill_all() {
    for (_, mut r) in running().lock().unwrap_or_else(|e| e.into_inner()).drain() {
        let _ = r.child.kill();
    }
}

fn home() -> PathBuf {
    std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" }).map(PathBuf::from).unwrap_or_default()
}

/// Where the apps install (an app started from the Dock has a short PATH).
fn search_dirs() -> Vec<PathBuf> {
    let h = home();
    vec![
        h.join(".local/bin"),
        "/usr/local/bin".into(),
        "/opt/homebrew/bin".into(),
        h.join(".npm-global/bin"),
        h.join(".bun/bin"),
        h.join(".volta/bin"),
        h.join(".grok/bin"),
    ]
}

fn path_dirs() -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> =
        std::env::var_os("PATH").map(|p| std::env::split_paths(&p).collect()).unwrap_or_default();
    for d in search_dirs() {
        if !dirs.contains(&d) {
            dirs.push(d);
        }
    }
    dirs
}

fn which(name: &str) -> Option<PathBuf> {
    let names: Vec<String> =
        if cfg!(windows) { vec![format!("{name}.cmd"), format!("{name}.exe"), name.into()] } else { vec![name.into()] };
    path_dirs().into_iter().flat_map(|d| names.iter().map(move |n| d.join(n))).find(|p| is_program(p))
}

fn is_program(p: &std::path::Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(p).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
    }
    #[cfg(not(unix))]
    {
        p.is_file()
    }
}

/// The model cache file PiAir reads for an app (what its plan offers), if there is one.
fn catalog(name: &str) -> Option<String> {
    let dir_of =
        |var: &str, default: &str| std::env::var_os(var).map(PathBuf::from).unwrap_or_else(|| home().join(default));
    let file = match name {
        "claude" => {
            let dir = dir_of("CLAUDE_CONFIG_DIR", ".claude").join("cache").join("model-catalog");
            std::fs::read_dir(dir)
                .ok()?
                .flatten()
                .filter(|e| e.file_name().to_string_lossy().ends_with("-cc.json"))
                .max_by_key(|e| e.metadata().and_then(|m| m.modified()).ok())?
                .path()
        }
        "codex" => dir_of("CODEX_HOME", ".codex").join("models_cache.json"),
        "grok" => dir_of("GROK_HOME", ".grok").join("models_cache.json"),
        _ => return None,
    };
    std::fs::read_to_string(file).ok().filter(|t| t.len() < 4_000_000)
}

/// The basics and the app's own settings folder variables: none of Lightair's or the streamer's
/// keys.
fn scrubbed_env(config_env: &[String]) -> Vec<(String, String)> {
    let basic = ["HOME", "USER", "LOGNAME", "LANG", "TERM", "SHELL", "TMPDIR"];
    let windows =
        ["SYSTEMROOT", "SystemRoot", "COMSPEC", "PATHEXT", "USERPROFILE", "APPDATA", "LOCALAPPDATA", "TEMP", "TMP"];
    let mut keep: Vec<&str> = basic.to_vec();
    if cfg!(windows) {
        keep.extend(windows);
    }
    // only settings-folder variables (CODEX_HOME, CLAUDE_CONFIG_DIR…), never keys
    let config: Vec<&str> =
        config_env.iter().map(String::as_str).filter(|k| k.ends_with("_HOME") || k.ends_with("_CONFIG_DIR")).collect();
    keep.extend(config);
    let mut env: Vec<(String, String)> =
        keep.iter().filter_map(|k| std::env::var(k).ok().map(|v| (k.to_string(), v))).collect();
    let path = std::env::join_paths(path_dirs()).map(|p| p.to_string_lossy().into_owned()).unwrap_or_default();
    env.push(("PATH".into(), path));
    env.push(("NO_COLOR".into(), "1".into()));
    env
}

fn spawn(name: &str, params: &Value, send: Send) -> Answer {
    let bin = which(name).ok_or_else(|| refuse("not_found", format!("{name} isn't installed on this computer")))?;
    let config_env: Vec<String> = params
        .get("config_env")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect())
        .unwrap_or_default();
    let scratch = if params.get("scratch").and_then(Value::as_bool) == Some(true) {
        let dir = std::env::temp_dir().join(format!(
            "lightair-piair-app-{}-{}",
            std::process::id(),
            NEXT.load(Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).map_err(|e| refuse("failed", e.to_string()))?;
        Some(dir)
    } else {
        None
    };
    let args: Vec<String> = params
        .get("args")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str())
                .map(|s| match &scratch {
                    Some(dir) => s.replace("{scratch}", &dir.to_string_lossy()),
                    None => s.to_string(),
                })
                .collect()
        })
        .unwrap_or_default();
    let mut cmd = Command::new(&bin);
    cmd.args(&args)
        .env_clear()
        .envs(scrubbed_env(&config_env))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    cmd.current_dir(scratch.clone().unwrap_or_else(home));
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    let mut child = cmd.spawn().map_err(|e| refuse("failed", format!("{name} couldn't start: {e}")))?;
    let id = NEXT.fetch_add(1, Ordering::Relaxed);
    tracing::info!(target: "piair", "started {name} for PiAir ({} arguments)", args.len());
    let pipes = [
        ("stdout", child.stdout.take().map(|p| Box::new(p) as Box<dyn Read + std::marker::Send>)),
        ("stderr", child.stderr.take().map(|p| Box::new(p) as Box<dyn Read + std::marker::Send>)),
    ];
    let stdin = child.stdin.take();
    running().lock().unwrap_or_else(|e| e.into_inner()).insert(id, Running { child, stdin });
    let mut readers = Vec::new();
    for (stream, pipe) in pipes {
        let Some(mut pipe) = pipe else { continue };
        let send = send.clone();
        readers.push(std::thread::spawn(move || {
            let mut buf = [0u8; 8192];
            let mut carry: Vec<u8> = Vec::new();
            while let Ok(n) = pipe.read(&mut buf) {
                if n == 0 {
                    break;
                }
                carry.extend_from_slice(&buf[..n]);
                // send whole UTF-8 characters; keep a split one for the next read
                let cut = match std::str::from_utf8(&carry) {
                    Ok(_) => carry.len(),
                    Err(e) if e.error_len().is_none() => e.valid_up_to(),
                    Err(_) => carry.len(),
                };
                let text = String::from_utf8_lossy(&carry[..cut]).into_owned();
                carry.drain(..cut);
                send(json!({"t": "cli_out", "id": id, "stream": stream, "data": text}));
            }
        }));
    }
    std::thread::spawn(move || {
        for r in readers {
            let _ = r.join();
        }
        let code = loop {
            let mut all = running().lock().unwrap_or_else(|e| e.into_inner());
            match all.get_mut(&id).map(|r| r.child.try_wait()) {
                Some(Ok(Some(status))) => {
                    all.remove(&id);
                    break status.code();
                }
                Some(Ok(None)) => {}
                _ => {
                    all.remove(&id);
                    break None;
                }
            }
            drop(all);
            std::thread::sleep(std::time::Duration::from_millis(30));
        };
        send(json!({"t": "cli_exit", "id": id, "code": code}));
        if let Some(dir) = scratch {
            let _ = std::fs::remove_dir_all(dir);
        }
    });
    Ok(json!({"id": id}))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nothing() -> Send {
        Arc::new(|_| {})
    }

    #[test]
    fn piair_cant_start_a_program_other_than_the_four_ai_apps() {
        for name in ["sh", "/bin/sh", "bash", "osascript", "claude; rm -rf ~", "../codex", ""] {
            let r = call("cli.spawn", &json!({"name": name, "args": ["-c", "id"]}), nothing());
            assert_eq!(r.map_err(|e| e.0), Err("not_allowed".to_string()), "{name:?} was started");
        }
    }

    #[test]
    fn piair_cant_look_up_where_other_programs_are() {
        assert!(call("cli.which", &json!({"name": "ssh"}), nothing()).is_err());
    }
}
