//! Pictures lightair-host's runtimes make for a source (a camera over USB, a media file FFmpeg
//! decodes: `lightair_host::frames`), drawn as they're published.

use lightair_host::frames;
use lightair_model::{Id, Source};

use crate::providers::{Frame, Provider};

pub(crate) struct Frames;

impl Provider for Frames {
    fn handles(&self, source: &Source) -> bool {
        frames::has(source.id)
    }

    /// Nothing to start or stop: the runtime runs while the document has the source.
    fn want(&self, _canvas: Id, _shown: &[&Source]) {}

    fn frame(&self, source: Id) -> Option<Frame> {
        let (buffer, serial) = frames::latest(source)?;
        Some(Frame { buffer, serial })
    }
}
