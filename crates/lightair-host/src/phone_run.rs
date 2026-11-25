//! A Phone camera source (macOS): each stream the phone sends (a new feed
//! generation) gets a decoder of its own, on a thread of its own. Its pictures go to the feed,
//! which the compositor's phone provider takes them from; its microphone goes into the mixer by
//! the source's id (a track takes it like any other source).

use std::sync::Arc;

use lightair_model::Id;

use crate::phone_camera::{Feed, Pictures, Unit, UnitKind};
use crate::phone_decode::Decoder;

/// One phone camera source's runtime.
pub struct PhoneCamera {
    id: Id,
    feed: Arc<Feed>,
    /// The feed generation a decoder was made for (`u64::MAX`: none yet).
    seen: u64,
}

impl PhoneCamera {
    pub fn new(id: Id) -> PhoneCamera {
        // its sound straight into a track of the mixer
        crate::audio_host::feed_slot(id);
        PhoneCamera { id, feed: crate::phone_camera::feed(&id.to_string()), seen: u64::MAX }
    }

    /// A new stream from the phone gets a new decoder (the host's tick).
    pub fn tick(&mut self) {
        let generation = self.feed.generation.load(std::sync::atomic::Ordering::Acquire);
        if generation == self.seen {
            return;
        }
        self.seen = generation;
        let (tx, rx) = crossbeam_channel::bounded::<Unit>(256);
        // (the previous decoding thread ends once its units stop)
        *self.feed.units.lock() = Some(tx);
        let (id, feed) = (self.id, self.feed.clone());
        let spawned =
            std::thread::Builder::new().name("lightair-phone-decode".into()).spawn(move || decode(id, feed, rx));
        if spawned.is_err() {
            tracing::warn!(%id, "phone camera: couldn't start its decoder");
        }
    }
}

impl Drop for PhoneCamera {
    fn drop(&mut self) {
        self.feed.units.lock().take();
        crate::audio_host::feeds_itself_no_more(self.id);
    }
}

/// Decodes a stream's units until they stop: pictures to the feed, sound into the mixer.
fn decode(id: Id, feed: Arc<Feed>, units: crossbeam_channel::Receiver<Unit>) {
    let to = feed;
    let pictures: Pictures = Arc::new(move |image, pts| {
        // (taken by the compositor's provider while it shows the source; else let go)
        let _ = to.offer(image);
        if let Some(pts) = pts {
            to.decoded(pts);
            to.shown_next(pts);
        }
    });
    let mut decoder = Decoder::new(true, pictures);
    for unit in units {
        match unit.kind {
            UnitKind::H264 | UnitKind::Hevc => decoder.video(unit.kind == UnitKind::Hevc, &unit.data, unit.pts, None),
            UnitKind::Aac => decoder.sound(&unit.data, &mut |planes, rate| crate::bridge::sound(id, planes, rate)),
        }
    }
}
