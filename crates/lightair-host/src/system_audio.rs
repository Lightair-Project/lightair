//! Desktop and app sound for the mixer (macOS): `lightair-capture`'s Core Audio process taps,
//! running only while a track takes the source ([`SystemSound`]), so nothing taps at launch or
//! for a source no track uses. Settings: `type` 0 (everything but Lightair, so monitoring doesn't
//! come back in) or 1 with `application` (one app's bundle id; the capture follows the app as it
//! quits and opens again).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use lightair_capture::sound::{SoundCapture, Target};
use lightair_capture::{Sound, SoundSink};
use lightair_model::Id;
use parking_lot::{Condvar, Mutex};

use crate::audio_host::{InputFeed, SoundFeeder};

/// What a system audio source captures: everything but Lightair (`None`), or one app.
#[derive(Debug, Clone, PartialEq)]
pub struct Settings {
    pub app: Option<String>,
}

impl Settings {
    /// From a system audio source's settings as the document keeps them.
    pub fn from_json(s: &serde_json::Value) -> Settings {
        let app = s.get("type").and_then(serde_json::Value::as_i64) == Some(1);
        let bundle = s.get("application").and_then(serde_json::Value::as_str).unwrap_or_default();
        Settings { app: app.then(|| bundle.to_string()) }
    }
}

struct Shared {
    settings: Mutex<Settings>,
    changed: Condvar,
    quit: AtomicBool,
    /// Where its sound goes while a track takes it; `None` while none does (and no capture runs).
    feed: Mutex<Option<InputFeed>>,
    /// Whether `feed` is set, changed under `settings`'s lock so the thread wakes for it.
    wanted: AtomicBool,
}

/// A system audio source's sound for the mixer, from when it's made until it's dropped.
pub struct SystemSound {
    source: Id,
    shared: Arc<Shared>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl SystemSound {
    /// Registers document source `source` with the mixer as feeding itself, capturing as
    /// `settings` say while a track takes it.
    pub fn new(source: Id, settings: Settings) -> SystemSound {
        let shared = Arc::new(Shared {
            settings: Mutex::new(settings),
            changed: Condvar::new(),
            quit: AtomicBool::new(false),
            feed: Mutex::new(None),
            wanted: AtomicBool::new(false),
        });
        crate::audio_host::feeds_itself(source, Arc::new(Feeder(shared.clone())));
        let thread = {
            let shared = shared.clone();
            std::thread::Builder::new().name("lightair-system-audio".into()).spawn(move || run_mixer(shared)).ok()
        };
        SystemSound { source, shared, thread }
    }

    /// New settings: the capture starts again into the same feed.
    pub fn set_settings(&self, settings: Settings) {
        *self.shared.settings.lock() = settings;
        self.shared.changed.notify_all();
    }
}

impl Drop for SystemSound {
    fn drop(&mut self) {
        crate::audio_host::feeds_itself_no_more(self.source);
        *self.shared.feed.lock() = None;
        self.shared.quit.store(true, Ordering::Release);
        self.shared.changed.notify_all();
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// What to capture for these settings: everything but Lightair, or one app (none for an app
/// setting left empty).
fn target_of(settings: &Settings) -> Option<Target> {
    match &settings.app {
        None => Some(Target::System { include_own: false }),
        Some(bundle) if bundle.is_empty() => None,
        Some(bundle) => Some(Target::App(bundle.clone())),
    }
}

/// The audio engine's side of a source in mixer mode: a track taking it hands over its feed.
struct Feeder(Arc<Shared>);

impl SoundFeeder for Feeder {
    fn attach(&self, feed: Option<InputFeed>) {
        let wanted = feed.is_some();
        // the old feed (if any) is dropped here, on the engine thread
        drop(std::mem::replace(&mut *self.0.feed.lock(), feed));
        let _settings = self.0.settings.lock();
        self.0.wanted.store(wanted, Ordering::Release);
        self.0.changed.notify_all();
    }
}

/// Where the capture delivers (Core Audio's real-time I/O thread): into the track's feed. Never
/// waits: while the feed is being swapped, that block is dropped.
struct Sink(Arc<Shared>);

impl SoundSink for Sink {
    fn sound(&mut self, sound: &Sound<'_>) {
        if let Some(mut feed) = self.0.feed.try_lock()
            && let Some(feed) = feed.as_mut()
        {
            feed.push(sound.samples, sound.channels, sound.rate);
        }
    }

    fn stopped(&mut self, reason: &str) {
        tracing::warn!(reason, "system audio: the capture stopped");
    }
}

/// How long a capture that failed waits before it's tried again (unless access was refused or
/// taps aren't supported: those wait for the settings or the track to change).
const RETRY: Duration = Duration::from_secs(10);

/// Mixer mode: keeps a capture matching the settings running while a track takes the source,
/// and none otherwise. A settings change starts the capture again into the same feed; an app's
/// capture follows the app as it quits and opens again by itself.
fn run_mixer(shared: Arc<Shared>) {
    let mut capture: Option<SoundCapture> = None;
    let mut running: Option<Target> = None;
    // the target that failed, when, and whether only a change gets it tried again
    let mut failed: Option<(Target, Instant, bool)> = None;
    loop {
        if shared.quit.load(Ordering::Acquire) {
            break;
        }
        let settings = shared.settings.lock().clone();
        let wanted = shared.wanted.load(Ordering::Acquire);
        let target = if wanted { target_of(&settings) } else { None };
        if target != running {
            // one tap at a time: the old one stops before another is made
            capture = None;
            running = None;
            if let Some(t) = &target {
                let retry = match &failed {
                    Some((f, at, for_good)) if f == t => !for_good && at.elapsed() >= RETRY,
                    _ => true,
                };
                if retry {
                    match SoundCapture::start(t, Sink(shared.clone())) {
                        Ok(c) => {
                            tracing::info!(of = ?t, format = ?c.format(), "system audio: captured for the mixer");
                            capture = Some(c);
                            running = target.clone();
                            failed = None;
                        }
                        Err(e) => {
                            tracing::warn!(error = %e, of = ?t, "system audio: no capture");
                            let for_good = matches!(
                                e,
                                lightair_capture::Error::Permission(_) | lightair_capture::Error::Unsupported(_)
                            );
                            failed = Some((t.clone(), Instant::now(), for_good));
                        }
                    }
                }
            }
        }
        if target.is_none() {
            failed = None;
        }
        let mut s = shared.settings.lock();
        if *s == settings && shared.wanted.load(Ordering::Acquire) == wanted && !shared.quit.load(Ordering::Acquire) {
            shared.changed.wait_for(&mut s, Duration::from_secs(2));
        }
    }
    drop(capture);
}
