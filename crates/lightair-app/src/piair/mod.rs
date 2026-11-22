//! PiAir, Lightair's AI co-host (the private `piair` repo; design in piair/docs/design.md).
//!
//! PiAir is a separate program (a Bun binary next to Lightair) started here with pipes. It sends
//! newline-delimited JSON: calls into Lightair (the remote API's methods, the same ones MCP
//! clients use, plus `computer.*` and `secret.*`), its voice (PCM played on the "PiAir" mixer
//! track, which moves the co-host avatar's mouth), face cues and its status. Lightair sends it
//! chat, what the streamer says, whether the streamer is talking, prompts from the panel and stop.
//!
//! Everything viewers see and hear, and every safety switch, stays here: if PiAir hangs or dies
//! the voice goes quiet, the avatar goes back to its camera and nothing moves the pointer.

pub mod computer;
#[cfg(target_os = "macos")]
pub mod input_mac;
pub mod panel;
mod path;

use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use base64::Engine as _;
use lightair_model::Id;
use lightair_remote::{ApiError, Call, ErrorKind, Reply};
use serde_json::{Value, json};

use lightair_engine::face_drive;
use lightair_engine::piair_voice;

/// Where calls go to be answered on the main thread (the remote API's queue).
pub type Jobs = futures_channel::mpsc::UnboundedSender<(Call, std::sync::mpsc::Sender<Reply>)>;

const KEYCHAIN_SERVICE: &str = "Lightair PiAir";

/// One line of the panel's running log.
#[derive(Clone, Debug, PartialEq)]
pub struct Line {
    pub role: String,
    pub who: Option<String>,
    pub text: String,
}

/// A yes/no question for the streamer (a destructive action), answered in the panel.
pub struct Question {
    pub text: String,
    pub answer: std::sync::mpsc::Sender<bool>,
}

/// A sign-in step from a provider (open a page, enter a code, paste a code back).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Auth {
    pub provider: String,
    pub message: String,
    pub url: Option<String>,
    pub code: Option<String>,
    pub wants_code: bool,
    pub done: bool,
    pub error: bool,
}

/// What the panel shows.
#[derive(Default)]
pub struct State {
    pub running: bool,
    pub ready: bool,
    /// idle, listening, thinking, speaking, acting, error
    pub status: String,
    pub detail: Option<String>,
    pub log: VecDeque<Line>,
    pub settings: Value,
    pub providers: Value,
    pub auth: Option<Auth>,
    pub question: Option<Question>,
    /// Why it isn't running (couldn't start, crashed too often).
    pub error: Option<String>,
    pub restarts: Vec<Instant>,
    /// The human touched the mouse or keyboard: the AI is paused.
    pub paused: bool,
    /// When a picture of the screen last went to the model (the panel shows it for a moment).
    pub last_shot: Option<Instant>,
}

pub struct Shared {
    pub state: Mutex<State>,
    stdin: Mutex<Option<ChildStdin>>,
    child: Mutex<Option<Child>>,
    pub computer: computer::Computer,
    jobs: Jobs,
    /// The remote API's own handler (set once the window exists), so PiAir's calls are answered
    /// exactly as MCP clients' are.
    handler: Mutex<Option<lightair_remote::Handler>>,
    hub: lightair_live::Hub,
    data_dir: PathBuf,
    /// Something changed for the panel.
    pub dirty: AtomicBool,
    /// Bumped on every start, so threads of an old run stop.
    generation: AtomicU64,
    /// The mic tracks' ids (for "the streamer is talking"), kept fresh by the panel.
    pub mics: Mutex<Vec<Id>>,
    /// The streamer wants PiAir on (it's restarted after a crash while this holds).
    pub wanted: AtomicBool,
    /// Listen to the streamer (on-device speech), from PiAir's settings.
    listen: AtomicBool,
}

#[derive(Clone)]
pub struct Host(pub Arc<Shared>);

impl Host {
    pub fn new(jobs: Jobs, hub: lightair_live::Hub) -> Host {
        let shared = Arc::new(Shared {
            state: Mutex::new(State { status: "idle".into(), ..Default::default() }),
            stdin: Mutex::new(None),
            child: Mutex::new(None),
            computer: computer::Computer::default(),
            jobs,
            handler: Mutex::new(None),
            hub,
            data_dir: lightair_platform::paths::data_dir(),
            dirty: AtomicBool::new(true),
            generation: AtomicU64::new(0),
            mics: Mutex::new(Vec::new()),
            wanted: AtomicBool::new(false),
            listen: AtomicBool::new(false),
        });
        let host = Host(shared);
        host.pump();
        host
    }

    /// Answers PiAir's Lightair calls the way the remote API answers MCP clients.
    pub fn set_handler(&self, handler: lightair_remote::Handler) {
        *self.0.handler.lock().unwrap_or_else(|e| e.into_inner()) = Some(handler);
    }

    pub fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.0.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn touch(&self) {
        self.0.dirty.store(true, Ordering::Relaxed);
    }

    pub fn running(&self) -> bool {
        self.state().running
    }

    /// Sends a message to PiAir (dropped when it isn't running).
    pub fn send(&self, m: Value) {
        let mut g = self.0.stdin.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(stdin) = g.as_mut() {
            let mut line = m.to_string();
            line.push('\n');
            if stdin.write_all(line.as_bytes()).and_then(|_| stdin.flush()).is_err() {
                *g = None;
            }
        }
    }

    /// Where PiAir's program is: `LIGHTAIR_PIAIR` (a path, or `bun` to run the piair checkout's
    /// source), next to Lightair, or the piair checkout's build beside this repo.
    fn program() -> Option<Vec<String>> {
        let exe = if cfg!(windows) { "piair.exe" } else { "piair" };
        let checkout = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../piair");
        if let Ok(v) = std::env::var("LIGHTAIR_PIAIR") {
            if v == "bun" {
                let main = checkout.join("src/main.ts");
                return main.exists().then(|| vec!["bun".into(), main.to_string_lossy().into_owned()]);
            }
            return Some(vec![v]);
        }
        if let Some(dir) = std::env::current_exe().ok().and_then(|e| e.parent().map(PathBuf::from)) {
            let p = dir.join(exe);
            if p.exists() {
                return Some(vec![p.to_string_lossy().into_owned()]);
            }
        }
        let target = match (std::env::consts::OS, std::env::consts::ARCH) {
            ("macos", "aarch64") => "darwin-arm64",
            ("macos", _) => "darwin-x64",
            _ => "windows-x64",
        };
        let built = checkout.join("dist").join(target).join(exe);
        built.exists().then(|| vec![built.to_string_lossy().into_owned()])
    }

    /// Starts PiAir (if it isn't running).
    pub fn start(&self) {
        self.0.wanted.store(true, Ordering::Relaxed);
        if self.running() {
            return;
        }
        let Some(cmd) = Self::program() else {
            let mut s = self.state();
            s.error = Some("PiAir isn't in this build of Lightair.".into());
            drop(s);
            self.touch();
            return;
        };
        let mut c = Command::new(&cmd[0]);
        c.args(&cmd[1..]).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            c.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
        }
        let mut child = match c.spawn() {
            Ok(c) => c,
            Err(e) => {
                self.state().error = Some(format!("PiAir couldn't start: {e}"));
                self.touch();
                return;
            }
        };
        let run_id = self.0.generation.fetch_add(1, Ordering::Relaxed) + 1;
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        *self.0.stdin.lock().unwrap_or_else(|e| e.into_inner()) = child.stdin.take();
        *self.0.child.lock().unwrap_or_else(|e| e.into_inner()) = Some(child);
        {
            let mut s = self.state();
            s.running = true;
            s.ready = false;
            s.error = None;
            s.status = "starting".into();
        }
        self.touch();
        if let Some(err) = stderr {
            std::thread::spawn(move || {
                for line in BufReader::new(err).lines().map_while(Result::ok) {
                    tracing::info!(target: "piair", "{line}");
                }
            });
        }
        if let Some(out) = stdout {
            let host = self.clone();
            std::thread::Builder::new()
                .name("piair-read".into())
                .spawn(move || {
                    for line in BufReader::new(out).lines().map_while(Result::ok) {
                        match parse_line(&line) {
                            Ok(m) => host.on_message(m),
                            Err(e) => tracing::warn!(%e, "piair: bad line"),
                        }
                    }
                    host.exited(run_id);
                })
                .ok();
        }
        self.send(json!({
            "t": "hello",
            "version": 1,
            "platform": std::env::consts::OS,
            "data_dir": self.0.data_dir,
            "lightair": {"version": env!("CARGO_PKG_VERSION")},
        }));
    }

    /// Stops PiAir (the panel's switch): asks it to stop, then ends the process.
    pub fn stop(&self) {
        self.0.wanted.store(false, Ordering::Relaxed);
        self.halt();
        *self.0.stdin.lock().unwrap_or_else(|e| e.into_inner()) = None;
        if let Some(mut c) = self.0.child.lock().unwrap_or_else(|e| e.into_inner()).take() {
            // stdin closed: it leaves on its own; make sure
            std::thread::spawn(move || {
                let until = Instant::now() + Duration::from_secs(2);
                while Instant::now() < until {
                    if c.try_wait().ok().flatten().is_some() {
                        return;
                    }
                    std::thread::sleep(Duration::from_millis(50));
                }
                let _ = c.kill();
                let _ = c.wait();
            });
        }
        let mut s = self.state();
        s.running = false;
        s.ready = false;
        s.status = "idle".into();
        drop(s);
        self.touch();
    }

    /// The kill switch: AI drives off, speech cut, PiAir told to stop; killed if it doesn't
    /// answer within a second. Works whatever PiAir is doing.
    pub fn kill_switch(&self) {
        self.halt();
        if !self.running() {
            return;
        }
        let host = self.clone();
        let run_id = self.0.generation.load(Ordering::Relaxed);
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_secs(1));
            // still thinking or acting a second later: end it (restarted if wanted)
            let busy = matches!(host.state().status.as_str(), "acting" | "thinking");
            if busy && host.0.generation.load(Ordering::Relaxed) == run_id {
                if let Some(c) = host.0.child.lock().unwrap_or_else(|e| e.into_inner()).as_mut() {
                    let _ = c.kill();
                }
            }
        });
    }

    /// Everything that must stop now, on Lightair's side.
    fn halt(&self) {
        self.0.computer.ai_drives.store(false, Ordering::Relaxed);
        piair_voice::cancel(None);
        lightair_engine::zoom_to::release();
        if let Some(q) = self.state().question.take() {
            let _ = q.answer.send(false);
        }
        self.send(json!({"t": "stop"}));
        self.send(json!({"t": "computer", "ai_drives": false, "paused": false, "reason": "stopped"}));
        self.touch();
    }

    fn exited(&self, run_id: u64) {
        if self.0.generation.load(Ordering::Relaxed) != run_id {
            return;
        }
        piair_voice::cancel(None);
        self.0.computer.ai_drives.store(false, Ordering::Relaxed);
        *self.0.stdin.lock().unwrap_or_else(|e| e.into_inner()) = None;
        if let Some(mut c) = self.0.child.lock().unwrap_or_else(|e| e.into_inner()).take() {
            let _ = c.wait();
        }
        let wanted = self.0.wanted.load(Ordering::Relaxed);
        let restart = {
            let mut s = self.state();
            s.running = false;
            s.ready = false;
            s.status = "idle".into();
            s.restarts.retain(|t| t.elapsed() < Duration::from_secs(300));
            if wanted && s.restarts.len() < 3 {
                s.restarts.push(Instant::now());
                true
            } else {
                if wanted {
                    s.error = Some("PiAir stopped three times in five minutes; it's off.".into());
                    self.0.wanted.store(false, Ordering::Relaxed);
                }
                false
            }
        };
        self.touch();
        if restart {
            tracing::warn!("piair stopped; starting it again");
            std::thread::sleep(Duration::from_millis(500));
            self.start();
        }
    }

    /// A prompt from the panel.
    pub fn prompt(&self, text: &str) {
        self.send(json!({"t": "prompt", "text": text}));
    }

    pub fn set_settings(&self, values: Value) {
        self.send(json!({"t": "settings", "values": values}));
    }

    // ---------------------------------------------------------------- from PiAir

    fn on_message(&self, m: Incoming) {
        match m {
            Incoming::Voice { utterance, rate, pcm, marks } => piair_voice::push(&utterance, rate, &pcm, &marks),
            Incoming::VoiceEnd { utterance } => piair_voice::end(&utterance),
            Incoming::VoiceCancel { utterance } => piair_voice::cancel(utterance.as_deref()),
            Incoming::Face { mood, look } => {
                if let Some(mood) = mood {
                    face_drive::set_mood(&mood);
                }
                match look {
                    Some(FaceLook::Named(l)) => face_drive::set_look(match l.as_str() {
                        "chat" => face_drive::Look::Chat,
                        "streamer" => face_drive::Look::Streamer,
                        "pointer" => face_drive::Look::Pointer,
                        _ => face_drive::Look::Camera,
                    }),
                    Some(FaceLook::At(x, y)) => face_drive::set_look_at(x, y),
                    None => {}
                }
            }
            Incoming::Call { id, method, params } => {
                let host = self.clone();
                std::thread::spawn(move || {
                    let msg = match host.answer(&method, params) {
                        Ok(result) => json!({"t": "reply", "id": id, "result": result}),
                        Err(e) => json!({"t": "reply", "id": id, "error": {"code": e.0, "message": e.1}}),
                    };
                    host.send(msg);
                });
            }
            Incoming::Ready { settings, providers } => {
                let mut s = self.state();
                s.ready = true;
                s.status = "listening".into();
                self.0.listen.store(
                    settings.pointer("/behaviour/listen").and_then(Value::as_bool).unwrap_or(false),
                    Ordering::Relaxed,
                );
                s.settings = settings;
                s.providers = providers;
            }
            Incoming::Settings { values } => {
                self.0.listen.store(
                    values.pointer("/behaviour/listen").and_then(Value::as_bool).unwrap_or(false),
                    Ordering::Relaxed,
                );
                self.state().settings = values;
            }
            Incoming::Providers { providers } => self.state().providers = providers,
            Incoming::Status { state, detail } => {
                let mut s = self.state();
                s.status = state;
                s.detail = detail;
            }
            Incoming::Transcript(line) => {
                let mut s = self.state();
                s.log.push_back(line);
                while s.log.len() > 200 {
                    s.log.pop_front();
                }
            }
            Incoming::Auth(a) => {
                if let Some(url) = &a.url {
                    open_url(url);
                }
                self.state().auth = Some(a);
            }
            Incoming::Other(_) => {}
        }
        self.touch();
    }

    /// Answers one of PiAir's calls: its own methods here, everything else through the remote
    /// API's dispatcher (the same methods MCP clients reach).
    pub(crate) fn answer(&self, method: &str, params: Value) -> Result<Value, (String, String)> {
        if method.starts_with("computer.") {
            let ask = |text: String| self.ask(text);
            let r = self.0.computer.call(method, &params, &ask).map_err(|r| (r.code.to_string(), r.message));
            if method == "computer.screenshot" && r.is_ok() {
                self.state().last_shot = Some(Instant::now());
                self.touch();
            }
            return r;
        }
        match method {
            "secret.get" => {
                let name = params.get("name").and_then(Value::as_str).unwrap_or("");
                let value = keyring::Entry::new(KEYCHAIN_SERVICE, name).ok().and_then(|e| e.get_password().ok());
                return Ok(json!({"value": value}));
            }
            "secret.set" => {
                let name = params.get("name").and_then(Value::as_str).unwrap_or("");
                let entry =
                    keyring::Entry::new(KEYCHAIN_SERVICE, name).map_err(|e| ("failed".to_string(), e.to_string()))?;
                return match params.get("value").and_then(Value::as_str) {
                    Some(v) => entry.set_password(v).map(|_| json!({})).map_err(|e| ("failed".into(), e.to_string())),
                    None => {
                        let _ = entry.delete_credential();
                        Ok(json!({}))
                    }
                };
            }
            // PiAir talks; it doesn't write in chat
            "chat.send" => return Err(("not_allowed".into(), "PiAir speaks instead of writing in chat.".into())),
            _ => {}
        }
        let call = Call { method: method.to_string(), params, client: "PiAir", device: None };
        let handler = self.0.handler.lock().unwrap_or_else(|e| e.into_inner()).clone();
        if let Some(h) = handler {
            return h(call).map_err(|e| (code_of(&e).into(), e.message));
        }
        let (tx, rx) = std::sync::mpsc::channel();
        self.0
            .jobs
            .unbounded_send((call, tx))
            .map_err(|_| ("closing".to_string(), "Lightair is closing".to_string()))?;
        match rx.recv_timeout(Duration::from_secs(20)) {
            Ok(Ok(v)) => Ok(v),
            Ok(Err(e)) => Err((code_of(&e).into(), e.message)),
            Err(_) => Err(("timeout".into(), "Lightair didn't answer in time".into())),
        }
    }

    /// Puts a yes/no question to the streamer in the panel; no answer in 30 s is a no.
    fn ask(&self, text: String) -> bool {
        let (tx, rx) = std::sync::mpsc::channel();
        if let Some(old) = self.state().question.replace(Question { text, answer: tx }) {
            let _ = old.answer.send(false);
        }
        self.touch();
        let yes = rx.recv_timeout(Duration::from_secs(30)).unwrap_or(false);
        self.state().question = None;
        self.touch();
        yes
    }

    pub fn answer_question(&self, yes: bool) {
        if let Some(q) = self.state().question.take() {
            let _ = q.answer.send(yes);
        }
        self.touch();
    }

    // ---------------------------------------------------------------- to PiAir, all along

    /// A thread that keeps PiAir told: voice playback, chat, the streamer talking, the human
    /// taking over, and runs the speech helper while listening.
    fn pump(&self) {
        let host = self.clone();
        std::thread::Builder::new()
            .name("piair-pump".into())
            .spawn(move || {
                let mut last_chat = host.0.hub.history().last().map_or(0, |e| e.id);
                let mut talking = false;
                let mut loud_until = Instant::now();
                let mut paused = false;
                let mut speech: Option<Speech> = None;
                loop {
                    std::thread::sleep(Duration::from_millis(20));
                    for e in piair_voice::take_events() {
                        let (u, state) = match e {
                            piair_voice::Event::Playing(u) => (u, "playing"),
                            piair_voice::Event::Done(u) => (u, "done"),
                            piair_voice::Event::Cancelled(u) => (u, "cancelled"),
                        };
                        host.send(json!({"t": "voice_state", "utterance": u, "state": state}));
                        let mut s = host.state();
                        if s.ready && s.status != "acting" {
                            s.status = if state == "playing" { "speaking".into() } else { "listening".into() };
                        }
                        drop(s);
                        host.touch();
                    }
                    if !host.running() {
                        speech = None;
                        continue;
                    }
                    for ev in host.0.hub.since(last_chat) {
                        last_chat = last_chat.max(ev.id);
                        if ev.test {
                            // the panel's own test messages still go: they're how a streamer tries it
                        }
                        if let Ok(v) = serde_json::to_value(&ev) {
                            host.send(json!({"t": "chat", "event": v}));
                        }
                    }
                    // the streamer talks: a mic above -42 dBFS, held 300 ms
                    let mics = host.0.mics.lock().unwrap_or_else(|e| e.into_inner()).clone();
                    let loud = mics.iter().filter_map(|id| lightair_engine::meters::meters().get(*id)).any(|c| c.levels().magnitude.iter().any(|m| *m > -42.0));
                    if loud {
                        loud_until = Instant::now() + Duration::from_millis(300);
                    }
                    let now_talking = Instant::now() < loud_until;
                    if now_talking != talking {
                        talking = now_talking;
                        host.send(json!({"t": "streamer", "speaking": talking}));
                    }
                    // the human took over the mouse or keyboard
                    #[cfg(target_os = "macos")]
                    {
                        let drives = host.0.computer.ai_drives.load(Ordering::Relaxed);
                        let now_paused = drives && input_mac::since_human().is_some_and(|d| d < computer::HUMAN_PAUSE);
                        if now_paused != paused {
                            paused = now_paused;
                            host.state().paused = paused;
                            host.touch();
                            host.send(json!({"t": "computer", "ai_drives": drives, "paused": paused, "reason": if paused { "human" } else { "" }}));
                        }
                    }
                    #[cfg(not(target_os = "macos"))]
                    let _ = &mut paused;
                    // listening: the on-device speech helper
                    let listen = host.0.listen.load(Ordering::Relaxed) && host.state().ready;
                    match (&mut speech, listen) {
                        (None, true) => speech = Speech::start(),
                        (Some(_), false) => speech = None,
                        (Some(s), true) => {
                            for (text, done) in s.take() {
                                host.send(json!({"t": "heard", "text": text, "final": done}));
                            }
                            if s.dead() {
                                speech = None;
                            }
                        }
                        (None, false) => {}
                    }
                }
            })
            .ok();
    }
}

/// Where the co-host looks, as PiAir says it.
#[derive(Clone, Debug, PartialEq)]
pub enum FaceLook {
    /// "chat", "streamer", "pointer" or "camera".
    Named(String),
    /// A point: x right, y up, -1..1.
    At(f32, f32),
}

/// A message from PiAir (piair/docs/design.md §2.2), read from one line.
#[derive(Clone, Debug, PartialEq)]
pub enum Incoming {
    Voice {
        utterance: String,
        rate: u32,
        pcm: Vec<u8>,
        marks: Vec<(f32, piair_voice::Shape)>,
    },
    VoiceEnd {
        utterance: String,
    },
    VoiceCancel {
        utterance: Option<String>,
    },
    Face {
        mood: Option<String>,
        look: Option<FaceLook>,
    },
    Call {
        id: Value,
        method: String,
        params: Value,
    },
    Ready {
        settings: Value,
        providers: Value,
    },
    Settings {
        values: Value,
    },
    Providers {
        providers: Value,
    },
    Status {
        state: String,
        detail: Option<String>,
    },
    Transcript(Line),
    Auth(Auth),
    /// A kind this Lightair doesn't know (a newer PiAir): ignored.
    Other(String),
}

/// Reads one line from PiAir. Missing fields take their defaults (a voice piece's rate 24 kHz,
/// a status "idle"); a line that isn't a JSON object with a `t` is an error.
pub fn parse_line(line: &str) -> Result<Incoming, String> {
    let m: Value = serde_json::from_str(line).map_err(|e| e.to_string())?;
    let t = m.get("t").and_then(Value::as_str).ok_or("no message kind (t)")?;
    let text = |k: &str| m.get(k).and_then(Value::as_str).map(str::to_owned);
    let flag = |k: &str| m.get(k).and_then(Value::as_bool).unwrap_or(false);
    let value = |k: &str| m.get(k).cloned().unwrap_or(Value::Null);
    Ok(match t {
        "voice" => Incoming::Voice {
            utterance: text("utterance").unwrap_or_default(),
            rate: m.get("rate").and_then(Value::as_u64).unwrap_or(24_000) as u32,
            pcm: text("pcm").and_then(|b| base64::engine::general_purpose::STANDARD.decode(b).ok()).unwrap_or_default(),
            marks: m
                .get("marks")
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(|k| {
                            Some((k.get("t_ms")?.as_f64()? as f32, piair_voice::Shape::parse(k.get("v")?.as_str()?)?))
                        })
                        .collect()
                })
                .unwrap_or_default(),
        },
        "voice_end" => Incoming::VoiceEnd { utterance: text("utterance").unwrap_or_default() },
        "voice_cancel" => Incoming::VoiceCancel { utterance: text("utterance") },
        "face" => Incoming::Face {
            mood: text("mood"),
            look: match m.get("look") {
                Some(Value::String(l)) => Some(FaceLook::Named(l.clone())),
                Some(Value::Object(o)) => Some(FaceLook::At(
                    o.get("x").and_then(Value::as_f64).unwrap_or(0.0) as f32,
                    o.get("y").and_then(Value::as_f64).unwrap_or(0.0) as f32,
                )),
                _ => None,
            },
        },
        "call" => Incoming::Call {
            id: value("id"),
            method: text("method").unwrap_or_default(),
            params: m.get("params").cloned().unwrap_or(json!({})),
        },
        "ready" => Incoming::Ready { settings: value("settings"), providers: value("providers") },
        "settings" => Incoming::Settings { values: value("values") },
        "providers" => Incoming::Providers { providers: value("providers") },
        "status" => Incoming::Status { state: text("state").unwrap_or_else(|| "idle".into()), detail: text("detail") },
        "transcript" => Incoming::Transcript(Line {
            role: text("role").unwrap_or_default(),
            who: text("who"),
            text: text("text").unwrap_or_default(),
        }),
        "auth" => Incoming::Auth(Auth {
            provider: text("provider").unwrap_or_default(),
            message: text("message").unwrap_or_default(),
            url: text("url"),
            code: text("code"),
            wants_code: flag("prompt"),
            done: flag("done"),
            error: flag("error"),
        }),
        other => Incoming::Other(other.to_string()),
    })
}

fn code_of(e: &ApiError) -> &'static str {
    match e.kind {
        ErrorKind::Unknown => "unknown",
        ErrorKind::Invalid => "invalid",
        ErrorKind::NotFound => "not_found",
        ErrorKind::State => "state",
        ErrorKind::Failed => "failed",
    }
}

/// Opens a sign-in page in the browser (from any thread).
fn open_url(url: &str) {
    if !(url.starts_with("https://") || url.starts_with("http://localhost") || url.starts_with("http://127.0.0.1")) {
        return;
    }
    #[cfg(target_os = "macos")]
    let _ = Command::new("/usr/bin/open").arg(url).spawn();
    #[cfg(windows)]
    let _ = Command::new("rundll32").args(["url.dll,FileProtocolHandler", url]).spawn();
}

/// The bundled `lightair-speech` helper (Apple's on-device speech recognition) while PiAir
/// listens: utterances as they end.
struct Speech {
    child: Child,
    lines: std::sync::mpsc::Receiver<(String, bool)>,
}

impl Speech {
    fn start() -> Option<Speech> {
        if !cfg!(target_os = "macos") {
            return None;
        }
        let exe = std::env::current_exe().ok()?;
        let dir = exe.parent()?;
        let helper = [dir.join("lightair-speech"), dir.join("../lightair-speech")].into_iter().find(|p| p.exists())?;
        let mut child =
            Command::new(helper).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null()).spawn().ok()?;
        let out = child.stdout.take()?;
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut last = String::new();
            for line in BufReader::new(out).lines().map_while(Result::ok) {
                if let Some(text) = line.strip_prefix("TEXT ") {
                    last = text.to_string();
                } else if line == "DONE" && !last.is_empty() {
                    if tx.send((std::mem::take(&mut last), true)).is_err() {
                        break;
                    }
                }
            }
        });
        Some(Speech { child, lines: rx })
    }

    fn take(&mut self) -> Vec<(String, bool)> {
        self.lines.try_iter().collect()
    }

    fn dead(&mut self) -> bool {
        self.child.try_wait().ok().flatten().is_some()
    }
}

impl Drop for Speech {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Makes sure the workspace has PiAir's voice source and its "PiAir" track: the commands to add
/// them, or `None` when they're there.
pub fn voice_track_commands(w: &lightair_model::Workspace) -> Option<lightair_model::Command> {
    if w.sources.values().any(|s| s.kind == piair_voice::KIND) {
        return None;
    }
    let source = lightair_model::Source::new("PiAir", piair_voice::KIND, json!({}));
    let track = lightair_model::Track::new("PiAir", source.id);
    Some(lightair_model::Command::Batch {
        label: "Add PiAir track".into(),
        commands: vec![
            lightair_model::Command::AddSource { source },
            lightair_model::Command::AddTrack { track, index: None },
        ],
    })
}

#[cfg(test)]
mod tests {
    use super::voice_track_commands;

    /// Turning PiAir on adds its voice source and the "PiAir" track once; a workspace that has
    /// them gets no second one.
    #[test]
    fn the_piair_track_is_added_once() {
        let mut w = lightair_model::Workspace::new("Show");
        let add = voice_track_commands(&w).expect("added the first time");
        add.apply(&mut w).unwrap();
        assert_eq!(w.tracks.iter().filter(|t| t.name == "PiAir").count(), 1);
        assert!(voice_track_commands(&w).is_none(), "already there");
    }
}
