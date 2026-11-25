//! The Library's player where Lightair has no media player of its own yet (Windows without
//! libobs): nothing opens, and opening says so in the log once.

use std::path::Path;

use lightair_model::Id;

use crate::media::MediaStatus;

/// A media file played on its own. None can be made here yet.
pub struct Player(std::convert::Infallible);

impl Player {
    /// Opens nothing here yet: `None`, said in the log once.
    pub fn open(path: &Path) -> Option<Player> {
        static SAID: std::sync::Once = std::sync::Once::new();
        SAID.call_once(|| tracing::warn!(?path, "not yet: the Library's player on this platform without libobs"));
        None
    }

    pub fn source_ref(&self) -> Option<(Id, Id)> {
        match self.0 {}
    }

    pub fn size(&self) -> (u32, u32) {
        match self.0 {}
    }

    pub fn status(&self) -> MediaStatus {
        match self.0 {}
    }

    pub fn toggle(&self) {
        match self.0 {}
    }

    pub fn seek(&self, _ms: i64) {
        match self.0 {}
    }
}
