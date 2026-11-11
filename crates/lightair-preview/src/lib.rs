//! Zero-copy canvas preview (docs/04-engine-libobs.md#preview-rendering).
//!
//! libobs renders the program canvas into a small pool of IOSurface-backed textures on its
//! graphics thread; the UI takes the newest finished buffer as a `CVPixelBuffer` and hands it
//! to GPUI's `surface()` element. No pixels are copied on the CPU.
//!
//! Threading: the render callback runs on libobs's graphics thread. It never waits on the UI;
//! the only shared lock is held for a pointer swap.

#[cfg(target_os = "macos")]
mod mac;

#[cfg(target_os = "macos")]
pub use mac::{Frame, Preview, sample_ycbcr};
