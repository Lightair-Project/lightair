//! A media file played on its own (the Library's player): a private [`MediaPlayer`], not in any
//! scene. Media calls are safe from any thread.
//!
//! Its picture is read straight from the player ([`Player::frame`]); a preview shows none
//! (`source_ref` gives none). Not yet: its sound in the headphones.

use std::path::Path;

use lightair_model::Id;

use crate::media::{MediaState, MediaStatus};
use crate::media_player::{MediaPlayer, Settings};

pub struct Player {
    player: MediaPlayer,
    id: Id,
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
        Some(Player { player: MediaPlayer::new(Some(id), settings, rate, true, None), id })
    }

    /// What a preview shows it as: nothing yet without libobs.
    pub fn source_ref(&self) -> Option<(Id, Id)> {
        None
    }

    /// The frame due now (BGRA, IOSurface-backed); `None` before the first, and for a file with
    /// a rotation.
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
