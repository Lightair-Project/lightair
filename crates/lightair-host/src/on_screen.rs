//! Cameras that went away and came back (an iPhone by USB, say): a capture of one that's running
//! starts again, as if new, since a device's own reconnection isn't always caught.

use std::sync::atomic::{AtomicU64, Ordering};

use parking_lot::Mutex;

/// Devices that came back: (unique id, when, as a count of reconnections).
static RECONNECTED: Mutex<Vec<(String, u64)>> = Mutex::new(Vec::new());
static RECONNECTIONS: AtomicU64 = AtomicU64::new(0);

/// A camera (or a phone's screen) with this unique id has (re)connected.
pub fn device_reconnected(uid: &str) {
    let n = RECONNECTIONS.fetch_add(1, Ordering::AcqRel) + 1;
    let mut list = RECONNECTED.lock();
    list.retain(|(u, _)| u != uid);
    list.push((uid.to_string(), n));
}

/// How many reconnections there have been (a capture remembers what it has looked at).
pub fn reconnections() -> u64 {
    RECONNECTIONS.load(Ordering::Acquire)
}

/// Whether `device` came back after reconnection number `seen`.
pub fn reconnected_since(device: &str, seen: u64) -> bool {
    RECONNECTED.lock().iter().any(|(u, at)| *at > seen && u == device)
}
