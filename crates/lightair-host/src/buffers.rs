//! BGRA IOSurface-backed pixel buffers: what Lightair's pictures travel in between captures,
//! providers, the compositor and the encoders (macOS).

use std::ffi::c_void;
use std::ptr::{self, NonNull};

use objc2_core_foundation::{CFBoolean, CFDictionary, CFRetained, CFString, CFType};
use objc2_core_video::{
    CVPixelBuffer, CVPixelBufferCreate, CVPixelBufferGetIOSurface, kCVPixelBufferIOSurfacePropertiesKey,
    kCVPixelBufferMetalCompatibilityKey, kCVPixelFormatType_32BGRA, kCVReturnSuccess,
};

/// A picture: a BGRA, IOSurface-backed pixel buffer, reference counted.
pub type Buffer = CFRetained<CVPixelBuffer>;

/// A BGRA, IOSurface-backed pixel buffer (Metal and OpenGL can use its surface).
pub fn bgra_buffer(width: u32, height: u32) -> Option<Buffer> {
    let empty: CFRetained<CFDictionary<CFString, CFType>> = CFDictionary::from_slices(&[], &[]);
    // SAFETY: CoreVideo's constant keys, read only.
    let keys: [&CFString; 2] = unsafe { [kCVPixelBufferIOSurfacePropertiesKey, kCVPixelBufferMetalCompatibilityKey] };
    let values: [&CFType; 2] = [empty.as_ref(), CFBoolean::new(true).as_ref()];
    let attrs = CFDictionary::from_slices(&keys, &values);
    let mut out: *mut CVPixelBuffer = ptr::null_mut();
    // SAFETY: `out` is a valid place for the new buffer; the attributes outlive the call.
    let rc = unsafe {
        CVPixelBufferCreate(
            None,
            width as usize,
            height as usize,
            kCVPixelFormatType_32BGRA,
            Some(attrs.as_opaque()),
            NonNull::from(&mut out),
        )
    };
    if rc != kCVReturnSuccess || out.is_null() {
        tracing::error!(rc, width, height, "CVPixelBufferCreate failed");
        return None;
    }
    // SAFETY: CVPixelBufferCreate returned a buffer we own one reference to.
    Some(unsafe { CFRetained::from_raw(NonNull::new_unchecked(out)) })
}

#[link(name = "CoreVideo", kind = "framework")]
unsafe extern "C" {
    fn CVPixelBufferCreateWithIOSurface(
        allocator: *const c_void,
        surface: *mut c_void,
        attributes: *const c_void,
        out: *mut *mut CVPixelBuffer,
    ) -> i32;
}

/// A pixel buffer over `surface` (an `IOSurfaceRef`, BGRA), sharing its memory: what someone
/// else's picture is drawn as without a copy. The buffer keeps the surface alive, not in use.
///
/// # Safety
/// `surface` must be null or a live `IOSurfaceRef`.
pub unsafe fn over_surface(surface: *mut c_void) -> Option<Buffer> {
    if surface.is_null() {
        return None;
    }
    let mut out: *mut CVPixelBuffer = ptr::null_mut();
    // SAFETY: `surface` is a live IOSurfaceRef (the caller's); `out` is a valid place for the
    // new buffer, which we own one reference to.
    let rc = unsafe { CVPixelBufferCreateWithIOSurface(ptr::null(), surface, ptr::null(), &mut out) };
    if rc != kCVReturnSuccess || out.is_null() {
        tracing::warn!(rc, "CVPixelBufferCreateWithIOSurface failed");
        return None;
    }
    Some(unsafe { CFRetained::from_raw(NonNull::new_unchecked(out)) })
}

/// The buffer's IOSurface, borrowed (as an `IOSurfaceRef`).
pub fn surface_of(buffer: &Buffer) -> Option<*mut c_void> {
    let surface = CVPixelBufferGetIOSurface(Some(buffer))?;
    // the buffer keeps its surface alive; the pointer is used while the buffer is held
    Some(CFRetained::as_ptr(&surface).as_ptr().cast())
}

/// A new buffer holding `rgba` (`width` × `height`, rows packed), its colours as they are
/// (opaque pictures, or premultiplied ones).
pub fn from_rgba(rgba: &[u8], width: u32, height: u32) -> Option<Buffer> {
    use objc2_core_video::{
        CVPixelBufferGetBaseAddress, CVPixelBufferGetBytesPerRow, CVPixelBufferLockBaseAddress, CVPixelBufferLockFlags,
        CVPixelBufferUnlockBaseAddress,
    };
    let (w, h) = (width as usize, height as usize);
    if rgba.len() < w * h * 4 {
        return None;
    }
    let buffer = bgra_buffer(width, height)?;
    // SAFETY: the buffer is locked while its bytes are written, `h` rows of `stride` bytes
    unsafe { CVPixelBufferLockBaseAddress(&buffer, CVPixelBufferLockFlags(0)) };
    let stride = CVPixelBufferGetBytesPerRow(&buffer);
    let base = CVPixelBufferGetBaseAddress(&buffer) as *mut u8;
    if !base.is_null() {
        for y in 0..h {
            let row = unsafe { std::slice::from_raw_parts_mut(base.add(y * stride), w * 4) };
            let from = &rgba[y * w * 4..(y + 1) * w * 4];
            for (out, px) in row.as_chunks_mut::<4>().0.iter_mut().zip(from.as_chunks::<4>().0) {
                *out = [px[2], px[1], px[0], px[3]];
            }
        }
    }
    unsafe { CVPixelBufferUnlockBaseAddress(&buffer, CVPixelBufferLockFlags(0)) };
    (!base.is_null()).then_some(buffer)
}
