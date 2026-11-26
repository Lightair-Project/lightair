//! A media file played on its own (the Library's player): a private [`MediaPlayer`], not in any
//! scene. Media calls are safe from any thread.
//!
//! Its picture is read straight from the player ([`Player::frame`]); a preview shows none
//! (`source_ref` gives none). Its sound plays on the monitoring device (`crate::monitoring`),
//! on an output of its own, not through the mixer.

use std::path::Path;
use std::sync::Arc;

use lightair_model::Id;

use crate::media::{MediaState, MediaStatus};
use crate::media_player::{MediaPlayer, Settings};

pub struct Player {
    player: MediaPlayer,
    id: Id,
    /// The output its sound plays on (closed as it drops).
    _speaker: Option<crate::devices::Devices>,
}

/// The player's sound, into the ring its output plays.
struct Speaker(parking_lot::Mutex<rtrb::Producer<f32>>);

impl crate::media_player::Elsewhere for Speaker {
    fn audio(&self, interleaved: &[f32], rate: u32, timestamp_ns: u64) {
        let mut p = self.0.lock();
        // handed over ahead of time: silence first, so it plays when it's due (what's queued
        // plays before it)
        let ahead = timestamp_ns.saturating_sub(crate::clock::now_ns());
        let due = (ahead as u128 * rate as u128 / 1_000_000_000) as usize * 2;
        let queued = p.buffer().capacity() - p.slots();
        let pad = due.saturating_sub(queued).min(p.slots()) & !1;
        if pad > 0
            && let Ok(chunk) = p.write_chunk_uninit(pad)
        {
            chunk.fill_from_iter(std::iter::repeat(0.0));
        }
        let room = p.slots().min(interleaved.len()) & !1;
        if let Ok(chunk) = p.write_chunk_uninit(room) {
            chunk.fill_from_iter(interleaved[..room].iter().copied());
        }
    }

    fn ended(&self) {}

    fn started(&self) {}
}

impl Player {
    /// Opens `path`, playing from the start.
    pub fn open(path: &Path) -> Option<Player> {
        let settings = Settings {
            path: path.to_string_lossy().into_owned(),
            looping: false,
            restart_on_activate: false,
            clear_on_end: false,
        };
        let rate = crate::audio_host::engine_rate();
        let id = Id::new();
        // its sound on the monitoring device: half a second of room
        let (producer, consumer) = rtrb::RingBuffer::new(rate as usize);
        let devices = crate::devices::Devices::start();
        let device = crate::monitoring::current().1;
        let device = if device == "default" { String::new() } else { device };
        let speaker = match devices.open_output(&device, rate, consumer) {
            Ok(()) => Some(devices),
            Err(e) => {
                tracing::warn!(%e, "the Library's player: no sound");
                None
            }
        };
        let elsewhere: Option<Arc<dyn crate::media_player::Elsewhere>> =
            speaker.is_some().then(|| Arc::new(Speaker(parking_lot::Mutex::new(producer))) as Arc<_>);
        // (not a document's source: no track takes its sound, which goes to the speaker)
        let player = MediaPlayer::new(Some(id), settings, rate, true, elsewhere);
        Some(Player { player, id, _speaker: speaker })
    }

    /// What a preview shows it as: nothing yet (the Library draws the player's frames itself).
    pub fn source_ref(&self) -> Option<(Id, Id)> {
        None
    }

    /// The frame due now (BGRA, IOSurface-backed, a rotated file's turned upright); `None` before
    /// the first.
    pub fn frame(&self) -> Option<crate::buffers::Buffer> {
        crate::media_player::picture(self.id).map(|p| p.buffer)
    }

    /// The picture's size (0×0 until the file has opened, and for sound only), turned as shown.
    pub fn size(&self) -> (u32, u32) {
        let ((w, h), rotation) = self.player.natural();
        if rotation % 180 == 90 { (h, w) } else { (w, h) }
    }

    pub fn status(&self) -> MediaStatus {
        MediaStatus {
            state: MediaState::from_code(self.player.state()),
            time_ms: self.player.time_ms(),
            duration_ms: self.player.duration_ms(),
        }
    }

    /// Plays, or pauses; from the start again once it has ended.
    pub fn toggle(&self) {
        match self.status().state {
            MediaState::Ended | MediaState::Stopped => self.player.restart(),
            MediaState::Playing => self.player.play_pause(true),
            _ => self.player.play_pause(false),
        }
    }

    pub fn seek(&self, ms: i64) {
        self.player.set_time_ms(ms.max(0));
    }
}
