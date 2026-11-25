//! macOS, with `LIGHTAIR_ENGINE=own`: what the compositor's native widgets show, from the same
//! places their pages read it: the app's chat hub (chat, events and this session's totals), the
//! widget server's storage (a counter's value, a subathon timer's state) and the widget server's
//! alert queue (the alert an alert box shows now).
//!
//! The alert queue lives in the widget server, which hands each alert to the alert box's page
//! over its event feed when the page is ready. The compositor listens on that feed too (one more
//! connection for the same widget, read only), so it draws each alert when the page shows it;
//! the page still plays its sound and speech, which reach the mix through libobs.

use std::collections::HashMap;
use std::io::ErrorKind;
use std::net::{SocketAddr, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

use lightair_engine::{alerts, widgets};
use lightair_live::{ChatEvent, Hub, Kind};
use lightair_widgets::session::Session;
use lightair_widgets::{Instance, Widgets};
use parking_lot::{Mutex, RwLock};
use serde_json::{Map, Value, json};

/// Chat and events kept for the widgets (a chat box shows at most its last 30 or so).
const KEPT: usize = 300;
/// The chat is read again at most this often while it changes.
const CHAT_PERIOD: Duration = Duration::from_millis(100);
/// A widget's storage is read again at most this often.
const STORAGE_PERIOD: Duration = Duration::from_millis(250);
/// An alert box's feed is closed when nothing has asked for its alert for this long.
const UNWANTED: Duration = Duration::from_secs(5);

static LIVE: RwLock<Option<Arc<Live>>> = RwLock::new(None);

/// Gives the compositor's widgets the workspace's chat hub and widget server (called when a
/// workspace opens; the newest replaces the last).
pub fn feed_widgets(hub: &Hub, server: Option<&Widgets>) {
    let seen = hub.history().last().map_or(0, |e| e.id);
    let live = Arc::new(Live {
        hub: hub.clone(),
        server: server.cloned(),
        dirty: AtomicBool::new(true),
        chat: Mutex::new(Chat {
            seen,
            events: Arc::new(Vec::new()),
            serial: 0,
            read_at: None,
            session: Session::default(),
            sessions: HashMap::new(),
        }),
        storage: Mutex::new(HashMap::new()),
        alerts: Mutex::new(HashMap::new()),
    });
    let weak: Weak<Live> = Arc::downgrade(&live);
    hub.on_change(move || {
        if let Some(live) = weak.upgrade() {
            live.dirty.store(true, Ordering::Release);
        }
    });
    *LIVE.write() = Some(live);
}

/// What the widgets are fed from, once the app has given it.
pub(crate) fn live() -> Option<Arc<Live>> {
    LIVE.read().clone()
}

pub(crate) struct Live {
    hub: Hub,
    server: Option<Widgets>,
    /// The hub has something new since the chat was last read.
    dirty: AtomicBool,
    chat: Mutex<Chat>,
    /// Each widget's storage as last read: when, a number that changes with it, and what it holds.
    storage: Mutex<HashMap<String, (Instant, u64, Map<String, Value>)>>,
    /// The alert boxes' feeds, by widget id.
    alerts: Mutex<HashMap<String, Arc<Watch>>>,
}

struct Chat {
    /// The last event counted in the session's totals.
    seen: u64,
    /// The newest events (oldest first), as the hub has them now (deletions marked).
    events: Arc<Vec<ChatEvent>>,
    /// Changes whenever `events` or the totals do.
    serial: u64,
    read_at: Option<Instant>,
    /// This session's totals, as the widget server counts them (every platform, then each).
    session: Session,
    sessions: HashMap<String, Session>,
}

impl Live {
    /// Reads what's new in the chat (at most every [`CHAT_PERIOD`]).
    pub(crate) fn refresh(&self) {
        if !self.dirty.load(Ordering::Acquire) {
            return;
        }
        let mut chat = self.chat.lock();
        if chat.read_at.is_some_and(|t| t.elapsed() < CHAT_PERIOD) {
            return;
        }
        self.dirty.store(false, Ordering::Release);
        chat.read_at = Some(Instant::now());
        // from the oldest kept, so deletions since are seen
        let from = chat.events.first().map_or(0, |e| e.id.saturating_sub(1));
        let read = self.hub.since(from);
        let seen = chat.seen;
        for e in read.iter().filter(|e| e.id > seen) {
            chat.session.add(e);
            chat.sessions.entry(e.platform.clone()).or_default().add(e);
            chat.seen = e.id;
        }
        let skip = read.len().saturating_sub(KEPT);
        chat.events = Arc::new(read[skip..].to_vec());
        chat.serial += 1;
    }

    /// The chat kept, and a number that changes with it.
    fn chat(&self) -> (Arc<Vec<ChatEvent>>, u64) {
        let chat = self.chat.lock();
        (chat.events.clone(), chat.serial)
    }

    /// The session's totals as the widget `i` gets them: every platform's, or those of the
    /// platforms it listens to.
    fn session_for(&self, i: &Instance) -> Session {
        let chat = self.chat.lock();
        if i.platforms.is_empty() {
            return chat.session.clone();
        }
        Session::merged(i.platforms.iter().filter_map(|p| chat.sessions.get(p)))
    }

    /// What the widget `id` stored, and a number that changes with it (`None` without a widget
    /// server).
    fn storage(&self, id: &str) -> Option<(u64, Map<String, Value>)> {
        let server = self.server.as_ref()?;
        let mut all = self.storage.lock();
        if let Some((at, version, stored)) = all.get(id)
            && at.elapsed() < STORAGE_PERIOD
        {
            return Some((*version, stored.clone()));
        }
        let stored = server.storage(id);
        let version = match all.get(id) {
            Some((_, v, old)) if *old == stored => *v,
            Some((_, v, _)) => v + 1,
            None => 1,
        };
        all.insert(id.to_string(), (Instant::now(), version, stored.clone()));
        Some((version, stored))
    }

    /// The alert the alert box `i` shows now, and when it came; starts listening to its feed.
    fn alert(&self, i: &Instance) -> Option<(ChatEvent, Instant)> {
        let server = self.server.as_ref()?;
        let watch = {
            let mut alerts = self.alerts.lock();
            alerts.retain(|_, w| !w.ended.load(Ordering::Acquire));
            alerts
                .entry(i.id.clone())
                .or_insert_with(|| Watch::start(server.port(), i.id.clone(), i.key.clone()))
                .clone()
        };
        *watch.asked.lock() = Instant::now();
        watch.current.lock().clone()
    }

    /// Whether widgets that keep their state in the widget server (counter, subathon timer,
    /// alert box) can be drawn here.
    fn has_server(&self) -> bool {
        self.server.is_some()
    }
}

// ---------------------------------------------------------------- an alert box's feed

/// One alert box's event feed, read on a thread of its own while the compositor draws it.
struct Watch {
    /// When the compositor last asked for its alert.
    asked: Mutex<Instant>,
    /// The alert on now (the event the page was handed) and when it came.
    current: Mutex<Option<(ChatEvent, Instant)>>,
    /// The thread has stopped (nothing asked for a while).
    ended: AtomicBool,
}

impl Watch {
    fn start(port: u16, id: String, key: String) -> Arc<Watch> {
        let watch = Arc::new(Watch {
            asked: Mutex::new(Instant::now()),
            current: Mutex::new(None),
            ended: AtomicBool::new(false),
        });
        let started = std::thread::Builder::new()
            .name("lightair-compose-alerts".into())
            .spawn({
                let watch = watch.clone();
                move || {
                    watch.run(port, &id, &key);
                    watch.ended.store(true, Ordering::Release);
                }
            })
            .is_ok();
        if !started {
            watch.ended.store(true, Ordering::Release);
        }
        watch
    }

    fn wanted(&self) -> bool {
        self.asked.lock().elapsed() < UNWANTED
    }

    /// Connects to the widget's feed (again after a failure) until it isn't wanted any more.
    fn run(&self, port: u16, id: &str, key: &str) {
        let mut wait = Duration::from_millis(500);
        while self.wanted() {
            match self.listen(port, id, key) {
                Ok(()) => wait = Duration::from_millis(500),
                Err(e) => {
                    tracing::debug!(%e, "compositor: an alert box's feed");
                    wait = (wait * 2).min(Duration::from_secs(5));
                }
            }
            *self.current.lock() = None;
            let until = Instant::now() + wait;
            while self.wanted() && Instant::now() < until {
                std::thread::sleep(Duration::from_millis(100));
            }
        }
    }

    /// Reads one connection's messages: an `alert` is the one shown from now, a `skip` ends it.
    fn listen(&self, port: u16, id: &str, key: &str) -> Result<(), String> {
        let address = SocketAddr::from(([127, 0, 0, 1], port));
        let stream = TcpStream::connect_timeout(&address, Duration::from_secs(2)).map_err(|e| e.to_string())?;
        stream.set_read_timeout(Some(Duration::from_secs(2))).map_err(|e| e.to_string())?;
        let url = format!("ws://127.0.0.1:{port}/w/{id}/ws?key={key}");
        let (mut ws, _) = tungstenite::client(url.as_str(), stream).map_err(|e| e.to_string())?;
        ws.get_ref().set_read_timeout(Some(Duration::from_millis(200))).map_err(|e| e.to_string())?;
        while self.wanted() {
            match ws.read() {
                Ok(tungstenite::Message::Text(text)) => {
                    let Ok(m) = serde_json::from_str::<Value>(text.as_str()) else { continue };
                    match m["type"].as_str() {
                        Some("alert") => {
                            if let Ok(e) = serde_json::from_value::<ChatEvent>(m["event"].clone()) {
                                *self.current.lock() = Some((e, Instant::now()));
                            }
                        }
                        Some("skip") => *self.current.lock() = None,
                        _ => {}
                    }
                }
                Ok(tungstenite::Message::Close(_)) => return Ok(()),
                Ok(_) => {}
                Err(tungstenite::Error::Io(e)) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {
                    // pongs and the like go out
                    let _ = ws.flush();
                }
                Err(tungstenite::Error::ConnectionClosed | tungstenite::Error::AlreadyClosed) => return Ok(()),
                Err(e) => return Err(e.to_string()),
            }
        }
        let _ = ws.close(None);
        let _ = ws.flush();
        Ok(())
    }
}

// ---------------------------------------------------------------- what a widget is drawn from

/// Whether the compositor can draw the widget `i` itself: one of the built-in kinds the native
/// widgets draw, not made its own (its page's code changed), with what it reads at hand.
pub(crate) fn drawn_here(i: &Instance) -> bool {
    if i.custom.is_some() || !widgets::NATIVE.contains(&i.kind.as_str()) {
        return false;
    }
    let Some(live) = live() else { return false };
    match i.kind.as_str() {
        "counter" | "subathon" | "alert-box" => live.has_server(),
        _ => true,
    }
}

/// A widget's picture now, as the native widgets draw it, with what its page would show.
pub(crate) struct Prepared {
    instance: Instance,
    size: (u32, u32),
    /// Chat for the chat box and event list (filtered as the page filters it when drawn).
    chat: Option<Arc<Vec<ChatEvent>>>,
    alert: Option<alerts::Alert>,
    stored: Option<Map<String, Value>>,
    now_ms: i64,
}

/// What decides the picture of widget `i` at `size` now, cheaply (without resolving it): the
/// picture is made again only when this changes.
pub(crate) fn probe(live: &Live, i: &Instance, size: (u32, u32)) -> String {
    let now_ms = now_ms();
    let (_, serial) = live.chat();
    let chat = match i.kind.as_str() {
        "counter" | "subathon" | "alert-box" => 0,
        _ => serial,
    };
    // a chat box whose messages go after a while, and a subathon's clock: each second
    let tick = match i.kind.as_str() {
        "subathon" => now_ms / 1000,
        "chat-box" if num(&widgets::values(i), "hideAfter") > 0.0 => now_ms / 1000,
        _ => 0,
    };
    let stored = match i.kind.as_str() {
        "counter" | "subathon" => live.storage(&i.id).map_or(0, |(v, _)| v),
        _ => 0,
    };
    let alert = match i.kind.as_str() {
        "alert-box" => alert_on(live, i, &widgets::values(i)).map_or(0, |(a, _)| a.id),
        _ => 0,
    };
    format!("{}|{}|{}x{}|{chat}|{tick}|{stored}|{alert}", i.kind, i.fields, size.0, size.1)
}

/// What widget `i` is drawn from now: its settings with what its page would show filled in
/// (the native drawing counts some things its own way; the page's values are given instead).
pub(crate) fn prepare(live: &Live, i: &Instance, size: (u32, u32)) -> Prepared {
    let now_ms = now_ms();
    let values = widgets::values(i);
    let mut instance = i.clone();
    let mut set = |k: &str, v: Value| {
        if !instance.fields.is_object() {
            instance.fields = Value::Object(Map::new());
        }
        if let Value::Object(m) = &mut instance.fields {
            m.insert(k.into(), v);
        }
    };
    let mut chat = None;
    let mut alert = None;
    let mut stored = None;
    match i.kind.as_str() {
        "chat-box" | "event-list" => chat = Some(live.chat().0),
        "goal" => {
            // the page's count: this session's totals, from "Already have"
            let session = live.session_for(i);
            let counts = text(&values, "counts");
            let n = match counts {
                "paid" => {
                    let currency = text(&values, "currency").to_uppercase();
                    session.totals.get(if currency.is_empty() { "USD" } else { &currency }).copied().unwrap_or(0.0)
                }
                "cheer" => session.totals.get("BITS").copied().unwrap_or(0.0),
                k => session.counts.get(k).copied().unwrap_or(0) as f64,
            };
            set("start", json!(num(&values, "start") + n));
        }
        "label" => {
            // the page's line, drawn as the label's "before there's one" text
            let line = label_line(&values, &live.session_for(i));
            set("shows", json!(""));
            set("empty", json!(line));
        }
        "counter" => {
            let s = live.storage(&i.id).map(|(_, s)| s).unwrap_or_default();
            set("_value", s.get("value").cloned().unwrap_or(json!(0)));
        }
        "subathon" => stored = live.storage(&i.id).map(|(_, s)| s),
        "alert-box" => {
            alert = alert_on(live, i, &values).map(|(a, _)| a);
            // the native box draws its title at 1.8 times the text size and the message at the
            // text size; the page draws the title at the text size
            set("fontSize", json!(num_or(&values, "fontSize", 44.0) / 1.8));
        }
        _ => {}
    }
    Prepared { instance, size, chat, alert, stored, now_ms }
}

impl Prepared {
    /// The picture: premultiplied RGBA and its size.
    pub(crate) fn draw(&self) -> (Vec<u8>, (u32, u32)) {
        let values = widgets::values(&self.instance);
        let shown: Vec<ChatEvent> = match &self.chat {
            Some(chat) => {
                let hidden: Vec<String> = text(&values, "hideUsers")
                    .split(',')
                    .map(|u| u.trim().to_lowercase())
                    .filter(|u| !u.is_empty())
                    .collect();
                let hide_after = num(&values, "hideAfter");
                chat.iter()
                    .filter(|e| self.instance.hears(&e.platform))
                    .filter(|e| hide_after <= 0.0 || e.ts_ms >= self.now_ms - (hide_after * 1000.0) as i64)
                    .filter(|e| {
                        // the page hides by login, else by the name shown
                        let who = e.user.as_ref().map_or(String::new(), |u| {
                            if u.login.is_empty() { u.display_name.to_lowercase() } else { u.login.to_lowercase() }
                        });
                        !hidden.contains(&who)
                    })
                    .filter(|e| !matches!(e.kind, Kind::Deletion { .. }))
                    .cloned()
                    .collect()
            }
            None => Vec::new(),
        };
        let feed = widgets::Feed {
            chat: &shown,
            alert: self.alert.as_ref(),
            stored: self.stored.as_ref(),
            now_ms: self.now_ms,
        };
        widgets::draw(&self.instance, self.size, &feed)
    }
}

/// The alert box `i`'s alert on now, as its page shows it (its rules: the kinds it shows, their
/// lines, the least amounts), while it's on screen; `None` between alerts.
fn alert_on(live: &Live, i: &Instance, values: &Map<String, Value>) -> Option<(alerts::Alert, Instant)> {
    let (event, at) = live.alert(i)?;
    let seconds = num_or(values, "duration", 8.0).max(1.0);
    if at.elapsed() >= Duration::from_secs_f64(seconds) {
        return None;
    }
    let rule = |kind: &str| alerts::Rule {
        kind: kind.into(),
        enabled: values.get(&format!("{kind}Enabled")).and_then(Value::as_bool).unwrap_or(false),
        min_amount: num(values, &format!("{kind}Min")),
        text: values.get(&format!("{kind}Text")).and_then(Value::as_str).unwrap_or("{name}").to_string(),
        duration_ms: (seconds * 1000.0) as u32,
        speak: false,
        sound: None,
    };
    let settings =
        alerts::Settings { rules: ["follow", "sub", "paid", "cheer", "raid"].into_iter().map(rule).collect() };
    alerts::from_event(&event, &settings).map(|a| (a, at))
}

/// A label's line as its page writes it.
fn label_line(values: &Map<String, Value>, session: &Session) -> String {
    let shows = match text(values, "shows") {
        "" => "follow",
        s => s,
    };
    let template = text(values, "template");
    if let Some(kind) = shows.strip_prefix("count-") {
        let template = if template.is_empty() { "{count}" } else { template };
        let count = session.counts.get(kind).copied().unwrap_or(0);
        return template
            .replacen("{count}", &count.to_string(), 1)
            .replacen("{name}", "", 1)
            .replacen("{amount}", "", 1);
    }
    let template = if template.is_empty() { "{name}" } else { template };
    let latest = if shows == "top" { session.top.as_ref() } else { session.latest.get(shows) };
    match latest {
        None => match text(values, "empty") {
            "" => template.replacen("{name}", "—", 1).replacen("{amount}", "", 1).replacen("{count}", "", 1),
            empty => empty.to_string(),
        },
        Some(v) => {
            let amount = match v["display"].as_str().filter(|d| !d.is_empty()) {
                Some(d) => d.to_string(),
                None => v["amount"].as_f64().map_or(String::new(), js_number),
            };
            template
                .replacen("{name}", v["name"].as_str().unwrap_or(""), 1)
                .replacen("{amount}", &amount, 1)
                .replacen("{count}", "", 1)
        }
    }
}

/// A number as a page's script writes it (no ".0" on whole numbers).
fn js_number(n: f64) -> String {
    if n.fract() == 0.0 && n.abs() < 1e15 { format!("{}", n as i64) } else { format!("{n}") }
}

fn text<'a>(v: &'a Map<String, Value>, k: &str) -> &'a str {
    v.get(k).and_then(Value::as_str).unwrap_or("")
}

fn num(v: &Map<String, Value>, k: &str) -> f64 {
    num_or(v, k, 0.0)
}

fn num_or(v: &Map<String, Value>, k: &str, default: f64) -> f64 {
    v.get(k).and_then(|x| x.as_f64().or_else(|| x.as_str().and_then(|s| s.trim().parse().ok()))).unwrap_or(default)
}

fn now_ms() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as i64)
}
