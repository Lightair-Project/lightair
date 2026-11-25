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

/// The buffer's IOSurface, borrowed (as an `IOSurfaceRef`).
pub fn surface_of(buffer: &Buffer) -> Option<*mut c_void> {
    let surface = CVPixelBufferGetIOSurface(Some(buffer))?;
    // the buffer keeps its surface alive; the pointer is used while the buffer is held
    Some(CFRetained::as_ptr(&surface).as_ptr().cast())
}
