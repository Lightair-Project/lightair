//! Where "Listen in headphones" plays (docs/10-audio.md, "Monitoring"): the audio monitoring
//! device. Call these on the engine thread.

use std::ffi::{CStr, CString, c_char, c_void};

use crate::sys;

/// Output devices to listen on: (name, id). The first is the system default ("default").
pub fn devices() -> Vec<(String, String)> {
    unsafe extern "C" fn each(param: *mut c_void, name: *const c_char, id: *const c_char) -> bool {
        let list = unsafe { &mut *(param as *mut Vec<(String, String)>) };
        let text = |p: *const c_char| {
            if p.is_null() { String::new() } else { unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned() }
        };
        list.push((text(name), text(id)));
        true
    }
    let mut out: Vec<(String, String)> = Vec::new();
    unsafe { sys::obs_enum_audio_monitoring_devices(Some(each), &mut out as *mut _ as *mut c_void) };
    if !out.iter().any(|(_, id)| id == "default") {
        out.insert(0, ("System default".into(), "default".into()));
    }
    out
}

/// Listens on this device (by id; "default" for the system's). Returns whether it took.
pub fn set(name: &str, id: &str) -> bool {
    let (Ok(name), Ok(id)) = (CString::new(name), CString::new(id)) else { return false };
    unsafe { sys::obs_set_audio_monitoring_device(name.as_ptr(), id.as_ptr()) }
}

/// The device listened on: (name, id).
pub fn current() -> (String, String) {
    let (mut name, mut id): (*const c_char, *const c_char) = (std::ptr::null(), std::ptr::null());
    unsafe { sys::obs_get_audio_monitoring_device(&mut name, &mut id) };
    let text = |p: *const c_char| {
        if p.is_null() { String::new() } else { unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned() }
    };
    (text(name), text(id))
}
