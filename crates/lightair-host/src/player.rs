//! A media file played on its own (the Library's player): a private [`MediaPlayer`], not in any
//! scene. Media calls are safe from any thread.
//!
//! Not yet without libobs: a preview showing its picture (`source_ref` gives none) and its sound
//! in the headphones.

use std::path::Path;

use lightair_model::Id;

use crate::media::{MediaState, MediaStatus};
use crate::media_player::{MediaPlayer, Settings};

pub struct Player {
    player: MediaPlayer,
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
        Some(Player { player: MediaPlayer::new(Some(Id::new()), settings, rate, true, None) })
    }

    /// What a preview shows it as: nothing yet without libobs.
    pub fn source_ref(&self) -> Option<(Id, Id)> {
        None
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
