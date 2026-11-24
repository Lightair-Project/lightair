//! Every face in a picture, by Apple's Vision (macOS): for Face blur, which hides them all
//! (`autoframe`). The picture is the small copy the face filters already make.

use std::ffi::c_void;

use objc2::rc::autoreleasepool;
use objc2::runtime::{AnyClass, AnyObject};
use objc2::{Encode, Encoding, RefEncode, msg_send};

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct CGPoint {
    x: f64,
    y: f64,
}
// SAFETY: CGPoint's Objective-C encoding.
unsafe impl Encode for CGPoint {
    const ENCODING: Encoding = Encoding::Struct("CGPoint", &[f64::ENCODING, f64::ENCODING]);
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct CGSize {
    width: f64,
    height: f64,
}
// SAFETY: CGSize's Objective-C encoding.
unsafe impl Encode for CGSize {
    const ENCODING: Encoding = Encoding::Struct("CGSize", &[f64::ENCODING, f64::ENCODING]);
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct CGRect {
    origin: CGPoint,
    size: CGSize,
}
// SAFETY: CGRect's Objective-C encoding.
unsafe impl Encode for CGRect {
    const ENCODING: Encoding = Encoding::Struct("CGRect", &[CGPoint::ENCODING, CGSize::ENCODING]);
}

/// A `CVPixelBufferRef`'s target.
#[repr(C)]
struct CVBuffer {
    _p: [u8; 0],
}
// SAFETY: CVPixelBufferRef's Objective-C encoding (^{__CVBuffer=}).
unsafe impl RefEncode for CVBuffer {
    const ENCODING_REF: Encoding = Encoding::Pointer(&Encoding::Struct("__CVBuffer", &[]));
}

#[link(name = "Vision", kind = "framework")]
unsafe extern "C" {}

#[link(name = "CoreVideo", kind = "framework")]
unsafe extern "C" {
    fn CVPixelBufferCreateWithBytes(
        allocator: *const c_void,
        width: usize,
        height: usize,
        format: u32,
        base: *mut c_void,
        bytes_per_row: usize,
        release: *const c_void,
        release_ref: *mut c_void,
        attributes: *const c_void,
        out: *mut *mut CVBuffer,
    ) -> i32;
    fn CVPixelBufferRelease(buffer: *mut CVBuffer);
}

/// kCVPixelFormatType_32BGRA.
const BGRA: u32 = u32::from_be_bytes(*b"BGRA");

/// The faces in a BGRA picture: (x, y, width, height) each, 0..1 from its top left.
pub fn faces(bgra: &mut [u8], width: usize, height: usize, stride: usize) -> Vec<[f32; 4]> {
    if width == 0 || height == 0 || bgra.len() < stride * height {
        return Vec::new();
    }
    let (Some(request_class), Some(handler_class), Some(array_class), Some(dict_class)) = (
        AnyClass::get(c"VNDetectFaceRectanglesRequest"),
        AnyClass::get(c"VNImageRequestHandler"),
        AnyClass::get(c"NSArray"),
        AnyClass::get(c"NSDictionary"),
    ) else {
        return Vec::new();
    };
    let mut pb: *mut CVBuffer = std::ptr::null_mut();
    let made = unsafe {
        CVPixelBufferCreateWithBytes(
            std::ptr::null(),
            width,
            height,
            BGRA,
            bgra.as_mut_ptr() as *mut c_void,
            stride,
            std::ptr::null(),
            std::ptr::null_mut(),
            std::ptr::null(),
            &mut pb,
        )
    };
    if made != 0 || pb.is_null() {
        return Vec::new();
    }
    let out = autoreleasepool(|_| unsafe {
        let request: *mut AnyObject = msg_send![request_class, new];
        let options: *mut AnyObject = msg_send![dict_class, dictionary];
        let handler: *mut AnyObject = msg_send![handler_class, alloc];
        let handler: *mut AnyObject = msg_send![handler, initWithCVPixelBuffer: pb, options: options];
        let mut found = Vec::new();
        if !request.is_null() && !handler.is_null() {
            let requests: *mut AnyObject = msg_send![array_class, arrayWithObject: request];
            let mut error: *mut AnyObject = std::ptr::null_mut();
            let ok: bool = msg_send![handler, performRequests: requests, error: &mut error];
            if ok {
                let results: *mut AnyObject = msg_send![request, results];
                let n: usize = if results.is_null() { 0 } else { msg_send![results, count] };
                for i in 0..n.min(16) {
                    let face: *mut AnyObject = msg_send![results, objectAtIndex: i];
                    let r: CGRect = msg_send![face, boundingBox];
                    // Vision's origin is the bottom left
                    let (x, w, h) = (r.origin.x as f32, r.size.width as f32, r.size.height as f32);
                    let y = 1.0 - r.origin.y as f32 - h;
                    found.push([x, y, w, h]);
                }
            }
        }
        if !handler.is_null() {
            let _: () = msg_send![handler, release];
        }
        if !request.is_null() {
            let _: () = msg_send![request, release];
        }
        found
    });
    unsafe { CVPixelBufferRelease(pb) };
    out
}
