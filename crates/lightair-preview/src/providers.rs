//! Picture providers: modules that make one kind of source's picture themselves (web pages,
//! NDI, Syphon, SRT from another computer, the phone camera, media files…), asked each frame.
//! A source a provider has a frame for is drawn from that frame, as a captured screen or camera
//! is, with the effects the compositor draws; until it has one, libobs's picture stands in.
//!
//! A provider gives Lightair's own BGRA pictures (`lightair_host::buffers`) with premultiplied
//! colours (opaque pictures are both), and must not draw into a buffer again while the
//! compositor may still hold it: give a new buffer for each new picture.
//!
//! Windows has only the pictures lightair-host's runtimes make (`provide_frames`) yet; the other
//! providers are macOS's.

use std::collections::HashMap;
use std::sync::{Arc, OnceLock};

use lightair_host::bridge;
use lightair_model::{Id, Source};

/// One kind of source whose picture is made outside libobs.
pub trait Provider: Send + Sync {
    /// Whether this provider makes `source`'s picture (its kind, and settings it can handle).
    fn handles(&self, source: &Source) -> bool;

    /// The sources this provider handles that `canvas` shows now (empty: none). Each canvas's
    /// compositor calls it every frame: start what's newly shown, and let go of what no canvas
    /// shows any more after a moment. Keep it cheap, and never block: opening anything slow
    /// belongs on a thread of the provider's own.
    fn want(&self, canvas: Id, shown: &[&Source]);

    /// `source`'s newest picture, if it has one yet.
    fn frame(&self, source: Id) -> Option<Frame>;
}

/// A provider's picture.
pub struct Frame {
    pub buffer: bridge::Buffer,
    /// Counts the provider's pictures of this source: a different value is a new picture.
    pub serial: u64,
}

fn providers() -> &'static [Arc<dyn Provider>] {
    static ALL: OnceLock<Vec<Arc<dyn Provider>>> = OnceLock::new();
    ALL.get_or_init(builtin)
}

/// The providers Lightair has. One line per kind, each kept by its own module.
// what's pushed depends on the platform
#[allow(clippy::vec_init_then_push)]
fn builtin() -> Vec<Arc<dyn Provider>> {
    #[allow(unused_mut)]
    let mut all: Vec<Arc<dyn Provider>> = Vec::new();
    #[cfg(any(target_os = "macos", windows))]
    all.push(Arc::new(crate::provide_web::Web::default())); // web pages
    #[cfg(target_os = "macos")]
    {
        all.push(Arc::new(crate::provide_ndi::Ndi::default())); // NDI
        all.push(Arc::new(crate::provide_syphon::Syphon::default())); // Syphon
        all.push(Arc::new(crate::provide_srt::Srt::default())); // SRT from another computer
        all.push(Arc::new(crate::provide_phone::Phone::default())); // the phone camera
        all.push(Arc::new(crate::provide_media::Media)); // media files
    }
    all.push(Arc::new(crate::provide_frames::Frames)); // what lightair-host's runtimes make
    all
}

/// Whether a provider makes `source`'s pictures (it may have none yet: a page still loading).
#[cfg(windows)]
pub(crate) fn handled(source: &Source) -> bool {
    providers().iter().any(|p| p.handles(source))
}

/// Tells every provider which of its sources `canvas` shows now.
pub(crate) fn want(canvas: Id, shown: &[&Source]) {
    for p in providers() {
        let mine: Vec<&Source> = shown.iter().copied().filter(|s| p.handles(s)).collect();
        p.want(canvas, &mine);
    }
}

/// The newest picture of each of `shown` a provider has one for, as the compositor draws
/// pictures given without libobs's filters.
pub(crate) fn frames(shown: &[&Source]) -> HashMap<Id, bridge::Picture> {
    let mut out = HashMap::new();
    for source in shown {
        let Some(p) = providers().iter().find(|p| p.handles(source)) else { continue };
        if let Some(f) = p.frame(source.id) {
            out.insert(source.id, bridge::Picture { buffer: f.buffer, unfiltered: true, serial: f.serial });
        }
    }
    out
}
