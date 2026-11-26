//! Lightair's engine: what the desktop runs its show on. The engine thread and the document host
//! (a runtime per source), the mixer, devices, plug-ins, meters, Lightair's own outputs and what
//! the compositor (`lightair-preview`) reads of the show.

pub mod access;
pub mod audio;
pub mod audio_host;
pub mod audio_tap;
pub mod avatar;
pub mod avatar_pins;
#[cfg(any(target_os = "macos", windows))]
pub mod avatar_run;
#[cfg(any(target_os = "macos", windows))]
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
#[cfg(any(target_os = "macos", windows))]
pub mod ffmpeg_media;
#[cfg(any(target_os = "macos", windows))]
pub mod frames;

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
#[cfg(windows)]
#[path = "phone_decode_win.rs"]
pub mod phone_decode;
#[cfg(target_os = "macos")]
pub mod phone_run;
pub mod phone_units;
pub mod piair_voice;
#[cfg(any(target_os = "macos", windows))]
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
#[cfg(any(target_os = "macos", windows))]
pub mod tracker;
#[cfg(any(target_os = "macos", windows))]
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
