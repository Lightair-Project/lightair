//! Lightair's engine without libobs: what the desktop runs its show on. It grows as the libobs
//! engine (`lightair-obs`) shrinks: each part moves here once nothing in it needs libobs, and
//! `lightair-obs` re-exports it until it's gone (docs/plans/no-libobs.md).

pub mod access;
pub mod audio;
pub mod audio_host;
pub mod clock;
pub mod devices;
pub mod gpu;
pub mod meters;
pub mod ndi;
pub mod outputs;
pub mod phone_camera;
#[cfg(target_os = "macos")]
pub mod phone_decode;
pub mod phone_units;
#[cfg(target_os = "macos")]
pub mod own_outputs;
pub mod program;
pub mod plugins;
pub mod rtsp;
pub mod show;
pub mod soundboard;
#[cfg(target_os = "macos")]
pub mod vcam_mac;
#[cfg(target_os = "macos")]
pub mod vision;
pub mod zoom_to;
