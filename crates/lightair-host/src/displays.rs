//! Display identifiers as screen captures name them.

/// UUID of the main display, as used by `screen_capture`'s `display_uuid` setting.
#[cfg(target_os = "macos")]
pub fn main_display_uuid() -> Option<String> {
    use std::ffi::{c_char, c_void};

    type CFTypeRef = *const c_void;
    #[link(name = "CoreGraphics", kind = "framework")]
    unsafe extern "C" {
        fn CGMainDisplayID() -> u32;
    }
    #[link(name = "ColorSync", kind = "framework")]
    unsafe extern "C" {
        fn CGDisplayCreateUUIDFromDisplayID(display: u32) -> CFTypeRef;
    }
    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        fn CFUUIDCreateString(alloc: CFTypeRef, uuid: CFTypeRef) -> CFTypeRef;
        fn CFStringGetCString(s: CFTypeRef, buf: *mut c_char, size: isize, encoding: u32) -> bool;
        fn CFRelease(cf: CFTypeRef);
    }
    const UTF8: u32 = 0x0800_0100;

    unsafe {
        let uuid = CGDisplayCreateUUIDFromDisplayID(CGMainDisplayID());
        if uuid.is_null() {
            return None;
        }
        let text = CFUUIDCreateString(std::ptr::null(), uuid);
        CFRelease(uuid);
        if text.is_null() {
            return None;
        }
        let mut buf = [0 as c_char; 64];
        let ok = CFStringGetCString(text, buf.as_mut_ptr(), buf.len() as isize, UTF8);
        CFRelease(text);
        ok.then(|| std::ffi::CStr::from_ptr(buf.as_ptr()).to_string_lossy().into_owned())
    }
}

#[cfg(not(target_os = "macos"))]
pub fn main_display_uuid() -> Option<String> {
    None
}
