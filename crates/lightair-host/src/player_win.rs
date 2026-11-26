//! A media file played on its own (the Library's player) on Windows: a private
//! [`FfmpegMedia`] (the `ffmpeg` tool), not in any scene. Media calls are safe from any thread.
//!
//! Its picture is read from the frames it publishes ([`Player::frame`]); a preview shows none
//! (`source_ref` gives none). Its sound plays on the monitoring device (`crate::monitoring`), on
//! an output of its own, not through the mixer.

use std::path::Path;
use std::sync::Arc;

use lightair_model::Id;

use crate::ffmpeg_media::{FfmpegMedia, Settings};
use crate::media::{MediaState, MediaStatus};

pub struct Player {
    media: FfmpegMedia,
    id: Id,
    /// The output its sound plays on (closed as it drops).
    _speaker: Option<crate::devices::Devices>,
}

impl Player {
    /// Opens `path`, playing from the start.
    pub fn open(path: &Path) -> Option<Player> {
        let settings = Settings {
            input: path.to_string_lossy().into_owned(),
            local: true,
            looping: false,
            restart_on_activate: true,
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
        let producer = parking_lot::Mutex::new(producer);
        // (the tool decodes at the engine's rate, in real time: straight into the ring)
        let sound_to: crate::ffmpeg_media::SoundTo = Arc::new(move |interleaved: &[f32], _rate: u32| {
            let mut p = producer.lock();
            let room = p.slots().min(interleaved.len()) & !1;
            if let Ok(chunk) = p.write_chunk_uninit(room) {
                chunk.fill_from_iter(interleaved[..room].iter().copied());
            }
        });
        let media = FfmpegMedia::with_sound_to(id, settings, rate, sound_to);
        media.activate();
        Some(Player { media, id, _speaker: speaker })
    }

    /// What a preview shows it as: nothing (the Library draws the player's frames itself).
    pub fn source_ref(&self) -> Option<(Id, Id)> {
        None
    }

    /// The newest frame (BGRA); `None` before the first.
    pub fn frame(&self) -> Option<crate::buffers::Buffer> {
        crate::frames::latest(self.id).map(|(buffer, _)| buffer)
    }

    /// The picture's size (0×0 until the file has opened, and for sound only).
    pub fn size(&self) -> (u32, u32) {
        self.media.size()
    }

    pub fn status(&self) -> MediaStatus {
        MediaStatus {
            state: MediaState::from_code(self.media.state()),
            time_ms: self.media.time_ms(),
            duration_ms: self.media.duration_ms(),
        }
    }

    /// Plays, or pauses; from the start again once it has ended.
    pub fn toggle(&self) {
        match self.status().state {
            MediaState::Ended | MediaState::Stopped => self.media.restart(),
            MediaState::Playing => self.media.play_pause(true),
            _ => self.media.play_pause(false),
        }
    }

    pub fn seek(&self, ms: i64) {
        self.media.set_time_ms(ms.max(0));
    }
}
