//! Windows: faces in a picture, found by Windows' own face detector
//! (`Windows.Media.FaceAnalysis.FaceDetector`, no model of Lightair's), as `vision.rs` finds them
//! with Apple's Vision on macOS: each face's box, 0–1 from the top left. For Face blur and
//! Auto-frame, on a small copy of the picture.

use std::cell::RefCell;

use windows::Graphics::Imaging::{BitmapPixelFormat, SoftwareBitmap};
use windows::Media::FaceAnalysis::FaceDetector;
use windows::Security::Cryptography::CryptographicBuffer;
use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx};

thread_local! {
    /// The detector this thread made (making one loads Windows' model: once).
    static DETECTOR: RefCell<Option<FaceDetector>> = const { RefCell::new(None) };
}

/// The faces in `bgra` (`width` × `height`, rows `stride` bytes apart): each one's box (x, y,
/// width, height, 0–1 from the top left). None when Windows has no face detector.
pub fn faces(bgra: &mut [u8], width: usize, height: usize, stride: usize) -> Vec<[f32; 4]> {
    if width < 16 || height < 16 || bgra.len() < stride * (height - 1) + width * 4 {
        return Vec::new();
    }
    DETECTOR.with(|d| {
        let mut d = d.borrow_mut();
        if d.is_none() {
            // (S_FALSE and RPC_E_CHANGED_MODE both leave the thread able to call WinRT)
            let _ = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
            *d = FaceDetector::CreateAsync().and_then(|op| op.join()).inspect_err(|e| {
                tracing::warn!(%e, "no face detector on this PC: Face blur and Auto-frame don't find faces");
            }).ok();
        }
        let Some(detector) = d.as_ref() else { return Vec::new() };
        detect(detector, bgra, width, height, stride).unwrap_or_default()
    })
}

fn detect(detector: &FaceDetector, bgra: &[u8], w: usize, h: usize, stride: usize) -> windows::core::Result<Vec<[f32; 4]>> {
    // the detector takes grey (or NV12): BT.601 luma of the BGRA
    let mut grey = Vec::with_capacity(w * h);
    for y in 0..h {
        let row = &bgra[y * stride..y * stride + w * 4];
        grey.extend(row.as_chunks::<4>().0.iter().map(|[b, g, r, _]| {
            ((*r as u32 * 77 + *g as u32 * 150 + *b as u32 * 29) >> 8) as u8
        }));
    }
    let buffer = CryptographicBuffer::CreateFromByteArray(&grey)?;
    let bitmap = SoftwareBitmap::CreateCopyFromBuffer(&buffer, BitmapPixelFormat::Gray8, w as i32, h as i32)?;
    let found = detector.DetectFacesAsync(&bitmap)?.join()?;
    let (fw, fh) = (w as f32, h as f32);
    let mut out = Vec::new();
    for face in found {
        let b = face.FaceBox()?;
        out.push([b.X as f32 / fw, b.Y as f32 / fh, b.Width as f32 / fw, b.Height as f32 / fh]);
    }
    Ok(out)
}
