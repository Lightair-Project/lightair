//! macOS's own camera effects (Control Center › Video Effects): Portrait blurs the background,
//! Backgrounds (macOS 15) replaces it. They run inside the camera pipeline before frames reach
//! Lightair, so they cost Lightair nothing. The user turns them on for each app; Lightair can
//! open that panel and read whether they're on.
//!
//! The green screen: Background removal's "macOS" model keys out a pure green, which Lightair
//! makes macOS paint behind the person, for Lightair only (each app has its own Backgrounds
//! picture and switch). Control Center does this through AVCaptureDevice class methods that
//! aren't in the public headers (checked on macOS 27):
//! - `setBackgroundReplacementEnabled:`: the switch (so Lightair can turn it on itself);
//! - `backgroundReplacementURL` / `setBackgroundReplacementURL:`: a picture file. The camera
//!   service opens it itself and can only read Apple's own folders (a file of Lightair's falls
//!   back to the default blue), so it's only used to put the user's picture back;
//! - `setBackgroundReplacementPixelBuffer:`: the picture as pixels, which macOS paints when
//!   they're made like its own (420v, BT.709, 1920×1280, IOSurface). The green goes this way.
//!
//! Lightair remembers the picture it replaced and puts it back when no camera keys the green.

use std::ffi::{c_char, c_void};

#[link(name = "AVFoundation", kind = "framework")]
unsafe extern "C" {}

#[link(name = "objc")]
unsafe extern "C" {
    fn objc_getClass(name: *const c_char) -> *mut c_void;
    fn sel_registerName(name: *const c_char) -> *mut c_void;
    fn class_getClassMethod(class: *mut c_void, sel: *mut c_void) -> *mut c_void;
    fn objc_msgSend();
}

/// A class method of AVCaptureDevice, if this macOS has it.
fn class_method(name: &std::ffi::CStr) -> Option<(*mut c_void, *mut c_void)> {
    unsafe {
        let class = objc_getClass(c"AVCaptureDevice".as_ptr());
        let sel = sel_registerName(name.as_ptr());
        (!class.is_null() && !class_getClassMethod(class, sel).is_null()).then_some((class, sel))
    }
}

fn class_bool(name: &std::ffi::CStr) -> Option<bool> {
    let (class, sel) = class_method(name)?;
    let send: extern "C" fn(*mut c_void, *mut c_void) -> i8 =
        unsafe { std::mem::transmute(objc_msgSend as unsafe extern "C" fn()) };
    Some(send(class, sel) != 0)
}

/// Opens Control Center's Video Effects (on the main thread).
pub fn open_video_effects() {
    if let Some((class, sel)) = class_method(c"showSystemUserInterface:") {
        let send: extern "C" fn(*mut c_void, *mut c_void, isize) =
            unsafe { std::mem::transmute(objc_msgSend as unsafe extern "C" fn()) };
        send(class, sel, 1); // AVCaptureSystemUserInterfaceVideoEffects
    }
}

/// Whether Portrait (background blur) is on in Control Center.
pub fn portrait_on() -> Option<bool> {
    class_bool(c"isPortraitEffectEnabled")
}

/// Whether Backgrounds (replacement) is on for Lightair; `None` before macOS 15.
pub fn backgrounds_on() -> Option<bool> {
    class_bool(c"isBackgroundReplacementEnabled")
}

/// The picture Backgrounds puts behind the person in Lightair (a file URL's path); `None` before
/// macOS 15 or where this macOS has no such method.
pub fn background_picture() -> Option<String> {
    use objc2::msg_send;
    use objc2::rc::Retained;
    use objc2::runtime::{AnyClass, AnyObject, Sel};
    let class = AnyClass::get(c"AVCaptureDevice")?;
    let sel = Sel::register(c"backgroundReplacementURL");
    class.metaclass().instance_method(sel)?;
    let url: Option<Retained<AnyObject>> = unsafe { msg_send![class, backgroundReplacementURL] };
    let path: Option<Retained<objc2_foundation::NSString>> = unsafe { msg_send![&*url?, path] };
    Some(path?.to_string())
}

/// Sets the picture Backgrounds puts behind the person in Lightair. False where it can't.
pub fn set_background_picture(path: &std::path::Path) -> bool {
    use objc2::msg_send;
    use objc2::rc::Retained;
    use objc2::runtime::{AnyClass, AnyObject, Sel};
    let (Some(class), Some(url_class)) = (AnyClass::get(c"AVCaptureDevice"), AnyClass::get(c"NSURL")) else { return false };
    if class.metaclass().instance_method(Sel::register(c"setBackgroundReplacementURL:")).is_none() {
        return false;
    }
    let s = objc2_foundation::NSString::from_str(&path.to_string_lossy());
    let url: Option<Retained<AnyObject>> = unsafe { msg_send![url_class, fileURLWithPath: &*s] };
    let Some(url) = url else { return false };
    let _: () = unsafe { msg_send![class, setBackgroundReplacementURL: &*url] };
    true
}

/// A camera's state for Backgrounds (by its unique id): whether macOS is replacing its
/// background now, and whether the format it runs in can (diagnostics).
pub fn camera_backgrounds(uid: &str) -> Option<(bool, bool)> {
    use objc2::msg_send;
    use objc2::rc::Retained;
    use objc2::runtime::{AnyClass, AnyObject, Bool};
    let class = AnyClass::get(c"AVCaptureDevice")?;
    let id = objc2_foundation::NSString::from_str(uid);
    let device: Option<Retained<AnyObject>> = unsafe { msg_send![class, deviceWithUniqueID: &*id] };
    let device = device?;
    let active: Bool = unsafe { msg_send![&*device, isBackgroundReplacementActive] };
    let format: Option<Retained<AnyObject>> = unsafe { msg_send![&*device, activeFormat] };
    let supported: Bool = match format {
        Some(f) => unsafe { msg_send![&*f, isBackgroundReplacementSupported] },
        None => Bool::NO,
    };
    Some((active.as_bool(), supported.as_bool()))
}

#[link(name = "CoreVideo", kind = "framework")]
unsafe extern "C" {
    fn CVPixelBufferCreate(alloc: *const c_void, w: usize, h: usize, format: u32, attrs: *const c_void, out: *mut *mut c_void) -> i32;
    fn CVPixelBufferLockBaseAddress(pb: *mut c_void, flags: u64) -> i32;
    fn CVPixelBufferUnlockBaseAddress(pb: *mut c_void, flags: u64) -> i32;
    fn CVPixelBufferGetBaseAddressOfPlane(pb: *mut c_void, plane: usize) -> *mut u8;
    fn CVPixelBufferGetBytesPerRowOfPlane(pb: *mut c_void, plane: usize) -> usize;
    fn CVPixelBufferRelease(pb: *mut c_void);
    fn CVBufferSetAttachment(buffer: *mut c_void, key: *const c_void, value: *const c_void, mode: u32);
    static kCVPixelBufferIOSurfacePropertiesKey: *const c_void;
    static kCVImageBufferColorPrimariesKey: *const c_void;
    static kCVImageBufferColorPrimaries_ITU_R_709_2: *const c_void;
    static kCVImageBufferTransferFunctionKey: *const c_void;
    static kCVImageBufferTransferFunction_ITU_R_709_2: *const c_void;
    static kCVImageBufferYCbCrMatrixKey: *const c_void;
    static kCVImageBufferYCbCrMatrix_ITU_R_709_2: *const c_void;
}

/// Hands Backgrounds the picture as pixels rather than a file it would have to open: a solid
/// colour, made as macOS's own background buffers are (420v, BT.709, 1920×1280, in an
/// IOSurface the camera service shares). False where it can't.
pub fn set_background_colour(rgb: [u8; 3]) -> bool {
    use objc2::msg_send;
    use objc2::rc::Retained;
    use objc2::runtime::{AnyClass, AnyObject};
    let Some((class, sel)) = class_method(c"setBackgroundReplacementPixelBuffer:") else { return false };
    let Some(dict) = AnyClass::get(c"NSDictionary") else { return false };
    let (w, h) = (1920usize, 1280usize);
    // BT.709, video range
    let [r, g, b] = rgb.map(|v| v as f32 / 255.0);
    let y = 16.0 + 219.0 * (0.2126 * r + 0.7152 * g + 0.0722 * b);
    let cb = 128.0 + 224.0 * (-0.1146 * r - 0.3854 * g + 0.5 * b);
    let cr = 128.0 + 224.0 * (0.5 * r - 0.4542 * g - 0.0458 * b);
    let (y, cb, cr) = (y.round() as u8, cb.round() as u8, cr.round() as u8);
    unsafe {
        let empty: Retained<AnyObject> = msg_send![dict, dictionary];
        let key = kCVPixelBufferIOSurfacePropertiesKey as *const AnyObject;
        let attrs: Retained<AnyObject> = msg_send![dict, dictionaryWithObject: &*empty, forKey: key];
        let mut pb: *mut c_void = std::ptr::null_mut();
        // '420v'
        if CVPixelBufferCreate(std::ptr::null(), w, h, 0x3432_3076, Retained::as_ptr(&attrs).cast(), &mut pb) != 0 || pb.is_null() {
            return false;
        }
        CVPixelBufferLockBaseAddress(pb, 0);
        let (luma, luma_row) = (CVPixelBufferGetBaseAddressOfPlane(pb, 0), CVPixelBufferGetBytesPerRowOfPlane(pb, 0));
        for row in 0..h {
            std::ptr::write_bytes(luma.add(row * luma_row), y, w);
        }
        let (chroma, chroma_row) = (CVPixelBufferGetBaseAddressOfPlane(pb, 1), CVPixelBufferGetBytesPerRowOfPlane(pb, 1));
        for row in 0..h / 2 {
            let line = std::slice::from_raw_parts_mut(chroma.add(row * chroma_row), w);
            for px in line.as_chunks_mut::<2>().0 {
                *px = [cb, cr];
            }
        }
        CVPixelBufferUnlockBaseAddress(pb, 0);
        // kCVAttachmentMode_ShouldPropagate
        CVBufferSetAttachment(pb, kCVImageBufferColorPrimariesKey, kCVImageBufferColorPrimaries_ITU_R_709_2, 1);
        CVBufferSetAttachment(pb, kCVImageBufferTransferFunctionKey, kCVImageBufferTransferFunction_ITU_R_709_2, 1);
        CVBufferSetAttachment(pb, kCVImageBufferYCbCrMatrixKey, kCVImageBufferYCbCrMatrix_ITU_R_709_2, 1);
        let send: extern "C" fn(*mut c_void, *mut c_void, *mut c_void) = std::mem::transmute(objc_msgSend as unsafe extern "C" fn());
        send(class, sel, pb);
        CVPixelBufferRelease(pb);
    }
    true
}

/// Turns Backgrounds on or off for Lightair (as Video Effects' switch does). False where it can't.
pub fn set_backgrounds_on(on: bool) -> bool {
    let Some((class, sel)) = class_method(c"setBackgroundReplacementEnabled:") else { return false };
    let send: extern "C" fn(*mut c_void, *mut c_void, i8) =
        unsafe { std::mem::transmute(objc_msgSend as unsafe extern "C" fn()) };
    send(class, sel, i8::from(on));
    true
}

/// The green Background removal keys out: pure sRGB green.
pub const GREEN: [u8; 3] = [0, 255, 0];

#[cfg(test)]
mod tests {
    #[test]
    #[ignore = "depends on this Mac: needs macOS 15 (Backgrounds); cargo xtask test --machine"]
    fn this_macos_reports_its_effects() {
        // Present since macOS 12 / 15; the values are the user's.
        assert!(super::portrait_on().is_some());
        assert!(super::backgrounds_on().is_some());
    }
}
