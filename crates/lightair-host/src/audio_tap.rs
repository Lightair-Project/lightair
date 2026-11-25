//! Listening to a source's sound (the AI co-host hearing a mixer track, piair/docs/design.md §4):
//! mono, 16 kHz, 16-bit. Without libobs every track's sound is the mixer's, heard through
//! `Host::listen_track`; a source on no track is heard as silence.

use lightair_model::Id;

/// Samples a tap hands out per second.
pub const RATE: u32 = 16_000;

/// A tap on a source: its sound comes out of the consumer [`Tap::open_source`] returns, until
/// dropped.
pub struct Tap {
    _source: Id,
    /// Kept so the consumer stays open (and silent).
    _producer: rtrb::Producer<i16>,
}

impl Tap {
    /// Taps `source`'s sound (silence: a source the mixer doesn't take isn't heard).
    pub fn open_source(source: Id) -> (Tap, rtrb::Consumer<i16>) {
        let (producer, consumer) = rtrb::RingBuffer::new(RATE as usize);
        (Tap { _source: source, _producer: producer }, consumer)
    }
}
