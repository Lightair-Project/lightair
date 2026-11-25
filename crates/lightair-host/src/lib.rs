//! Lightair's engine without libobs: what the desktop runs its show on. It grows as the libobs
//! engine (`lightair-obs`) shrinks: each part moves here once nothing in it needs libobs, and
//! `lightair-obs` re-exports it until it's gone (docs/plans/no-libobs.md).

pub mod access;
pub mod audio;
pub mod audio_host;
pub mod audio_tap;
pub mod avatar;
pub mod avatar_pins;
#[cfg(target_os = "macos")]
pub mod avatar_run;
#[cfg(target_os = "macos")]
pub mod bridge;
#[cfg(target_os = "macos")]
pub mod buffers;
#[cfg(windows)]
#[path = "buffers_win.rs"]
pub mod buffers;
pub mod clock;
pub mod devices;
pub mod displays;
pub mod dslr;
pub mod engine;
pub mod face_drive;
#[cfg(target_os = "macos")]
pub mod ffmpeg_media;
#[cfg(target_os = "macos")]
pub mod frames;
pub mod gpu;
pub mod host;
pub mod kinds;
pub mod loudness;
pub mod media;
#[cfg(target_os = "macos")]
pub mod media_player;
pub mod meters;
pub mod monitoring;
pub mod ndi;
pub mod on_screen;
pub mod outputs;
pub mod outputs_host;
pub mod phone_camera;
#[cfg(target_os = "macos")]
pub mod phone_decode;
#[cfg(target_os = "macos")]
pub mod phone_run;
pub mod phone_units;
pub mod piair_voice;
#[cfg(target_os = "macos")]
pub mod own_outputs;
pub mod program;
#[cfg(target_os = "macos")]
pub mod player;
#[cfg(not(target_os = "macos"))]
#[path = "player_elsewhere.rs"]
pub mod player;
pub mod playlists;
pub mod plugins;
pub mod pointer;
pub mod rtsp;
pub mod shared_video;
pub mod show;
pub mod timers;
#[cfg(target_os = "macos")]
pub mod tracker;
#[cfg(target_os = "macos")]
pub mod system_audio;
#[cfg(any(target_os = "macos", windows))]
pub mod web;
pub mod soundboard;
pub mod studio_take;
pub mod vcam;
#[cfg(target_os = "macos")]
pub mod vcam_mac;
#[cfg(target_os = "macos")]
pub mod vision;
pub mod zoom_to;

pub use engine::{Capabilities, Engine, EngineConfig, EngineError, EngineState, Stats, StudioTake, read_stats};
pub use host::Host as Reconciler;
pub use outputs_host::{Outputs, VirtualCameraScene};
