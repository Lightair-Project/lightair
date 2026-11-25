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
pub mod keys;
mod path;
pub mod view;
pub mod voice_sheet;
pub mod voices;

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

use lightair_obs::face_drive;
use lightair_obs::piair_voice;

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
    /// A choice to make (id, label) instead of a code to paste.
    pub options: Vec<(String, String)>,
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
    /// Realtime voice is on (the active co-host's voice is "ChatGPT voice"): conversation audio is
    /// streamed up to PiAir raw, not transcribed.
    realtime: AtomicBool,
    /// The co-host object's settings PiAir was started with (sent again on a restart).
    start_settings: Mutex<Value>,
    /// The mixer tracks the co-host hears, and as what (the active co-host's "Hears").
    pub hears: Mutex<Vec<Hearing>>,
    /// The engine, to tap what it hears.
    engine: std::sync::Arc<lightair_obs::Engine>,
    /// AI keys, voice lists and previews (Settings → Connections, the Voice section).
    pub ai: Mutex<AiState>,
    /// How each heard track's listening is going, for the Inspector's Hears list.
    pub hear_status: Mutex<std::collections::HashMap<Id, HearStatus>>,
    /// A subscription sign-in's own PiAir (it only signs in), with its run number.
    signin: Mutex<Option<(u64, Child, ChildStdin)>>,
    signin_gen: AtomicU64,
}

/// What a heard track is to the co-host.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    /// People talking to it: it waits while they speak, understands them, answers its name.
    Conversation,
    /// Something playing (a video, a game): context it may react to, never someone addressing it.
    Content,
}

/// How listening to a heard track is going: what the Inspector says under it.
#[derive(Clone, Debug, PartialEq)]
pub struct HearStatus {
    pub text: String,
    /// 0 fine, 1 worth a look (no sound), 2 it can't hear (the reason in `text`).
    pub level: u8,
}

impl Role {
    /// Its name in the co-host's settings (`cohost.hearing.<key>`).
    pub fn key(self) -> &'static str {
        match self {
            Role::Conversation => "conversation",
            Role::Content => "content",
        }
    }
}

/// What turns a heard track's sound into words, chosen per role on the co-host
/// (`cohost.hearing.conversation` / `.content`: `{engine, model, language}`).
#[derive(Clone, Debug, PartialEq, Default)]
pub struct Engine {
    /// "apple" (on this Mac, the default), "elevenlabs" (Scribe Live) or "openrouter".
    pub kind: String,
    /// The engine's model ("scribe_v3_live", an OpenRouter model id); empty for its default.
    pub model: String,
    /// A language code to hear in; empty: any (ElevenLabs, OpenRouter) or the system's (Apple).
    pub language: String,
}

impl Engine {
    pub fn of(cohost: &Value, role: Role) -> Engine {
        let v = &cohost["hearing"][role.key()];
        let text = |k: &str| v[k].as_str().unwrap_or("").to_string();
        let kind = text("engine");
        Engine {
            kind: if kind.is_empty() { "apple".into() } else { kind },
            model: text("model"),
            language: text("language"),
        }
    }
}

/// A mixer track the co-host hears.
#[derive(Clone, Debug, PartialEq)]
pub struct Hearing {
    pub track: Id,
    pub source: Id,
    pub name: String,
    pub role: Role,
    /// What hears it (its role's engine).
    pub engine: Engine,
    /// The track is muted (nothing of it is heard then).
    pub muted: bool,
    /// The co-hosts' voice tracks: what may come back in through a mic from the speakers.
    pub echo: Vec<Id>,
}

/// What a co-host hears, from its `cohost.hears` (`[{track, role}]`; an older single track id is
/// a conversation track). Not set: the first mic in the mixer, as conversation. Tracks that are
/// gone and co-host voices are left out.
pub fn hearing(w: &lightair_model::Workspace, cohost: &Value) -> Vec<crate::piair::Hearing> {
    let echo: Vec<Id> = w
        .tracks
        .iter()
        .filter(|t| w.sources.get(&t.source).is_some_and(|s| s.kind == lightair_obs::piair_voice::KIND))
        .map(|t| t.id)
        .collect();
    let make = |track: &str, role: Role| -> Option<Hearing> {
        let t = w.tracks.iter().find(|t| t.id.to_string() == track)?;
        let source = w.sources.get(&t.source)?;
        (source.kind != lightair_obs::piair_voice::KIND).then(|| Hearing {
            track: t.id,
            source: t.source,
            name: t.name.clone(),
            role,
            engine: Engine::of(cohost, role),
            muted: t.muted,
            echo: echo.clone(),
        })
    };
    match &cohost["hears"] {
        Value::Array(list) => list
            .iter()
            .filter_map(|h| {
                let role = if h["role"] == "content" { Role::Content } else { Role::Conversation };
                make(h["track"].as_str()?, role)
            })
            .collect(),
        Value::String(t) => make(t, Role::Conversation).into_iter().collect(),
        // nothing set: the first audio-input track (a mic or an audio device), as conversation
        _ => w
            .tracks
            .iter()
            .find(|t| {
                w.sources.get(&t.source).is_some_and(|s| {
                    s.kind == lightair_model::kinds::MIC || s.kind == lightair_model::kinds::AUDIO_DEVICE
                })
            })
            .and_then(|t| make(&t.id.to_string(), Role::Conversation))
            .into_iter()
            .collect(),
    }
}

/// Whether a service's key is there and works.
#[derive(Clone, Debug, PartialEq)]
pub enum KeyState {
    Unknown,
    Missing,
    Saved,
    Checking,
    /// It works (with a note when it can't do everything).
    Works(Option<String>),
    Problem(String),
}

/// A vendor's voices, as far as they're known.
#[derive(Clone, Debug, PartialEq)]
pub enum Voices {
    Loading,
    Ready(Vec<voices::VoiceInfo>),
    Failed(String),
}

#[derive(Default)]
pub struct AiState {
    pub keys: std::collections::HashMap<String, KeyState>,
    pub voices: std::collections::HashMap<String, Voices>,
    /// The voice a preview is being made for, and the one playing.
    pub previewing: Option<String>,
    pub playing: Option<String>,
    pub preview_error: Option<String>,
    /// OpenRouter's transcription models (id, name), fetched once when asked for.
    pub stt_models: Option<Result<Vec<(String, String)>, String>>,
    /// Which subscriptions are signed in (Settings → Connections).
    pub subs: std::collections::HashMap<String, bool>,
    /// The subscription sign-in under way, at its latest step.
    pub signin: Option<Auth>,
}

#[derive(Clone)]
pub struct Host(pub Arc<Shared>);

impl Host {
    pub fn new(jobs: Jobs, hub: lightair_live::Hub, engine: std::sync::Arc<lightair_obs::Engine>) -> Host {
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
            realtime: AtomicBool::new(false),
            start_settings: Mutex::new(Value::Null),
            hears: Mutex::new(Vec::new()),
            engine,
            ai: Mutex::new(AiState::default()),
            hear_status: Mutex::new(std::collections::HashMap::new()),
            signin: Mutex::new(None),
            signin_gen: AtomicU64::new(0),
        });
        let host = Host(shared);
        *host.0.computer.allowed.lock().unwrap_or_else(|e| e.into_inner()) = ComputerPrefs::load().allowed;
        host.pump();
        // which AI keys are saved (the Keychain: off the main thread)
        let h = host.clone();
        std::thread::spawn(move || {
            for (id, ..) in keys::SERVICES {
                let state = if keys::get(id).is_some() { KeyState::Saved } else { KeyState::Missing };
                h.0.ai.lock().unwrap_or_else(|e| e.into_inner()).keys.insert((*id).to_string(), state);
            }
            for (id, ..) in keys::SUBSCRIPTIONS {
                let on = keys::signed_in(id);
                h.0.ai.lock().unwrap_or_else(|e| e.into_inner()).subs.insert((*id).to_string(), on);
            }
            h.touch();
        });
        host
    }

    /// Answers PiAir's Lightair calls the way the remote API answers MCP clients.
    pub fn set_handler(&self, handler: lightair_remote::Handler) {
        *self.0.handler.lock().unwrap_or_else(|e| e.into_inner()) = Some(handler);
    }

    pub fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.0.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub(crate) fn touch(&self) {
        self.0.dirty.store(true, Ordering::Relaxed);
    }

    pub fn running(&self) -> bool {
        self.state().running
    }

    /// Whether realtime voice is on (the active co-host's voice is "ChatGPT voice").
    pub fn realtime(&self) -> bool {
        self.0.realtime.load(Ordering::Relaxed)
    }

    pub fn set_realtime(&self, on: bool) {
        self.0.realtime.store(on, Ordering::Relaxed);
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

    /// Starts PiAir (if it isn't running) with the co-host object's settings.
    pub fn start(&self, settings: Value) {
        *self.0.start_settings.lock().unwrap_or_else(|e| e.into_inner()) = settings;
        self.launch();
    }

    fn launch(&self) {
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
            "settings": self.0.start_settings.lock().unwrap_or_else(|e| e.into_inner()).clone(),
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
        lightair_obs::zoom_to::release();
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
            self.launch();
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
            "secret.get" | "secret.set" if crate::secrets::keychain_off() => {
                return Err(("failed".into(), "the Keychain is off for this run (LIGHTAIR_NO_KEYCHAIN)".into()));
            }
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
                let mut listeners: std::collections::HashMap<Id, Listener> = std::collections::HashMap::new();
                let mut failed: std::collections::HashMap<Id, (Instant, String)> = std::collections::HashMap::new();
                let mut forwarders: std::collections::HashMap<Id, Forwarder> = std::collections::HashMap::new();
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
                        listeners.clear();
                        failed.clear();
                        forwarders.clear();
                        host.0.hear_status.lock().unwrap_or_else(|e| e.into_inner()).clear();
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
                    let loud = mics.iter().filter_map(|id| lightair_obs::meters::meters().get(*id)).any(|c| c.levels().magnitude.iter().any(|m| *m > -42.0));
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
                    // listening: one on-device speech helper per heard track, while PiAir runs
                    let ready = host.state().ready;
                    let hears = if ready { host.0.hears.lock().unwrap_or_else(|e| e.into_inner()).clone() } else { Vec::new() };
                    // realtime voice: conversation audio is forwarded raw for the live model to hear,
                    // with no on-device speech recognition
                    let realtime = host.realtime();
                    if realtime {
                        listeners.clear();
                        failed.clear();
                        forwarders.retain(|track, f| {
                            hears.iter().any(|h| h.track == *track && h.role == Role::Conversation && h.source == f.source) && !f.dead()
                        });
                        for h in hears.iter().filter(|h| h.role == Role::Conversation) {
                            if let Some(f) = forwarders.get(&h.track) {
                                f.muted.store(h.muted, Ordering::Relaxed);
                            }
                            if !forwarders.contains_key(&h.track)
                                && let Some(f) = Forwarder::start(&host, h)
                            {
                                forwarders.insert(h.track, f);
                            }
                        }
                        let status: std::collections::HashMap<Id, HearStatus> = hears
                            .iter()
                            .map(|h| {
                                let st = match h.role {
                                    Role::Conversation if forwarders.contains_key(&h.track) => {
                                        HearStatus { text: "Heard live by ChatGPT voice".into(), level: 0 }
                                    }
                                    Role::Conversation => HearStatus { text: "Starting…".into(), level: 1 },
                                    Role::Content => HearStatus { text: "Content isn't used with ChatGPT voice".into(), level: 1 },
                                };
                                (h.track, st)
                            })
                            .collect();
                        let mut current = host.0.hear_status.lock().unwrap_or_else(|e| e.into_inner());
                        if *current != status {
                            *current = status;
                            drop(current);
                            host.touch();
                        }
                        continue;
                    }
                    forwarders.clear();
                    listeners.retain(|track, l| {
                        hears.iter().any(|h| h.track == *track && h.role == l.role && h.source == l.source && h.engine == l.engine)
                    });
                    // one that stopped: why, and a new try in a few seconds (not in a loop)
                    let ended: Vec<Id> = listeners.iter_mut().filter_map(|(t, l)| l.dead().then_some(*t)).collect();
                    for t in ended {
                        if let Some(l) = listeners.remove(&t) {
                            let why = l.state.lock().unwrap_or_else(|e| e.into_inner()).problem.clone();
                            let fallback = if l.engine.kind == "apple" {
                                "Speech recognition stopped; trying again. If it keeps stopping, allow Lightair in System Settings › Privacy & Security › Speech Recognition."
                            } else {
                                "Hearing stopped; trying again…"
                            };
                            failed.insert(t, (Instant::now(), why.unwrap_or_else(|| fallback.into())));
                        }
                    }
                    failed.retain(|t, _| hears.iter().any(|h| h.track == *t));
                    for h in &hears {
                        if let Some(l) = listeners.get(&h.track) {
                            l.muted.store(h.muted, Ordering::Relaxed);
                        }
                        if listeners.contains_key(&h.track) || failed.get(&h.track).is_some_and(|f| f.0.elapsed() < Duration::from_secs(5)) {
                            continue;
                        }
                        match Listener::start(&host.0.engine, h) {
                            Ok(l) => {
                                failed.remove(&h.track);
                                listeners.insert(h.track, l);
                            }
                            Err(why) => {
                                tracing::warn!(track = %h.name, %why, "piair: can't listen");
                                failed.insert(h.track, (Instant::now(), why));
                            }
                        }
                    }
                    // what the Inspector says under each heard track
                    let status: std::collections::HashMap<Id, HearStatus> = hears
                        .iter()
                        .map(|h| {
                            let st = match (listeners.get(&h.track), failed.get(&h.track)) {
                                (Some(l), _) => l.status(),
                                (None, Some((_, why))) => HearStatus { text: why.clone(), level: 2 },
                                (None, None) => HearStatus { text: "Starting…".into(), level: 1 },
                            };
                            (h.track, st)
                        })
                        .collect();
                    let mut current = host.0.hear_status.lock().unwrap_or_else(|e| e.into_inner());
                    if *current != status {
                        *current = status;
                        drop(current);
                        host.touch();
                    }
                    for l in listeners.values_mut() {
                        for (text, done) in l.take() {
                            match l.role {
                                Role::Conversation => host.send(json!({"t": "heard", "text": text, "final": done, "who": l.name})),
                                Role::Content if done => host.send(json!({"t": "content", "text": text, "source": l.name})),
                                Role::Content => {}
                            }
                        }
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
            options: m
                .get("options")
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(|o| Some((o.get("id")?.as_str()?.to_owned(), o.get("label")?.as_str()?.to_owned())))
                        .collect()
                })
                .unwrap_or_default(),
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

// -------------------------------------------------------------------- the AI co-host object

/// The settings key an avatar source carries when it's an AI co-host: `{on, active, voice_track,
/// piair: {provider, model, voice, persona, behaviour}}` (piair/docs/design.md §9).
pub const COHOST: &str = "cohost";

/// An avatar source that is an AI co-host.
pub fn is_cohost(source: &lightair_model::Source) -> bool {
    source.kind == lightair_obs::avatar::KIND && source.settings.get(COHOST).is_some_and(Value::is_object)
}

/// The Hierarchy's and Inspector's icon for a source: the co-host's own, else the kind's.
pub fn icon_for(source: Option<&lightair_model::Source>, kind: &str) -> &'static str {
    if source.is_some_and(is_cohost) { "bot" } else { crate::ui::source_icon(kind) }
}

/// A command as the co-host's voice track should follow it, in the same undo step: removing a
/// co-host's last object removes its voice track (and the track's source), renaming the object
/// renames the track and its source; a copied co-host gets a voice track of its own and comes in
/// inactive while another is active. Other commands as they are.
pub fn with_cohost_voice(
    before: &lightair_model::Workspace,
    command: lightair_model::Command,
) -> lightair_model::Command {
    use lightair_model::Command;
    fn touches(c: &Command) -> bool {
        match c {
            Command::RemoveObject { .. } | Command::RemoveScene { .. } | Command::RenameObject { .. } => true,
            Command::Batch { commands, .. } => commands.iter().any(touches),
            _ => false,
        }
    }
    // a co-host copied in (paste, duplicate): inactive while another is active, and a voice
    // track of its own, not the original's
    let mut command = command;
    let copies = adopt_copies(before, &mut command);
    if !copies.is_empty() {
        let label = command.label();
        let mut commands = vec![command];
        commands.extend(copies);
        return Command::Batch { label, commands };
    }
    if !touches(&command) || !before.sources.values().any(|s| is_cohost(s)) {
        return command;
    }
    let mut after = before.clone();
    if command.apply(&mut after).is_err() {
        return command;
    }
    let mut extra = vec![];
    for (id, source) in &before.sources {
        if !is_cohost(source) {
            continue;
        }
        let Some(track) = source.settings[COHOST]["voice_track"]
            .as_str()
            .and_then(|t| after.tracks.iter().find(|x| x.id.to_string() == t))
        else {
            continue;
        };
        let was = before.objects_using(*id).next().map(|(_, o)| o.name.clone());
        let now = after.objects_using(*id).next().map(|(_, o)| o.name.clone());
        match (was, now) {
            (Some(_), None) => {
                extra.push(Command::RemoveTrack { track: track.id });
                extra.push(Command::RemoveSource { source: track.source });
            }
            (Some(a), Some(b)) if a != b && track.name != b => {
                // the track and its source (what the mixer strip's picker shows) both
                extra.push(Command::RenameTrack { track: track.id, name: b.clone() });
                extra.push(Command::RenameSource { source: track.source, name: b });
            }
            _ => {}
        }
    }
    if extra.is_empty() {
        return command;
    }
    let label = command.label();
    let mut commands = vec![command];
    commands.extend(extra);
    Command::Batch { label, commands }
}

/// Co-host sources `command` adds as copies of others (their voice track already some other
/// co-host's, or not in this workspace): given a voice track of their own, and made inactive
/// while another co-host is active. Returns the commands that add the new voice tracks.
fn adopt_copies(
    before: &lightair_model::Workspace,
    command: &mut lightair_model::Command,
) -> Vec<lightair_model::Command> {
    use lightair_model::Command;
    fn added_tracks(c: &Command, out: &mut Vec<String>) {
        match c {
            Command::AddTrack { track, .. } => out.push(track.id.to_string()),
            Command::Batch { commands, .. } => commands.iter().for_each(|c| added_tracks(c, out)),
            _ => {}
        }
    }
    fn walk(c: &mut Command, f: &mut dyn FnMut(&mut lightair_model::Source)) {
        match c {
            Command::AddSource { source } => f(source),
            Command::Batch { commands, .. } => commands.iter_mut().for_each(|c| walk(c, f)),
            _ => {}
        }
    }
    let mut ours = Vec::new();
    added_tracks(command, &mut ours);
    let live: Vec<&lightair_model::Source> = before
        .sources
        .values()
        .map(|s| &**s)
        .filter(|s| is_cohost(s) && before.objects_using(s.id).next().is_some())
        .collect();
    let another_active = live.iter().any(|s| s.settings[COHOST]["active"].as_bool() != Some(false));
    let mut extra = Vec::new();
    walk(command, &mut |source| {
        if !is_cohost(source) || before.sources.contains_key(&source.id) {
            return;
        }
        let vt = source.settings[COHOST]["voice_track"].as_str().unwrap_or("").to_string();
        let shared = before
            .sources
            .values()
            .any(|o| is_cohost(o) && o.settings[COHOST]["voice_track"].as_str() == Some(vt.as_str()));
        let missing = !before.tracks.iter().any(|t| t.id.to_string() == vt) && !ours.contains(&vt);
        if !(shared || missing) {
            return;
        }
        let voice = lightair_model::Source::new(source.name.clone(), lightair_obs::piair_voice::KIND, json!({}));
        let track = lightair_model::Track::new(source.name.clone(), voice.id);
        source.settings[COHOST]["voice_track"] = json!(track.id.to_string());
        if crate::avatar_library::kind_of(source.settings["file"].as_str().unwrap_or("")) == "PNGtuber" {
            source.settings["mic"] = json!(track.id.to_string());
        }
        if another_active {
            source.settings[COHOST]["active"] = json!(false);
        }
        extra.push(Command::AddSource { source: voice });
        extra.push(Command::AddTrack { track, index: None });
    });
    extra
}

impl Host {
    pub fn key_state(&self, service: &str) -> KeyState {
        self.0.ai.lock().unwrap_or_else(|e| e.into_inner()).keys.get(service).cloned().unwrap_or(KeyState::Unknown)
    }

    fn set_key_state(&self, service: &str, state: KeyState) {
        self.0.ai.lock().unwrap_or_else(|e| e.into_inner()).keys.insert(service.to_string(), state);
        self.touch();
    }

    /// Saves a key (or removes it), checks it works, and tells PiAir; the voice lists that used
    /// it are read again.
    pub fn save_key(&self, service: &'static str, key: Option<String>) {
        self.set_key_state(service, if key.is_some() { KeyState::Checking } else { KeyState::Missing });
        let host = self.clone();
        std::thread::spawn(move || {
            if let Err(e) = keys::set(service, key.as_deref()) {
                host.set_key_state(service, KeyState::Problem(format!("Couldn't keep it in the Keychain: {e}")));
                return;
            }
            {
                let mut ai = host.0.ai.lock().unwrap_or_else(|e| e.into_inner());
                ai.voices.retain(|vendor, _| keys::service_for_voice(vendor) != service);
            }
            host.send(json!({"t": "secrets_changed"}));
            let Some(key) = key else { return };
            let state = match keys::check(service, &key) {
                Ok(note) => KeyState::Works(note),
                Err(e) => KeyState::Problem(e),
            };
            host.set_key_state(service, state);
        });
    }

    // ------------------------------------------------------- subscriptions

    /// Whether a subscription is signed in (`None` while that's being read).
    pub fn sub_state(&self, provider: &str) -> Option<bool> {
        self.0.ai.lock().unwrap_or_else(|e| e.into_inner()).subs.get(provider).copied()
    }

    /// The subscription sign-in under way (or just over), at its latest step.
    pub fn sign_in_step(&self) -> Option<Auth> {
        self.0.ai.lock().unwrap_or_else(|e| e.into_inner()).signin.clone()
    }

    fn set_sign_in_step(&self, step: Option<Auth>) {
        self.0.ai.lock().unwrap_or_else(|e| e.into_inner()).signin = step;
        self.touch();
    }

    /// Signs in to a provider's subscription (Settings → Connections): a PiAir of its own that only
    /// signs in, so it works whether or not a co-host is on. The provider's page opens in the
    /// browser; the steps show in Settings; the credential goes to the Keychain through
    /// `secret.set`, and the co-host's PiAir reads it again.
    pub fn sign_in(&self, provider: &str) {
        self.cancel_sign_in();
        let step = |message: &str, error: bool| Auth {
            provider: provider.to_string(),
            message: message.to_string(),
            error,
            ..Auth::default()
        };
        let Some(cmd) = Self::program() else {
            self.set_sign_in_step(Some(step("PiAir isn't in this build of Lightair.", true)));
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
                self.set_sign_in_step(Some(step(&format!("Couldn't start the sign-in: {e}"), true)));
                return;
            }
        };
        let (Some(stdin), Some(stdout)) = (child.stdin.take(), child.stdout.take()) else { return };
        if let Some(err) = child.stderr.take() {
            std::thread::spawn(move || {
                for line in BufReader::new(err).lines().map_while(Result::ok) {
                    tracing::info!(target: "piair", "sign-in: {line}");
                }
            });
        }
        let run = self.0.signin_gen.fetch_add(1, Ordering::Relaxed) + 1;
        *self.0.signin.lock().unwrap_or_else(|e| e.into_inner()) = Some((run, child, stdin));
        self.set_sign_in_step(Some(step("Opening the sign-in page…", false)));
        let login = json!({"t": "login", "provider": provider, "kind": "oauth"});
        let (host, provider) = (self.clone(), provider.to_string());
        std::thread::Builder::new()
            .name("piair-signin".into())
            .spawn(move || {
                let current = || host.0.signin_gen.load(Ordering::Relaxed) == run;
                let mut finished = false;
                for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                    if !current() {
                        break;
                    }
                    match parse_line(&line) {
                        // it only keeps its credential: nothing else of Lightair's is open to it
                        Ok(Incoming::Call { id, method, params }) => {
                            let msg = if method.starts_with("secret.") {
                                match host.answer(&method, params) {
                                    Ok(result) => json!({"t": "reply", "id": id, "result": result}),
                                    Err(e) => json!({"t": "reply", "id": id, "error": {"code": e.0, "message": e.1}}),
                                }
                            } else {
                                json!({"t": "reply", "id": id, "error": {"code": "not_allowed", "message": "Only signing in here."}})
                            };
                            host.sign_in_send(run, msg);
                        }
                        Ok(Incoming::Auth(mut a)) => {
                            if let Some(url) = &a.url {
                                open_url(url);
                            }
                            a.provider = provider.clone();
                            if a.done {
                                a.message = format!("Signed in to {}", keys::subscription_name(&provider));
                            }
                            finished |= a.done || a.error;
                            host.set_sign_in_step(Some(a));
                        }
                        // PiAir says where providers stand once the sign-in is over: it's done
                        Ok(Incoming::Providers { .. }) => break,
                        Ok(_) => {}
                        Err(e) => tracing::warn!(%e, "piair sign-in: bad line"),
                    }
                }
                if !current() {
                    return;
                }
                let proc = host.0.signin.lock().unwrap_or_else(|e| e.into_inner()).take_if(|s| s.0 == run);
                if let Some((_, mut c, stdin)) = proc {
                    drop(stdin);
                    let _ = c.wait();
                }
                let on = keys::signed_in(&provider);
                {
                    let mut ai = host.0.ai.lock().unwrap_or_else(|e| e.into_inner());
                    ai.subs.insert(provider.clone(), on);
                    if let Some(id) = keys::SERVICES.iter().map(|s| s.0).find(|s| *s == provider) {
                        let state = if keys::get(id).is_some() { KeyState::Saved } else { KeyState::Missing };
                        ai.keys.insert(id.to_string(), state);
                    }
                    if !finished && !on {
                        ai.signin = Some(Auth {
                            provider: provider.clone(),
                            message: "The sign-in stopped before it finished.".into(),
                            error: true,
                            ..Auth::default()
                        });
                    }
                }
                host.send(json!({"t": "secrets_changed"}));
                host.touch();
            })
            .ok();
        self.sign_in_send(run, login);
    }

    fn sign_in_send(&self, run: u64, m: Value) {
        let mut g = self.0.signin.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((_, _, stdin)) = g.as_mut().filter(|s| s.0 == run) {
            let mut line = m.to_string();
            line.push('\n');
            let _ = stdin.write_all(line.as_bytes()).and_then(|_| stdin.flush());
        }
    }

    /// Answers the sign-in's question: the code from the provider's page, or a choice's id.
    pub fn sign_in_answer(&self, code: &str) {
        let run = self.0.signin_gen.load(Ordering::Relaxed);
        self.sign_in_send(run, json!({"t": "auth_code", "code": code}));
        if let Some(a) = self.0.ai.lock().unwrap_or_else(|e| e.into_inner()).signin.as_mut() {
            a.wants_code = false;
            a.options.clear();
            a.message = "Checking…".into();
        }
        self.touch();
    }

    /// Stops a sign-in under way (and forgets the last one's message).
    pub fn cancel_sign_in(&self) {
        self.0.signin_gen.fetch_add(1, Ordering::Relaxed);
        if let Some((_, mut c, stdin)) = self.0.signin.lock().unwrap_or_else(|e| e.into_inner()).take() {
            drop(stdin);
            let _ = c.kill();
            std::thread::spawn(move || {
                let _ = c.wait();
            });
        }
        self.set_sign_in_step(None);
    }

    /// Signs a subscription out, and tells the co-host's PiAir.
    pub fn sign_out(&self, provider: &'static str) {
        let host = self.clone();
        std::thread::spawn(move || {
            let result = keys::sign_out(provider);
            let on = keys::signed_in(provider);
            host.0.ai.lock().unwrap_or_else(|e| e.into_inner()).subs.insert(provider.to_string(), on);
            if let Err(e) = result {
                host.set_sign_in_step(Some(Auth {
                    provider: provider.to_string(),
                    message: format!("Couldn't sign out: {e}"),
                    error: true,
                    ..Auth::default()
                }));
            }
            host.send(json!({"t": "secrets_changed"}));
            host.touch();
        });
    }

    /// The voices a vendor offers, fetched the first time they're asked for.
    pub fn voices(&self, vendor: &str) -> Voices {
        let mut ai = self.0.ai.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(v) = ai.voices.get(vendor) {
            return v.clone();
        }
        ai.voices.insert(vendor.to_string(), Voices::Loading);
        drop(ai);
        let (host, vendor) = (self.clone(), vendor.to_string());
        std::thread::spawn(move || {
            let v = match voices::list(&vendor) {
                Ok(list) => Voices::Ready(list),
                Err(e) => Voices::Failed(e),
            };
            host.0.ai.lock().unwrap_or_else(|e| e.into_inner()).voices.insert(vendor, v);
            host.touch();
        });
        Voices::Loading
    }

    /// The models a hearing engine offers (id, name): ElevenLabs' Scribe Live models, or
    /// OpenRouter's transcription models (fetched the first time; `None` while that's under way).
    pub fn stt_models(&self, engine: &str) -> Option<Result<Vec<(String, String)>, String>> {
        match engine {
            "elevenlabs" => Some(Ok(vec![
                ("scribe_v3_live".into(), "Scribe v3 Live (follows mixed languages)".into()),
                ("scribe_v2_realtime".into(), "Scribe v2 Realtime".into()),
            ])),
            "openrouter" => {
                let mut ai = self.0.ai.lock().unwrap_or_else(|e| e.into_inner());
                // (an empty error is the fetch under way)
                if let Some(r) = &ai.stt_models {
                    return if matches!(r, Err(e) if e.is_empty()) { None } else { Some(r.clone()) };
                }
                ai.stt_models = Some(Err(String::new()));
                drop(ai);
                let host = self.clone();
                std::thread::spawn(move || {
                    let r = voices::openrouter_transcription_models();
                    host.0.ai.lock().unwrap_or_else(|e| e.into_inner()).stt_models = Some(r);
                    host.touch();
                });
                None
            }
            _ => Some(Ok(Vec::new())),
        }
    }

    /// Stops the voice preview.
    pub fn stop_preview(&self) {
        voices::stop();
        let mut ai = self.0.ai.lock().unwrap_or_else(|e| e.into_inner());
        ai.playing = None;
        ai.previewing = None;
        drop(ai);
        self.touch();
    }

    /// Which voice's preview is loading or playing (`None` when it's over).
    pub fn preview_state(&self) -> (Option<String>, Option<String>) {
        let mut ai = self.0.ai.lock().unwrap_or_else(|e| e.into_inner());
        if ai.playing.is_some() && !voices::playing() {
            ai.playing = None;
        }
        (ai.previewing.clone(), ai.playing.clone())
    }

    /// Plays a voice's preview through this computer's own output.
    pub fn preview_voice(&self, vendor: &str, voice: voices::VoiceInfo) {
        {
            let mut ai = self.0.ai.lock().unwrap_or_else(|e| e.into_inner());
            ai.previewing = Some(voice.id.clone());
            ai.preview_error = None;
        }
        self.touch();
        let (host, vendor) = (self.clone(), vendor.to_string());
        std::thread::spawn(move || {
            let r = voices::preview(&vendor, &voice);
            let mut ai = host.0.ai.lock().unwrap_or_else(|e| e.into_inner());
            // another was asked for meanwhile: that one plays
            if ai.previewing.as_deref() != Some(voice.id.as_str()) {
                return;
            }
            ai.previewing = None;
            match r {
                Ok(file) => {
                    voices::play(&file);
                    ai.playing = Some(voice.id.clone());
                }
                Err(e) => ai.preview_error = Some(e),
            }
            drop(ai);
            host.touch();
        });
    }
}

/// What the computer allows PiAir, kept on this Mac (Settings → AI co-host), not in a project.
#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct ComputerPrefs {
    /// Bundle ids (macOS) or executable names (Windows) PiAir may act in.
    pub allowed: Vec<String>,
}

impl ComputerPrefs {
    fn path() -> PathBuf {
        lightair_platform::paths::data_dir().join("PiAir").join("lightair.json")
    }
    pub fn load() -> ComputerPrefs {
        let mut p: ComputerPrefs =
            std::fs::read_to_string(Self::path()).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default();
        let mut seen = std::collections::HashSet::new();
        p.allowed.retain(|a| seen.insert(a.to_lowercase()));
        p
    }
    pub fn save(&self) {
        let p = Self::path();
        if let Some(d) = p.parent() {
            let _ = std::fs::create_dir_all(d);
        }
        let _ = std::fs::write(p, serde_json::to_string_pretty(self).unwrap_or_default());
    }
}

impl Host {
    /// The allowed apps, saved and in force.
    pub fn set_allowed(&self, mut allowed: Vec<String>) {
        let mut seen = std::collections::HashSet::new();
        allowed.retain(|a| seen.insert(a.to_lowercase()));
        ComputerPrefs { allowed: allowed.clone() }.save();
        *self.0.computer.allowed.lock().unwrap_or_else(|e| e.into_inner()) = allowed;
        self.touch();
    }

    pub fn allowed(&self) -> Vec<String> {
        self.0.computer.allowed.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    pub fn ai_drives(&self) -> bool {
        self.0.computer.ai_drives.load(Ordering::Relaxed)
    }

    /// Turns AI drives on or off. On needs Input Monitoring (so the human can always take over)
    /// and Accessibility; without them it stays off and says why.
    pub fn set_ai_drives(&self, on: bool) -> Result<(), String> {
        #[cfg(target_os = "macos")]
        if on {
            if !input_mac::watch() {
                lightair_platform::input::request_listen();
                return Err("Allow Lightair in Input Monitoring so you can always take over.".into());
            }
            if !input_mac::can_post(true) {
                return Err("Allow Lightair in Accessibility so PiAir can move the pointer.".into());
            }
        }
        #[cfg(not(target_os = "macos"))]
        if on {
            return Err("Computer use isn't available on this system yet.".into());
        }
        self.0.computer.ai_drives.store(on, Ordering::Relaxed);
        self.send(json!({"t": "computer", "ai_drives": on, "paused": false}));
        self.touch();
        Ok(())
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

/// Sets its flag when dropped (a thread that ends, however it ends).
struct Finally(std::sync::Arc<AtomicBool>);

impl Drop for Finally {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Relaxed);
    }
}

/// Apple's recognizer wants a language with a region: the co-host's language code as one ("-":
/// the system's).
fn apple_locale(code: &str) -> &str {
    match code {
        "" => "-",
        "en" => "en-US",
        "id" => "id-ID",
        "ms" => "ms-MY",
        "ja" => "ja-JP",
        "ko" => "ko-KR",
        "zh" => "zh-CN",
        "th" => "th-TH",
        "vi" => "vi-VN",
        "hi" => "hi-IN",
        "es" => "es-ES",
        "pt" => "pt-BR",
        "fr" => "fr-FR",
        "de" => "de-DE",
        "ru" => "ru-RU",
        "ar" => "ar-SA",
        other => other,
    }
}

/// Listening to one heard track: Lightair taps its sound (`lightair_obs::audio_tap`) and the
/// bundled `lightair-speech` helper (Apple's on-device speech recognition) turns it into text.
struct Listener {
    child: Child,
    role: Role,
    source: Id,
    name: String,
    lines: std::sync::mpsc::Receiver<(String, bool)>,
    stop: std::sync::Arc<AtomicBool>,
    state: std::sync::Arc<Mutex<ListenState>>,
    engine: Engine,
    /// Its track is muted (silence goes in).
    muted: std::sync::Arc<AtomicBool>,
    /// Its output has been read to the end (so a reason it gave before stopping is in `state`).
    read_all: std::sync::Arc<AtomicBool>,
}

/// What a listener knows of itself, from its helper's lines and the sound going in.
#[derive(Default)]
struct ListenState {
    /// The helper is listening ("on-device" or "online").
    ready: Option<String>,
    /// The words it last made out, and when.
    words: Option<(String, Instant)>,
    /// When the track last had sound in it (above a whisper).
    sound_at: Option<Instant>,
    started: Option<Instant>,
    /// Why it can't hear, as the helper says.
    problem: Option<String>,
    /// The language it heard last, when the engine says.
    language: Option<String>,
}

impl Listener {
    /// Starts listening to a track, or says why it can't.
    fn start(engine: &std::sync::Arc<lightair_obs::Engine>, h: &Hearing) -> Result<Listener, String> {
        if !cfg!(target_os = "macos") {
            return Err("Hearing needs macOS for now.".into());
        }
        // the helper for its engine: Apple's on this Mac, or PiAir's `hear` for a cloud one (its
        // configuration, with the key, as the first line in: never on its command line)
        let (mut command, config) = match h.engine.kind.as_str() {
            "elevenlabs" | "openrouter" => {
                let service = if h.engine.kind == "elevenlabs" { "elevenlabs" } else { "openrouter" };
                let key = keys::get(service).ok_or_else(|| {
                    format!("Add your {} key in Settings → Connections to hear with it.", keys::name_of(service))
                })?;
                let program = Host::program().ok_or("PiAir isn't in this build of Lightair.")?;
                let mut c = Command::new(&program[0]);
                c.args(&program[1..]).arg("hear");
                let config = json!({"engine": h.engine.kind, "model": h.engine.model, "language": h.engine.language, "key": key});
                (c, Some(config))
            }
            _ => {
                let missing = || "Speech recognition isn't in this build of Lightair.".to_string();
                let exe = std::env::current_exe().map_err(|_| missing())?;
                let dir = exe.parent().ok_or_else(missing)?;
                let helper = [dir.join("lightair-speech"), dir.join("../lightair-speech")]
                    .into_iter()
                    .find(|p| p.exists())
                    .ok_or_else(missing)?;
                let mut c = Command::new(helper);
                c.arg(apple_locale(&h.engine.language)).arg("-pcm");
                (c, None)
            }
        };
        let muted = std::sync::Arc::new(AtomicBool::new(h.muted));
        let (tap, mut consumer) =
            track_sound(engine, h.track, h.source, muted.clone()).ok_or("Couldn't take this track's sound (is its source on?).")?;
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            command.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
        }
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| format!("Speech recognition didn't start: {e}"))?;
        let (Some(mut stdin), Some(out)) = (child.stdin.take(), child.stdout.take()) else {
            return Err("Speech recognition didn't start.".into());
        };
        if let Some(config) = config {
            let mut line = config.to_string();
            line.push('\n');
            stdin.write_all(line.as_bytes()).map_err(|e| format!("Speech recognition didn't start: {e}"))?;
        }
        let stop = std::sync::Arc::new(AtomicBool::new(false));
        let state =
            std::sync::Arc::new(Mutex::new(ListenState { started: Some(Instant::now()), ..Default::default() }));
        // the track's sound into the helper
        let (s, st) = (stop.clone(), state.clone());
        std::thread::spawn(move || {
            let _tap = tap;
            let mut bytes = Vec::with_capacity(8192);
            while !s.load(Ordering::Relaxed) && !consumer.is_abandoned() {
                std::thread::sleep(Duration::from_millis(40));
                let n = consumer.slots();
                if n == 0 {
                    continue;
                }
                let Ok(chunk) = consumer.read_chunk(n) else { continue };
                let (a, b) = chunk.as_slices();
                // above a whisper (about -40 dBFS)
                if a.iter().chain(b).any(|v| v.unsigned_abs() > 330) {
                    st.lock().unwrap_or_else(|e| e.into_inner()).sound_at = Some(Instant::now());
                }
                bytes.clear();
                bytes.extend(chunk.into_iter().flat_map(i16::to_le_bytes));
                if stdin.write_all(&bytes).is_err() {
                    break;
                }
            }
        });
        // its words out
        let (tx, rx) = std::sync::mpsc::channel();
        let (st, name) = (state.clone(), h.name.clone());
        let read_all = std::sync::Arc::new(AtomicBool::new(false));
        let done_reading = read_all.clone();
        std::thread::spawn(move || {
            let _done = Finally(done_reading);
            let mut last = String::new();
            for line in BufReader::new(out).lines().map_while(Result::ok) {
                tracing::debug!(track = %name, %line, "piair: speech helper");
                if let Some(text) = line.strip_prefix("TEXT ") {
                    last = text.to_string();
                    {
                        let mut g = st.lock().unwrap_or_else(|e| e.into_inner());
                        g.words = Some((last.clone(), Instant::now()));
                        g.problem = None;
                    }
                    if tx.send((last.clone(), false)).is_err() {
                        break;
                    }
                } else if line == "DONE" {
                    if !last.is_empty() && tx.send((std::mem::take(&mut last), true)).is_err() {
                        break;
                    }
                } else if let Some(code) = line.strip_prefix("LANG ") {
                    st.lock().unwrap_or_else(|e| e.into_inner()).language = Some(code.trim().to_string());
                } else if let Some(how) = line.strip_prefix("READY ") {
                    tracing::info!(track = %name, %how, "piair: listening");
                    st.lock().unwrap_or_else(|e| e.into_inner()).ready = Some(how.to_string());
                } else if let Some(why) = line.strip_prefix("ERROR ") {
                    tracing::warn!(track = %name, %why, "piair: speech recognition");
                    st.lock().unwrap_or_else(|e| e.into_inner()).problem = Some(why.to_string());
                }
            }
        });
        Ok(Listener {
            child,
            role: h.role,
            source: h.source,
            name: h.name.clone(),
            lines: rx,
            stop,
            state,
            engine: h.engine.clone(),
            muted,
            read_all,
        })
    }

    /// What the Inspector says under its track.
    fn status(&self) -> HearStatus {
        let g = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let ago = |t: Option<Instant>| t.map(|t| t.elapsed());
        if let Some(p) = &g.problem {
            return HearStatus { text: p.clone(), level: 2 };
        }
        if let Some((w, at)) = &g.words
            && at.elapsed() < Duration::from_secs(10)
        {
            let w: String = w.chars().rev().take(80).collect::<Vec<_>>().into_iter().rev().collect();
            let lang = g.language.as_deref().map(|l| format!(" ({l})")).unwrap_or_default();
            return HearStatus { text: format!("Heard{lang}: “{w}”"), level: 0 };
        }
        if g.ready.is_none() {
            return if ago(g.started).is_some_and(|d| d > Duration::from_secs(8)) {
                HearStatus { text: "Speech recognition hasn't started (Speech Recognition allowed in System Settings › Privacy & Security?)".into(), level: 2 }
            } else {
                HearStatus { text: "Starting speech recognition…".into(), level: 1 }
            };
        }
        let quiet_for = ago(g.sound_at).or(ago(g.started)).unwrap_or_default();
        if quiet_for > Duration::from_secs(5) {
            return HearStatus { text: "No sound from this track: muted, or nothing playing?".into(), level: 1 };
        }
        HearStatus { text: "Listening".into(), level: 0 }
    }

    /// What it heard since last time: (text so far, whether the utterance ended).
    fn take(&mut self) -> Vec<(String, bool)> {
        self.lines.try_iter().filter(|(_, done)| *done).collect()
    }

    /// It stopped, and what it said before stopping has been read.
    fn dead(&mut self) -> bool {
        self.child.try_wait().ok().flatten().is_some() && self.read_all.load(Ordering::Relaxed)
    }
}

impl Drop for Listener {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}


/// A track's sound to hear (mono, [`lightair_obs::audio_host::LISTEN_RATE`]): from Lightair's
/// mixer, after the track's effects and before its fader, silent while `muted` (a mic or audio
/// interface is captured by the mixer itself: its libobs source is only a placeholder with no
/// sound). With no mixer running, from the libobs source (the tap is then held with it).
fn track_sound(
    engine: &std::sync::Arc<lightair_obs::Engine>,
    track: Id,
    source: Id,
    muted: std::sync::Arc<AtomicBool>,
) -> Option<(Option<lightair_obs::audio_tap::Tap>, rtrb::Consumer<i16>)> {
    engine.call(move |e| {
        if let Some(c) = e.reconciler.listen_track(track, lightair_audio::Tap::PreFader, muted) {
            return Some((None, c));
        }
        e.reconciler.source_ref(source).map(|s| {
            let (tap, c) = lightair_obs::audio_tap::Tap::open_source(s);
            (Some(tap), c)
        })
    })
}

/// What the live model hears of a track: the co-host's own voice taken out (WebRTC's echo
/// canceller, with the voice as it plays as its reference, the way a voice call does), and only
/// while someone is talking (WebRTC's speech detector), so nothing goes up the call in silence:
/// the model isn't kept listening (and spending the plan) to a quiet room, and never to itself.
struct Hearer {
    apm: sonora::AudioProcessing,
    vad: sonora_agc2::vad_wrapper::VoiceActivityDetectorWrapper,
    /// The last moments before speech starts (sent with it, so its first syllable isn't lost).
    lead: VecDeque<[i16; Hearer::FRAME]>,
    /// Frames still sent after the last speech (the model hears the pause that ends a turn).
    hold: u32,
    /// Speech frames in a row (one alone, a click or what's left of an echo, doesn't open it).
    streak: u32,
    /// For the debug log: the loudest heard and the likeliest speech since it was last read.
    seen: (i16, f32),
}

impl Hearer {
    /// 10 ms at 16 kHz.
    const FRAME: usize = 160;
    const LEAD: usize = 30;
    const HOLD: u32 = 120;
    /// Speech probability that opens it, and the least level (RMS, of 32768) of a frame that
    /// counts: a quiet room the detector mistakes for a voice stays shut.
    const SPEECH: f32 = 0.6;
    const LEVEL: f32 = 100.0;

    fn new() -> Self {
        use sonora::config::EchoCanceller;
        let rate = lightair_obs::audio_host::LISTEN_RATE;
        let config = sonora::Config { echo_canceller: Some(EchoCanceller::default()), ..Default::default() };
        let apm = sonora::AudioProcessing::builder()
            .config(config)
            .capture_config(sonora::StreamConfig::new(rate, 1))
            .render_config(sonora::StreamConfig::new(rate, 1))
            .build();
        let vad = sonora_agc2::vad_wrapper::VoiceActivityDetectorWrapper::new(sonora_simd::detect_backend(), rate as i32);
        Self { apm, vad, lead: VecDeque::with_capacity(Self::LEAD + 1), hold: 0, streak: 0, seen: (0, 0.0) }
    }

    /// One frame heard and the voice playing at the same time: what to send up the call (`out`
    /// gets nothing when no one's talking).
    fn frame(&mut self, heard: &[i16], voice: &[i16], out: &mut Vec<i16>) {
        let mut scratch = [0i16; Self::FRAME];
        let _ = self.apm.process_render_i16(voice, &mut scratch);
        let mut clean = [0i16; Self::FRAME];
        if self.apm.process_capture_i16(heard, &mut clean).is_err() {
            clean.copy_from_slice(heard);
        }
        let floats: [f32; Self::FRAME] = std::array::from_fn(|k| clean[k] as f32);
        let p = self.vad.analyze(&floats);
        let level = (floats.iter().map(|v| v * v).sum::<f32>() / Self::FRAME as f32).sqrt();
        self.streak = if p > Self::SPEECH && level > Self::LEVEL { self.streak + 1 } else { 0 };
        let speech = self.streak >= 2;
        let top = heard.iter().map(|v| v.saturating_abs()).max().unwrap_or(0);
        self.seen = (self.seen.0.max(top), self.seen.1.max(p));
        if speech {
            // starting: what came just before goes first
            for f in self.lead.drain(..) {
                out.extend_from_slice(&f);
            }
            self.hold = Self::HOLD;
        }
        if self.hold > 0 {
            self.hold -= 1;
            out.extend_from_slice(&clean);
        } else {
            self.lead.push_back(clean);
            if self.lead.len() > Self::LEAD {
                self.lead.pop_front();
            }
        }
    }
}

/// Realtime voice: forwards a conversation track's sound to PiAir (`audio` messages) for the
/// live model to hear, through a [`Hearer`].
struct Forwarder {
    source: Id,
    muted: std::sync::Arc<AtomicBool>,
    stop: std::sync::Arc<AtomicBool>,
    done: std::sync::Arc<AtomicBool>,
}

impl Forwarder {
    fn start(host: &Host, h: &Hearing) -> Option<Forwarder> {
        let engine = &host.0.engine;
        let muted = std::sync::Arc::new(AtomicBool::new(h.muted));
        let (tap, mut heard) = track_sound(engine, h.track, h.source, muted.clone())?;
        // the co-hosts' voices as they play (after their faders: what can reach the speakers)
        let mut voices: Vec<rtrb::Consumer<i16>> = h
            .echo
            .iter()
            .filter_map(|&t| {
                let never = std::sync::Arc::new(AtomicBool::new(false));
                engine.call(move |e| e.reconciler.listen_track(t, lightair_audio::Tap::PostFader, never))
            })
            .collect();
        let stop = std::sync::Arc::new(AtomicBool::new(false));
        let done = std::sync::Arc::new(AtomicBool::new(false));
        let (s, d, host) = (stop.clone(), done.clone(), host.clone());
        let name = h.name.clone();
        std::thread::spawn(move || {
            let _tap = tap;
            let _guard = Finally(d);
            let mut hearer = Hearer::new();
            let mut out = Vec::<i16>::with_capacity(4096);
            let (mut pending, mut voice) = (Vec::<i16>::new(), Vec::<i16>::new());
            let mut bytes = Vec::with_capacity(8192);
            let (mut peak, mut sent): (i16, usize) = (0, 0);
            let mut report = Instant::now();
            while !s.load(Ordering::Relaxed) && !heard.is_abandoned() {
                std::thread::sleep(Duration::from_millis(20));
                // the heard sound and the voices are written in the same mixer blocks: take as
                // much of each, so they stay lined up (unless a voice stalls: the heard sound
                // never waits on it for long)
                voices.retain(|v| !v.is_abandoned());
                let n = match heard.slots() {
                    n if n > lightair_obs::audio_host::LISTEN_RATE as usize => n,
                    n => voices.iter().map(|v| v.slots()).fold(n, usize::min),
                };
                if n == 0 {
                    continue;
                }
                let Ok(chunk) = heard.read_chunk(n) else { continue };
                let start = pending.len();
                pending.extend(chunk);
                voice.resize(pending.len(), 0);
                for v in &mut voices {
                    if let Ok(chunk) = v.read_chunk(n.min(v.slots())) {
                        for (k, x) in chunk.into_iter().enumerate() {
                            voice[start + k] = voice[start + k].saturating_add(x);
                        }
                    }
                }
                let frames = pending.len() / Hearer::FRAME * Hearer::FRAME;
                if frames == 0 {
                    continue;
                }
                out.clear();
                for (h, v) in pending[..frames].chunks(Hearer::FRAME).zip(voice[..frames].chunks(Hearer::FRAME)) {
                    hearer.frame(h, v, &mut out);
                }
                pending.drain(..frames);
                voice.drain(..frames);
                if report.elapsed() > Duration::from_secs(2) {
                    let (heard, speech) = std::mem::take(&mut hearer.seen);
                    tracing::debug!(track = %name, heard, speech, peak, sent, "piair: heard audio sent up the call");
                    (peak, sent) = (0, 0);
                    report = Instant::now();
                }
                if out.is_empty() {
                    continue;
                }
                bytes.clear();
                for &x in &out {
                    peak = peak.max(x.saturating_abs());
                    bytes.extend_from_slice(&x.to_le_bytes());
                }
                sent += out.len();
                host.send(json!({"t": "audio", "pcm": base64::engine::general_purpose::STANDARD.encode(&bytes)}));
            }
        });
        tracing::info!(track = %h.name, echo = h.echo.len(), "piair: forwarding this track's audio to ChatGPT voice");
        Some(Forwarder { source: h.source, muted, stop, done })
    }

    fn dead(&self) -> bool {
        self.done.load(Ordering::Relaxed)
    }
}

impl Drop for Forwarder {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

/// Makes sure the workspace has PiAir's voice source and its "PiAir" track: the commands to add
/// them, or `None` when they're there. (From before the co-host object, which has a voice track
/// of its own; kept while a test still names it.)
#[cfg_attr(not(test), allow(dead_code))]
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
    use serde_json::json;

    use super::{Auth, FaceLook, Incoming, Line, parse_line, voice_track_commands};
    use lightair_obs::piair_voice::Shape;

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

    /// A voice piece: its sound decoded, its mouth shapes read (an unknown shape dropped), 24 kHz
    /// when the rate isn't said.
    #[test]
    fn a_voice_piece_is_read_with_its_sound_and_mouth_shapes() {
        let line = r#"{"t":"voice","utterance":"u1","pcm":"AAEC","marks":[{"t_ms":0,"v":"A"},{"t_ms":80,"v":"M"},{"t_ms":120,"v":"?"}]}"#;
        assert_eq!(
            parse_line(line).unwrap(),
            Incoming::Voice { utterance: "u1".into(), rate: 24_000, pcm: vec![0, 1, 2], marks: vec![(0.0, Shape::A), (80.0, Shape::M)] }
        );
    }

    /// Each kind of line PiAir sends, read into its message.
    #[test]
    fn every_kind_of_message_is_read() {
        let read = |v: serde_json::Value| parse_line(&v.to_string()).unwrap();
        assert_eq!(read(json!({"t": "voice_end", "utterance": "u1"})), Incoming::VoiceEnd { utterance: "u1".into() });
        assert_eq!(read(json!({"t": "voice_cancel"})), Incoming::VoiceCancel { utterance: None }, "all of them");
        assert_eq!(
            read(json!({"t": "face", "mood": "happy", "look": "chat"})),
            Incoming::Face { mood: Some("happy".into()), look: Some(FaceLook::Named("chat".into())) }
        );
        assert_eq!(read(json!({"t": "face", "look": {"x": 0.5, "y": -0.25}})), Incoming::Face { mood: None, look: Some(FaceLook::At(0.5, -0.25)) });
        assert_eq!(
            read(json!({"t": "call", "id": 7, "method": "computer.click", "params": {"x": 1}})),
            Incoming::Call { id: json!(7), method: "computer.click".into(), params: json!({"x": 1}) }
        );
        assert_eq!(read(json!({"t": "call", "id": 8, "method": "state.get"})), Incoming::Call { id: json!(8), method: "state.get".into(), params: json!({}) });
        assert_eq!(read(json!({"t": "ready", "settings": {"a": 1}, "providers": []})), Incoming::Ready { settings: json!({"a": 1}), providers: json!([]) });
        assert_eq!(read(json!({"t": "settings", "values": {"b": 2}})), Incoming::Settings { values: json!({"b": 2}) });
        assert_eq!(read(json!({"t": "providers", "providers": [1]})), Incoming::Providers { providers: json!([1]) });
        assert_eq!(read(json!({"t": "status"})), Incoming::Status { state: "idle".into(), detail: None });
        assert_eq!(
            read(json!({"t": "transcript", "role": "viewer", "who": "kiwi", "text": "hi"})),
            Incoming::Transcript(Line { role: "viewer".into(), who: Some("kiwi".into()), text: "hi".into() })
        );
        assert_eq!(
            read(json!({"t": "auth", "provider": "anthropic", "message": "Paste the code", "prompt": true})),
            Incoming::Auth(Auth { provider: "anthropic".into(), message: "Paste the code".into(), wants_code: true, ..Auth::default() })
        );
        assert_eq!(read(json!({"t": "something_new"})), Incoming::Other("something_new".into()), "a newer PiAir's kind is ignored");
    }

    /// A line that isn't a JSON object with a kind is refused (and the reader goes on).
    #[test]
    fn bad_lines_are_refused() {
        for line in ["", "not json", "[1, 2]", r#"{"utterance": "u1"}"#, r#"{"t": 5}"#] {
            assert!(parse_line(line).is_err(), "{line:?}");
        }
    }
}
