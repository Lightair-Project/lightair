//! NDI's libobs side (the runtime itself is `lightair_host::ndi`, re-exported here): the
//! `lightair_ndi` source, a shell over a [`Receiver`] that also hands libobs its pictures and
//! the sound no track takes; and sending the program from libobs's picture and mixes when
//! Lightair's own outputs are off (from Lightair's compositor and mixer otherwise,
//! [`OwnSender`]).

use std::ffi::{CStr, CString, c_char, c_void};
use std::sync::{Arc, Mutex, OnceLock};

pub use lightair_host::ndi::*;

use crate::sys;

// ---------------------------------------------------------------------------------------------
// Receiving: the source

struct Shell {
    source: *mut sys::obs_source_t,
    receiver: Option<Receiver>,
}

/// Where a receiver hands libobs its pictures and sound.
struct ToLibobs(usize);

impl Elsewhere for ToLibobs {
    fn video(&self, picture: &Picture) {
        let mut frame: sys::obs_source_frame = unsafe { std::mem::zeroed() };
        frame.data[0] = picture.data.as_ptr() as *mut u8;
        frame.linesize[0] = picture.stride as u32;
        frame.width = picture.width;
        frame.height = picture.height;
        frame.format = sys::VIDEO_FORMAT_BGRA;
        frame.full_range = true;
        frame.timestamp = crate::clock::now_ns();
        // (libobs copies it)
        unsafe { sys::obs_source_output_video(self.0 as *mut sys::obs_source_t, &frame) };
    }

    fn audio(&self, planar: &[f32], stride: usize, frames: usize, channels: usize, rate: u32) {
        let mut out: sys::obs_source_audio = unsafe { std::mem::zeroed() };
        for c in 0..channels.min(8) {
            out.data[c] = planar[c * stride..].as_ptr() as *const u8;
        }
        out.frames = frames as u32;
        out.speakers = match channels {
            1 => sys::SPEAKERS_MONO,
            2 => sys::SPEAKERS_STEREO,
            3 => sys::SPEAKERS_2POINT1,
            4 => sys::SPEAKERS_4POINT0,
            5 => sys::SPEAKERS_4POINT1,
            6 => sys::SPEAKERS_5POINT1,
            _ => sys::SPEAKERS_7POINT1,
        };
        out.format = sys::AUDIO_FORMAT_FLOAT_PLANAR;
        out.samples_per_sec = rate;
        out.timestamp = crate::clock::now_ns();
        unsafe { sys::obs_source_output_audio(self.0 as *mut sys::obs_source_t, &out) };
    }
}

unsafe extern "C" fn get_name(_: *mut c_void) -> *const c_char {
    c"NDI".as_ptr()
}

unsafe extern "C" fn create(settings: *mut sys::obs_data_t, source: *mut sys::obs_source_t) -> *mut c_void {
    let p = Box::into_raw(Box::new(Shell { source, receiver: None })) as *mut c_void;
    unsafe { update(p, settings) };
    p
}

unsafe extern "C" fn update(data: *mut c_void, settings: *mut sys::obs_data_t) {
    let shell = unsafe { &mut *(data as *mut Shell) };
    let p = unsafe { sys::obs_data_get_string(settings, c"ndi_name".as_ptr()) };
    let name = if p.is_null() { String::new() } else { unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned() };
    if shell.receiver.as_ref().is_some_and(|r| r.name() == name) {
        return;
    }
    // the old one stops first (its sound slot goes with it)
    shell.receiver = None;
    // a document source is known by its id (the compositor's tap and the mixer find it by that)
    let id = crate::obs::doc_id(shell.source).unwrap_or_default();
    let elsewhere: Arc<dyn Elsewhere> = Arc::new(ToLibobs(shell.source as usize));
    shell.receiver = Some(Receiver::start(id, &name, Some(elsewhere)));
}

unsafe extern "C" fn destroy(data: *mut c_void) {
    drop(unsafe { Box::from_raw(data as *mut Shell) });
}

// ---------------------------------------------------------------------------------------------
// Sending the program

struct Sender {
    send: Ptr,
    /// Planar audio laid out for NDI (its channels one after another).
    audio: Vec<f32>,
    fps: (i32, i32),
    rate: u32,
    /// The libobs output whose frames are sent (an `obs_output_t`, as a number); 0 when they come
    /// from libobs's main canvas directly.
    output: usize,
}

unsafe impl Send for Sender {}

fn sender() -> &'static Mutex<Option<Box<Sender>>> {
    static S: OnceLock<Mutex<Option<Box<Sender>>>> = OnceLock::new();
    S.get_or_init(|| Mutex::new(None))
}

/// The program sent from Lightair's compositor and mixer (macOS, Lightair's own outputs).
#[cfg(target_os = "macos")]
static OWN: Mutex<Option<OwnSender>> = Mutex::new(None);

unsafe extern "C" fn on_video(param: *mut c_void, frame: *mut sys::video_data) {
    let _ = param;
    let Some(l) = lib() else { return };
    let Ok(guard) = sender().try_lock() else { return };
    let (Some(s), Some(f)) = (guard.as_ref(), unsafe { frame.as_ref() }) else { return };
    let mut ovi: sys::obs_video_info = unsafe { std::mem::zeroed() };
    if !unsafe { sys::obs_get_video_info(&mut ovi) } {
        return;
    }
    let v = VideoFrame {
        xres: ovi.output_width as i32,
        yres: ovi.output_height as i32,
        four_cc: FOURCC_BGRA,
        frame_rate_n: s.fps.0,
        frame_rate_d: s.fps.1,
        picture_aspect_ratio: 0.0,
        frame_format_type: PROGRESSIVE,
        timecode: TIMECODE_SYNTHESIZE,
        p_data: f.data[0],
        line_stride_in_bytes: f.linesize[0] as i32,
        p_metadata: std::ptr::null(),
        timestamp: 0,
    };
    unsafe { (l.send_send_video_v2)(s.send, &v) };
}

unsafe extern "C" fn on_audio(param: *mut c_void, _mix: usize, data: *mut sys::audio_data) {
    let _ = param;
    let Some(l) = lib() else { return };
    let Ok(mut guard) = sender().try_lock() else { return };
    let (Some(s), Some(a)) = (guard.as_mut(), unsafe { data.as_ref() }) else { return };
    let n = a.frames as usize;
    if n == 0 || a.data[0].is_null() || a.data[1].is_null() {
        return;
    }
    s.audio.clear();
    for c in 0..2 {
        s.audio.extend_from_slice(unsafe { std::slice::from_raw_parts(a.data[c] as *const f32, n) });
    }
    let frame = AudioFrame {
        sample_rate: s.rate as i32,
        no_channels: 2,
        no_samples: n as i32,
        timecode: TIMECODE_SYNTHESIZE,
        p_data: s.audio.as_mut_ptr(),
        channel_stride_in_bytes: (n * 4) as i32,
        p_metadata: std::ptr::null(),
        timestamp: 0,
    };
    unsafe { (l.send_send_audio_v2)(s.send, &frame) };
}

/// Sends the program out as the NDI source `name` ("" stops it). Call on the engine thread.
pub fn set_output(name: &str) -> Result<(), String> {
    #[cfg(target_os = "macos")]
    {
        let own = OWN.lock().ok().and_then(|mut o| o.take());
        drop(own);
    }
    let running = sender().lock().ok().and_then(|s| s.as_ref().map(|s| s.output));
    if let Some(output) = running {
        unsafe {
            #[cfg(target_os = "macos")]
            if output != 0 {
                stop_output(output as *mut sys::obs_output_t);
            }
            if output == 0 {
                sys::obs_remove_raw_video_callback(Some(on_video), std::ptr::null_mut());
            }
            sys::obs_remove_raw_audio_callback(0, Some(on_audio), std::ptr::null_mut());
        }
        if let Ok(mut s) = sender().lock()
            && let Some(old) = s.take()
            && let Some(l) = lib()
        {
            unsafe { (l.send_destroy)(old.send) };
        }
    }
    if name.is_empty() {
        return Ok(());
    }
    // Lightair's own outputs: the primary canvas's compositor frames and the mixer's Program
    #[cfg(target_os = "macos")]
    if crate::own_outputs::enabled()
        && let Some(canvas) = crate::program_source::canvas_of_video(None)
    {
        match OwnSender::start(name, canvas) {
            Ok(own) => {
                if let Ok(mut o) = OWN.lock() {
                    *o = Some(own);
                }
                return Ok(());
            }
            Err(e) => tracing::warn!(%e, "NDI: sending libobs's picture"),
        }
    }
    let l = lib().ok_or("NDI isn't installed on this computer (NDI Tools from ndi.video brings it)")?;
    let cname = CString::new(name).map_err(|e| e.to_string())?;
    let create =
        SendCreate { p_ndi_name: cname.as_ptr(), p_groups: std::ptr::null(), clock_video: false, clock_audio: false };
    let send = unsafe { (l.send_create)(&create) };
    if send.is_null() {
        return Err("NDI couldn't start sending".into());
    }
    let mut ovi: sys::obs_video_info = unsafe { std::mem::zeroed() };
    let fps = if unsafe { sys::obs_get_video_info(&mut ovi) } {
        (ovi.fps_num as i32, ovi.fps_den.max(1) as i32)
    } else {
        (60, 1)
    };
    let mut ai = sys::obs_audio_info::default();
    let rate = if unsafe { sys::obs_get_audio_info(&mut ai) } { ai.samples_per_sec } else { 48_000 };
    let conversion = sys::video_scale_info {
        format: sys::VIDEO_FORMAT_BGRA,
        width: ovi.output_width,
        height: ovi.output_height,
        range: sys::VIDEO_RANGE_FULL,
        colorspace: sys::VIDEO_CS_709,
    };
    if let Ok(mut s) = sender().lock() {
        *s = Some(Box::new(Sender { send, audio: Vec::new(), fps, rate, output: 0 }));
    }
    // the primary canvas's output canvas (Lightair's own compositor's picture) when there is one
    #[cfg(target_os = "macos")]
    let output = crate::program_source::video(None)
        .map_or(std::ptr::null_mut(), |video| unsafe { start_output(video, &conversion) });
    #[cfg(not(target_os = "macos"))]
    let output: *mut sys::obs_output_t = std::ptr::null_mut();
    if let Ok(mut s) = sender().lock()
        && let Some(s) = s.as_mut()
    {
        s.output = output as usize;
    }
    unsafe {
        if output.is_null() {
            sys::obs_add_raw_video_callback(&conversion, Some(on_video), std::ptr::null_mut());
        }
        sys::obs_add_raw_audio_callback(0, std::ptr::null(), Some(on_audio), std::ptr::null_mut());
    }
    tracing::info!(name, "sending the program over NDI");
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// macOS, with `LIGHTAIR_ENGINE=own` and `LIGHTAIR_OUTPUTS=libobs`: the output that sends the
// primary canvas's output canvas
// (Lightair's own compositor's picture) instead of libobs's main canvas

#[cfg(target_os = "macos")]
const OUTPUT_ID: &CStr = c"lightair_ndi_output";

#[cfg(target_os = "macos")]
unsafe extern "C" fn out_name(_: *mut c_void) -> *const c_char {
    c"NDI".as_ptr()
}

#[cfg(target_os = "macos")]
unsafe extern "C" fn out_create(_: *mut sys::obs_data_t, output: *mut sys::obs_output_t) -> *mut c_void {
    Box::into_raw(Box::new(output as usize)) as *mut c_void
}

#[cfg(target_os = "macos")]
unsafe extern "C" fn out_destroy(data: *mut c_void) {
    drop(unsafe { Box::from_raw(data as *mut usize) });
}

#[cfg(target_os = "macos")]
unsafe extern "C" fn out_start(data: *mut c_void) -> bool {
    let output = unsafe { *(data as *const usize) } as *mut sys::obs_output_t;
    unsafe { sys::obs_output_can_begin_data_capture(output, 0) && sys::obs_output_begin_data_capture(output, 0) }
}

#[cfg(target_os = "macos")]
unsafe extern "C" fn out_stop(data: *mut c_void, _: u64) {
    let output = unsafe { *(data as *const usize) } as *mut sys::obs_output_t;
    unsafe { sys::obs_output_end_data_capture(output) };
}

/// An output sending `video`'s frames, converted to `conversion`, started (null if it couldn't).
#[cfg(target_os = "macos")]
unsafe fn start_output(video: *mut sys::video_t, conversion: &sys::video_scale_info) -> *mut sys::obs_output_t {
    unsafe {
        let output =
            sys::obs_output_create(OUTPUT_ID.as_ptr(), c"NDI".as_ptr(), std::ptr::null_mut(), std::ptr::null_mut());
        if output.is_null() {
            return output;
        }
        sys::obs_output_set_media(output, video, sys::obs_get_audio());
        sys::obs_output_set_video_conversion(output, conversion);
        if !sys::obs_output_start(output) {
            tracing::warn!("NDI: the output didn't start; sending libobs's own picture");
            sys::obs_output_release(output);
            return std::ptr::null_mut();
        }
        output
    }
}

#[cfg(target_os = "macos")]
unsafe fn stop_output(output: *mut sys::obs_output_t) {
    unsafe {
        sys::obs_output_stop(output);
        sys::obs_output_release(output);
    }
}

/// Registers the NDI source and sending output with libobs (once; after startup).
pub fn register() {
    #[cfg(target_os = "macos")]
    static OUTPUT: std::sync::Once = std::sync::Once::new();
    #[cfg(target_os = "macos")]
    OUTPUT.call_once(|| {
        if !crate::bridge::own() {
            return;
        }
        let info = sys::obs_output_info {
            id: OUTPUT_ID.as_ptr(),
            flags: sys::OBS_OUTPUT_VIDEO,
            get_name: Some(out_name),
            create: Some(out_create),
            destroy: Some(out_destroy),
            start: Some(out_start),
            stop: Some(out_stop),
            raw_video: Some(on_video),
            ..Default::default()
        };
        unsafe { sys::obs_register_output_s(&info, std::mem::size_of::<sys::obs_output_info>()) };
    });
    static DONE: std::sync::Once = std::sync::Once::new();
    DONE.call_once(|| unsafe {
        let mut info: sys::obs_source_info = std::mem::zeroed();
        info.id = c"lightair_ndi".as_ptr();
        info.type_ = sys::OBS_SOURCE_TYPE_INPUT;
        info.output_flags = sys::OBS_SOURCE_ASYNC_VIDEO | sys::OBS_SOURCE_AUDIO | sys::OBS_SOURCE_DO_NOT_DUPLICATE;
        info.get_name = Some(get_name);
        info.create = Some(create);
        info.destroy = Some(destroy);
        info.update = Some(update);
        sys::obs_register_source_s(&info, std::mem::size_of::<sys::obs_source_info>());
    });
}
