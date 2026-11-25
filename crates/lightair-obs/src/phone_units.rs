//! The private frames source phone decoders draw into (libobs), and the shared units
//! (`lightair_host::phone_units`, re-exported).

use std::ffi::c_void;

use crate::sys;

// ---------------------------------------------------------------- the frames source

pub(crate) const FRAMES_ID: &std::ffi::CStr = c"lightair_phone_frames";

unsafe extern "C" fn frames_name(_: *mut c_void) -> *const std::ffi::c_char {
    c"Phone camera frames".as_ptr()
}

unsafe extern "C" fn frames_create(_: *mut sys::obs_data_t, _: *mut sys::obs_source_t) -> *mut c_void {
    Box::into_raw(Box::new(0u8)) as *mut c_void
}

unsafe extern "C" fn frames_destroy(data: *mut c_void) {
    drop(unsafe { Box::from_raw(data as *mut u8) });
}

/// Registers the private async source decoded frames go into (once).
pub(crate) fn register() {
    static DONE: std::sync::Once = std::sync::Once::new();
    DONE.call_once(|| unsafe {
        let mut info: sys::obs_source_info = std::mem::zeroed();
        info.id = FRAMES_ID.as_ptr();
        info.type_ = sys::OBS_SOURCE_TYPE_INPUT;
        info.output_flags = sys::OBS_SOURCE_ASYNC_VIDEO | sys::OBS_SOURCE_AUDIO | sys::OBS_SOURCE_DO_NOT_DUPLICATE;
        info.get_name = Some(frames_name);
        info.create = Some(frames_create);
        info.destroy = Some(frames_destroy);
        sys::obs_register_source_s(&info, std::mem::size_of::<sys::obs_source_info>());
    });
}
