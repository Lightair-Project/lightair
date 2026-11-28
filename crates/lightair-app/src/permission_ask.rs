//! A running package asks for some of its optional permissions (`permissions.request`, from a
//! helper's or PiAir's protocol, or a script's `lightair.permissions.request`): the request waits
//! here, the window shows the person a second screen with the package's reason and only those
//! extra powers, and the answer goes back to the asker (plugins.md §4.8, "optional permissions").
//!
//! [`ask`] blocks its caller (a helper's reader thread, a script's thread), never the window.

use std::collections::VecDeque;
use std::sync::mpsc;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

/// One request waiting for the person.
pub struct Request {
    pub id: String,
    /// The package's name, as the screen says it.
    pub name: String,
    pub permissions: Vec<String>,
    /// The package's own reason, shown as its words (untrusted text).
    pub reason: String,
    answer: mpsc::Sender<Vec<String>>,
}

impl Request {
    /// Answers it: the permissions allowed (the rest are refused).
    pub fn answer(self, allowed: Vec<String>) {
        let allowed: Vec<String> = allowed.into_iter().filter(|p| self.permissions.contains(p)).collect();
        crate::grants::grant_optional(&self.id, &allowed);
        for p in &self.permissions {
            let ok = allowed.contains(p);
            if let Some(kind) = crate::plugin_activity::Kind::of_permission(p) {
                crate::plugin_activity::record(
                    &self.name,
                    kind,
                    &format!("asked for {p}"),
                    ok,
                    if ok { "" } else { "you said no" },
                );
            }
        }
        let _ = self.answer.send(allowed);
    }
}

fn queue() -> &'static Mutex<VecDeque<Request>> {
    static Q: OnceLock<Mutex<VecDeque<Request>>> = OnceLock::new();
    Q.get_or_init(Mutex::default)
}

type Waker = Box<dyn Fn() + Send + Sync>;

fn waker() -> &'static Mutex<Option<Waker>> {
    static W: OnceLock<Mutex<Option<Waker>>> = OnceLock::new();
    W.get_or_init(Mutex::default)
}

/// The window's way to be told a request is waiting (set once it exists).
pub fn set_waker(f: impl Fn() + Send + Sync + 'static) {
    *waker().lock().unwrap_or_else(|e| e.into_inner()) = Some(Box::new(f));
}

/// The next request waiting, if any.
pub fn take() -> Option<Request> {
    queue().lock().unwrap_or_else(|e| e.into_inner()).pop_front()
}

/// How long a request waits for the person before it counts as a no.
const PATIENCE: Duration = Duration::from_secs(10 * 60);

/// Asks the person to give package `id` some of its optional permissions; returns the ones
/// they allowed. Only `permissions` it declared as optional and doesn't hold yet are asked; with
/// none left it returns at once. Never call it on the window's thread.
pub fn ask(id: &str, name: &str, permissions: &[String], reason: &str) -> Vec<String> {
    let declared: Vec<String> = crate::packages::installed()
        .iter()
        .filter(|i| i.id().as_str() == id)
        .flat_map(|i| i.manifest.optional_permissions.clone())
        .collect();
    let held = crate::grants::optional_granted(id);
    let wanted: Vec<String> =
        permissions.iter().filter(|p| declared.contains(p) && !held.contains(*p)).cloned().collect();
    if wanted.is_empty() {
        return Vec::new();
    }
    let (tx, rx) = mpsc::channel();
    let reason: String = reason.chars().take(300).collect();
    queue().lock().unwrap_or_else(|e| e.into_inner()).push_back(Request {
        id: id.to_string(),
        name: name.to_string(),
        permissions: wanted,
        reason,
        answer: tx,
    });
    if let Some(wake) = waker().lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
        wake();
    }
    rx.recv_timeout(PATIENCE).unwrap_or_default()
}

/// What `id` declared as optional, minus what it holds: for `permissions.release`, the subset
/// it really holds.
pub fn release(id: &str, permissions: &[String]) -> Vec<String> {
    let held = crate::grants::optional_granted(id);
    let gone: Vec<String> = permissions.iter().filter(|p| held.contains(*p)).cloned().collect();
    crate::grants::release_optional(id, &gone);
    gone
}
