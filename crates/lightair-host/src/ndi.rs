//! NDI (docs/13, "Built-in replacements"): receiving NDI sources on the network as a source
//! (`lightair_ndi`, video and sound) and sending the program out as an NDI source. NDI's own
//! runtime does the work: Lightair loads it when it's installed (ndi.video's free "NDI Tools" or
//! its runtime; `NDI_RUNTIME_DIR_V6`/`V5` say where) and never ships it. Without it there are no
//! NDI sources and sending says so.
//!
//! The runtime is called through its documented C functions (NDI SDK, "Processing.NDI.Lib"),
//! with the structures it documents.
//!
//! A source is received once, by its runtime ([`Receiver`]), whatever shows it: with
//! `LIGHTAIR_ENGINE=own`, Lightair's compositor takes the same pictures through a [`Tap`]
//! instead of connecting a second receiver, and (with the audio engine, macOS) a track taking
//! the source's sound gets it straight into the mixer (`audio_host::feeds_itself`), not through
//! libobs.

use std::collections::HashMap;
use std::ffi::{CStr, CString, c_char, c_void};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use lightair_model::Id;

pub const KIND: &str = "lightair_ndi";

#[repr(C)]
pub struct FindCreate {
    pub show_local_sources: bool,
    pub p_groups: *const c_char,
    pub p_extra_ips: *const c_char,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct Source {
    pub p_ndi_name: *const c_char,
    pub p_url_address: *const c_char,
}

#[repr(C)]
pub struct RecvCreate {
    pub source_to_connect_to: Source,
    pub color_format: i32,
    pub bandwidth: i32,
    pub allow_video_fields: bool,
    pub p_ndi_recv_name: *const c_char,
}

#[repr(C)]
pub struct VideoFrame {
    pub xres: i32,
    pub yres: i32,
    pub four_cc: u32,
    pub frame_rate_n: i32,
    pub frame_rate_d: i32,
    pub picture_aspect_ratio: f32,
    pub frame_format_type: i32,
    pub timecode: i64,
    pub p_data: *mut u8,
    pub line_stride_in_bytes: i32,
    pub p_metadata: *const c_char,
    pub timestamp: i64,
}

#[repr(C)]
pub struct AudioFrame {
    pub sample_rate: i32,
    pub no_channels: i32,
    pub no_samples: i32,
    pub timecode: i64,
    pub p_data: *mut f32,
    pub channel_stride_in_bytes: i32,
    pub p_metadata: *const c_char,
    pub timestamp: i64,
}

#[repr(C)]
pub struct SendCreate {
    pub p_ndi_name: *const c_char,
    pub p_groups: *const c_char,
    pub clock_video: bool,
    pub clock_audio: bool,
}

pub const FRAME_VIDEO: i32 = 1;
pub const FRAME_AUDIO: i32 = 2;
pub const COLOR_BGRX_BGRA: i32 = 0;
pub const BANDWIDTH_HIGHEST: i32 = 100;
pub const FOURCC_BGRA: u32 = u32::from_le_bytes(*b"BGRA");
#[cfg(any(target_os = "macos", windows))]
pub const FOURCC_BGRX: u32 = u32::from_le_bytes(*b"BGRX");
pub const PROGRESSIVE: i32 = 1;
pub const TIMECODE_SYNTHESIZE: i64 = i64::MAX;

pub type Ptr = *mut c_void;

pub struct Lib {
    pub _lib: libloading::Library,
    pub find_create_v2: unsafe extern "C" fn(*const FindCreate) -> Ptr,
    pub find_get_current_sources: unsafe extern "C" fn(Ptr, *mut u32) -> *const Source,
    pub recv_create_v3: unsafe extern "C" fn(*const RecvCreate) -> Ptr,
    pub recv_destroy: unsafe extern "C" fn(Ptr),
    pub recv_capture_v2: unsafe extern "C" fn(Ptr, *mut VideoFrame, *mut AudioFrame, *mut c_void, u32) -> i32,
    pub recv_free_video_v2: unsafe extern "C" fn(Ptr, *const VideoFrame),
    pub recv_free_audio_v2: unsafe extern "C" fn(Ptr, *const AudioFrame),
    pub send_create: unsafe extern "C" fn(*const SendCreate) -> Ptr,
    pub send_destroy: unsafe extern "C" fn(Ptr),
    pub send_send_video_v2: unsafe extern "C" fn(Ptr, *const VideoFrame),
    pub send_send_audio_v2: unsafe extern "C" fn(Ptr, *const AudioFrame),
}

unsafe impl Send for Lib {}
unsafe impl Sync for Lib {}

/// Where NDI's runtime may be.
pub fn candidates() -> Vec<std::path::PathBuf> {
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

pub fn lib() -> Option<&'static Lib> {
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

pub fn taps() -> &'static Mutex<HashMap<String, Tap>> {
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

pub fn tap_of(source: &str) -> Option<Tap> {
    taps().lock().ok()?.get(source).cloned()
}

// ---------------------------------------------------------------------------------------------
// Receiving: a source's runtime

/// Where a receiver's pictures and sound go besides the compositor's tap and the mixer: libobs's
/// NDI source, while libobs is there.
pub trait Elsewhere: Send + Sync {
    /// A picture (BGRA, or BGRX when `opaque`), as NDI gave it.
    fn video(&self, picture: &Picture);
    /// A block of sound no track of the mixer took: `channels` planes `stride` floats apart in
    /// `planar`, `frames` long.
    fn audio(&self, planar: &[f32], stride: usize, frames: usize, channels: usize, rate: u32);
}

/// One NDI source received for document source `source`, on a thread of its own, from when
/// it's made until it's dropped: its pictures go to the compositor's tap (and `elsewhere`), its
/// sound to a track of the mixer taking it (else `elsewhere`).
pub struct Receiver {
    source: Id,
    name: String,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Receiver {
    /// Starts receiving the NDI source `name` for `source` ("" receives nothing).
    pub fn start(source: Id, name: &str, elsewhere: Option<Arc<dyn Elsewhere>>) -> Receiver {
        let stop = Arc::new(AtomicBool::new(false));
        let mut r = Receiver { source, name: name.to_string(), stop: stop.clone(), thread: None };
        let Some(l) = lib().filter(|_| !name.is_empty()) else { return r };
        let sound = crate::audio_host::mixer_on().then(|| crate::audio_host::feed_slot(source));
        let me_name = source.to_string();
        let name = name.to_string();
        r.thread = std::thread::Builder::new()
            .name("lightair-ndi-recv".into())
            .spawn(move || {
                let cname = CString::new(name).unwrap_or_default();
                let me = CString::new("Lightair").unwrap_or_default();
                let create = RecvCreate {
                    source_to_connect_to: Source { p_ndi_name: cname.as_ptr(), p_url_address: std::ptr::null() },
                    color_format: COLOR_BGRX_BGRA,
                    bandwidth: BANDWIDTH_HIGHEST,
                    allow_video_fields: false,
                    p_ndi_recv_name: me.as_ptr(),
                };
                // SAFETY: `create` and the strings it points to outlive the call.
                let recv = unsafe { (l.recv_create_v3)(&create) };
                if recv.is_null() {
                    tracing::warn!("NDI: couldn't receive");
                    return;
                }
                let mut planar: Vec<f32> = Vec::new();
                while !stop.load(Ordering::Relaxed) {
                    // SAFETY: zeroed frames are what NDI expects to fill in.
                    let mut video: VideoFrame = unsafe { std::mem::zeroed() };
                    let mut audio: AudioFrame = unsafe { std::mem::zeroed() };
                    // SAFETY: `recv` is this thread's receiver; the frames are valid places.
                    match unsafe { (l.recv_capture_v2)(recv, &mut video, &mut audio, std::ptr::null_mut(), 100) } {
                        FRAME_VIDEO => {
                            if !video.p_data.is_null()
                                && video.xres > 0
                                && video.yres > 0
                                && video.line_stride_in_bytes >= video.xres * 4
                            {
                                let stride = video.line_stride_in_bytes as usize;
                                let picture = Picture {
                                    // SAFETY: NDI's frame holds `yres` rows of `stride` bytes.
                                    data: unsafe {
                                        std::slice::from_raw_parts(video.p_data, stride * video.yres as usize)
                                    },
                                    stride,
                                    width: video.xres as u32,
                                    height: video.yres as u32,
                                    opaque: video.four_cc != FOURCC_BGRA,
                                };
                                if let Some(e) = &elsewhere {
                                    e.video(&picture);
                                }
                                if let Some(tap) = tap_of(&me_name) {
                                    tap(&picture);
                                }
                            }
                            // SAFETY: the frame came from this receiver and is given back once.
                            unsafe { (l.recv_free_video_v2)(recv, &video) };
                        }
                        FRAME_AUDIO => {
                            let (ch, n) = (audio.no_channels.clamp(0, 8) as usize, audio.no_samples.max(0) as usize);
                            if !audio.p_data.is_null() && ch > 0 && n > 0 {
                                let stride = (audio.channel_stride_in_bytes as usize / 4).max(n);
                                planar.clear();
                                // SAFETY: NDI's frame holds `ch` planes `stride` floats apart.
                                planar.extend_from_slice(unsafe {
                                    std::slice::from_raw_parts(audio.p_data, stride * ch)
                                });
                                let rate = audio.sample_rate.max(1) as u32;
                                // a track of the mixer takes the sound itself: into its feed
                                let fed = sound.as_ref().is_some_and(|slot| {
                                    let planes: Vec<&[f32]> =
                                        (0..ch.min(2)).map(|c| &planar[c * stride..c * stride + n]).collect();
                                    slot.push_planar(&planes, rate)
                                });
                                if !fed && let Some(e) = &elsewhere {
                                    e.audio(&planar, stride, n, ch, rate);
                                }
                            }
                            // SAFETY: the frame came from this receiver and is given back once.
                            unsafe { (l.recv_free_audio_v2)(recv, &audio) };
                        }
                        _ => {}
                    }
                }
                // SAFETY: made above, used only on this thread, destroyed once.
                unsafe { (l.recv_destroy)(recv) };
            })
            .ok();
        r
    }

    /// The NDI source it receives.
    pub fn name(&self) -> &str {
        &self.name
    }
}

impl Drop for Receiver {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
        if crate::audio_host::mixer_on() {
            crate::audio_host::feeds_itself_no_more(self.source);
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Sending the program, from Lightair's compositor and mixer (macOS and Windows)

/// The program sent over NDI from Lightair's own outputs: a canvas's compositor frames (BGRX:
/// they're opaque) and the mixer's Program, each with its time on the program clock as NDI
/// timecodes, so receivers line them up. Dropping it stops sending.
#[cfg(any(target_os = "macos", windows))]
pub struct OwnSender {
    send: Arc<SendPtr>,
    tap: u64,
    sound: Option<crate::audio_host::MixTap>,
}

/// An NDI sender, usable from the tap's and the mix tap's threads.
pub struct SendPtr(pub Ptr);
// SAFETY: NDI's send functions may be called from any thread (one at a time per kind of frame).
unsafe impl Send for SendPtr {}
unsafe impl Sync for SendPtr {}

#[cfg(any(target_os = "macos", windows))]
impl OwnSender {
    /// Sends `canvas`'s compositor frames and the mixer's Program as the NDI source `name`.
    /// Engine thread.
    pub fn start(name: &str, canvas: Id) -> Result<OwnSender, String> {
        let l = lib().ok_or("NDI isn't installed on this computer (NDI Tools from ndi.video brings it)")?;
        let cname = CString::new(name).map_err(|e| e.to_string())?;
        let create = SendCreate {
            p_ndi_name: cname.as_ptr(),
            p_groups: std::ptr::null(),
            clock_video: false,
            clock_audio: false,
        };
        // SAFETY: `create` and its string outlive the call.
        let send = unsafe { (l.send_create)(&create) };
        if send.is_null() {
            return Err("NDI couldn't start sending".into());
        }
        let send = Arc::new(SendPtr(send));
        let fps = crate::show::snapshot()
            .and_then(|s| s.workspace.canvas(canvas).map(|c| (c.fps_num.max(1) as i32, c.fps_den.max(1) as i32)))
            .unwrap_or((60, 1));
        let to = send.clone();
        let f: crate::own_outputs::TapFn = Box::new(move |picture, pts_ns| send_picture(&to, fps, picture, pts_ns));
        let tap = match crate::own_outputs::add_tap(canvas, None, None, f) {
            Ok(tap) => tap,
            Err(e) => {
                // SAFETY: made above, not used by anything else yet.
                unsafe { (l.send_destroy)(send.0) };
                return Err(e);
            }
        };
        let to = send.clone();
        let mut audio = Vec::new();
        let sound = crate::audio_host::tap_mix(0, move |interleaved, ts| send_sound(&to, &mut audio, interleaved, ts));
        tracing::info!(name, "sending the program over NDI (the compositor's frames)");
        Ok(OwnSender { send, tap, sound: Some(sound) })
    }
}

#[cfg(any(target_os = "macos", windows))]
impl Drop for OwnSender {
    fn drop(&mut self) {
        // nothing sends once the taps are gone
        crate::own_outputs::remove_tap(self.tap);
        self.sound.take();
        if let Some(l) = lib() {
            // SAFETY: the taps that used it are gone.
            unsafe { (l.send_destroy)(self.send.0) };
        }
    }
}

/// A block of the mixer's Program (interleaved stereo, at its time) sent over NDI.
#[cfg(any(target_os = "macos", windows))]
fn send_sound(send: &SendPtr, audio: &mut Vec<f32>, interleaved: &[f32], ts: u64) {
    let Some(l) = lib() else { return };
    let n = interleaved.len() / 2;
    if n == 0 {
        return;
    }
    audio.clear();
    audio.extend(interleaved.iter().step_by(2));
    audio.extend(interleaved.iter().skip(1).step_by(2));
    let frame = AudioFrame {
        sample_rate: crate::audio_host::engine_rate() as i32,
        no_channels: 2,
        no_samples: n as i32,
        timecode: (ts / 100) as i64,
        p_data: audio.as_mut_ptr(),
        channel_stride_in_bytes: (n * 4) as i32,
        p_metadata: std::ptr::null(),
        timestamp: 0,
    };
    // SAFETY: the frame points into `audio`, alive for the call.
    unsafe { (l.send_send_audio_v2)(send.0, &frame) };
}

/// Sends one BGRA picture (the tap's own copy) as NDI video. The tap's thread.
#[cfg(target_os = "macos")]
fn send_picture(send: &SendPtr, fps: (i32, i32), picture: &objc2_core_video::CVPixelBuffer, pts_ns: i64) {
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
        unsafe { (l.send_send_video_v2)(send.0, &v) };
    }
    unsafe { CVPixelBufferUnlockBaseAddress(picture, CVPixelBufferLockFlags::ReadOnly) };
}

/// Sends one BGRA picture (the tap's own copy, read back) as NDI video. The tap's thread.
#[cfg(windows)]
fn send_picture(send: &SendPtr, fps: (i32, i32), picture: &crate::own_outputs::Picture<'_>, pts_ns: i64) {
    let Some(l) = lib() else { return };
    if picture.format != crate::own_outputs::TapFormat::Bgra {
        return;
    }
    let v = VideoFrame {
        xres: picture.width as i32,
        yres: picture.height as i32,
        four_cc: FOURCC_BGRX,
        frame_rate_n: fps.0,
        frame_rate_d: fps.1,
        picture_aspect_ratio: 0.0,
        frame_format_type: PROGRESSIVE,
        timecode: pts_ns / 100,
        // NDI only reads the picture
        p_data: picture.data.as_ptr() as *mut u8,
        line_stride_in_bytes: picture.stride as i32,
        p_metadata: std::ptr::null(),
        timestamp: 0,
    };
    // NDI has the picture (compressed or copied) when this returns
    unsafe { (l.send_send_video_v2)(send.0, &v) };
}

/// NDI out: the primary canvas's compositor frames and the mixer's Program, sent as `name`.
#[cfg(any(target_os = "macos", windows))]
static SENDING: std::sync::Mutex<Option<OwnSender>> = std::sync::Mutex::new(None);

/// Sends the program over NDI as `name` (empty: stops).
pub fn set_output(name: &str) -> Result<(), String> {
    #[cfg(any(target_os = "macos", windows))]
    {
        let old = SENDING.lock().ok().and_then(|mut s| s.take());
        drop(old);
        if name.is_empty() {
            return Ok(());
        }
        let canvas = crate::program::primary_canvas().ok_or("There's no canvas to send yet.")?;
        let own = OwnSender::start(name, canvas)?;
        if let Ok(mut s) = SENDING.lock() {
            *s = Some(own);
        }
        Ok(())
    }
    #[cfg(not(any(target_os = "macos", windows)))]
    {
        if name.is_empty() {
            Ok(())
        } else {
            Err("NDI out without libobs works on macOS and Windows only for now.".into())
        }
    }
}
