//! The RTSP server's libobs side: the `lightair_rtsp` output that hands libobs's encoded packets to
//! the server (`lightair_host::rtsp`), and starting it with libobs's encoders.

use std::ffi::{c_char, c_void};
use std::net::TcpListener;
use std::sync::{Mutex, OnceLock};

pub use lightair_host::rtsp::*;

use crate::sys;

unsafe extern "C" fn out_name(_: *mut c_void) -> *const c_char {
    c"RTSP server".as_ptr()
}

unsafe extern "C" fn out_create(_: *mut sys::obs_data_t, output: *mut sys::obs_output_t) -> *mut c_void {
    Box::into_raw(Box::new(output as usize)) as *mut c_void
}

unsafe extern "C" fn out_destroy(data: *mut c_void) {
    drop(unsafe { Box::from_raw(data as *mut usize) });
}

unsafe extern "C" fn out_start(data: *mut c_void) -> bool {
    let output = unsafe { *(data as *const usize) } as *mut sys::obs_output_t;
    unsafe {
        if !sys::obs_output_can_begin_data_capture(output, 0) || !sys::obs_output_initialize_encoders(output, 0) {
            return false;
        }
        // the parameter sets and the audio's config, for the description
        let venc = sys::obs_output_get_video_encoder(output);
        let aenc = sys::obs_output_get_audio_encoder(output, 0);
        if let Ok(mut s) = shared().lock() {
            let mut p: *mut u8 = std::ptr::null_mut();
            let mut n = 0usize;
            if sys::obs_encoder_get_extra_data(venc, &mut p, &mut n) && !p.is_null() {
                (s.sps, s.pps) = parameter_sets(std::slice::from_raw_parts(p, n));
            }
            if !aenc.is_null() && sys::obs_encoder_get_extra_data(aenc, &mut p, &mut n) && !p.is_null() {
                s.asc = std::slice::from_raw_parts(p, n).to_vec();
                s.audio = (sys::obs_encoder_get_sample_rate(aenc), 2);
            }
        }
        sys::obs_output_begin_data_capture(output, 0)
    }
}

unsafe extern "C" fn out_stop(data: *mut c_void, _: u64) {
    let output = unsafe { *(data as *const usize) } as *mut sys::obs_output_t;
    unsafe { sys::obs_output_end_data_capture(output) };
}

unsafe extern "C" fn out_packet(_: *mut c_void, packet: *mut sys::encoder_packet) {
    let Some(p) = (unsafe { packet.as_ref() }) else { return };
    if p.data.is_null() || p.size == 0 {
        return;
    }
    let data = unsafe { std::slice::from_raw_parts(p.data, p.size) };
    let Ok(mut s) = shared().lock() else { return };
    if s.players.is_empty() {
        // learn parameter sets from a keyframe even with nobody watching
        if p.type_ == sys::OBS_ENCODER_VIDEO && s.sps.is_empty() {
            for nal in nal_units(data) {
                match nal[0] & 0x1f {
                    7 => s.sps = nal.to_vec(),
                    8 => s.pps = nal.to_vec(),
                    _ => {}
                }
            }
        }
        return;
    }
    let seconds = |t: i64| t as f64 * p.timebase_num as f64 / p.timebase_den.max(1) as f64;
    if p.type_ == sys::OBS_ENCODER_VIDEO {
        let ts = (seconds(p.pts) * 90_000.0) as i64 as u32;
        video_packet(&mut s, nal_units(data), ts);
    } else {
        let rate = s.audio.0.max(1) as f64;
        let ts = (seconds(p.pts) * rate) as i64 as u32;
        audio_packet(&mut s, data, ts);
    }
}

// ---------------------------------------------------------------------------------------------
// Packets from Lightair's own encoders

pub fn register() {
    let info = sys::obs_output_info {
        id: c"lightair_rtsp".as_ptr(),
        flags: sys::OBS_OUTPUT_AV | sys::OBS_OUTPUT_ENCODED,
        get_name: Some(out_name),
        create: Some(out_create),
        destroy: Some(out_destroy),
        start: Some(out_start),
        stop: Some(out_stop),
        encoded_packet: Some(out_packet),
        encoded_video_codecs: c"h264".as_ptr(),
        encoded_audio_codecs: c"aac".as_ptr(),
        ..Default::default()
    };
    unsafe { sys::obs_register_output_s(&info, std::mem::size_of::<sys::obs_output_info>()) };
}

enum Running {
    /// libobs's encoders and the `lightair_rtsp` output.
    Libobs { output: *mut sys::obs_output_t, video: *mut sys::obs_encoder_t, audio: *mut sys::obs_encoder_t },
    /// Lightair's own encoders (macOS, `LIGHTAIR_ENGINE=own`).
    #[cfg(target_os = "macos")]
    Own,
}

unsafe impl Send for Running {}

fn running() -> &'static Mutex<Option<Running>> {
    static R: OnceLock<Mutex<Option<Running>>> = OnceLock::new();
    R.get_or_init(|| Mutex::new(None))
}

/// The address players open, when it's on.
pub fn address() -> Option<String> {
    running().lock().ok()?.as_ref()?;
    Some(format!("rtsp://{}:{PORT}/live", lightair_platform::net::local_ip().unwrap_or_else(|| "127.0.0.1".into())))
}

/// Serves the program over RTSP (or stops). Call on the engine thread.
pub fn set(on: bool, available_encoders: &[String]) -> Result<(), String> {
    let mut r = running().lock().map_err(|_| "busy".to_string())?;
    if let Some(old) = r.take() {
        match old {
            Running::Libobs { output, video, audio } => unsafe {
                sys::obs_output_stop(output);
                sys::obs_output_release(output);
                sys::obs_encoder_release(video);
                sys::obs_encoder_release(audio);
            },
            #[cfg(target_os = "macos")]
            Running::Own => crate::own_outputs::stop_rtsp(),
        }
        if let Ok(mut s) = shared().lock() {
            s.players.clear();
            (s.sps, s.pps, s.asc) = (Vec::new(), Vec::new(), Vec::new());
        }
    }
    if !on {
        return Ok(());
    }
    // the server, once
    static LISTENING: OnceLock<bool> = OnceLock::new();
    let ok = *LISTENING.get_or_init(|| match TcpListener::bind(("0.0.0.0", PORT)) {
        Ok(l) => {
            std::thread::Builder::new()
                .name("lightair-rtsp".into())
                .spawn(move || {
                    for s in l.incoming().flatten() {
                        let _ = s.set_nodelay(true);
                        std::thread::spawn(move || serve(s));
                    }
                })
                .is_ok()
        }
        Err(e) => {
            tracing::warn!(%e, "RTSP: port {PORT} is taken");
            false
        }
    });
    if !ok {
        return Err(format!("Port {PORT} is taken by another app"));
    }
    // Lightair's own encoders, from the primary canvas's compositor
    #[cfg(target_os = "macos")]
    if crate::own_outputs::enabled()
        && let Some(canvas) = crate::program_source::canvas_of_video(None)
    {
        crate::own_outputs::start_rtsp(canvas)?;
        *r = Some(Running::Own);
        tracing::info!(port = PORT, "serving the program over RTSP (Lightair's encoders)");
        return Ok(());
    }
    let venc_id = crate::outputs::first_available(available_encoders, crate::outputs::H264_ENCODERS).unwrap_or_else(|| "obs_x264".into());
    let aenc_id = crate::outputs::aac_encoder(available_encoders);
    let c = |s: &str| std::ffi::CString::new(s).unwrap_or_default();
    unsafe {
        let v = crate::obs::Data::from_json(&serde_json::json!({
            "bitrate": 6000, "rate_control": "CBR", "profile": "high", "keyint_sec": 1,
            "bframes": false, "x264opts": "bframes=0", "preset": "veryfast", "tune": "zerolatency",
        }));
        let video = sys::obs_video_encoder_create(c(&venc_id).as_ptr(), c"rtsp video".as_ptr(), v.as_ptr(), std::ptr::null_mut());
        let a = crate::obs::Data::from_json(&serde_json::json!({"bitrate": 160}));
        let audio = sys::obs_audio_encoder_create(c(&aenc_id).as_ptr(), c"rtsp audio".as_ptr(), a.as_ptr(), 0, std::ptr::null_mut());
        if video.is_null() || audio.is_null() {
            return Err("Couldn't make the encoders".into());
        }
        sys::obs_encoder_set_video(video, crate::outputs::primary_video());
        sys::obs_encoder_set_audio(audio, sys::obs_get_audio());
        let output = sys::obs_output_create(c"lightair_rtsp".as_ptr(), c"rtsp".as_ptr(), std::ptr::null_mut(), std::ptr::null_mut());
        if output.is_null() {
            sys::obs_encoder_release(video);
            sys::obs_encoder_release(audio);
            return Err("Couldn't start the RTSP output".into());
        }
        sys::obs_output_set_video_encoder(output, video);
        sys::obs_output_set_audio_encoder(output, audio, 0);
        if !sys::obs_output_start(output) {
            sys::obs_output_release(output);
            sys::obs_encoder_release(video);
            sys::obs_encoder_release(audio);
            return Err("The RTSP output didn't start".into());
        }
        *r = Some(Running::Libobs { output, video, audio });
    }
    tracing::info!(port = PORT, "serving the program over RTSP");
    Ok(())
}
