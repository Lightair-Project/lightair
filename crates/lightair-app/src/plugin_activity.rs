//! Every plugin's use of every permission (plugins.md §4.13): what the status bar's indicator,
//! the notices and the Plugin activity panel show. Fed by the helpers' proxy lanes
//! (`lightair_helpers::Event`), refused requests of pages from packages
//! (`lightair_web::on_blocked`), and the calls helpers make over their protocol.
//!
//! Kept for this run of Lightair only: a bounded log per plugin, and per plugin and kind the
//! count and last use. The first use of each kind by each plugin, and every start of a
//! sensitive kind, raise a notice.

// Parts are for the helper host and the Packages panel's permission list, which come next.
#![allow(dead_code)]

use std::collections::{BTreeMap, VecDeque};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime};

/// The kinds the indicator has an icon for.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Kind {
    Network,
    HomeNetwork,
    Screen,
    Camera,
    Microphone,
    Files,
    Devices,
    OtherApp,
    Chat,
    FullAccess,
}

impl Kind {
    /// Starting to use it always raises a notice (not only the first time).
    pub fn sensitive(self) -> bool {
        matches!(self, Kind::Screen | Kind::Camera | Kind::Microphone | Kind::Files | Kind::OtherApp | Kind::FullAccess)
    }

    pub fn label(self) -> &'static str {
        match self {
            Kind::Network => "Internet",
            Kind::HomeNetwork => "Home network",
            Kind::Screen => "Screen",
            Kind::Camera => "Camera",
            Kind::Microphone => "Microphone",
            Kind::Files => "Files",
            Kind::Devices => "Devices",
            Kind::OtherApp => "Another app",
            Kind::Chat => "Chat",
            Kind::FullAccess => "Full access",
        }
    }

    /// The kind a permission string belongs to (catalog-formats.md §6).
    pub fn of_permission(p: &str) -> Option<Kind> {
        Some(match p {
            _ if p.starts_with("network.lan") => Kind::HomeNetwork,
            _ if p.starts_with("network") => Kind::Network,
            "screen.capture" => Kind::Screen,
            "camera" => Kind::Camera,
            "mic" => Kind::Microphone,
            _ if p.starts_with("fs.") => Kind::Files,
            _ if p.starts_with("device.") => Kind::Devices,
            _ if p.starts_with("ipc:") || p.starts_with("automation:") => Kind::OtherApp,
            "chat.read" | "chat.write" => Kind::Chat,
            _ if p.starts_with("full-access") => Kind::FullAccess,
            _ => return None,
        })
    }
}

/// One use, allowed or refused.
#[derive(Clone, Debug)]
pub struct Use {
    pub plugin: String,
    pub kind: Kind,
    /// What, in a few words: "api.example.com:443", "screenshot", "~/Games/logs".
    pub what: String,
    pub allowed: bool,
    /// Why it was refused, when it was ("not declared", "taken back").
    pub reason: String,
    pub at: SystemTime,
}

/// A notice to show (never drawn into the program: it's Lightair's interface).
#[derive(Clone, Debug)]
pub struct Notice {
    pub plugin: String,
    /// The permission it's about (none for notices about the package itself: withdrawn).
    pub kind: Option<Kind>,
    pub text: String,
    pub refused: bool,
    /// Shows even while live or recording (a full-access plugin starting, a refused screen,
    /// camera or microphone use).
    pub urgent: bool,
}

/// Per plugin and kind.
#[derive(Clone, Debug, Default)]
pub struct Tally {
    pub count: u64,
    pub refused: u64,
    pub last: Option<SystemTime>,
}

#[derive(Default)]
struct State {
    log: BTreeMap<String, VecDeque<Use>>,
    tally: BTreeMap<(String, Kind), Tally>,
    /// When each (plugin, kind) was last used, for the live indicator.
    recent: BTreeMap<(String, Kind), Instant>,
    notices: VecDeque<Notice>,
    /// The newest notice and when it came, for the status bar.
    newest: Option<(Notice, Instant)>,
    generation: u64,
}

const LOG_PER_PLUGIN: usize = 500;
/// How long a use keeps the indicator lit.
const LIT: Duration = Duration::from_secs(4);

fn state() -> &'static Mutex<State> {
    static S: OnceLock<Mutex<State>> = OnceLock::new();
    S.get_or_init(Mutex::default)
}

/// Records a use. `plugin` is the package's display name or id.
pub fn record(plugin: &str, kind: Kind, what: &str, allowed: bool, reason: &str) {
    let mut s = state().lock().unwrap_or_else(|e| e.into_inner());
    let key = (plugin.to_string(), kind);
    let first = !s.tally.contains_key(&key);
    let was_lit = s.recent.get(&key).is_some_and(|t| t.elapsed() < LIT);
    let tally = s.tally.entry(key.clone()).or_default();
    tally.count += 1;
    if !allowed {
        tally.refused += 1;
    }
    tally.last = Some(SystemTime::now());
    if allowed {
        s.recent.insert(key, Instant::now());
    }
    let log = s.log.entry(plugin.to_string()).or_default();
    if log.len() >= LOG_PER_PLUGIN {
        log.pop_front();
    }
    log.push_back(Use {
        plugin: plugin.into(),
        kind,
        what: what.into(),
        allowed,
        reason: reason.into(),
        at: SystemTime::now(),
    });

    let notice = if !allowed {
        Some(Notice {
            plugin: plugin.into(),
            kind: Some(kind),
            text: format!("{plugin} was refused {}: {what}", kind.label().to_lowercase()),
            refused: true,
            urgent: matches!(kind, Kind::Screen | Kind::Camera | Kind::Microphone),
        })
    } else if first || (kind.sensitive() && !was_lit) {
        // the first use of each kind, and every start of a sensitive one
        Some(Notice {
            plugin: plugin.into(),
            kind: Some(kind),
            text: first_use_text(plugin, kind, what),
            refused: false,
            urgent: kind == Kind::FullAccess,
        })
    } else {
        None
    };
    if let Some(n) = notice {
        if s.notices.len() >= 100 {
            s.notices.pop_front();
        }
        s.newest = Some((n.clone(), Instant::now()));
        s.notices.push_back(n);
    }
    s.generation += 1;
}

fn first_use_text(plugin: &str, kind: Kind, what: &str) -> String {
    match kind {
        Kind::Network => format!("{plugin} connected to {what}"),
        Kind::HomeNetwork => format!("{plugin} reached {what} on your network"),
        Kind::Screen => format!("{plugin} is looking at your screen"),
        Kind::Camera => format!("{plugin} is using the camera"),
        Kind::Microphone => format!("{plugin} is using the microphone"),
        Kind::Files => format!("{plugin} opened {what}"),
        Kind::Devices => format!("{plugin} opened {what}"),
        Kind::OtherApp => format!("{plugin} is talking to {what}"),
        Kind::Chat => format!("{plugin} is using chat"),
        Kind::FullAccess => format!("{plugin} is running with full access: Lightair can't see what it does"),
    }
}

/// A notice about a package itself (withdrawn by its catalog, switched off).
pub fn notice(plugin: &str, text: &str, urgent: bool) {
    let mut s = state().lock().unwrap_or_else(|e| e.into_inner());
    let n = Notice { plugin: plugin.into(), kind: None, text: text.into(), refused: true, urgent };
    if s.notices.len() >= 100 {
        s.notices.pop_front();
    }
    s.newest = Some((n.clone(), Instant::now()));
    s.notices.push_back(n);
    s.generation += 1;
}

/// How long the status bar shows a notice.
const NOTICE_SHOWN: Duration = Duration::from_secs(6);

/// The notice the status bar shows now, if one came in the last few seconds.
pub fn current_notice() -> Option<Notice> {
    let s = state().lock().unwrap_or_else(|e| e.into_inner());
    s.newest.as_ref().filter(|(_, t)| t.elapsed() < NOTICE_SHOWN).map(|(n, _)| n.clone())
}

/// A notice a plugin asked to show (`notifications.show`): not a refusal, not urgent.
pub fn message(plugin: &str, text: &str) {
    let mut s = state().lock().unwrap_or_else(|e| e.into_inner());
    let n = Notice { plugin: plugin.into(), kind: None, text: text.into(), refused: false, urgent: false };
    if s.notices.len() >= 100 {
        s.notices.pop_front();
    }
    s.newest = Some((n.clone(), Instant::now()));
    s.notices.push_back(n);
    s.generation += 1;
}

/// The notices of this run, newest last.
pub fn notices() -> Vec<Notice> {
    state().lock().unwrap_or_else(|e| e.into_inner()).notices.iter().cloned().collect()
}

/// What's in use right now, for the status bar's indicator: (plugin, kind), each once.
pub fn lit() -> Vec<(String, Kind)> {
    let s = state().lock().unwrap_or_else(|e| e.into_inner());
    s.recent.iter().filter(|(_, t)| t.elapsed() < LIT).map(|(k, _)| k.clone()).collect()
}

/// Marks a full-access plugin as running (the indicator shows it as long as it runs).
pub fn full_access_running(plugin: &str) {
    record(plugin, Kind::FullAccess, "started", true, "");
}

/// Every plugin's tallies, for the panel: plugin → kind → tally.
pub fn tallies() -> BTreeMap<String, BTreeMap<Kind, Tally>> {
    let s = state().lock().unwrap_or_else(|e| e.into_inner());
    let mut out: BTreeMap<String, BTreeMap<Kind, Tally>> = BTreeMap::new();
    for ((plugin, kind), t) in &s.tally {
        out.entry(plugin.clone()).or_default().insert(*kind, t.clone());
    }
    out
}

/// A plugin's log, newest last.
pub fn log(plugin: &str) -> Vec<Use> {
    state()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .log
        .get(plugin)
        .map(|l| l.iter().cloned().collect())
        .unwrap_or_default()
}

/// Changes whenever something is recorded (the panel redraws on change).
pub fn generation() -> u64 {
    state().lock().unwrap_or_else(|e| e.into_inner()).generation
}

/// Connects the sources Lightair has today: refused requests of pages from packages. Called
/// once at start.
pub fn connect() {
    // pages from packages follow the current grants and withdrawals, not only what they were
    // added with
    lightair_web::set_policy(crate::packages::page_policy);
    lightair_web::on_blocked(|owner, url| {
        record(&crate::packages::display_name(owner), Kind::Network, &host_of(url), false, "not declared");
    });
}

/// A helper's proxy event.
pub fn helper_event(plugin: &str, e: &lightair_helpers::Event) {
    let kind = if e.reason.contains("home network") { Kind::HomeNetwork } else { Kind::Network };
    let what = format!("{}:{}", e.host, e.port);
    record(plugin, kind, &what, e.allowed, if e.allowed { "" } else { e.reason });
}

fn host_of(url: &str) -> String {
    url.split_once("://")
        .map(|(_, rest)| rest.split(['/', '?', '#']).next().unwrap_or(rest).to_string())
        .unwrap_or_else(|| url.chars().take(80).collect())
}
