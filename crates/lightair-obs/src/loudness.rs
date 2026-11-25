//! Loudness of the stream mix (`lightair_host::loudness`), metered from the mixer, or from
//! libobs's mix 0 with `LIGHTAIR_LEGACY_AUDIO=1`.

pub use lightair_host::loudness::{Loudness, LoudnessReading, reset_stream_loudness, stream_loudness};

/// libobs audio thread: the stream mix (planar float).
unsafe extern "C" fn on_mix(_: *mut std::ffi::c_void, _mix: usize, data: *mut libobs_sys::audio_data) {
    let Some(data) = (unsafe { data.as_ref() }) else { return };
    let frames = data.frames as usize;
    let planes: Vec<&[f32]> = (0..2)
        .filter(|c| !data.data[*c].is_null())
        .map(|c| unsafe { std::slice::from_raw_parts(data.data[c] as *const f32, frames) })
        .collect();
    lightair_host::loudness::push(&planes);
}

/// Starts metering the stream mix (Program, what the encoders send). Engine thread.
pub(crate) fn attach(sample_rate: u32, channels: usize) {
    if crate::audio_host::mixer_on() {
        lightair_host::loudness::attach(sample_rate, channels);
        return;
    }
    lightair_host::loudness::attach_pushed(sample_rate, channels);
    unsafe { libobs_sys::obs_add_raw_audio_callback(0, std::ptr::null(), Some(on_mix), std::ptr::null_mut()) };
}

pub(crate) fn detach() {
    if !lightair_host::loudness::detach() {
        unsafe { libobs_sys::obs_remove_raw_audio_callback(0, Some(on_mix), std::ptr::null_mut()) };
    }
}
