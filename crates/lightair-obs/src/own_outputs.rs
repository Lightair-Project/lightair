//! Lightair's own outputs (`lightair_host::own_outputs`), and the one thing about them that needs
//! libobs: which canvas a libobs video given to an output is.

pub use lightair_host::own_outputs::*;

use lightair_model::Id;

use crate::outputs;

/// The canvas whose compositor feeds an output given `video` (`None`: the primary canvas), when
/// Lightair makes the output itself; `None` leaves the output to libobs (the switch off, or a
/// video that isn't a canvas's program, such as a Studio take's own canvas).
pub(crate) fn takes(video: Option<outputs::VideoOutput>) -> Option<Id> {
    if !enabled() {
        return None;
    }
    crate::program_source::canvas_of_video(video.map(|v| v.0).filter(|p| !p.is_null()))
}
