//! A Studio take's video (docs/plans/screen-studio.md, 4.2): each screen and camera in the
//! program recorded to a file of its own. Not made without libobs yet: starting one says so.

use std::path::{Path, PathBuf};

use lightair_model::{Id, Source};

/// One recorded file.
#[derive(Debug, Clone)]
pub struct Track {
    /// "Screen", "Camera", "Camera 2"…
    pub name: String,
    /// A screen (else a camera): what it is, apart from its name.
    pub screen: bool,
    pub source: Id,
    pub file: PathBuf,
    pub width: u32,
    pub height: u32,
}

/// A take's per-source recordings.
#[derive(Debug, Default)]
pub struct StudioTake {
    pub tracks: Vec<Track>,
}

impl StudioTake {
    /// Starts recording `sources` into `folder` (not yet without libobs: an error).
    pub fn start(
        _sources: Vec<(Source, Id)>,
        _folder: &Path,
        _fps: (u32, u32),
        _outputs: &mut crate::outputs_host::Outputs,
        _encoders: &[String],
    ) -> Result<StudioTake, String> {
        Err("Studio takes aren't recorded without libobs yet.".into())
    }

    /// Whether every recording of the take has stopped.
    pub fn finished(&self) -> bool {
        true
    }
}
