//! "Zoom to here" (piair/docs/design.md §6.3): a request every screen zoom honours for a while: in
//! to a point (display points) at a scale, then out as usual. PiAir asks for it to show viewers
//! something without clicking; the follow-zoom in [`crate::pointer`] does the moving.

use std::sync::Mutex;
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Request {
    /// Where, in global display points.
    pub at: (f32, f32),
    /// How far in (`None`: the look's own zoom scale).
    pub scale: Option<f32>,
    pub until: Instant,
    /// When it was asked for (a new one snaps the view there).
    pub since: Instant,
}

static REQUEST: Mutex<Option<Request>> = Mutex::new(None);
/// A request may be in force: checked first, without a lock, on every zoom's frame.
static ASKED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Zooms every screen capture with zoom on in to `at` for `hold`.
pub fn zoom_to(at: (f32, f32), scale: Option<f32>, hold: Duration) {
    let now = Instant::now();
    *REQUEST.lock().unwrap_or_else(|e| e.into_inner()) =
        Some(Request { at, scale: scale.map(|s| s.clamp(1.1, 6.0)), until: now + hold, since: now });
    ASKED.store(true, std::sync::atomic::Ordering::Relaxed);
}

/// Lets go of a zoom request early.
pub fn release() {
    *REQUEST.lock().unwrap_or_else(|e| e.into_inner()) = None;
    ASKED.store(false, std::sync::atomic::Ordering::Relaxed);
}

/// The request in force now.
pub fn current() -> Option<Request> {
    if !ASKED.load(std::sync::atomic::Ordering::Relaxed) {
        return None;
    }
    let mut g = REQUEST.lock().unwrap_or_else(|e| e.into_inner());
    if g.is_some_and(|r| r.until <= Instant::now()) {
        *g = None;
        ASKED.store(false, std::sync::atomic::Ordering::Relaxed);
    }
    *g
}
