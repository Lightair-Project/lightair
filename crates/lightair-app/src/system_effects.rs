//! macOS's own camera effects (Control Center › Video Effects): Portrait blurs the background,
//! Backgrounds (macOS 15) replaces it. They run inside the camera pipeline before frames reach
//! Lightair, so they cost Lightair nothing. The user turns them on for each app; Lightair can
//! open that panel and read whether they're on.

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
