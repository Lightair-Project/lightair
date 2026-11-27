//! `lightair-helper-run`: the check workflows' helper run (plugins.md §4.4). Starts a package's
//! helper exactly as Lightair does (lightair-helpers: the profile from its permissions, its proxy
//! lane, the disclaimed launch through lightair-sandbox), plays a fake Lightair (a `hello` line,
//! then waits for its first answer), closes its stdin and expects it to leave, and writes what it
//! saw in the check report's shape (catalog formats §8): the check's results, every connection it
//! tried, the sandbox's refusals, its CPU time and memory.
//!
//! On Windows helpers aren't sandboxed yet (AppContainer isn't built): the helper runs as a plain
//! program in the throwaway machine, and the report says so.
//!
//!   lightair-helper-run --package-dir <dir> --entry <program> --id <id> --platform <platform>
//!       --permission <p> [--permission <p> …] [--launcher <lightair-sandbox>] [--json <out>]
//!       [--ready-secs <n>]

use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::sync::mpsc;
use std::time::{Duration, Instant, SystemTime};

use serde_json::{Value, json};

struct Args {
    package_dir: PathBuf,
    entry: PathBuf,
    id: String,
    platform: String,
    permissions: Vec<String>,
    launcher: Option<PathBuf>,
    json: Option<PathBuf>,
    ready_secs: u64,
}

fn args() -> Result<Args, String> {
    let raw: Vec<String> = std::env::args().skip(1).collect();
    let mut a = Args {
        package_dir: PathBuf::new(),
        entry: PathBuf::new(),
        id: String::new(),
        platform: String::new(),
        permissions: Vec::new(),
        launcher: None,
        json: None,
        ready_secs: 15,
    };
    let mut i = 0;
    while i < raw.len() {
        let v = raw.get(i + 1).cloned().ok_or_else(|| format!("{} needs a value", raw[i]))?;
        match raw[i].as_str() {
            "--package-dir" => a.package_dir = v.into(),
            "--entry" => a.entry = v.into(),
            "--id" => a.id = v,
            "--platform" => a.platform = v,
            "--permission" => a.permissions.push(v),
            "--launcher" => a.launcher = Some(v.into()),
            "--json" => a.json = Some(v.into()),
            "--ready-secs" => a.ready_secs = v.parse().map_err(|_| "--ready-secs needs a number")?,
            other => return Err(format!("unknown argument {other}")),
        }
        i += 2;
    }
    if a.package_dir.as_os_str().is_empty() || a.entry.as_os_str().is_empty() || a.id.is_empty() {
        return Err("--package-dir, --entry and --id are needed".into());
    }
    Ok(a)
}

fn main() {
    let a = match args() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("lightair-helper-run: {e}");
            std::process::exit(2);
        }
    };
    let report = run(&a);
    let text = serde_json::to_string_pretty(&report).unwrap_or_default();
    match &a.json {
        Some(p) => {
            if let Err(e) = std::fs::write(p, text) {
                eprintln!("lightair-helper-run: couldn't write {}: {e}", p.display());
                std::process::exit(1);
            }
        }
        None => println!("{text}"),
    }
}

fn run(a: &Args) -> Value {
    let mut results: Vec<Value> = Vec::new();
    let fail = |r: &mut Vec<Value>, m: String| r.push(json!({"verdict": "fail", "message": m, "file": null}));
    let note = |r: &mut Vec<Value>, m: String| r.push(json!({"verdict": "note", "message": m, "file": null}));

    // the sandbox's rules name real paths: the temporary folder is behind a link on macOS
    // (/var → /private/var), so every path is resolved first
    let package_dir = a.package_dir.canonicalize().unwrap_or_else(|_| a.package_dir.clone());
    let entry = if a.entry.is_absolute() { a.entry.clone() } else { package_dir.join(&a.entry) };
    let entry = entry.canonicalize().unwrap_or(entry);
    make_executable(&entry);
    let data_dir = std::env::temp_dir().join(format!("lightair-helper-run-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&data_dir);
    let _ = std::fs::create_dir_all(&data_dir);
    let data_dir = data_dir.canonicalize().unwrap_or(data_dir);

    let mut grants = lightair_helpers::Grants::from_permissions(a.permissions.iter().map(String::as_str));
    grants.jit = signature_allows_jit(&entry);
    let sandboxed = cfg!(target_os = "macos") && !grants.full_access;
    if cfg!(windows) && !grants.full_access {
        note(&mut results, "not sandboxed: Windows' helper sandbox (AppContainer) isn't built yet, so the helper ran as a plain program in this throwaway machine".into());
        grants.full_access = true;
    }
    if grants.full_access && !cfg!(windows) {
        note(&mut results, "full access: the helper ran outside any sandbox, observed only".into());
    }
    let spec = lightair_helpers::Spec {
        id: a.id.clone(),
        entry: entry.clone(),
        args: Vec::new(),
        package_dir: package_dir.clone(),
        data_dir: data_dir.clone(),
        grants,
        env: vec![("LIGHTAIR_CHECK".into(), "1".into())],
    };
    let launcher = a.launcher.clone().or_else(|| {
        std::env::current_exe().ok().and_then(|e| e.parent().map(|d| d.join("lightair-sandbox"))).filter(|p| p.exists())
    });
    let (tx, rx) = lightair_helpers::events();
    let started_at = SystemTime::now();
    let before = children_usage();
    let mut helper = match lightair_helpers::launch(
        &spec,
        launcher.as_deref().unwrap_or(std::path::Path::new("lightair-sandbox")),
        tx,
    ) {
        Ok(h) => h,
        Err(e) => {
            fail(&mut results, format!("the helper couldn't be started: {e}"));
            return report(a, results, Vec::new(), Vec::new(), Vec::new());
        }
    };
    let pid = helper.child.pid();

    // its log, and its answers
    let (lines_tx, lines_rx) = mpsc::channel::<String>();
    if let Some(out) = helper.child.stdout.take() {
        std::thread::spawn(move || {
            for line in BufReader::new(out).lines().map_while(Result::ok) {
                if lines_tx.send(line).is_err() {
                    break;
                }
            }
        });
    }
    let log: std::sync::Arc<std::sync::Mutex<Vec<String>>> = Default::default();
    if let Some(err) = helper.child.stderr.take() {
        let log = log.clone();
        std::thread::spawn(move || {
            for line in BufReader::new(err).lines().map_while(Result::ok) {
                let mut l = log.lock().unwrap_or_else(|e| e.into_inner());
                if l.len() < 200 {
                    l.push(line);
                }
            }
        });
    }

    // a fake Lightair: the hello PiAir's protocol starts with (a helper of another protocol
    // answers or not; either way it must leave when its stdin closes)
    let hello = json!({
        "t": "hello", "version": 2, "platform": std::env::consts::OS,
        "data_dir": data_dir, "lightair": {"name": "Lightair (check)", "version": "check"}, "settings": {}
    });
    if let Some(stdin) = helper.child.stdin.as_mut() {
        let _ = writeln!(stdin, "{hello}");
        let _ = stdin.flush();
    }
    // its calls are answered "not in a check" (no secrets, no Lightair methods), and listed
    let started = Instant::now();
    let deadline = started + Duration::from_secs(a.ready_secs);
    let mut asked: std::collections::BTreeSet<String> = Default::default();
    let mut answered = false;
    let mut ready = false;
    while Instant::now() < deadline && !ready {
        match lines_rx.recv_timeout(Duration::from_millis(100)) {
            Ok(line) => {
                answered = true;
                let Ok(m) = serde_json::from_str::<Value>(&line) else { continue };
                match m["t"].as_str() {
                    Some("ready") => ready = true,
                    Some("call") => {
                        asked.insert(m["method"].as_str().unwrap_or("?").to_string());
                        let reply = json!({"t": "reply", "id": m["id"], "error": {"code": "check", "message": "not available in a check"}});
                        if let Some(stdin) = helper.child.stdin.as_mut() {
                            let _ = writeln!(stdin, "{reply}");
                            let _ = stdin.flush();
                        }
                    }
                    _ => {}
                }
            }
            Err(_) if helper.child.try_wait().is_some() => break,
            Err(_) => {}
        }
    }
    match (ready, answered) {
        (true, _) => note(&mut results, format!("ready in {} ms", started.elapsed().as_millis())),
        (false, true) => note(&mut results, format!("it spoke but didn't say ready within {} s", a.ready_secs)),
        (false, false) if helper.child.try_wait().is_some() => fail(&mut results, "it stopped before answering".into()),
        (false, false) => fail(&mut results, format!("it didn't answer within {} s", a.ready_secs)),
    }
    if !asked.is_empty() {
        note(&mut results, format!("it asked Lightair for: {}", asked.into_iter().collect::<Vec<_>>().join(", ")));
    }
    // a little more time to work (it may fetch what it needs), then its stdin closes
    let settle = Instant::now() + Duration::from_secs(3);
    while Instant::now() < settle && helper.child.try_wait().is_none() {
        let _ = lines_rx.recv_timeout(Duration::from_millis(100));
    }
    helper.child.stdin = None;
    let until = Instant::now() + Duration::from_secs(5);
    let mut exited = helper.child.try_wait().is_some();
    while !exited && Instant::now() < until {
        std::thread::sleep(Duration::from_millis(50));
        exited = helper.child.try_wait().is_some();
    }
    if !exited {
        fail(&mut results, "it didn't leave within 5 s of its stdin closing".into());
        helper.child.kill();
    }

    let usage = children_usage().minus(&before);
    if usage.max_rss_mb > 2048.0 {
        fail(&mut results, format!("it used {:.0} MB of memory", usage.max_rss_mb));
    }
    if usage.cpu_s > 30.0 {
        fail(&mut results, format!("it used {:.1} s of CPU in a few seconds", usage.cpu_s));
    } else {
        note(&mut results, format!("{:.2} s of CPU, {:.0} MB of memory at most", usage.cpu_s, usage.max_rss_mb));
    }

    let network: Vec<Value> =
        rx.try_iter().map(|e| json!({"host": e.host, "port": e.port, "allowed": e.allowed})).collect();
    for n in network.iter().filter(|n| n["allowed"] == false) {
        fail(
            &mut results,
            format!(
                "it tried to reach {}:{}, which its permissions don't allow",
                n["host"].as_str().unwrap_or(""),
                n["port"]
            ),
        );
    }
    let violations = if sandboxed { sandbox_refusals(pid, started_at) } else { Ok(Vec::new()) };
    let violations = match violations {
        Ok(v) => v,
        Err(e) => {
            note(&mut results, format!("the sandbox's refusals couldn't be read: {e}"));
            Vec::new()
        }
    };
    for v in violations.iter().take(20) {
        // the system's certificate stores: read by most network runtimes, harmless
        if ["/private/etc/ssl", "/System/Library/Keychains", "/Library/Keychains", "com.apple.trustd"]
            .iter()
            .any(|b| v.contains(b))
        {
            note(&mut results, format!("the sandbox refused a certificate store read (harmless): {v}"));
        } else {
            fail(&mut results, format!("the sandbox refused: {v}"));
        }
    }
    if sandboxed {
        note(&mut results, "ran in Lightair's own sandbox (deny by default, its proxy as the only way out, none of Lightair's privacy permissions)".into());
    }
    let processes = log.lock().map(|l| l.iter().take(20).cloned().collect()).unwrap_or_default();
    drop(helper);
    let _ = std::fs::remove_dir_all(&data_dir);
    report(a, results, network, violations, processes)
}

fn report(a: &Args, results: Vec<Value>, network: Vec<Value>, violations: Vec<String>, log: Vec<String>) -> Value {
    let mut results = results;
    if !results.iter().any(|r| r["verdict"] == "fail") {
        results
            .insert(0, json!({"verdict": "pass", "message": "it started, answered and left as asked", "file": null}));
    }
    json!({
        "checks": [{"name": format!("helper-run-{}", a.platform), "version": "1.0.0", "results": results}],
        "observed": {"network": network, "files": [], "processes": log.iter().map(|l| format!("log: {l}")).collect::<Vec<_>>(), "sandbox_violations": violations},
    })
}

/// Installed package files may come without their executable bit.
fn make_executable(path: &std::path::Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(meta) = std::fs::metadata(path) {
            let mut p = meta.permissions();
            p.set_mode(p.mode() | 0o755);
            let _ = std::fs::set_permissions(path, p);
        }
    }
    #[cfg(not(unix))]
    let _ = path;
}

/// Whether the program's code signature allows JIT (macOS: `com.apple.security.cs.allow-jit`),
/// which its sandbox then allows too.
fn signature_allows_jit(path: &std::path::Path) -> bool {
    if !cfg!(target_os = "macos") {
        return false;
    }
    std::process::Command::new("/usr/bin/codesign")
        .args(["-d", "--entitlements", "-", "--xml"])
        .arg(path)
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).contains("com.apple.security.cs.allow-jit"))
        .unwrap_or(false)
}

/// What the sandbox refused the helper, from the system log (macOS).
fn sandbox_refusals(pid: i32, since: SystemTime) -> Result<Vec<String>, String> {
    // the log takes a moment to have them
    std::thread::sleep(Duration::from_secs(2));
    let secs = since.elapsed().map(|d| d.as_secs() + 5).unwrap_or(60);
    let out = std::process::Command::new("/usr/bin/log")
        .args(["show", "--style", "compact", "--last", &format!("{secs}s"), "--predicate"])
        .arg(format!("eventMessage CONTAINS \"Sandbox:\" AND eventMessage CONTAINS \"({pid})\""))
        .output()
        .map_err(|e| e.to_string())?;
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).trim().to_string());
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let mut seen = std::collections::BTreeSet::new();
    for line in text.lines() {
        if let Some(i) = line.find("deny(") {
            let what = line[i..].split_whitespace().skip(1).collect::<Vec<_>>().join(" ");
            seen.insert(what.chars().take(200).collect::<String>());
        }
    }
    Ok(seen.into_iter().collect())
}

#[derive(Clone, Copy, Default)]
struct Usage {
    cpu_s: f64,
    max_rss_mb: f64,
}

impl Usage {
    fn minus(self, before: &Usage) -> Usage {
        Usage { cpu_s: (self.cpu_s - before.cpu_s).max(0.0), max_rss_mb: self.max_rss_mb }
    }
}

/// CPU time and peak memory of the processes this one waited for.
fn children_usage() -> Usage {
    #[cfg(unix)]
    unsafe {
        let mut u: libc::rusage = std::mem::zeroed();
        if libc::getrusage(libc::RUSAGE_CHILDREN, &mut u) == 0 {
            let t = |tv: libc::timeval| tv.tv_sec as f64 + tv.tv_usec as f64 / 1e6;
            // ru_maxrss: bytes on macOS, kilobytes elsewhere
            let rss =
                if cfg!(target_os = "macos") { u.ru_maxrss as f64 / 1048576.0 } else { u.ru_maxrss as f64 / 1024.0 };
            return Usage { cpu_s: t(u.ru_utime) + t(u.ru_stime), max_rss_mb: rss };
        }
    }
    Usage::default()
}
