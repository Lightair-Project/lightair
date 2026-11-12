//! The eyedropper: macOS's colour sampler (NSColorSampler), the magnifying loupe that picks any
//! pixel on screen. Escape cancels. Main thread only.

use std::sync::mpsc;

use block2::RcBlock;
use objc2_app_kit::{NSColor, NSColorSampler, NSColorSpace};

/// Shows the loupe; the receiver gets the picked colour as 0xRRGGBB (sRGB), or `None` if the
/// pick was cancelled.
pub fn pick() -> mpsc::Receiver<Option<u32>> {
    let (tx, rx) = mpsc::channel();
    let handler = RcBlock::new(move |color: *mut NSColor| {
        let picked = unsafe { color.as_ref() }.and_then(|c| {
            let srgb = c.colorUsingColorSpace(&NSColorSpace::sRGBColorSpace())?;
            let ch = |v: f64| (v.clamp(0.0, 1.0) * 255.0).round() as u32;
            Some((ch(srgb.redComponent()) << 16) | (ch(srgb.greenComponent()) << 8) | ch(srgb.blueComponent()))
        });
        let _ = tx.send(picked);
    });
    let sampler = NSColorSampler::new();
    unsafe { sampler.showSamplerWithSelectionHandler(&handler) };
    rx
}
