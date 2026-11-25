//! NDI (docs/13, "Built-in replacements"): receiving NDI sources on the network as a source
//! (`lightair_ndi`, video and sound) and sending the program out as an NDI source. NDI's own
//! runtime does the work: Lightair loads it when it's installed (ndi.video's free "NDI Tools" or
//! its runtime; `NDI_RUNTIME_DIR_V6`/`V5` say where) and never ships it. Without it there are no
//! NDI sources and sending says so.
//!
//! The runtime is called through its documented C functions (NDI SDK, "Processing.NDI.Lib"),
//! with the structures it documents.
//!
//! A source is received once, by its libobs source, whatever shows it: with
//! `LIGHTAIR_ENGINE=own`, Lightair's compositor takes the same pictures through a [`Tap`]
//! instead of connecting a second receiver, and (with the audio engine, macOS) a track taking
//! the source's sound gets it straight into the mixer (`audio_host::feeds_itself`), not through
//! libobs.

use std::collections::HashMap;
use std::ffi::{CStr, CString, c_char, c_void};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use crate::sys;

pub const KIND: &str = "lightair_ndi";

#[repr(C)]
struct FindCreate {
    show_local_sources: bool,
    p_groups: *const c_char,
    p_extra_ips: *const c_char,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct Source {
    p_ndi_name: *const c_char,
    p_url_address: *const c_char,
}

#[repr(C)]
struct RecvCreate {
    source_to_connect_to: Source,
    color_format: i32,
    bandwidth: i32,
    allow_video_fields: bool,
    p_ndi_recv_name: *const c_char,
}

#[repr(C)]
struct VideoFrame {
    xres: i32,
    yres: i32,
    four_cc: u32,
    frame_rate_n: i32,
    frame_rate_d: i32,
    picture_aspect_ratio: f32,
    frame_format_type: i32,
    timecode: i64,
    p_data: *mut u8,
    line_stride_in_bytes: i32,
    p_metadata: *const c_char,
    timestamp: i64,
}

#[repr(C)]
struct AudioFrame {
    sample_rate: i32,
    no_channels: i32,
    no_samples: i32,
    timecode: i64,
    p_data: *mut f32,
    channel_stride_in_bytes: i32,
    p_metadata: *const c_char,
    timestamp: i64,
}

#[repr(C)]
struct SendCreate {
    p_ndi_name: *const c_char,
    p_groups: *const c_char,
    clock_video: bool,
    clock_audio: bool,
}

const FRAME_VIDEO: i32 = 1;
const FRAME_AUDIO: i32 = 2;
const COLOR_BGRX_BGRA: i32 = 0;
const BANDWIDTH_HIGHEST: i32 = 100;
const FOURCC_BGRA: u32 = u32::from_le_bytes(*b"BGRA");
#[cfg(target_os = "macos")]
const FOURCC_BGRX: u32 = u32::from_le_bytes(*b"BGRX");
const PROGRESSIVE: i32 = 1;
const TIMECODE_SYNTHESIZE: i64 = i64::MAX;

type Ptr = *mut c_void;

struct Lib {
    _lib: libloading::Library,
    find_create_v2: unsafe extern "C" fn(*const FindCreate) -> Ptr,
    find_get_current_sources: unsafe extern "C" fn(Ptr, *mut u32) -> *const Source,
    recv_create_v3: unsafe extern "C" fn(*const RecvCreate) -> Ptr,
    recv_destroy: unsafe extern "C" fn(Ptr),
    recv_capture_v2: unsafe extern "C" fn(Ptr, *mut VideoFrame, *mut AudioFrame, *mut c_void, u32) -> i32,
    recv_free_video_v2: unsafe extern "C" fn(Ptr, *const VideoFrame),
    recv_free_audio_v2: unsafe extern "C" fn(Ptr, *const AudioFrame),
    send_create: unsafe extern "C" fn(*const SendCreate) -> Ptr,
    send_destroy: unsafe extern "C" fn(Ptr),
    send_send_video_v2: unsafe extern "C" fn(Ptr, *const VideoFrame),
    send_send_audio_v2: unsafe extern "C" fn(Ptr, *const AudioFrame),
}

unsafe impl Send for Lib {}
unsafe impl Sync for Lib {}

/// Where NDI's runtime may be.
fn candidates() -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    let file = if cfg!(windows) { "Processing.NDI.Lib.x64.dll" } else { "libndi.dylib" };
    for var in ["NDI_RUNTIME_DIR_V6", "NDI_RUNTIME_DIR_V5"] {
        if let Ok(dir) = std::env::var(var) {
            out.push(std::path::Path::new(&dir).join(file));
        }
    }
    if cfg!(target_os = "macos") {
        for p in [
            "/usr/local/lib/libndi.dylib",
            "/Library/NDI SDK for Apple/lib/macOS/libndi.dylib",
            "/Library/Application Support/NDI/lib/libndi.dylib",
            "/Applications/NDI Video Monitor.app/Contents/Frameworks/libndi_advanced.dylib",
        ] {
            out.push(p.into());
        }
    } else if cfg!(windows) {
        out.push(r"C:\Program Files\NDI\NDI 6 Runtime\v6\Processing.NDI.Lib.x64.dll".into());
        out.push(r"C:\Program Files\NDI\NDI 5 Runtime\v5\Processing.NDI.Lib.x64.dll".into());
    }
    out
}

fn lib() -> Option<&'static Lib> {
    static LIB: OnceLock<Option<Lib>> = OnceLock::new();
    LIB.get_or_init(|| {
        for path in candidates().into_iter().filter(|p| p.exists()) {
            let Ok(l) = (unsafe { libloading::Library::new(&path) }) else { continue };
            let lib = unsafe {
                macro_rules! sym {
                    ($name:literal) => {
                        match l.get($name) {
                            Ok(s) => *s,
                            Err(_) => continue,
                        }
                    };
                }
                let initialize: unsafe extern "C" fn() -> bool = sym!(b"NDIlib_initialize\0");
                if !initialize() {
                    tracing::warn!(path = %path.display(), "NDI's runtime didn't start (this processor isn't supported)");
                    continue;
                }
                Lib {
                    find_create_v2: sym!(b"NDIlib_find_create_v2\0"),
                    find_get_current_sources: sym!(b"NDIlib_find_get_current_sources\0"),
                    recv_create_v3: sym!(b"NDIlib_recv_create_v3\0"),
                    recv_destroy: sym!(b"NDIlib_recv_destroy\0"),
                    recv_capture_v2: sym!(b"NDIlib_recv_capture_v2\0"),
                    recv_free_video_v2: sym!(b"NDIlib_recv_free_video_v2\0"),
                    recv_free_audio_v2: sym!(b"NDIlib_recv_free_audio_v2\0"),
                    send_create: sym!(b"NDIlib_send_create\0"),
                    send_destroy: sym!(b"NDIlib_send_destroy\0"),
                    send_send_video_v2: sym!(b"NDIlib_send_send_video_v2\0"),
                    send_send_audio_v2: sym!(b"NDIlib_send_send_audio_v2\0"),
                    _lib: l,
                }
            };
            tracing::info!(path = %path.display(), "NDI runtime loaded");
            return Some(lib);
        }
        None
    })
    .as_ref()
}

/// NDI's runtime is installed.
pub fn available() -> bool {
    lib().is_some()
}

/// The NDI sources on the network now (their names, "MACHINE (Source)"). The first call starts
/// looking, so it may find more a moment later.
pub fn sources() -> Vec<String> {
    let Some(l) = lib() else { return Vec::new() };
    static FINDER: OnceLock<usize> = OnceLock::new();
    let finder = *FINDER.get_or_init(|| {
        let create = FindCreate { show_local_sources: true, p_groups: std::ptr::null(), p_extra_ips: std::ptr::null() };
        unsafe { (l.find_create_v2)(&create) as usize }
    }) as Ptr;
    if finder.is_null() {
        return Vec::new();
    }
    let mut n = 0u32;
    let list = unsafe { (l.find_get_current_sources)(finder, &mut n) };
    if list.is_null() {
        return Vec::new();
    }
    (0..n as usize)
        .filter_map(|i| {
            let s = unsafe { *list.add(i) };
            (!s.p_ndi_name.is_null()).then(|| unsafe { CStr::from_ptr(s.p_ndi_name) }.to_string_lossy().into_owned())
        })
        .collect()
}

// ---------------------------------------------------------------------------------------------
// Handing received pictures on (Lightair's compositor)

/// A received picture as NDI gave it: `height` rows `stride` bytes apart, each starting with
/// `width` BGRA pixels with straight colours (`opaque`: the sender has no alpha, and the fourth
/// byte means nothing). Valid only during the call it's given to.
pub struct Picture<'a> {
    pub data: &'a [u8],
    pub stride: usize,
    pub width: u32,
    pub height: u32,
    pub opaque: bool,
}

/// Takes a source's pictures as they're received, besides libobs. Called on the source's
/// receiving thread: keep it short, and never wait on libobs there.
pub type Tap = Arc<dyn Fn(&Picture) + Send + Sync>;

fn taps() -> &'static Mutex<HashMap<String, Tap>> {
    static TAPS: OnceLock<Mutex<HashMap<String, Tap>>> = OnceLock::new();
    TAPS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Hands every picture the NDI source named `source` (its libobs name: the document source's
/// id) receives to `tap` as well, from now on, in place of any tap it had. A source made later
/// picks it up when it starts receiving.
pub fn tap(source: &str, tap: Tap) {
    if let Ok(mut t) = taps().lock() {
        t.insert(source.to_owned(), tap);
    }
}

/// Stops handing `source`'s pictures on.
pub fn untap(source: &str) {
    if let Ok(mut t) = taps().lock() {
        t.remove(source);
    }
}

fn tap_of(source: &str) -> Option<Tap> {
    taps().lock().ok()?.get(source).cloned()
}

// ---------------------------------------------------------------------------------------------
// Receiving: the source

struct Receiver {
    source: *mut sys::obs_source_t,
    name: Mutex<String>,
    /// Where the sound goes instead of libobs while a track of the mixer takes it.
    sound: Option<Arc<crate::audio_host::FeedSlot>>,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

unsafe impl Send for Receiver {}

struct SrcPtr(*mut sys::obs_source_t);
unsafe impl Send for SrcPtr {}

impl Receiver {
    fn start(&mut self) {
        self.stop_thread();
        let name = self.name.lock().map(|n| n.clone()).unwrap_or_default();
        let Some(l) = lib().filter(|_| !name.is_empty()) else { return };
        let stop = Arc::new(AtomicBool::new(false));
        self.stop = stop.clone();
        let src = SrcPtr(self.source);
        let sound = self.sound.clone();
        // the libobs name, which a tap is registered by
        let me_name = unsafe {
            let p = sys::obs_source_get_name(self.source);
            if p.is_null() { String::new() } else { CStr::from_ptr(p).to_string_lossy().into_owned() }
        };
        self.thread = std::thread::Builder::new()
            .name("lightair-ndi-recv".into())
            .spawn(move || {
                let src = src;
                let cname = CString::new(name).unwrap_or_default();
                let me = CString::new("Lightair").unwrap_or_default();
                let create = RecvCreate {
                    source_to_connect_to: Source { p_ndi_name: cname.as_ptr(), p_url_address: std::ptr::null() },
                    color_format: COLOR_BGRX_BGRA,
                    bandwidth: BANDWIDTH_HIGHEST,
                    allow_video_fields: false,
                    p_ndi_recv_name: me.as_ptr(),
                };
                let recv = unsafe { (l.recv_create_v3)(&create) };
                if recv.is_null() {
                    tracing::warn!("NDI: couldn't receive");
                    return;
                }
                let mut planar: Vec<f32> = Vec::new();
                while !stop.load(Ordering::Relaxed) {
                    let mut video: VideoFrame = unsafe { std::mem::zeroed() };
                    let mut audio: AudioFrame = unsafe { std::mem::zeroed() };
                    match unsafe { (l.recv_capture_v2)(recv, &mut video, &mut audio, std::ptr::null_mut(), 100) } {
                        FRAME_VIDEO => {
                            if !video.p_data.is_null() && video.xres > 0 && video.yres > 0 {
                                let mut frame: sys::obs_source_frame = unsafe { std::mem::zeroed() };
                                frame.data[0] = video.p_data;
                                frame.linesize[0] = video.line_stride_in_bytes as u32;
                                frame.width = video.xres as u32;
                                frame.height = video.yres as u32;
                                frame.format = sys::VIDEO_FORMAT_BGRA;
                                frame.full_range = true;
                                frame.timestamp = crate::clock::now_ns();
                                unsafe { sys::obs_source_output_video(src.0, &frame) };
                                if let Some(tap) = tap_of(&me_name).filter(|_| video.line_stride_in_bytes >= video.xres * 4) {
                                    let stride = video.line_stride_in_bytes as usize;
                                    let picture = Picture {
                                        data: unsafe { std::slice::from_raw_parts(video.p_data, stride * video.yres as usize) },
                                        stride,
                                        width: video.xres as u32,
                                        height: video.yres as u32,
                                        opaque: video.four_cc != FOURCC_BGRA,
                                    };
                                    tap(&picture);
                                }
                            }
                            unsafe { (l.recv_free_video_v2)(recv, &video) };
                        }
                        FRAME_AUDIO => {
                            let (ch, n) = (audio.no_channels.clamp(0, 8) as usize, audio.no_samples.max(0) as usize);
                            if !audio.p_data.is_null() && ch > 0 && n > 0 {
                                let stride = (audio.channel_stride_in_bytes as usize / 4).max(n);
                                planar.clear();
                                planar.extend_from_slice(unsafe { std::slice::from_raw_parts(audio.p_data, stride * ch) });
                                // a track of the mixer takes the sound itself: into its feed
                                let fed = sound.as_ref().is_some_and(|slot| {
                                    let planes: Vec<&[f32]> =
                                        (0..ch.min(2)).map(|c| &planar[c * stride..c * stride + n]).collect();
                                    slot.push_planar(&planes, audio.sample_rate.max(1) as u32)
                                });
                                if fed {
                                    unsafe { (l.recv_free_audio_v2)(recv, &audio) };
                                    continue;
                                }
                                let mut out: sys::obs_source_audio = unsafe { std::mem::zeroed() };
                                for c in 0..ch {
                                    out.data[c] = planar[c * stride..].as_ptr() as *const u8;
                                }
                                out.frames = n as u32;
                                out.speakers = match ch {
                                    1 => sys::SPEAKERS_MONO,
                                    2 => sys::SPEAKERS_STEREO,
                                    3 => sys::SPEAKERS_2POINT1,
                                    4 => sys::SPEAKERS_4POINT0,
                                    5 => sys::SPEAKERS_4POINT1,
                                    6 => sys::SPEAKERS_5POINT1,
                                    _ => sys::SPEAKERS_7POINT1,
                                };
                                out.format = sys::AUDIO_FORMAT_FLOAT_PLANAR;
                                out.samples_per_sec = audio.sample_rate.max(1) as u32;
                                out.timestamp = crate::clock::now_ns();
                                unsafe { sys::obs_source_output_audio(src.0, &out) };
                            }
                            unsafe { (l.recv_free_audio_v2)(recv, &audio) };
                        }
                        _ => {}
                    }
                }
                unsafe { (l.recv_destroy)(recv) };
            })
            .ok();
    }

    fn stop_thread(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

unsafe extern "C" fn get_name(_: *mut c_void) -> *const c_char {
    c"NDI".as_ptr()
}

unsafe extern "C" fn create(settings: *mut sys::obs_data_t, source: *mut sys::obs_source_t) -> *mut c_void {
    #[cfg(target_os = "macos")]
    let sound = crate::system_audio::mixer_captures().then(|| crate::audio_host::feed_slot(source));
    #[cfg(not(target_os = "macos"))]
    let sound = None;
    let r = Box::new(Receiver {
        source,
        name: Mutex::new(String::new()),
        sound,
        stop: Arc::new(AtomicBool::new(true)),
        thread: None,
    });
    let p = Box::into_raw(r) as *mut c_void;
    unsafe { update(p, settings) };
    p
}

unsafe extern "C" fn update(data: *mut c_void, settings: *mut sys::obs_data_t) {
    let r = unsafe { &mut *(data as *mut Receiver) };
    let p = unsafe { sys::obs_data_get_string(settings, c"ndi_name".as_ptr()) };
    let name = if p.is_null() { String::new() } else { unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned() };
    let changed = r.name.lock().map(|mut n| if *n != name { *n = name; true } else { false }).unwrap_or(false);
    if changed || r.thread.is_none() {
        r.start();
    }
}

unsafe extern "C" fn destroy(data: *mut c_void) {
    let mut r = unsafe { Box::from_raw(data as *mut Receiver) };
    crate::audio_host::feeds_itself_no_more(r.source);
    r.stop_thread();
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
    /// from libobs's main canvas directly, or from the compositor.
    output: usize,
    /// macOS, with Lightair's own outputs: the tap that sends the compositor's frames (0: none).
    tap: u64,
    /// Video and sound carry their times as NDI timecodes (on the program clock, so receivers
    /// line them up); otherwise NDI makes them up as they're sent.
    timed: bool,
}

unsafe impl Send for Sender {}

fn sender() -> &'static Mutex<Option<Box<Sender>>> {
    static S: OnceLock<Mutex<Option<Box<Sender>>>> = OnceLock::new();
    S.get_or_init(|| Mutex::new(None))
}

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
        timecode: if s.timed { (a.timestamp / 100) as i64 } else { TIMECODE_SYNTHESIZE },
        p_data: s.audio.as_mut_ptr(),
        channel_stride_in_bytes: (n * 4) as i32,
        p_metadata: std::ptr::null(),
        timestamp: 0,
    };
    unsafe { (l.send_send_audio_v2)(s.send, &frame) };
}

/// The mixer's Program, taken for NDI out (the audio engine on, with Lightair's own outputs).
static SOUND: Mutex<Option<crate::audio_host::MixTap>> = Mutex::new(None);

/// A block of the mixer's Program (interleaved stereo, at its time) sent over NDI.
fn send_sound(interleaved: &[f32], ts: u64) {
    let Some(l) = lib() else { return };
    let Ok(mut guard) = sender().lock() else { return };
    let Some(s) = guard.as_mut() else { return };
    let n = interleaved.len() / 2;
    if n == 0 {
        return;
    }
    s.audio.clear();
    s.audio.extend(interleaved.iter().step_by(2));
    s.audio.extend(interleaved.iter().skip(1).step_by(2));
    let frame = AudioFrame {
        sample_rate: s.rate as i32,
        no_channels: 2,
        no_samples: n as i32,
        timecode: if s.timed { (ts / 100) as i64 } else { TIMECODE_SYNTHESIZE },
        p_data: s.audio.as_mut_ptr(),
        channel_stride_in_bytes: (n * 4) as i32,
        p_metadata: std::ptr::null(),
        timestamp: 0,
    };
    unsafe { (l.send_send_audio_v2)(s.send, &frame) };
}

/// Sends the program out as the NDI source `name` ("" stops it). Call on the engine thread.
pub fn set_output(name: &str) -> Result<(), String> {
    // the mixer's sound stops first (its thread waits for the sender's lock: not held here)
    let sound = SOUND.lock().ok().and_then(|mut s| s.take());
    drop(sound);
    let running = sender().lock().ok().and_then(|s| s.as_ref().map(|s| (s.output, s.tap)));
    if let Some((output, tap)) = running {
        // the compositor's frames stop first: once the tap is gone nothing sends video
        #[cfg(target_os = "macos")]
        if tap != 0 {
            crate::own_outputs::remove_tap(tap);
        }
        unsafe {
            #[cfg(target_os = "macos")]
            if output != 0 {
                stop_output(output as *mut sys::obs_output_t);
            }
            if output == 0 && tap == 0 {
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
    let l = lib().ok_or("NDI isn't installed on this computer (NDI Tools from ndi.video brings it)")?;
    let cname = CString::new(name).map_err(|e| e.to_string())?;
    let create = SendCreate { p_ndi_name: cname.as_ptr(), p_groups: std::ptr::null(), clock_video: false, clock_audio: false };
    let send = unsafe { (l.send_create)(&create) };
    if send.is_null() {
        return Err("NDI couldn't start sending".into());
    }
    let mut ovi: sys::obs_video_info = unsafe { std::mem::zeroed() };
    let fps = if unsafe { sys::obs_get_video_info(&mut ovi) } { (ovi.fps_num as i32, ovi.fps_den.max(1) as i32) } else { (60, 1) };
    let mut ai = sys::obs_audio_info::default();
    let rate = if unsafe { sys::obs_get_audio_info(&mut ai) } { ai.samples_per_sec } else { 48_000 };
    let conversion = sys::video_scale_info {
        format: sys::VIDEO_FORMAT_BGRA,
        width: ovi.output_width,
        height: ovi.output_height,
        range: sys::VIDEO_RANGE_FULL,
        colorspace: sys::VIDEO_CS_709,
    };
    // Lightair's own outputs: the primary canvas's compositor frames, straight from it
    #[cfg(target_os = "macos")]
    let tap = start_tap(send, fps);
    #[cfg(not(target_os = "macos"))]
    let tap = 0u64;
    if let Ok(mut s) = sender().lock() {
        *s = Some(Box::new(Sender { send, audio: Vec::new(), fps, rate, output: 0, tap, timed: tap != 0 }));
    }
    if tap != 0 {
        if crate::audio_host::mixer_on() {
            if let Ok(mut s) = SOUND.lock() {
                *s = Some(crate::audio_host::tap_mix(0, send_sound));
            }
        } else {
            unsafe { sys::obs_add_raw_audio_callback(0, std::ptr::null(), Some(on_audio), std::ptr::null_mut()) };
        }
        tracing::info!(name, "sending the program over NDI (the compositor's frames)");
        return Ok(());
    }
    // the primary canvas's output canvas (Lightair's own compositor's picture) when there is one
    #[cfg(target_os = "macos")]
    let output = crate::program_source::video(None).map_or(std::ptr::null_mut(), |video| unsafe { start_output(video, &conversion) });
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
// macOS, with Lightair's own outputs: the compositor's frames of the primary canvas, sent as
// they come (BGRX: they're opaque), each with its time on the program clock

/// Starts sending the primary canvas's compositor frames through `send` (0 when Lightair's own
/// outputs are off or there's no primary canvas: libobs's frames are sent then).
#[cfg(target_os = "macos")]
fn start_tap(send: Ptr, fps: (i32, i32)) -> u64 {
    if !crate::own_outputs::enabled() {
        return 0;
    }
    let Some(canvas) = crate::program_source::canvas_of_video(None) else { return 0 };
    let send = send as usize;
    let f: crate::own_outputs::TapFn = Box::new(move |picture, pts_ns| send_picture(send as Ptr, fps, picture, pts_ns));
    match crate::own_outputs::add_tap(canvas, None, None, f) {
        Ok(tap) => tap,
        Err(e) => {
            tracing::warn!(%e, "NDI: sending libobs's picture");
            0
        }
    }
}

/// Sends one BGRA picture (the tap's own copy) as NDI video. The tap's thread.
#[cfg(target_os = "macos")]
fn send_picture(send: Ptr, fps: (i32, i32), picture: &objc2_core_video::CVPixelBuffer, pts_ns: i64) {
    use objc2_core_video::{
        CVPixelBufferGetBaseAddress, CVPixelBufferGetBytesPerRow, CVPixelBufferGetHeight, CVPixelBufferGetWidth,
        CVPixelBufferLockBaseAddress, CVPixelBufferLockFlags, CVPixelBufferUnlockBaseAddress,
    };
    let Some(l) = lib() else { return };
    if unsafe { CVPixelBufferLockBaseAddress(picture, CVPixelBufferLockFlags::ReadOnly) } != 0 {
        return;
    }
    let data = CVPixelBufferGetBaseAddress(picture) as *mut u8;
    if !data.is_null() {
        let v = VideoFrame {
            xres: CVPixelBufferGetWidth(picture) as i32,
            yres: CVPixelBufferGetHeight(picture) as i32,
            four_cc: FOURCC_BGRX,
            frame_rate_n: fps.0,
            frame_rate_d: fps.1,
            picture_aspect_ratio: 0.0,
            frame_format_type: PROGRESSIVE,
            timecode: pts_ns / 100,
            p_data: data,
            line_stride_in_bytes: CVPixelBufferGetBytesPerRow(picture) as i32,
            p_metadata: std::ptr::null(),
            timestamp: 0,
        };
        // NDI has the picture (compressed or copied) when this returns
        unsafe { (l.send_send_video_v2)(send, &v) };
    }
    unsafe { CVPixelBufferUnlockBaseAddress(picture, CVPixelBufferLockFlags::ReadOnly) };
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
        let output = sys::obs_output_create(OUTPUT_ID.as_ptr(), c"NDI".as_ptr(), std::ptr::null_mut(), std::ptr::null_mut());
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
