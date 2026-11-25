//! The portable preview path (Windows today; any platform without a zero-copy one): frames are
//! BGRA pixels in memory, shown by the UI as images ([`CpuFrame`]). They come from Lightair's
//! own compositor (`own`); on Windows the window's preview is presented into a swap chain
//! instead (`Layer`).

use std::sync::Arc;

mod own;
pub use own::Preview;
#[cfg(windows)]
pub use own::frame_of_buffer;
// what Lightair's own compositor feeds
#[cfg(windows)]
pub(crate) use own::{SceneView, Shared};

/// A finished preview frame: BGRA pixels, top row first.
pub struct CpuFrame {
    pub width: usize,
    pub height: usize,
    /// `width * 4` bytes per row.
    pub bgra: Vec<u8>,
}

pub type Frame = Arc<CpuFrame>;

/// A frame's pixel size.
pub fn frame_size(frame: &Frame) -> (usize, usize) {
    (frame.width, frame.height)
}

/// A frame shrunk to at most `max_width` pixels wide, as RGBA (thumbnails).
pub fn frame_rgba(frame: &Frame, max_width: usize) -> Option<(usize, usize, Vec<u8>)> {
    let (w, h) = (frame.width, frame.height);
    if w == 0 || h == 0 {
        return None;
    }
    let step = w.div_ceil(max_width.max(1)).max(1);
    let (ow, oh) = (w / step, h / step);
    let mut rgba = Vec::with_capacity(ow * oh * 4);
    for oy in 0..oh {
        for ox in 0..ow {
            let i = (oy * step * w + ox * step) * 4;
            rgba.extend_from_slice(&[frame.bgra[i + 2], frame.bgra[i + 1], frame.bgra[i], 255]);
        }
    }
    Some((ow, oh, rgba))
}

/// The (Y, Cb, Cr) values (full-range BT.601, as the macOS path produces) at a point given as
/// fractions of the frame's size (diagnostics and tests).
pub fn sample_ycbcr(frame: &Frame, fx: f32, fy: f32) -> Option<(u8, u8, u8)> {
    let (w, h) = (frame.width, frame.height);
    if w == 0 || h == 0 {
        return None;
    }
    let (x, y) = (((w as f32 * fx) as usize).min(w - 1), ((h as f32 * fy) as usize).min(h - 1));
    let i = (y * w + x) * 4;
    let (b, g, r) = (frame.bgra[i] as f32, frame.bgra[i + 1] as f32, frame.bgra[i + 2] as f32);
    let yv = 0.299 * r + 0.587 * g + 0.114 * b;
    let cb = 128.0 + (b - yv) * 0.564;
    let cr = 128.0 + (r - yv) * 0.713;
    let q = |v: f32| v.round().clamp(0.0, 255.0) as u8;
    Some((q(yv), q(cb), q(cr)))
}
