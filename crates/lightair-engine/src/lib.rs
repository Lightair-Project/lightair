//! Lightair's engine adapter over libobs.
//!
//! libobs is a process-wide singleton. [`Engine`] owns it on a dedicated **engine actor**
//! thread: every mutating libobs call happens there, in order (docs/03-architecture.md,
//! "Threading model"). Other threads send jobs with [`Engine::call`] / [`Engine::post`].

pub mod displays;
mod engine;
mod log;
pub mod meters;
pub mod obs;
pub mod outputs;
mod reconcile;
mod runtime;
mod transition;

pub use engine::{Capabilities, Engine, EngineConfig, EngineError, EngineState, Graphics, Modules, Stats, read_stats};
pub use libobs_sys as sys;
pub use reconcile::Reconciler;
pub use runtime::Runtime;

/// OBS modules Lightair loads from an OBS 33 plugin set: the ones that only need libobs.
/// Modules that link Qt or obs-frontend-api are handled by the compatibility layer
/// (docs/13-extensibility-and-compatibility.md).
pub const DEFAULT_MODULES: &[&str] = &[
    "aja",
    "coreaudio-encoder",
    "decklink",
    "image-source",
    "mac-avcapture",
    "mac-avcapture-legacy",
    "mac-capture",
    "mac-syphon",
    "mac-videotoolbox",
    "obs-ffmpeg",
    "obs-filters",
    "obs-outputs",
    "obs-transitions",
    "obs-webrtc",
    "obs-x264",
    "rtmp-services",
    "text-freetype2",
    "vlc-video",
];
