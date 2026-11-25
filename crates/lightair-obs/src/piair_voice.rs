//! PiAir's voice as a libobs audio source (`lightair_piair_voice`): a shell over
//! `lightair_host::piair_voice`'s [`Voice`] that outputs its blocks into libobs when no track of
//! the mixer takes them.

use std::ffi::{c_char, c_void};
use std::sync::Arc;

pub use lightair_host::piair_voice::*;

use crate::sys;

struct ToLibobs(usize);

impl Elsewhere for ToLibobs {
    fn block(&self, block: &[f32], timestamp_ns: u64) {
        let null = std::ptr::null::<u8>();
        let audio = sys::obs_source_audio {
            data: [block.as_ptr() as *const u8, block.as_ptr() as *const u8, null, null, null, null, null, null],
            frames: block.len() as u32,
            speakers: sys::SPEAKERS_STEREO,
            format: sys::AUDIO_FORMAT_FLOAT_PLANAR,
            samples_per_sec: RATE,
            timestamp: timestamp_ns,
        };
        unsafe { sys::obs_source_output_audio(self.0 as *mut sys::obs_source_t, &audio) };
    }
}

unsafe extern "C" fn get_name(_: *mut c_void) -> *const c_char {
    c"PiAir voice".as_ptr()
}

unsafe extern "C" fn create(_settings: *mut sys::obs_data_t, source: *mut sys::obs_source_t) -> *mut c_void {
    let id = crate::obs::doc_id(source).unwrap_or_default();
    let elsewhere: Arc<dyn Elsewhere> = Arc::new(ToLibobs(source as usize));
    Box::into_raw(Box::new(Voice::start(id, Some(elsewhere)))) as *mut c_void
}

unsafe extern "C" fn destroy(data: *mut c_void) {
    drop(unsafe { Box::from_raw(data as *mut Voice) });
}

pub fn register() {
    static DONE: std::sync::Once = std::sync::Once::new();
    DONE.call_once(|| unsafe {
        let mut info: sys::obs_source_info = std::mem::zeroed();
        info.id = c"lightair_piair_voice".as_ptr();
        info.type_ = sys::OBS_SOURCE_TYPE_INPUT;
        info.output_flags = sys::OBS_SOURCE_AUDIO | sys::OBS_SOURCE_DO_NOT_DUPLICATE;
        info.get_name = Some(get_name);
        info.create = Some(create);
        info.destroy = Some(destroy);
        sys::obs_register_source_s(&info, std::mem::size_of::<sys::obs_source_info>());
    });
}
