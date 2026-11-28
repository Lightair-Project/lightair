//! Script plugins in the desktop (tier 3, docs/13 stage D): each installed `extension` package's
//! `main.js` runs in its own isolate (`lightair_script`), started and stopped like helpers (the
//! same on/off switch and settings, `helpers_host`), with every call checked against what it
//! holds now and recorded in the plugin activity.
//!
//! What scripts add reaches the app through the same places helpers' do: commands in the command
//! palette and steps in automations (`helpers_host` lists both), settings in the Packages panel,
//! their panels in the Plugin panels panel (`shell::plugin_panels`), and overlays in Add Object ›
//! Widgets › From packages (pages with a message channel to the script).

use std::collections::BTreeMap;
use std::net::ToSocketAddrs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use lightair_packages::{Installed, Kind, Tier};
use lightair_script::{Budget, Declared, FetchRequest, FetchResponse, Package, Record, Script, State};
use serde_json::{Value, json};

use crate::plugin_activity::{self as pa, Kind as Use};

/// One script Lightair runs.
pub struct Run {
    pub id: String,
    pub name: String,
    pub version: String,
    granted: Vec<String>,
    /// The script's folder in the installed package.
    pub folder: PathBuf,
    script: Script,
}

#[derive(Default)]
struct Host {
    running: Mutex<BTreeMap<String, Arc<Run>>>,
    /// Gave up on (suspended or failed) until started again.
    held: Mutex<Vec<String>>,
    widgets: Mutex<Option<lightair_widgets::Widgets>>,
    /// Overlay objects in the open show: (package id, overlay id) → widget instance ids.
    overlays: Mutex<BTreeMap<(String, String), Vec<String>>>,
    last_scene: Mutex<String>,
    last_chat: AtomicU64,
    /// A script's `lightair.files.pick()`, waiting for the window to ask the user.
    pick: Mutex<Option<std::sync::mpsc::Sender<Option<PathBuf>>>>,
}

fn host() -> &'static Host {
    static H: OnceLock<Host> = OnceLock::new();
    H.get_or_init(Host::default)
}

static GENERATION: AtomicU64 = AtomicU64::new(0);

fn bump() {
    GENERATION.fetch_add(1, Ordering::Relaxed);
}

/// Changes when a script starts, stops, or changes what it shows (panels redraw).
pub fn generation() -> u64 {
    GENERATION.load(Ordering::Relaxed)
}

fn running(id: &str) -> Option<Arc<Run>> {
    host().running.lock().unwrap_or_else(|e| e.into_inner()).get(id).cloned()
}

fn all() -> Vec<Arc<Run>> {
    host().running.lock().unwrap_or_else(|e| e.into_inner()).values().cloned().collect()
}

/// Starts the host's loop (once): keeps the right scripts running and gives them chat and scene
/// changes.
pub fn init() {
    static STARTED: OnceLock<()> = OnceLock::new();
    if STARTED.set(()).is_err() {
        return;
    }
    let _ = std::thread::Builder::new().name("scripts".into()).spawn(|| {
        loop {
            sync();
            forward();
            std::thread::sleep(Duration::from_millis(500));
        }
    });
}

/// The widget server, for overlays' messages (set by the window once it has one).
pub fn set_widgets(w: lightair_widgets::Widgets) {
    let mut slot = host().widgets.lock().unwrap_or_else(|e| e.into_inner());
    if slot.is_some() {
        return;
    }
    w.set_message_hook(Box::new(|instance, message| {
        let found = host()
            .overlays
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .find(|(_, ids)| ids.iter().any(|i| i == instance))
            .map(|((pkg, overlay), _)| (pkg.clone(), overlay.clone()));
        if let Some((pkg, overlay)) = found
            && let Some(r) = running(&pkg)
        {
            r.script.overlay_message(&overlay, message);
        }
    }));
    *slot = Some(w);
}

/// The show's overlay objects: (package, overlay, widget instance) (kept current by the window).
pub fn set_overlays(list: Vec<(String, String, String)>) {
    let mut map: BTreeMap<(String, String), Vec<String>> = BTreeMap::new();
    for (pkg, overlay, instance) in list {
        map.entry((pkg, overlay)).or_default().push(instance);
    }
    *host().overlays.lock().unwrap_or_else(|e| e.into_inner()) = map;
}

/// A script's request to pick a file, for the window to answer (`answer_pick`).
pub fn wants_pick() -> bool {
    host().pick.lock().unwrap_or_else(|e| e.into_inner()).is_some()
}

pub fn answer_pick(path: Option<PathBuf>) {
    if let Some(tx) = host().pick.lock().unwrap_or_else(|e| e.into_inner()).take() {
        let _ = tx.send(path);
    }
}

/// The scripts that should run: installed, usable, not withdrawn, switched on; the newest of each.
fn wanted() -> BTreeMap<String, Installed> {
    let mut out: BTreeMap<String, Installed> = BTreeMap::new();
    for i in crate::packages::installed().iter() {
        let id = i.id().as_str();
        if !i.manifest.contents.iter().any(|c| c.kind == Kind::Extension)
            || !i.manifest.check_supported_up_to(Tier::Native).is_empty()
            || crate::catalogs::revoked_reason(id, i.version()).is_some()
            || !crate::helpers_host::enabled(id)
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

fn script_folder(i: &Installed) -> Option<PathBuf> {
    i.manifest.contents.iter().find(|c| c.kind == Kind::Extension).map(|c| i.content_path(&c.path))
}

fn sync() {
    let want = wanted();
    let held = host().held.lock().unwrap_or_else(|e| e.into_inner()).clone();
    let mut map = host().running.lock().unwrap_or_else(|e| e.into_inner());
    let stale: Vec<String> = map
        .iter()
        .filter(|(id, r)| match want.get(*id) {
            None => true,
            Some(i) => r.version != i.version().to_string() || r.granted != crate::packages::granted_permissions(i),
        })
        .map(|(id, _)| id.clone())
        .collect();
    for id in stale {
        if let Some(r) = map.remove(&id) {
            r.script.stop();
            tracing::info!(target: "plugins", "script {} stopped", r.name);
            bump();
        }
    }
    for (id, i) in want {
        if map.contains_key(&id) || held.contains(&id) {
            continue;
        }
        let Some(folder) = script_folder(&i) else { continue };
        let code = match std::fs::read_to_string(folder.join("main.js")) {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(target: "plugins", "{}: no main.js ({e})", i.manifest.name);
                host().held.lock().unwrap_or_else(|e| e.into_inner()).push(id.clone());
                continue;
            }
        };
        let granted = crate::packages::granted_permissions(&i);
        let package = Package {
            id: id.clone(),
            name: i.manifest.name.clone(),
            code,
            permissions: granted.clone(),
            data_dir: Some(lightair_platform::paths::data_dir().join("PackageData").join(&id)),
            settings: Value::Object(crate::helpers_host::setting_values(&id)),
        };
        let app = Arc::new(AppHost { id: id.clone(), name: i.manifest.name.clone(), granted: granted.clone() });
        let script = Script::start(package, app, Budget::default());
        tracing::info!(target: "plugins", "script {} {} started", i.manifest.name, i.version());
        map.insert(
            id.clone(),
            Arc::new(Run {
                id,
                name: i.manifest.name.clone(),
                version: i.version().to_string(),
                granted,
                folder,
                script,
            }),
        );
        bump();
    }
}

/// New chat to scripts that may read it, and the program scene's changes to those that may read
/// the show.
fn forward() {
    let list = all();
    if list.is_empty() {
        return;
    }
    if let Some(hub) = crate::helpers_host::hub() {
        let since = host().last_chat.load(Ordering::Relaxed);
        let mut events = Vec::new();
        for ev in hub.since(since) {
            host().last_chat.fetch_max(ev.id, Ordering::Relaxed);
            if since > 0 {
                events.push(serde_json::to_value(&ev).unwrap_or(Value::Null));
            }
        }
        if since == 0 {
            // the first look only notes where chat is: scripts get what comes after they start
            host().last_chat.store(hub.history().last().map_or(1, |e| e.id.max(1)), Ordering::Relaxed);
        }
        for r in list.iter().filter(|r| r.granted.iter().any(|g| g == "chat.read")) {
            for e in &events {
                r.script.event("chat", e.clone());
            }
            if !events.is_empty() {
                pa::record(&r.name, Use::Chat, "read chat", true, "");
            }
        }
    }
    let readers: Vec<&Arc<Run>> = list.iter().filter(|r| r.granted.iter().any(|g| g == "project.read")).collect();
    if !readers.is_empty()
        && let Ok(state) = crate::helpers_host::call_as("Scripts", "state.get", json!({}))
    {
        let name = state["program"]["name"].as_str().unwrap_or_default().to_string();
        let mut last = host().last_scene.lock().unwrap_or_else(|e| e.into_inner());
        if !name.is_empty() && *last != name {
            let first = last.is_empty();
            *last = name.clone();
            if !first {
                for r in readers {
                    r.script.event("scene", json!({"name": name}));
                }
            }
        }
    }
}

/// What a script reaches through.
struct AppHost {
    id: String,
    name: String,
    granted: Vec<String>,
}

impl lightair_script::Host for AppHost {
    fn call(&self, method: &str, params: Value) -> Result<Value, String> {
        crate::helpers_host::call_as(&self.name, method, params)
    }

    fn fetch(&self, req: &FetchRequest) -> Result<FetchResponse, String> {
        // the name as written was allowed; where it really is decides the rest
        let (host, port) = host_port(&req.url).ok_or("not an address")?;
        let addrs: Vec<std::net::SocketAddr> =
            (host.as_str(), port).to_socket_addrs().map_err(|e| e.to_string())?.collect();
        let lan = self.granted.iter().any(|g| g == "network.lan");
        let local = self.granted.iter().any(|g| g == &format!("network:localhost:{port}"));
        for a in &addrs {
            let ip = a.ip();
            if ip.is_loopback() && !local {
                return Err(format!("{host} is this computer, and it didn't declare network:localhost:{port}"));
            }
            if is_private(ip) && !lan && !ip.is_loopback() {
                return Err(format!("{host} is on the home network, and it didn't declare network.lan"));
            }
        }
        let agent = ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(20)))
            .http_status_as_error(false)
            .build()
            .new_agent();
        let mut builder = match req.method.to_ascii_uppercase().as_str() {
            "POST" | "PUT" | "PATCH" | "DELETE" => {
                let mut b =
                    ureq::http::Request::builder().method(req.method.to_ascii_uppercase().as_str()).uri(&req.url);
                for (k, v) in &req.headers {
                    b = b.header(k, v);
                }
                let request = b.body(req.body.clone().unwrap_or_default()).map_err(|e| e.to_string())?;
                let mut resp = agent.run(request).map_err(|e| e.to_string())?;
                return Ok(response(&mut resp));
            }
            _ => agent.get(&req.url),
        };
        for (k, v) in &req.headers {
            builder = builder.header(k, v);
        }
        let mut resp = builder.call().map_err(|e| e.to_string())?;
        Ok(response(&mut resp))
    }

    fn record(&self, r: &Record) {
        let kind = match r.permission.as_str() {
            "network" => Some(Use::Network),
            p => Use::of_permission(p),
        };
        match kind {
            _ if r.permission == "time" => {
                pa::notice(&self.name, &format!("{}'s code ran too long and was stopped", self.name), false)
            }
            Some(k) => pa::record(&self.name, k, &r.what, r.allowed, &r.reason),
            None if !r.allowed => {
                pa::notice(&self.name, &format!("{} was refused {}: {}", self.name, r.what, r.reason), false)
            }
            None => {}
        }
    }

    fn declared(&self, _d: &Declared) {
        bump();
    }

    fn panel_update(&self, _panel: &str, _ui: &Value) {
        bump();
    }

    fn overlay_send(&self, overlay: &str, message: &Value) {
        let ids = host()
            .overlays
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&(self.id.clone(), overlay.to_string()))
            .cloned();
        if let (Some(ids), Some(w)) = (ids, host().widgets.lock().unwrap_or_else(|e| e.into_inner()).clone()) {
            for id in ids {
                w.send_message(&id, message);
            }
        }
    }

    fn notify(&self, text: &str) {
        pa::message(&self.name, &format!("{}: {text}", self.name));
    }

    fn pick(&self) -> Option<PathBuf> {
        let (tx, rx) = std::sync::mpsc::channel();
        *host().pick.lock().unwrap_or_else(|e| e.into_inner()) = Some(tx);
        bump();
        rx.recv_timeout(Duration::from_secs(300)).ok().flatten()
    }

    fn log(&self, level: &str, text: &str) {
        match level {
            "error" => tracing::warn!(target: "plugins", "{}: {text}", self.name),
            _ => tracing::info!(target: "plugins", "{}: {text}", self.name),
        }
    }

    fn state(&self, s: &State) {
        match s {
            State::Suspended(why) => {
                pa::notice(&self.name, &format!("{} was suspended: {why}", self.name), true);
                host().held.lock().unwrap_or_else(|e| e.into_inner()).push(self.id.clone());
            }
            State::Failed(why) => {
                pa::notice(&self.name, &format!("{} didn't start: {why}", self.name), false);
                host().held.lock().unwrap_or_else(|e| e.into_inner()).push(self.id.clone());
            }
            _ => {}
        }
        bump();
    }
}

fn response(resp: &mut ureq::http::Response<ureq::Body>) -> FetchResponse {
    let status = resp.status().as_u16();
    let headers = resp
        .headers()
        .iter()
        .filter_map(|(k, v)| Some((k.as_str().to_string(), v.to_str().ok()?.to_string())))
        .collect();
    let body = resp.body_mut().with_config().limit(5 << 20).read_to_string().unwrap_or_default();
    FetchResponse { status, headers, body }
}

fn host_port(url: &str) -> Option<(String, u16)> {
    let (scheme, rest) = url.split_once("://")?;
    let authority = rest.split(['/', '?', '#']).next()?;
    let authority = authority.rsplit_once('@').map_or(authority, |(_, a)| a);
    let default = if scheme.eq_ignore_ascii_case("https") { 443 } else { 80 };
    if let Some(v6) = authority.strip_prefix('[') {
        let (h, after) = v6.split_once(']')?;
        return Some((h.to_string(), after.strip_prefix(':').and_then(|p| p.parse().ok()).unwrap_or(default)));
    }
    Some(match authority.rsplit_once(':') {
        Some((h, p)) => (h.to_string(), p.parse().ok()?),
        None => (authority.to_string(), default),
    })
}

fn is_private(ip: std::net::IpAddr) -> bool {
    match ip {
        std::net::IpAddr::V4(v) => v.is_private() || v.is_link_local(),
        std::net::IpAddr::V6(v) => (v.segments()[0] & 0xfe00) == 0xfc00 || (v.segments()[0] & 0xffc0) == 0xfe80,
    }
}

// ------------------------------------------------------------ what the app offers from them

/// A script's state for the Packages panel: `None` when it isn't running.
pub fn status(id: &str) -> Option<State> {
    running(id).map(|r| r.script.state()).or_else(|| {
        host()
            .held
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .any(|h| h == id)
            .then(|| State::Failed("held".into()))
    })
}

/// Starts a script again after it was suspended or didn't start.
pub fn start_again(id: &str) {
    host().held.lock().unwrap_or_else(|e| e.into_inner()).retain(|h| h != id);
    if let Some(r) = host().running.lock().unwrap_or_else(|e| e.into_inner()).remove(id) {
        r.script.stop();
    }
    crate::helpers_host::set_enabled(id, true);
    bump();
}

/// What a running script has declared.
pub fn declared(id: &str) -> Option<Declared> {
    running(id).map(|r| r.script.declared())
}

/// Every running script's declarations: (package id, package name, what it declared).
pub fn everything() -> Vec<(String, String, Declared)> {
    all().into_iter().map(|r| (r.id.clone(), r.name.clone(), r.script.declared())).collect()
}

/// Whether `id` is a running script.
pub fn is_running(id: &str) -> bool {
    running(id).is_some()
}

pub fn run_command(id: &str, command: &str) -> Result<Value, String> {
    running(id).ok_or_else(|| format!("{id} isn't running"))?.script.command(command)
}

pub fn run_step(id: &str, step: &str, params: Value) -> Result<Value, String> {
    running(id).ok_or_else(|| format!("{id} isn't running"))?.script.step(step, params)
}

/// Someone used an element of a script's panel.
pub fn ui(id: &str, panel: &str, element: &str, value: Value) {
    if let Some(r) = running(id) {
        r.script.ui(panel, element, value);
    }
}

/// A script's settings changed (the Packages panel).
pub fn settings_changed(id: &str) {
    if let Some(r) = running(id) {
        r.script.set_settings(Value::Object(crate::helpers_host::setting_values(id)));
    }
}

/// Every running script's overlays: (package id, version, overlay, its widget folder).
pub fn overlays() -> Vec<(String, String, lightair_script::OverlayDecl, PathBuf)> {
    let mut out = Vec::new();
    for r in all() {
        for o in r.script.declared().overlays {
            let dir = r.folder.join(&o.page);
            if dir.join("index.html").is_file() {
                out.push((r.id.clone(), r.version.clone(), o, dir));
            }
        }
    }
    out
}

/// Development checks (`LIGHTAIR_OPEN=check-driver`): `script-file <path>` (the install screen's
/// text logged, then installed, as a dropped file is), `scripts` (each script's state and what it
/// declared), `script-command <package> <command>`, `script-ui <package> <panel> <element>
/// [json value]`, `script-event <package> <event> <json>`, `script-notices`.
pub fn check(cmd: &str, rest: &str) {
    let mut args = rest.split_whitespace();
    match cmd {
        "script-file" => match crate::packages::prepare(std::path::Path::new(rest.trim())) {
            Ok(p) => {
                let (title, detail) = crate::shell::install_screen_text(&p);
                tracing::info!("CHECK install screen: {title} / {}", detail.replace('\n', " | "));
                match crate::packages::install(p) {
                    Ok(i) => tracing::info!("CHECK installed {}", i.installed.id()),
                    Err(e) => tracing::warn!("CHECK install failed: {e}"),
                }
            }
            Err(e) => tracing::warn!("CHECK prepare failed: {e}"),
        },
        "scripts" => {
            for r in all() {
                tracing::info!(id = r.id, state = ?r.script.state(), declared = ?r.script.declared(), "CHECK script");
            }
            for id in host().held.lock().unwrap_or_else(|e| e.into_inner()).iter() {
                tracing::info!(id, "CHECK script held");
            }
        }
        "script-command" => {
            let (id, command) =
                (args.next().unwrap_or_default().to_string(), args.next().unwrap_or_default().to_string());
            std::thread::spawn(move || tracing::info!(result = ?run_command(&id, &command), "CHECK script command"));
        }
        "script-ui" => {
            let (id, panel, element) =
                (args.next().unwrap_or_default(), args.next().unwrap_or_default(), args.next().unwrap_or_default());
            let value: Value = serde_json::from_str(&args.collect::<Vec<_>>().join(" ")).unwrap_or(Value::Null);
            ui(id, panel, element, value);
        }
        "script-event" => {
            let (id, event) = (args.next().unwrap_or_default(), args.next().unwrap_or_default());
            let data: Value = serde_json::from_str(&args.collect::<Vec<_>>().join(" ")).unwrap_or(Value::Null);
            if let Some(r) = running(id) {
                r.script.event(event, data);
            }
        }
        "script-notices" => {
            for n in pa::notices() {
                tracing::info!(text = n.text, refused = n.refused, "CHECK notice");
            }
        }
        _ => tracing::warn!(cmd, "CHECK unknown script command"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app_host(name: &str, granted: &[&str]) -> Arc<AppHost> {
        Arc::new(AppHost {
            id: format!("io.github.test.{name}"),
            name: name.into(),
            granted: granted.iter().map(|g| g.to_string()).collect(),
        })
    }

    fn start(name: &str, code: &str, granted: &[&str], budget: Budget) -> Script {
        let package = Package {
            id: format!("io.github.test.{name}"),
            name: name.into(),
            code: code.into(),
            permissions: granted.iter().map(|g| g.to_string()).collect(),
            data_dir: None,
            settings: json!({}),
        };
        Script::start(package, app_host(name, granted), budget)
    }

    fn notices_of(name: &str) -> Vec<pa::Notice> {
        pa::notices().into_iter().filter(|n| n.plugin == name).collect()
    }

    #[test]
    fn any_host_doesnt_let_a_script_fetch_this_computer_or_the_home_network() {
        use lightair_script::Host as _;
        let h = app_host("Fetcher", &["network:*"]);
        let get = |url: &str| FetchRequest { url: url.into(), method: "GET".into(), ..Default::default() };
        let local = h.fetch(&get("http://localhost:9/")).expect_err("this computer needs network:localhost:<port>");
        assert!(local.contains("network:localhost:9"), "{local}");
        let lan = h.fetch(&get("http://10.20.30.40/")).expect_err("the home network needs network.lan");
        assert!(lan.contains("network.lan"), "{lan}");
    }

    #[test]
    fn a_refused_call_raises_a_notice_naming_the_plugin() {
        let s = start(
            "Refused Caller",
            r#"lightair.command({ id: "go", title: "Go", run: () => lightair.call("scene.switch", { scene: "Main" }) });"#,
            &["project.read"],
            Budget::default(),
        );
        assert!(s.command("go").is_err());
        let notices = notices_of("Refused Caller");
        assert!(notices.iter().any(|n| n.refused && n.text.contains("scene.switch")), "{notices:?}");
    }

    #[test]
    fn a_suspended_script_raises_a_notice_that_shows_even_while_live() {
        let s = start(
            "Spinner",
            r#"lightair.command({ id: "spin", title: "Spin", run: () => { for (;;) {} } });"#,
            &[],
            Budget { per_call: Duration::from_millis(30), overruns: 2, ..Budget::default() },
        );
        assert!(s.command("spin").is_err());
        assert!(s.command("spin").is_err());
        assert!(matches!(s.state(), State::Suspended(_)), "{:?}", s.state());
        let notices = notices_of("Spinner");
        assert!(notices.iter().any(|n| n.text.contains("ran too long")), "each overrun is announced: {notices:?}");
        assert!(
            notices.iter().any(|n| n.urgent && n.text.contains("was suspended")),
            "suspension is announced even while live: {notices:?}"
        );
    }
}
