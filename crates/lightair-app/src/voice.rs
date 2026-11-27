//! Lightair's built-in voices (lightair-docs/lightair/docs/plans/voices-and-hearing.md): the voice
//! profiles set up in Settings › Voices, the Voice track's queue (what alerts, automations, the
//! chat reader and remote clients say, one after another), and the chat reader.
//!
//! Text reaches the queue through the remote API's `voice.say` (on the main thread, which makes
//! the Voice track when it's missing); other threads call [`call_say`], which goes the same way.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

use lightair_host::piair_voice::{self, Channel};
use lightair_voice::{Event, Profile, Vendor, Voice};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// What the chat reader reads.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ReadWhich {
    /// Messages with money on them (Super Chats, bits, donations).
    #[default]
    Paid,
    /// Each viewer's first message of the session, and paid ones.
    First,
    /// Every chat message.
    Everyone,
}

impl ReadWhich {
    pub const ALL: [ReadWhich; 3] = [ReadWhich::Paid, ReadWhich::First, ReadWhich::Everyone];

    pub fn label(self) -> &'static str {
        match self {
            ReadWhich::Paid => "Paid messages",
            ReadWhich::First => "First messages",
            ReadWhich::Everyone => "Every message",
        }
    }
}

/// The chat reader's choices.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ChatReader {
    pub on: bool,
    pub which: ReadWhich,
    /// The longest message read (characters; cut at a word).
    pub max_chars: usize,
    /// "Nova says …" rather than the message alone.
    pub say_name: bool,
}

impl Default for ChatReader {
    fn default() -> ChatReader {
        ChatReader { on: false, which: ReadWhich::Paid, max_chars: 200, say_name: true }
    }
}

/// Settings › Voices, saved in `<data dir>/voices.json`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Voices {
    /// The profiles the user made (the system voice is always there besides these).
    pub profiles: Vec<Profile>,
    /// The profile used when none is named (an id).
    pub default: String,
    pub chat_reader: ChatReader,
    /// Swearing isn't read either.
    pub family_friendly: bool,
}

impl Default for Voices {
    fn default() -> Voices {
        Voices {
            profiles: Vec::new(),
            default: "system".into(),
            chat_reader: ChatReader::default(),
            family_friendly: true,
        }
    }
}

fn path() -> PathBuf {
    lightair_platform::paths::data_dir().join("voices.json")
}

fn store() -> &'static Mutex<Voices> {
    static S: OnceLock<Mutex<Voices>> = OnceLock::new();
    S.get_or_init(|| {
        Mutex::new(std::fs::read(path()).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default())
    })
}

/// The saved settings.
pub fn settings() -> Voices {
    store().lock().unwrap_or_else(|e| e.into_inner()).clone()
}

/// Saves new settings.
pub fn set_settings(v: Voices) {
    if let Ok(text) = serde_json::to_vec_pretty(&v) {
        let _ = std::fs::create_dir_all(lightair_platform::paths::data_dir());
        let _ = std::fs::write(path(), text);
    }
    *store().lock().unwrap_or_else(|e| e.into_inner()) = v;
    CHANGED.fetch_add(1, Ordering::Relaxed);
}

/// Bumped by every change (for views that cache).
pub static CHANGED: AtomicU64 = AtomicU64::new(0);

/// Every profile: the system voice first, then the user's.
pub fn profiles() -> Vec<Profile> {
    let mut all = vec![Profile::default()];
    all.extend(settings().profiles.into_iter().filter(|p| p.id != "system"));
    all
}

/// A profile by id or name (case ignored).
pub fn find(which: &str) -> Option<Profile> {
    let w = which.trim();
    profiles().into_iter().find(|p| p.id == w || p.name.eq_ignore_ascii_case(w))
}

/// The default profile (the system voice when the chosen one is gone).
pub fn default_profile() -> Profile {
    find(&settings().default).unwrap_or_default()
}

/// The key a vendor needs, from Settings › Connections.
pub fn key_for(vendor: Vendor) -> Option<String> {
    vendor.key_service().and_then(crate::piair::keys::get)
}

// ---------------------------------------------------------------------------------------------
// The Voice track's queue

struct Request {
    id: String,
    text: String,
    profile: Profile,
}

#[derive(Default)]
struct Queue {
    waiting: VecDeque<Request>,
    /// The one being made now, and whether to stop it.
    making: Option<(String, Arc<AtomicBool>)>,
    /// The last problem, and when.
    problem: Option<(String, Instant)>,
}

fn queue() -> &'static (Mutex<Queue>, Condvar) {
    static Q: OnceLock<(Mutex<Queue>, Condvar)> = OnceLock::new();
    Q.get_or_init(|| {
        let _ = std::thread::Builder::new().name("voice-queue".into()).spawn(run);
        (Mutex::new(Queue::default()), Condvar::new())
    })
}

/// At most this many things wait to be said; more are dropped (a raid of messages shouldn't
/// talk for an hour).
const MAX_WAITING: usize = 20;

static NEXT: AtomicU64 = AtomicU64::new(1);

/// Why text can't be said, as a sentence.
pub fn refused(text: &str) -> Option<String> {
    let s = settings();
    match lightair_voice::filter::viewer_text(text, s.family_friendly, 10_000) {
        Some(_) => None,
        None => {
            Some(match lightair_voice::filter::check(&lightair_voice::filter::speakable(text), s.family_friendly) {
                Err(r) => format!("not said: it has {}", r.reason()),
                Ok(()) => "not said: there's nothing to say once links are left out".into(),
            })
        }
    }
}

/// Queues `text` to be said on the Voice track with `profile` (a name or id; `None`: the
/// default). Links, long numbers and anything the filter refuses are never said. Returns what
/// will be said. The Voice track must exist (the caller makes it).
pub fn say(text: &str, profile: Option<&str>, max_chars: usize) -> Result<String, String> {
    let s = settings();
    let profile = match profile.filter(|p| !p.trim().is_empty()) {
        Some(p) => find(p).ok_or_else(|| format!("there's no voice called {p} (Settings › Voices)"))?,
        None => default_profile(),
    };
    let said = lightair_voice::filter::viewer_text(text, s.family_friendly, max_chars)
        .ok_or_else(|| refused(text).unwrap_or_default())?;
    let (lock, cv) = queue();
    let mut q = lock.lock().unwrap_or_else(|e| e.into_inner());
    if q.waiting.len() >= MAX_WAITING {
        return Err("not said: too much is waiting to be said already".into());
    }
    q.waiting.push_back(Request {
        id: format!("v{}", NEXT.fetch_add(1, Ordering::Relaxed)),
        text: said.clone(),
        profile,
    });
    cv.notify_all();
    Ok(said)
}

/// Stops what's being said now (skips to the next), or everything with `all`.
pub fn stop(all: bool) {
    let (lock, cv) = queue();
    let mut q = lock.lock().unwrap_or_else(|e| e.into_inner());
    if all {
        q.waiting.clear();
        if let Some((_, stop)) = &q.making {
            stop.store(true, Ordering::Relaxed);
        }
        piair_voice::cancel(Channel::Voice, None);
    } else {
        let playing = piair_voice::playing(Channel::Voice);
        match (&q.making, playing) {
            (Some((id, stop)), Some(p)) if *id == p => {
                stop.store(true, Ordering::Relaxed);
                piair_voice::cancel(Channel::Voice, Some(&p));
            }
            (_, Some(p)) => piair_voice::cancel(Channel::Voice, Some(&p)),
            (Some((_, stop)), None) => stop.store(true, Ordering::Relaxed),
            (None, None) => {}
        }
    }
    cv.notify_all();
}

/// How many things wait to be said, whether something is being said, and the last problem (for
/// a minute after it happened).
pub fn status() -> (usize, bool, Option<String>) {
    let q = queue().0.lock().unwrap_or_else(|e| e.into_inner());
    let speaking = q.making.is_some() || piair_voice::playing(Channel::Voice).is_some();
    let problem = q.problem.as_ref().filter(|(_, at)| at.elapsed() < Duration::from_secs(60)).map(|(p, _)| p.clone());
    (q.waiting.len(), speaking, problem)
}

/// Voices made so far, by their profile (the key they were made with too: a new key, a new voice).
struct Made {
    profile: Profile,
    key: Option<String>,
    voice: Voice,
}

fn voice_for(made: &mut Vec<Made>, profile: &Profile) -> Result<Voice, String> {
    let key = key_for(profile.vendor);
    if let Some(m) = made.iter().find(|m| m.profile == *profile && m.key == key) {
        return Ok(m.voice.clone());
    }
    let voice = Voice::new(profile, key.clone())?;
    made.retain(|m| m.profile.id != profile.id);
    made.push(Made { profile: profile.clone(), key, voice: voice.clone() });
    Ok(voice)
}

fn run() {
    let mut made: Vec<Made> = Vec::new();
    loop {
        let (lock, cv) = queue();
        let (req, stop) = {
            let mut q = lock.lock().unwrap_or_else(|e| e.into_inner());
            while q.waiting.is_empty() {
                q = cv.wait(q).unwrap_or_else(|e| e.into_inner());
            }
            let req = q.waiting.pop_front().expect("waiting isn't empty");
            let stop = Arc::new(AtomicBool::new(false));
            q.making = Some((req.id.clone(), stop.clone()));
            (req, stop)
        };
        let problem = speak(&mut made, &req, &stop);
        let mut q = lock.lock().unwrap_or_else(|e| e.into_inner());
        q.making = None;
        if let Some(p) = problem {
            tracing::warn!(problem = %p, "voice");
            q.problem = Some((p, Instant::now()));
        }
    }
}

/// Makes one request's sound into the Voice track's queue; a problem, if it couldn't.
fn speak(made: &mut Vec<Made>, req: &Request, stop: &AtomicBool) -> Option<String> {
    if !piair_voice::available(Channel::Voice) {
        return Some("there's no Voice track to say it on".into());
    }
    let voice = match voice_for(made, &req.profile) {
        Ok(v) => v,
        Err(e) => return Some(e),
    };
    let (tx, rx) = std::sync::mpsc::channel::<Option<String>>();
    let id = req.id.clone();
    let mut u = voice.say_all(
        &req.text,
        Box::new(move |e| match e {
            Event::Audio(c) => piair_voice::push_samples(Channel::Voice, &id, c.rate, &c.pcm, &[]),
            Event::Done => {
                piair_voice::end(Channel::Voice, &id);
                let _ = tx.send(None);
            }
            Event::Failed(why) => {
                piair_voice::end(Channel::Voice, &id);
                let _ = tx.send(Some(why));
            }
        }),
    );
    let started = Instant::now();
    loop {
        match rx.recv_timeout(Duration::from_millis(30)) {
            Ok(problem) => {
                if problem.is_some() {
                    // a failed voice is made again next time (its connection may be gone)
                    made.retain(|m| m.profile != req.profile);
                }
                return problem;
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                if stop.load(Ordering::Relaxed) {
                    u.cancel();
                    piair_voice::cancel(Channel::Voice, Some(&req.id));
                    return None;
                }
                if started.elapsed() > Duration::from_secs(120) {
                    u.cancel();
                    piair_voice::end(Channel::Voice, &req.id);
                    made.retain(|m| m.profile != req.profile);
                    return Some(format!("{} took too long to answer", req.profile.vendor.name()));
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return None,
        }
    }
}

// ---------------------------------------------------------------------------------------------
// From other threads

type Caller = Box<dyn Fn(&str, Value) + Send + Sync>;

fn caller() -> &'static Mutex<Option<Caller>> {
    static C: Mutex<Option<Caller>> = Mutex::new(None);
    &C
}

/// How other threads reach the remote API (set once at start).
pub fn set_caller(c: Caller) {
    *caller().lock().unwrap_or_else(|e| e.into_inner()) = Some(c);
}

/// Says `text` with the default voice, from any thread (through `voice.say`, so the Voice track
/// is made when it's missing). `max_chars` cuts a long message.
pub fn call_say(text: &str, max_chars: usize) {
    if let Some(c) = caller().lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
        c("voice.say", json!({"text": text, "max_chars": max_chars}));
    }
}

/// The chat reader: reads chat aloud as Settings › Voices says, until Lightair quits.
pub fn start_chat_reader(hub: lightair_live::Hub) {
    let _ = std::thread::Builder::new().name("voice-chat-reader".into()).spawn(move || {
        let mut last = hub.history().last().map_or(0, |e| e.id);
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        loop {
            std::thread::sleep(Duration::from_millis(250));
            let events = hub.since(last);
            let Some(newest) = events.last().map(|e| e.id) else { continue };
            last = last.max(newest);
            let r = settings().chat_reader;
            for e in events {
                let who = e.user.as_ref().map(|u| (u.id.clone(), u.display_name.clone()));
                let first = who.as_ref().is_some_and(|(id, _)| seen.insert(format!("{}:{id}", e.platform)));
                if !r.on || e.deleted || e.text.trim().is_empty() {
                    continue;
                }
                let paid = matches!(e.kind, lightair_live::Kind::Paid) || e.paid.is_some();
                let message = matches!(e.kind, lightair_live::Kind::Message | lightair_live::Kind::Paid);
                let read = match r.which {
                    ReadWhich::Paid => paid,
                    ReadWhich::First => paid || (message && first),
                    ReadWhich::Everyone => message || paid,
                };
                if !read {
                    continue;
                }
                let text = match (&who, r.say_name) {
                    (Some((_, name)), true) if !name.is_empty() => format!("{name} says: {}", e.text),
                    _ => e.text.clone(),
                };
                call_say(&text, r.max_chars);
            }
        }
    });
}
