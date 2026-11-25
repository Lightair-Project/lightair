//! Pictures a runtime makes for a source (a camera over USB, a media file FFmpeg decodes), by
//! the source's id: the newest one, which the compositor draws (`lightair-preview`'s
//! `provide_frames`).

use std::collections::HashMap;

use lightair_model::Id;
use parking_lot::Mutex;

use crate::buffers::Buffer;

/// Each source's newest picture and how many it has had.
struct Frames(Option<HashMap<Id, (Buffer, u64)>>);

// SAFETY: Lightair's buffers are reference counted thread-safely, and a picture isn't written
// again once it's published.
unsafe impl Send for Frames {}
unsafe impl Sync for Frames {}

static FRAMES: Mutex<Frames> = Mutex::new(Frames(None));

/// `buffer` (BGRA, IOSurface-backed, premultiplied or opaque) is `source`'s newest picture. Never
/// draw into it again: publish a new buffer for each picture.
pub fn publish(source: Id, buffer: Buffer) {
    let mut f = FRAMES.lock();
    let all = f.0.get_or_insert_with(HashMap::new);
    let serial = all.get(&source).map_or(1, |(_, n)| n + 1);
    // (the old one is let go outside the lock)
    let old = all.insert(source, (buffer, serial));
    drop(f);
    drop(old);
}

/// `source`'s newest picture and its count; `None` before its first.
pub fn latest(source: Id) -> Option<(Buffer, u64)> {
    FRAMES.lock().0.as_ref()?.get(&source).cloned()
}

/// Whether `source` has pictures here.
pub fn has(source: Id) -> bool {
    FRAMES.lock().0.as_ref().is_some_and(|f| f.contains_key(&source))
}

/// `source` makes no more pictures.
pub fn forget(source: Id) {
    let gone = FRAMES.lock().0.as_mut().and_then(|f| f.remove(&source));
    drop(gone);
}
