//! Canvas preview and Lightair's own compositor.
//!
//! The compositor (`compose`) draws every canvas and preview on the GPU; the UI takes the newest
//! finished frame. On macOS frames are IOSurface-backed `CVPixelBuffer`s for GPUI's `surface()`
//! element, with no pixel copied on the CPU (`mac`). Elsewhere (Windows today) frames are read
//! back to memory and shown as images (`readback`), or presented into a swap chain (`layer`).
//!
//! Threading: the compositor never waits on the UI; the only shared lock is held for a pointer
//! swap.

#[cfg(any(target_os = "macos", windows))]
pub mod avatars;
#[cfg(any(target_os = "macos", windows))]
mod backdrop;
#[cfg(any(target_os = "macos", windows))]
mod capture;
#[cfg(any(target_os = "macos", windows))]
mod buffer_gpu;
#[cfg(any(target_os = "macos", windows))]
mod pointer;
#[cfg(any(target_os = "macos", windows))]
mod timed;
#[cfg(any(target_os = "macos", windows))]
mod compose;
#[cfg(any(target_os = "macos", windows))]
pub mod providers;
#[cfg(any(target_os = "macos", windows))]
mod provide_web;
#[cfg(target_os = "macos")]
mod provide_ndi;
#[cfg(target_os = "macos")]
mod provide_syphon;
#[cfg(any(target_os = "macos", windows))]
mod post;
#[cfg(any(target_os = "macos", windows))]
mod provide_frames;
#[cfg(target_os = "macos")]
mod provide_media;
#[cfg(any(target_os = "macos", windows))]
mod provide_srt;
#[cfg(any(target_os = "macos", windows))]
mod provide_phone;
#[cfg(any(target_os = "macos", windows))]
mod looks;
#[cfg(target_os = "macos")]
mod mac;
#[cfg(any(target_os = "macos", windows))]
mod stickers;
#[cfg(any(target_os = "macos", windows))]
mod stinger;
#[cfg(any(target_os = "macos", windows))]
mod slideshow;
#[cfg(any(target_os = "macos", windows))]
mod text;
#[cfg(any(target_os = "macos", windows))]
mod widget_feed;
#[cfg(not(target_os = "macos"))]
mod readback;
#[cfg(windows)]
mod layer;
#[cfg(windows)]
pub mod win_gpu;
#[cfg(windows)]
pub use layer::Layer;

#[cfg(target_os = "macos")]
pub use mac::{Frame, Preview, frame_is_nv12, frame_rgba, frame_size, nv12, sample_ycbcr};
#[cfg(any(target_os = "macos", windows))]
pub use widget_feed::feed_widgets;
#[cfg(not(target_os = "macos"))]
pub use readback::{CpuFrame, Frame, Preview, frame_rgba, frame_size, sample_ycbcr};
#[cfg(windows)]
pub use readback::frame_of_buffer;
