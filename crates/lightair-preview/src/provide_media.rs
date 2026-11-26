//! Media files as a picture provider: a media source the desktop's own media source type plays
//! (MP4 and QuickTime files, decoded by the hardware decoder straight into IOSurfaces) is drawn
//! from the frame that source shows now, with no copy.
//!
//! The libobs source keeps playing: it reads the file, keeps the clock (pause, seek, looping,
//! restart on activate, the end and clearing), answers the media controls and the remote API,
//! and hands its sound to the mix as before. This provider only takes the frame it picked on
//! libobs's clock, so it never runs a clock of its own. Files that source type can't play
//! (WebM, MKV, streams, playlists, a changed speed) are played by ffmpeg instead. Either way a
//! file with a rotation comes turned upright.

use lightair_model::{Id, Source};
use lightair_host::media_player as media_source;

use crate::providers::{Frame, Provider};

/// The document's kind for a media file.
const KIND: &str = "ffmpeg_source";

pub(crate) struct Media;

impl Provider for Media {
    fn handles(&self, source: &Source) -> bool {
        source.kind == KIND && media_source::plays(source.id)
    }

    /// Nothing to start or stop: the libobs source plays while libobs has it on air.
    fn want(&self, _canvas: Id, _shown: &[&Source]) {}

    fn frame(&self, source: Id) -> Option<Frame> {
        let p = media_source::picture(source)?;
        Some(Frame { buffer: p.buffer, serial: p.serial })
    }
}
