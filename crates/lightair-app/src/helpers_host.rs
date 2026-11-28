//! Third-party helpers (tier 4 packages other than PiAir) and the general helper protocol v1
//! (lightair-docs/lightair/docs/helper-protocol.md, plugins.md §4.8, §4.11, §4.12).
//!
//! Every installed, usable, not withdrawn helper the user hasn't switched off is started through
//! `lightair-helpers` (its sandbox, or as a plain program for full access) and kept running:
//! restarted after a crash, at most three times in five minutes. It speaks newline-delimited
//! JSON over stdin and stdout; stderr is its log. Every call it makes into Lightair is checked
//! against the permissions it holds now (`packages::granted_permissions`) and recorded in
//! `plugin_activity`. Taking a permission back, a withdrawal, removing it or switching it off
//! restarts or stops it within a second.

use std::collections::{BTreeMap, HashMap};
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use lightair_model::Id;
use lightair_packages::catalog::Target;
use lightair_packages::{Installed, Kind, Tier};
use lightair_remote::{Call, Handler};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::plugin_activity::{self as pa, Kind as Use};

/// The protocol version this Lightair speaks.
pub const PROTOCOL: u64 = 1;
/// PiAir has its own host (`crate::piair`).
const PIAIR: &str = "app.lightair.piair";

// ------------------------------------------------------------ what a helper provides

/// A command it adds to the command palette.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct CommandDecl {
    pub id: String,
    pub title: String,
}

/// A setting it declares (its Packages panel section), or an automation step's field.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct Setting {
    pub key: String,
    pub label: String,
    /// `text`, `number`, `toggle` or `choice`.
    #[serde(default = "text_kind")]
    pub kind: String,
    #[serde(default)]
    pub default: Value,
    /// A choice's options.
    #[serde(default)]
    pub options: Vec<String>,
}

fn text_kind() -> String {
    "text".into()
}

/// An automation step it adds.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct StepDecl {
    pub id: String,
    pub title: String,
    #[serde(default)]
    pub fields: Vec<Setting>,
}

/// What its `ready` said it provides.
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq)]
pub struct Provides {
    #[serde(default)]
    pub commands: Vec<CommandDecl>,
    #[serde(default)]
    pub steps: Vec<StepDecl>,
    #[serde(default)]
    pub settings: Vec<Setting>,
}

/// How a helper is doing, for the Packages panel.
#[derive(Clone, Debug, Default)]
pub struct Status {
    pub running: bool,
    pub ready: bool,
    pub full_access: bool,
    pub error: Option<String>,
}

// ------------------------------------------------------------ saved choices

#[derive(Default, Serialize, Deserialize)]
struct Saved {
    /// Switched off by the user (on by default: installing it granted what it asks).
    #[serde(default)]
    off: Vec<String>,
    /// Each helper's settings.
    #[serde(default)]
    settings: BTreeMap<String, Map<String, Value>>,
}

fn saved_file() -> PathBuf {
    lightair_platform::paths::data_dir().join("Plugins").join("helpers.json")
}

fn saved() -> &'static Mutex<Saved> {
    static S: OnceLock<Mutex<Saved>> = OnceLock::new();
    S.get_or_init(|| {
        let s = std::fs::read(saved_file()).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
        Mutex::new(s)
    })
}

fn save(s: &Saved) {
    let path = saved_file();
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(bytes) = serde_json::to_vec_pretty(s) {
        let tmp = path.with_extension("json.tmp");
        if std::fs::write(&tmp, bytes).is_ok() {
            let _ = std::fs::rename(&tmp, &path);
        }
    }
}

/// Whether the user left it switched on.
pub fn enabled(id: &str) -> bool {
    !saved().lock().unwrap_or_else(|e| e.into_inner()).off.iter().any(|o| o == id)
}

/// Switches a helper on or off (off stops it now; on starts it within a second).
pub fn set_enabled(id: &str, on: bool) {
    let mut s = saved().lock().unwrap_or_else(|e| e.into_inner());
    s.off.retain(|o| o != id);
    if !on {
        s.off.push(id.to_string());
    }
    save(&s);
    drop(s);
    if on && let Some(r) = host().running.lock().unwrap_or_else(|e| e.into_inner()).get(id) {
        *r.gave_up.lock().unwrap_or_else(|e| e.into_inner()) = false;
    }
    bump();
}

/// A helper's settings as saved, with the declared defaults filled in.
pub fn setting_values(id: &str) -> Map<String, Value> {
    let mut out = Map::new();
    if let Some(p) = provides(id) {
        for s in p.settings {
            out.insert(s.key.clone(), s.default.clone());
        }
    }
    if let Some(saved) = saved().lock().unwrap_or_else(|e| e.into_inner()).settings.get(id) {
        for (k, v) in saved {
            out.insert(k.clone(), v.clone());
        }
    }
    out
}

/// Changes one setting; the helper is told at once.
pub fn set_setting(id: &str, key: &str, value: Value) {
    {
        let mut s = saved().lock().unwrap_or_else(|e| e.into_inner());
        s.settings.entry(id.to_string()).or_default().insert(key.to_string(), value);
        save(&s);
    }
    if let Some(r) = running(id) {
        r.send(json!({"t": "settings", "values": setting_values(id)}));
    }
    crate::scripts_host::settings_changed(id);
    bump();
}

// ------------------------------------------------------------ the host

struct Host {
    handler: Mutex<Option<Handler>>,
    hub: Mutex<Option<lightair_live::Hub>>,
    running: Mutex<BTreeMap<String, Arc<Running>>>,
    tracks: Mutex<Vec<(Id, String)>>,
    started: AtomicBool,
}

fn host() -> &'static Host {
    static H: OnceLock<Host> = OnceLock::new();
    H.get_or_init(|| Host {
        handler: Mutex::new(None),
        hub: Mutex::new(None),
        running: Mutex::new(BTreeMap::new()),
        tracks: Mutex::new(Vec::new()),
        started: AtomicBool::new(false),
    })
}

static GENERATION: AtomicU64 = AtomicU64::new(1);

fn bump() {
    GENERATION.fetch_add(1, Ordering::Relaxed);
}

/// Changes when a helper starts, stops, says what it provides or fails (the panel redraws).
pub fn generation() -> u64 {
    GENERATION.load(Ordering::Relaxed)
}

/// One installed helper Lightair manages (running or not).
struct Running {
    id: String,
    name: String,
    version: String,
    /// What it was started with (declared, not taken back): a change restarts it.
    granted: Vec<String>,
    /// What it may ask for while running; held ones are checked live (no restart).
    optional: Vec<String>,
    full_access: bool,
    proc: Mutex<Option<lightair_helpers::Helper>>,
    stdin: Mutex<Option<Box<dyn Write + Send>>>,
    ready: AtomicBool,
    provides: Mutex<Option<Provides>>,
    /// Lightair's requests waiting for a reply (commands, steps).
    pending: Mutex<HashMap<u64, mpsc::Sender<Result<Value, String>>>>,
    next: AtomicU64,
    error: Mutex<Option<String>>,
    restarts: Mutex<Vec<Instant>>,
    /// Stopped three times in five minutes: left off until the user starts it again.
    gave_up: Mutex<bool>,
    run: AtomicU64,
    last_chat: AtomicU64,
}

impl Running {
    fn send(&self, m: Value) {
        let mut g = self.stdin.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(stdin) = g.as_mut() {
            let mut line = m.to_string();
            line.push('\n');
            if stdin.write_all(line.as_bytes()).and_then(|_| stdin.flush()).is_err() {
                *g = None;
            }
        }
    }

    fn alive(&self) -> bool {
        let mut p = self.proc.lock().unwrap_or_else(|e| e.into_inner());
        match p.as_mut() {
            Some(h) => h.child.try_wait().is_none(),
            None => false,
        }
    }

    fn has(&self, permission: &str) -> bool {
        self.granted.iter().any(|g| g == permission)
            || (self.optional.iter().any(|o| o == permission)
                && crate::grants::optional_granted(&self.id).contains(permission))
    }

    /// Asks it to stop: `stop`, stdin closed, killed after two seconds.
    fn halt(&self) {
        self.send(json!({"t": "stop"}));
        *self.stdin.lock().unwrap_or_else(|e| e.into_inner()) = None;
        if let Some(mut h) = self.proc.lock().unwrap_or_else(|e| e.into_inner()).take() {
            std::thread::spawn(move || {
                let until = Instant::now() + Duration::from_secs(2);
                while Instant::now() < until {
                    if h.child.try_wait().is_some() {
                        return;
                    }
                    std::thread::sleep(Duration::from_millis(50));
                }
                h.child.kill();
            });
        }
        for (_, tx) in self.pending.lock().unwrap_or_else(|e| e.into_inner()).drain() {
            let _ = tx.send(Err("the helper stopped".into()));
        }
        self.ready.store(false, Ordering::Relaxed);
    }

    /// Sends a request and waits for its reply.
    fn request(&self, mut m: Value, timeout: Duration) -> Result<Value, String> {
        if !self.ready.load(Ordering::Relaxed) {
            return Err(format!("{} isn't running", self.name));
        }
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = mpsc::channel();
        self.pending.lock().unwrap_or_else(|e| e.into_inner()).insert(id, tx);
        m["id"] = json!(id);
        self.send(m);
        let out = rx.recv_timeout(timeout).unwrap_or_else(|_| Err(format!("{} didn't answer in time", self.name)));
        self.pending.lock().unwrap_or_else(|e| e.into_inner()).remove(&id);
        out
    }
}

fn running(id: &str) -> Option<Arc<Running>> {
    host().running.lock().unwrap_or_else(|e| e.into_inner()).get(id).cloned()
}

/// Starts the host: Lightair's method handler (as MCP clients'), the chat hub, and the thread
/// that keeps the right helpers running. Called once when the window exists.
pub fn init(handler: Handler, hub: lightair_live::Hub) {
    let h = host();
    *h.handler.lock().unwrap_or_else(|e| e.into_inner()) = Some(handler);
    *h.hub.lock().unwrap_or_else(|e| e.into_inner()) = Some(hub);
    if h.started.swap(true, Ordering::Relaxed) {
        return;
    }
    let _ = std::thread::Builder::new().name("helpers".into()).spawn(|| {
        loop {
            sync();
            forward_chat();
            std::thread::sleep(Duration::from_secs(1));
        }
    });
}

/// The show's tracks, for `audio.levels` (kept current by the window).
pub fn set_tracks(tracks: Vec<(Id, String)>) {
    *host().tracks.lock().unwrap_or_else(|e| e.into_inner()) = tracks;
}

/// The helpers that should run: installed, usable, not withdrawn, not PiAir; the newest version
/// of each.
fn wanted() -> BTreeMap<String, Installed> {
    let mut out: BTreeMap<String, Installed> = BTreeMap::new();
    for i in crate::packages::installed().iter() {
        let id = i.id().as_str();
        if id == PIAIR
            || !i.manifest.contents.iter().any(|c| c.kind == Kind::Helper)
            || !i.manifest.check_supported_up_to(Tier::Native).is_empty()
            || crate::catalogs::revoked_reason(id, i.version()).is_some()
            || entry_of(i).is_none()
        {
            continue;
        }
        match out.get(id) {
            Some(have) if have.version() >= i.version() => {}
            _ => {
                out.insert(id.to_string(), i.clone());
            }
        }
    }
    out
}

/// Every installed helper (other than PiAir) with its status, for the panel.
pub fn status(id: &str) -> Option<Status> {
    let r = running(id)?;
    Some(Status {
        running: r.alive(),
        ready: r.ready.load(Ordering::Relaxed),
        full_access: r.full_access,
        error: r.error.lock().unwrap_or_else(|e| e.into_inner()).clone(),
    })
}

/// Starts a helper again after it gave up (or was stopped).
pub fn start_again(id: &str) {
    if let Some(r) = running(id) {
        *r.gave_up.lock().unwrap_or_else(|e| e.into_inner()) = false;
        r.restarts.lock().unwrap_or_else(|e| e.into_inner()).clear();
    }
    set_enabled(id, true);
}

fn sync() {
    let want: BTreeMap<String, Installed> = wanted().into_iter().filter(|(id, _)| enabled(id)).collect();
    let mut map = host().running.lock().unwrap_or_else(|e| e.into_inner());
    // gone, off, withdrawn, a new version, or permissions changed: stop
    let stale: Vec<String> = map
        .iter()
        .filter(|(id, r)| match want.get(*id) {
            None => true,
            Some(i) => r.version != i.version().to_string() || r.granted != crate::packages::declared_granted(i),
        })
        .map(|(id, _)| id.clone())
        .collect();
    for id in stale {
        if let Some(r) = map.remove(&id) {
            r.halt();
            tracing::info!(target: "plugins", "{} stopped", r.name);
            bump();
        }
    }
    let mut to_start: Vec<(Arc<Running>, Installed)> = Vec::new();
    for (id, i) in &want {
        let r = map.entry(id.clone()).or_insert_with(|| {
            Arc::new(Running {
                id: id.clone(),
                name: i.manifest.name.clone(),
                version: i.version().to_string(),
                granted: crate::packages::declared_granted(i),
                optional: i.manifest.optional_permissions.clone(),
                full_access: i.manifest.full_access_on(Target::current())
                    && crate::packages::granted_permissions(i).iter().any(|p| p.starts_with("full-access")),
                proc: Mutex::new(None),
                stdin: Mutex::new(None),
                ready: AtomicBool::new(false),
                provides: Mutex::new(None),
                pending: Mutex::new(HashMap::new()),
                next: AtomicU64::new(1),
                error: Mutex::new(None),
                restarts: Mutex::new(Vec::new()),
                gave_up: Mutex::new(false),
                run: AtomicU64::new(0),
                last_chat: AtomicU64::new(0),
            })
        });
        if r.alive() || *r.gave_up.lock().unwrap_or_else(|e| e.into_inner()) {
            continue;
        }
        // not running: a first start, or after an exit (counted)
        if r.run.load(Ordering::Relaxed) > 0 {
            let mut restarts = r.restarts.lock().unwrap_or_else(|e| e.into_inner());
            restarts.retain(|t| t.elapsed() < Duration::from_secs(300));
            if restarts.len() >= 3 {
                *r.gave_up.lock().unwrap_or_else(|e| e.into_inner()) = true;
                *r.error.lock().unwrap_or_else(|e| e.into_inner()) =
                    Some("It stopped three times in five minutes; it's off until you start it again.".into());
                pa::notice(&r.name, &format!("{} stopped three times in five minutes and is off", r.name), false);
                bump();
                continue;
            }
            restarts.push(Instant::now());
        }
        to_start.push((r.clone(), i.clone()));
    }
    // started without the map held: a starting helper's hello reads its settings through it
    drop(map);
    for (r, i) in to_start {
        start(&r, &i);
    }
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

/// Whether the program's signature allows JIT (Bun, V8, .NET): its sandbox then allows generating
/// code (macOS).
fn wants_jit(program: &std::path::Path) -> bool {
    if !cfg!(target_os = "macos") {
        return false;
    }
    std::process::Command::new("/usr/bin/codesign")
        .args(["-d", "--entitlements", "-", "--xml"])
        .arg(program)
        .output()
        .ok()
        .is_some_and(|o| String::from_utf8_lossy(&o.stdout).contains("com.apple.security.cs.allow-jit"))
}

fn start(r: &Arc<Running>, i: &Installed) {
    let run = r.run.fetch_add(1, Ordering::Relaxed) + 1;
    let Some(entry) = entry_of(i) else { return };
    // installed files can lose their executable bit on the way
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(meta) = std::fs::metadata(&entry) {
            let mut perm = meta.permissions();
            if perm.mode() & 0o111 == 0 {
                perm.set_mode(perm.mode() | 0o755);
                let _ = std::fs::set_permissions(&entry, perm);
            }
        }
    }
    let data_dir = lightair_platform::paths::data_dir().join("PackageData").join(&r.id);
    // optional permissions it already holds open their sandbox holes too (new ones at the next start)
    let held: Vec<String> = r.optional.iter().filter(|p| r.has(p)).cloned().collect();
    let mut grants = lightair_helpers::Grants::from_permissions(r.granted.iter().chain(&held).map(String::as_str));
    grants.full_access = r.full_access;
    grants.jit = wants_jit(&entry);
    let spec = lightair_helpers::Spec {
        id: r.id.clone(),
        entry,
        args: Vec::new(),
        package_dir: i.path.clone(),
        data_dir: data_dir.clone(),
        grants,
        env: Vec::new(),
    };
    let launcher =
        std::env::current_exe().ok().and_then(|e| e.parent().map(|d| d.join("lightair-sandbox"))).unwrap_or_default();
    let (tx, rx) = lightair_helpers::events();
    let name = r.name.clone();
    let _ = std::thread::Builder::new().name("helper-network".into()).spawn(move || {
        for e in rx {
            pa::helper_event(&name, &e);
        }
    });
    let mut helper = match lightair_helpers::launch(&spec, &launcher, tx) {
        Ok(h) => h,
        Err(e) => {
            *r.error.lock().unwrap_or_else(|e| e.into_inner()) = Some(format!("It couldn't start: {e}"));
            tracing::warn!(target: "plugins", "{} couldn't start: {e}", r.name);
            bump();
            return;
        }
    };
    if r.full_access {
        pa::full_access_running(&r.name);
    }
    let (stdin, stdout, stderr) = (helper.child.stdin.take(), helper.child.stdout.take(), helper.child.stderr.take());
    *r.stdin.lock().unwrap_or_else(|e| e.into_inner()) = stdin.map(|s| Box::new(s) as Box<dyn Write + Send>);
    *r.proc.lock().unwrap_or_else(|e| e.into_inner()) = Some(helper);
    *r.error.lock().unwrap_or_else(|e| e.into_inner()) = None;
    r.ready.store(false, Ordering::Relaxed);
    if let Some(hub) = host().hub.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
        r.last_chat.store(hub.history().last().map_or(0, |e| e.id), Ordering::Relaxed);
    }
    tracing::info!(target: "plugins", "{} {} started", r.name, r.version);
    bump();
    if let Some(err) = stderr {
        let name = r.name.clone();
        let _ = std::thread::Builder::new().name("helper-log".into()).spawn(move || {
            for line in BufReader::new(err).lines().map_while(Result::ok) {
                tracing::info!(target: "plugins", "{name}: {line}");
            }
        });
    }
    if let Some(out) = stdout {
        let r2 = r.clone();
        let _ = std::thread::Builder::new().name("helper-read".into()).spawn(move || {
            for line in BufReader::new(out).lines().map_while(Result::ok) {
                if r2.run.load(Ordering::Relaxed) != run {
                    break;
                }
                match serde_json::from_str::<Value>(&line) {
                    Ok(m) => on_message(&r2, m),
                    Err(e) => tracing::warn!(target: "plugins", "{}: not a message ({e}): {line}", r2.name),
                }
            }
            r2.ready.store(false, Ordering::Relaxed);
            bump();
        });
    }
    r.send(json!({
        "t": "hello",
        "version": PROTOCOL,
        "lightair": {"version": env!("CARGO_PKG_VERSION")},
        "package": {"id": r.id, "version": r.version},
        "data_dir": data_dir,
        "granted": r.granted,
        "settings": setting_values(&r.id),
    }));
}

fn on_message(r: &Arc<Running>, m: Value) {
    match m["t"].as_str().unwrap_or_default() {
        "ready" => {
            let provides: Provides =
                serde_json::from_value(m.get("provides").cloned().unwrap_or(Value::Null)).unwrap_or_default();
            let v = m["version"].as_u64().unwrap_or(1);
            if v > PROTOCOL {
                *r.error.lock().unwrap_or_else(|e| e.into_inner()) =
                    Some(format!("It speaks protocol {v}; this Lightair speaks {PROTOCOL}: update Lightair."));
            }
            *r.provides.lock().unwrap_or_else(|e| e.into_inner()) = Some(provides);
            r.ready.store(true, Ordering::Relaxed);
            tracing::info!(target: "plugins", "{} is ready", r.name);
            bump();
        }
        "reply" => {
            let Some(id) = m["id"].as_u64() else { return };
            let result = match m.get("error") {
                Some(e) if !e.is_null() => Err(e["message"].as_str().unwrap_or("it failed").to_string()),
                _ => Ok(m.get("result").cloned().unwrap_or(Value::Null)),
            };
            if let Some(tx) = r.pending.lock().unwrap_or_else(|e| e.into_inner()).remove(&id) {
                let _ = tx.send(result);
            }
        }
        "call" => {
            let r = r.clone();
            let _ = std::thread::Builder::new().name("helper-call".into()).spawn(move || {
                let id = m.get("id").cloned().unwrap_or(Value::Null);
                let method = m["method"].as_str().unwrap_or_default().to_string();
                let params = m.get("params").cloned().unwrap_or_else(|| json!({}));
                let reply = match answer(&r, &method, params) {
                    Ok(result) => json!({"t": "reply", "id": id, "result": result}),
                    Err((code, message)) => {
                        json!({"t": "reply", "id": id, "error": {"code": code, "message": message}})
                    }
                };
                r.send(reply);
            });
        }
        other => tracing::debug!(target: "plugins", "{}: unknown message {other}", r.name),
    }
}

type Refusal = (String, String);

fn refuse(r: &Running, kind: Option<Use>, what: &str, permission: &str) -> Refusal {
    if let Some(kind) = kind {
        pa::record(&r.name, kind, what, false, "not granted");
    }
    tracing::info!(target: "plugins", "{} was refused {what}: it doesn't hold {permission}", r.name);
    ("not_allowed".into(), format!("This plugin doesn't have the permission {permission}."))
}

/// `Call::client` is a `&'static str`: each plugin's name kept once for the app's life.
fn client_name(name: &str) -> &'static str {
    static NAMES: OnceLock<Mutex<HashMap<String, &'static str>>> = OnceLock::new();
    let mut map = NAMES.get_or_init(Default::default).lock().unwrap_or_else(|e| e.into_inner());
    if let Some(s) = map.get(name) {
        return s;
    }
    let s: &'static str = Box::leak(name.to_string().into_boxed_str());
    map.insert(name.to_string(), s);
    s
}

/// The chat hub (once the window exists), for script plugins' chat.
pub fn hub() -> Option<lightair_live::Hub> {
    host().hub.lock().unwrap_or_else(|e| e.into_inner()).clone()
}

/// One of Lightair's methods, answered as MCP clients' are, as client `name` (script plugins:
/// already checked against their permissions).
pub fn call_as(name: &str, method: &str, params: Value) -> Result<Value, String> {
    let handler = host().handler.lock().unwrap_or_else(|e| e.into_inner()).clone();
    let Some(h) = handler else { return Err("Lightair isn't ready yet".into()) };
    h(Call { method: method.to_string(), params, client: client_name(name), device: None }).map_err(|e| e.message)
}

fn lightair_call(r: &Running, method: &str, params: Value) -> Result<Value, Refusal> {
    let handler = host().handler.lock().unwrap_or_else(|e| e.into_inner()).clone();
    let Some(h) = handler else { return Err(("not_ready".into(), "Lightair isn't ready yet".into())) };
    h(Call { method: method.to_string(), params, client: client_name(&r.name), device: None })
        .map_err(|e| ("failed".into(), e.message))
}

/// Answers one call, each checked against what it holds now.
fn answer(r: &Running, method: &str, params: Value) -> Result<Value, Refusal> {
    match method {
        // some of its optional permissions: the person answers on a second screen
        "permissions.request" => {
            let wanted = string_list(&params["permissions"]);
            if let Some(p) = wanted.iter().find(|p| !r.optional.contains(p)) {
                return Err(("not_optional".into(), format!("{p} isn't one of the permissions this helper may ask for")));
            }
            let reason = params["reason"].as_str().unwrap_or_default();
            crate::permission_ask::ask(&r.id, &r.name, &wanted, reason);
            let held: Vec<&String> = r.optional.iter().filter(|p| r.has(p)).collect();
            Ok(json!({ "granted": held }))
        }
        "permissions.release" => {
            let gone = crate::permission_ask::release(&r.id, &string_list(&params["permissions"]));
            Ok(json!({ "released": gone }))
        }
        "api.call" => {
            if !r.has("api.call") {
                return Err(refuse(r, None, "Lightair's methods", "api.call"));
            }
            let m = params["method"].as_str().unwrap_or_default().to_string();
            // its own kind of calls go through this protocol, not the remote API
            if m.starts_with("plugins.") || m == "chat.send" {
                return Err(("not_allowed".into(), format!("{m} isn't available through api.call")));
            }
            lightair_call(r, &m, params.get("params").cloned().unwrap_or_else(|| json!({})))
        }
        "chat.send" => {
            if !r.has("chat.write") {
                return Err(refuse(r, Some(Use::Chat), "writing in chat", "chat.write"));
            }
            pa::record(&r.name, Use::Chat, "wrote in chat", true, "");
            lightair_call(r, "chat.send", params)
        }
        "screen.capture" => {
            if !r.has("screen.capture") {
                return Err(refuse(r, Some(Use::Screen), "a screenshot", "screen.capture"));
            }
            pa::record(&r.name, Use::Screen, "a screenshot", true, "");
            screenshot(&params)
        }
        "secret.get" | "secret.set" => {
            if !r.has("secrets") {
                return Err(refuse(r, None, "its saved secrets", "secrets"));
            }
            let name = params["name"].as_str().unwrap_or_default();
            if name.is_empty() || name.len() > 100 {
                return Err(("invalid".into(), "A secret needs a name".into()));
            }
            let entry = keyring::Entry::new("Lightair Plugins", &format!("{}:{name}", r.id))
                .map_err(|e| ("failed".to_string(), e.to_string()))?;
            if method == "secret.get" {
                return Ok(json!({"value": entry.get_password().ok()}));
            }
            match params["value"].as_str() {
                Some(v) => entry.set_password(v).map(|_| json!({})).map_err(|e| ("failed".to_string(), e.to_string())),
                None => {
                    let _ = entry.delete_credential();
                    Ok(json!({}))
                }
            }
        }
        "notifications.show" => {
            if !r.has("notifications") {
                return Err(refuse(r, None, "a notice", "notifications"));
            }
            let text: String = params["text"].as_str().unwrap_or_default().chars().take(200).collect();
            pa::message(&r.name, &format!("{}: {text}", r.name));
            Ok(json!({}))
        }
        "audio.levels" => {
            if !r.has("audio.levels") {
                return Err(refuse(r, None, "sound levels", "audio.levels"));
            }
            let db = |v: f32| if v.is_finite() { json!(v) } else { Value::Null };
            let tracks: Vec<Value> = host()
                .tracks
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .iter()
                .map(|(id, name)| {
                    let l = lightair_host::meters::meters().get(*id).map(|c| c.levels());
                    let (peak, rms) = l.map_or(([f32::NEG_INFINITY; 2], [f32::NEG_INFINITY; 2]), |l| (l.peak, l.magnitude));
                    json!({"track": id, "name": name, "peak": [db(peak[0]), db(peak[1])], "rms": [db(rms[0]), db(rms[1])]})
                })
                .collect();
            Ok(json!({"tracks": tracks}))
        }
        "settings.get" => Ok(Value::Object(setting_values(&r.id))),
        _ => Err(("unknown_method".into(), format!("This Lightair doesn't know {method}"))),
    }
}

/// A display as JPEG (the one under the pointer, or `display`), at most `max_width` wide, with
/// Lightair's own windows blacked out (they can show stream keys and chat).
#[cfg(target_os = "macos")]
fn screenshot(p: &Value) -> Result<Value, Refusal> {
    use crate::piair::input_mac as imp;
    use base64::Engine as _;
    let fail = |e: String| ("failed".to_string(), format!("Couldn't take a screenshot: {e}"));
    let (index, bounds) = match p.get("display").and_then(Value::as_u64) {
        Some(i) => (
            i as u32,
            imp::display_by_index(i as u32).ok_or_else(|| ("invalid".to_string(), "No such display.".to_string()))?,
        ),
        None => imp::display_at(imp::pointer()),
    };
    let jpeg = imp::capture(index).map_err(fail)?;
    let img = image::load_from_memory(&jpeg).map_err(|e| fail(e.to_string()))?;
    let max_w = p.get("max_width").and_then(Value::as_u64).unwrap_or(1280).clamp(320, 1920) as u32;
    let img =
        if img.width() > max_w { img.resize(max_w, u32::MAX, image::imageops::FilterType::Triangle) } else { img };
    let mut rgb = img.to_rgb8();
    let k = rgb.width() as f64 / bounds.size.width.max(1.0);
    let me = std::process::id() as i32;
    for w in imp::windows_on_screen().into_iter().filter(|w| w.pid == me) {
        let x0 = ((w.frame.origin.x - bounds.origin.x) * k).max(0.0) as u32;
        let y0 = ((w.frame.origin.y - bounds.origin.y) * k).max(0.0) as u32;
        let x1 = (((w.frame.origin.x + w.frame.size.width - bounds.origin.x) * k).max(0.0) as u32).min(rgb.width());
        let y1 = (((w.frame.origin.y + w.frame.size.height - bounds.origin.y) * k).max(0.0) as u32).min(rgb.height());
        for y in y0.min(y1)..y1 {
            for x in x0.min(x1)..x1 {
                rgb.put_pixel(x, y, image::Rgb([0, 0, 0]));
            }
        }
    }
    let mut out = Vec::new();
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, 78)
        .encode_image(&rgb)
        .map_err(|e| fail(e.to_string()))?;
    Ok(
        json!({"image": base64::engine::general_purpose::STANDARD.encode(&out), "width": rgb.width(), "height": rgb.height(), "display": index}),
    )
}

#[cfg(not(target_os = "macos"))]
fn screenshot(_: &Value) -> Result<Value, Refusal> {
    Err(("unsupported".into(), "Screenshots for plugins aren't available on this platform yet".into()))
}

/// New chat to the helpers that may read it.
fn forward_chat() {
    let Some(hub) = host().hub.lock().unwrap_or_else(|e| e.into_inner()).clone() else { return };
    let list: Vec<Arc<Running>> = host().running.lock().unwrap_or_else(|e| e.into_inner()).values().cloned().collect();
    for r in list {
        if !r.has("chat.read") || !r.ready.load(Ordering::Relaxed) {
            continue;
        }
        let since = r.last_chat.load(Ordering::Relaxed);
        let mut sent = false;
        for ev in hub.since(since) {
            r.last_chat.fetch_max(ev.id, Ordering::Relaxed);
            if let Ok(v) = serde_json::to_value(&ev) {
                r.send(json!({"t": "chat", "event": v}));
                sent = true;
            }
        }
        if sent {
            pa::record(&r.name, Use::Chat, "read chat", true, "");
        }
    }
}

// ------------------------------------------------------------ what the app offers from them

fn provides(id: &str) -> Option<Provides> {
    running(id)?.provides.lock().unwrap_or_else(|e| e.into_inner()).clone()
}

/// What a helper or script declared (for the panel's settings section), once it's said.
pub fn declared(id: &str) -> Option<Provides> {
    provides(id).or_else(|| crate::scripts_host::declared(id).map(|d| from_script(&d)))
}

/// A script's declarations in the helper protocol's shape.
fn from_script(d: &lightair_script::Declared) -> Provides {
    let setting = |v: &Value| serde_json::from_value::<Setting>(v.clone()).ok();
    Provides {
        commands: d.commands.iter().chain(&d.hotkeys).map(|c| CommandDecl { id: c.id.clone(), title: c.title.clone() }).collect(),
        steps: d
            .steps
            .iter()
            .map(|s| StepDecl { id: s.id.clone(), title: s.title.clone(), fields: s.fields.iter().filter_map(setting).collect() })
            .collect(),
        settings: d.settings.iter().filter_map(setting).collect(),
    }
}

/// Every running helper's commands: (package id, package name, command).
pub fn commands() -> Vec<(String, String, CommandDecl)> {
    let list: Vec<Arc<Running>> = host().running.lock().unwrap_or_else(|e| e.into_inner()).values().cloned().collect();
    let mut out = Vec::new();
    for r in list {
        if !r.ready.load(Ordering::Relaxed) {
            continue;
        }
        if let Some(p) = r.provides.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
            for c in &p.commands {
                out.push((r.id.clone(), r.name.clone(), c.clone()));
            }
        }
    }
    for (id, name, d) in crate::scripts_host::everything() {
        for c in from_script(&d).commands {
            out.push((id.clone(), name.clone(), c));
        }
    }
    out
}

/// Every running helper's automation steps: (package id, package name, step).
pub fn steps() -> Vec<(String, String, StepDecl)> {
    let list: Vec<Arc<Running>> = host().running.lock().unwrap_or_else(|e| e.into_inner()).values().cloned().collect();
    let mut out = Vec::new();
    for r in list {
        if let Some(p) = r.provides.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
            for s in &p.steps {
                out.push((r.id.clone(), r.name.clone(), s.clone()));
            }
        }
    }
    for (id, name, d) in crate::scripts_host::everything() {
        for s in from_script(&d).steps {
            out.push((id.clone(), name.clone(), s));
        }
    }
    out
}

/// Runs a helper's command (the palette, `plugins.command`).
pub fn run_command(id: &str, command: &str) -> Result<Value, String> {
    if crate::scripts_host::is_running(id) {
        return crate::scripts_host::run_command(id, command);
    }
    let r = running(id).ok_or_else(|| format!("{id} isn't running"))?;
    r.request(json!({"t": "command", "command": command}), Duration::from_secs(20))
}

/// Runs a helper's automation step with its fields (`plugins.step`, from automations).
pub fn run_step(id: &str, step: &str, params: Value) -> Result<Value, String> {
    if crate::scripts_host::is_running(id) {
        return crate::scripts_host::run_step(id, step, params);
    }
    let r = running(id).ok_or_else(|| format!("{id} isn't running"))?;
    r.request(json!({"t": "step", "step": step, "params": params}), Duration::from_secs(30))
}

/// The remote API's `plugins.*` methods: Lightair's own way to reach its helpers.
pub fn api(method: &str, params: &Value) -> Option<Result<Value, String>> {
    let package = params["package"].as_str().unwrap_or_default();
    Some(match method {
        "plugins.command" => run_command(package, params["command"].as_str().unwrap_or_default()),
        "plugins.step" => {
            let mut rest = params.clone();
            if let Some(o) = rest.as_object_mut() {
                o.remove("package");
                o.remove("step");
            }
            run_step(package, params["step"].as_str().unwrap_or_default(), rest)
        }
        "plugins.list" => Ok(json!(
            host()
                .running
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .values()
                .map(|r| json!({"package": r.id, "name": r.name, "version": r.version, "running": r.alive(), "ready": r.ready.load(Ordering::Relaxed)}))
                .chain(crate::scripts_host::everything().into_iter().map(|(id, name, _)| json!({"package": id, "name": name, "script": true, "running": true, "ready": true})))
                .collect::<Vec<_>>()
        )),
        _ => return None,
    })
}

/// A JSON array of strings (anything else left out).
fn string_list(v: &Value) -> Vec<String> {
    v.as_array().map(|a| a.iter().filter_map(|x| x.as_str().map(str::to_string)).collect()).unwrap_or_default()
}
