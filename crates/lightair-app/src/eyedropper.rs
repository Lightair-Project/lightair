//! The eyedropper: picks any pixel on screen (`lightair_platform::pick_color`: macOS's colour
//! sampler, Lightair's own loupe on Windows). Escape cancels. Main thread only.

use std::sync::mpsc;

/// Whether this platform has an eyedropper.
pub const AVAILABLE: bool = cfg!(any(target_os = "macos", windows));

/// Shows the loupe; the receiver gets the picked colour as 0xRRGGBB (sRGB), or `None` if the
/// pick was cancelled.
pub fn pick() -> mpsc::Receiver<Option<u32>> {
    lightair_platform::pick_color::pick()
}
