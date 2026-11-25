//! The Phone camera source on libobs's side (the feeds, their timing and the decoder are
//! `lightair_host::phone_camera`, re-exported here): `lightair_phone_camera` wraps a feed's
//! decoder (a private frames source, or `ffmpeg_source` over UDP), shows a placeholder while the
//! phone is away, turns the picture upright, passes the phone's microphone on as its sound, and
//! draws the compositor's pictures where libobs still shows the source.

use std::ffi::{CStr, c_char, c_void};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use parking_lot::Mutex;
use serde_json::json;

pub use lightair_host::phone_camera::*;
/// One unit of a stream's AAC decoded as a libobs source's sound (the mixer's feed first).
#[cfg(target_os = "macos")]
pub use crate::phone_decode::sound_into;

use crate::obs::{Data, ObsSource};
use crate::sys;

const ID: &CStr = c"lightair_phone_camera";

/// The placeholder's typeface (the system's own).
#[cfg(windows)]
const FACE: &str = "Segoe UI";
#[cfg(not(windows))]
const FACE: &str = "Helvetica Neue";

/// The decoder's settings for a feed on `port`: MPEG-TS over UDP from this machine, played as it
/// comes (no buffering). A phone that drops leaves it waiting; a new stream gets a new decoder.
fn decoder_settings(port: u16) -> serde_json::Value {
    json!({
        "is_local_file": false,
        "input": format!(
            "udp://127.0.0.1:{port}?fifo_size=262144&overrun_nonfatal=1&buffer_size=8388608"
        ),
        "input_format": "mpegts",
        "buffering_mb": 0,
        "hw_decode": true,
        "restart_on_activate": false,
        "close_when_inactive": false,
        "clear_on_media_end": true,
        "reconnect_delay_sec": 1,
        // what's read to learn the stream is played in real time afterwards, so the look must be
        // short (it was 1.5 s, which stayed as delay for good) yet long enough to see the
        // parameter sets the phone puts before each keyframe (a 32 KB look missed them at
        // 25 Mb/s: no size, no picture). So: as long as it takes to see them, and not a frame
        // more (fpsprobesize=0: the frame rate isn't measured). The phone sends a keyframe as
        // soon as a stream starts.
        "ffmpeg_options": "probesize=5000000 analyzeduration=3000000 fpsprobesize=0 fflags=nobuffer",
    })
}

struct Child(ObsSource);

struct Shared {
    source: usize,
    /// A weak reference to the wrapper: where the decoder's sound goes.
    weak: usize,
    feed: Arc<Feed>,
    decoder: Mutex<Option<Child>>,
    /// The placeholder's line, and what it says.
    text: Mutex<Option<(Child, String)>>,
    /// The feed generation the decoder was made for.
    seen: AtomicU64,
    /// A worker is making or letting go of a decoder.
    busy: AtomicBool,
    /// The last size shown (width << 32 | height), before turning.
    size: AtomicU64,
    /// The compositor's picture as the wrapper last drew it.
    #[cfg(target_os = "macos")]
    drawn: Mutex<Drawn>,
}

/// The compositor's picture as the wrapper draws it: a libobs texture over its IOSurface, made
/// once and rebound to each new surface, and the buffer held while it's drawn. Graphics thread
/// only (destroyed inside the graphics context).
#[cfg(target_os = "macos")]
struct Drawn {
    buffer: Option<crate::bridge::Buffer>,
    tex: *mut sys::gs_texture_t,
    size: (u32, u32),
}

impl Shared {
    /// A new decoder for the feed's current stream (a worker thread).
    fn renew(self: &Arc<Self>, generation: u64) {
        self.drop_decoder();
        let name = unsafe { CStr::from_ptr(sys::obs_source_get_name(self.source as *mut sys::obs_source_t)) }
            .to_string_lossy()
            .into_owned();
        // macOS and Windows: frames decoded here (VideoToolbox, Media Foundation) into a
        // private async source; elsewhere libobs's media source reading the feed's UDP port
        let child = if DECODES_DIRECTLY {
            ObsSource::create_private("lightair_phone_frames", &format!("{name} (phone frames)"), None)
        } else {
            let settings = Data::from_json(&decoder_settings(self.feed.port));
            ObsSource::create_private("ffmpeg_source", &format!("{name} (phone feed)"), Some(&settings))
        };
        if let Some(child) = child {
            unsafe {
                sys::obs_source_add_audio_capture_callback(child.as_ptr(), Some(sound), self.weak as *mut c_void)
            };
            // the newest picture as soon as it's decoded: libobs otherwise holds a stream's
            // frames back to their timestamps (the phone's mic is lined up by the track's offset)
            unsafe { sys::obs_source_set_async_unbuffered(child.as_ptr(), true) };
            #[cfg(any(target_os = "macos", windows))]
            if let Some(frames) = unsafe { ObsSource::from_borrowed(child.as_ptr()) } {
                let (tx, rx) = crossbeam_channel::bounded(256);
                *self.feed.units.lock() = Some(tx);
                let feed = self.feed.clone();
                #[cfg(target_os = "macos")]
                let run = crate::phone_decode::run;
                #[cfg(windows)]
                let run = crate::phone_decode_win::run;
                let _ = std::thread::Builder::new().name("lightair-phone-decode".into()).spawn(move || run(frames, feed, rx));
            }
            *self.decoder.lock() = Some(Child(child));
        }
        self.seen.store(generation, Ordering::Release);
        self.busy.store(false, Ordering::Release);
    }

    fn drop_decoder(&self) {
        // (the decoding thread ends once its units stop)
        self.feed.units.lock().take();
        if let Some(Child(c)) = self.decoder.lock().take() {
            unsafe { sys::obs_source_remove_audio_capture_callback(c.as_ptr(), Some(sound), self.weak as *mut c_void) };
        }
    }

    /// The placeholder's line, made or changed (a worker thread: text is drawn when it's set).
    fn retext(self: &Arc<Self>, message: String) {
        let settings = Data::from_json(&json!({
            "text": message,
            "font": {"face": FACE, "size": 64, "flags": 0},
            "color1": 0xFFB4B4B4u32,
            "color2": 0xFFB4B4B4u32,
        }));
        let mut text = self.text.lock();
        match text.as_mut() {
            Some((Child(t), said)) => {
                t.update(&settings);
                *said = message;
            }
            None => {
                if let Some(t) =
                    ObsSource::create_private("text_ft2_source_v2", "Phone camera placeholder", Some(&settings))
                {
                    *text = Some((Child(t), message));
                }
            }
        }
        drop(text);
        self.busy.store(false, Ordering::Release);
    }

    fn on_worker(self: &Arc<Self>, f: impl FnOnce(&Arc<Self>) + Send + 'static) {
        if self.busy.swap(true, Ordering::AcqRel) {
            return;
        }
        let me = self.clone();
        // (making or releasing a source from inside libobs's tick isn't safe)
        let spawned = std::thread::Builder::new().name("lightair-phone-camera".into()).spawn(move || f(&me));
        if spawned.is_err() {
            self.busy.store(false, Ordering::Release);
        }
    }

    /// The decoder's picture size, before turning (the last seen while it has none).
    fn picture_size(&self) -> (u32, u32) {
        #[cfg(target_os = "macos")]
        if self.feed.live()
            && let Some(b) = self.feed.newest_taken()
        {
            let (w, h) = (
                objc2_core_video::CVPixelBufferGetWidth(&b) as u64,
                objc2_core_video::CVPixelBufferGetHeight(&b) as u64,
            );
            if w > 0 && h > 0 {
                self.size.store((w << 32) | h, Ordering::Relaxed);
                return (w as u32, h as u32);
            }
        }
        if let Some(d) = self.decoder.try_lock()
            && let Some(Child(c)) = d.as_ref()
        {
            let (w, h) = (c.width(), c.height());
            if w > 0 && h > 0 && self.feed.live() {
                self.size.store(((w as u64) << 32) | h as u64, Ordering::Relaxed);
            }
        }
        let packed = self.size.load(Ordering::Relaxed);
        ((packed >> 32) as u32, packed as u32)
    }

    /// The size the layer has: the picture's, turned upright.
    fn shown_size(&self) -> (u32, u32) {
        let (w, h) = self.picture_size();
        if self.feed.rotation() % 180 == 90 { (h, w) } else { (w, h) }
    }

    /// Draws the compositor's picture `buffer` at its own size (`size`), as decoded: the caller
    /// turns it. sRGB, as the screen capture draws its frames. Graphics thread.
    #[cfg(target_os = "macos")]
    unsafe fn draw_taken(&self, buffer: crate::bridge::Buffer, size: (u32, u32)) {
        use objc2_core_foundation::CFRetained;
        // (never waits: a size query may hold it)
        let Some(mut d) = self.drawn.try_lock() else { return };
        if size.0 == 0 || size.1 == 0 {
            return;
        }
        let changed = d.buffer.as_ref().is_none_or(|b| CFRetained::as_ptr(b) != CFRetained::as_ptr(&buffer));
        if changed {
            let Some(surface) = crate::bridge::surface_of(&buffer) else { return };
            unsafe {
                // the texture follows the newest surface: rebound while the size holds, else made again
                let rebound = !d.tex.is_null() && d.size == size && sys::gs_texture_rebind_iosurface(d.tex, surface);
                if !rebound {
                    if !d.tex.is_null() {
                        sys::gs_texture_destroy(d.tex);
                    }
                    d.tex = sys::gs_texture_create_from_iosurface(surface);
                }
            }
            d.size = size;
            d.buffer = Some(buffer);
        }
        if d.tex.is_null() {
            return;
        }
        unsafe {
            // an IOSurface is a rectangle texture on OpenGL (Metal has none)
            let rect = sys::gs_get_device_type() as u32 != sys::GS_DEVICE_METAL;
            let effect =
                sys::obs_get_base_effect(if rect { sys::OBS_EFFECT_DEFAULT_RECT } else { sys::OBS_EFFECT_DEFAULT });
            let image = sys::gs_effect_get_param_by_name(effect, c"image".as_ptr());
            let srgb = sys::gs_framebuffer_srgb_enabled();
            sys::gs_enable_framebuffer_srgb(true);
            sys::gs_effect_set_texture_srgb(image, d.tex);
            while sys::gs_effect_loop(effect, c"Draw".as_ptr()) {
                sys::gs_draw_sprite(d.tex, 0, size.0, size.1);
            }
            sys::gs_enable_framebuffer_srgb(srgb);
        }
    }
}

// SAFETY: libobs sources are reference counted and thread-safe; the rest is locked or atomic.
unsafe impl Send for Shared {}
unsafe impl Sync for Shared {}

/// A stretch of the decoder's sound on its way to the wrapper.
struct Sound {
    to: usize,
    planes: Vec<Vec<f32>>,
    frames: u32,
    timestamp: u64,
    speakers: sys::speaker_layout,
    samples_per_sec: u32,
}

// SAFETY: the weak reference is libobs's (thread-safe), the samples are owned.
unsafe impl Send for Sound {}

/// The decoder's sound, out of the wrapper (libobs mixes the wrapper, not its private decoder).
/// Passed on by a thread of its own: libobs calls this holding the decoder's locks.
unsafe extern "C" fn sound(param: *mut c_void, _: *mut sys::obs_source_t, audio: *const sys::audio_data, _muted: bool) {
    let weak = param as *mut sys::obs_weak_source_t;
    let a = unsafe { &*audio };
    let mut info = sys::obs_audio_info::default();
    if !unsafe { sys::obs_get_audio_info(&mut info) } || a.frames == 0 {
        return;
    }
    let channels = (info.speakers as usize).min(a.data.len());
    let planes = a.data[..channels]
        .iter()
        .take_while(|d| !d.is_null())
        .map(|d| unsafe { std::slice::from_raw_parts(*d as *const f32, a.frames as usize) }.to_vec())
        .collect::<Vec<_>>();
    if planes.is_empty() {
        return;
    }
    unsafe { sys::obs_weak_source_addref(weak) };
    let sound = Sound {
        to: weak as usize,
        planes,
        frames: a.frames,
        timestamp: a.timestamp,
        speakers: info.speakers,
        samples_per_sec: info.samples_per_sec,
    };
    if let Err(e) = sounds().try_send(sound) {
        unsafe { sys::obs_weak_source_release(e.into_inner().to as *mut sys::obs_weak_source_t) };
    }
}

fn sounds() -> &'static crossbeam_channel::Sender<Sound> {
    static SENDER: OnceLock<crossbeam_channel::Sender<Sound>> = OnceLock::new();
    SENDER.get_or_init(|| {
        let (tx, rx) = crossbeam_channel::bounded::<Sound>(512);
        let spawned = std::thread::Builder::new().name("lightair-phone-sound".into()).spawn(move || {
            for s in rx {
                let weak = s.to as *mut sys::obs_weak_source_t;
                let parent = unsafe { sys::obs_weak_source_get_source(weak) };
                // a track of the mixer takes the wrapper's sound itself: into its feed
                #[cfg(target_os = "macos")]
                let fed = !parent.is_null()
                    && crate::obs::doc_id(parent).and_then(crate::audio_host::slot_of).is_some_and(|slot| {
                        let planes: Vec<&[f32]> = s.planes.iter().map(Vec::as_slice).collect();
                        slot.push_planar(&planes, s.samples_per_sec)
                    });
                #[cfg(not(target_os = "macos"))]
                let fed = false;
                if fed {
                    unsafe { sys::obs_source_release(parent) };
                } else if !parent.is_null() {
                    let mut out = sys::obs_source_audio {
                        frames: s.frames,
                        speakers: s.speakers,
                        format: sys::AUDIO_FORMAT_FLOAT_PLANAR,
                        samples_per_sec: s.samples_per_sec,
                        timestamp: s.timestamp,
                        ..Default::default()
                    };
                    for (o, p) in out.data.iter_mut().zip(&s.planes) {
                        *o = p.as_ptr() as *const u8;
                    }
                    unsafe {
                        sys::obs_source_output_audio(parent, &out);
                        sys::obs_source_release(parent);
                    }
                }
                unsafe { sys::obs_weak_source_release(weak) };
            }
        });
        if spawned.is_err() {
            tracing::warn!("couldn't start the phone camera sound thread");
        }
        tx
    })
}

fn shared<'a>(data: *mut c_void) -> &'a Arc<Shared> {
    unsafe { &*(data as *const Arc<Shared>) }
}

unsafe extern "C" fn get_name(_: *mut c_void) -> *const c_char {
    c"Phone camera".as_ptr()
}

unsafe extern "C" fn create(settings: *mut sys::obs_data_t, source: *mut sys::obs_source_t) -> *mut c_void {
    let name = unsafe { CStr::from_ptr(sys::obs_source_get_name(source)) }.to_string_lossy().into_owned();
    let s = Arc::new(Shared {
        source: source as usize,
        weak: unsafe { sys::obs_source_get_weak_source(source) } as usize,
        feed: feed(&name),
        decoder: Mutex::new(None),
        text: Mutex::new(None),
        // (the first tick makes a decoder)
        seen: AtomicU64::new(u64::MAX),
        busy: AtomicBool::new(false),
        size: AtomicU64::new(0),
        #[cfg(target_os = "macos")]
        drawn: Mutex::new(Drawn { buffer: None, tex: std::ptr::null_mut(), size: (0, 0) }),
    });
    // the phone's sound straight into a track of the mixer while one takes it
    #[cfg(target_os = "macos")]
    if crate::system_audio::mixer_captures() {
        if let Some(id) = crate::obs::doc_id(source) {
            crate::audio_host::feed_slot(id);
        }
    }
    unsafe { update(&s as *const Arc<Shared> as *mut c_void, settings) };
    Box::into_raw(Box::new(s)) as *mut c_void
}

unsafe extern "C" fn destroy(data: *mut c_void) {
    let s = unsafe { Box::from_raw(data as *mut Arc<Shared>) };
    #[cfg(target_os = "macos")]
    if let Some(id) = crate::obs::doc_id(s.source as *mut sys::obs_source_t) {
        crate::audio_host::feeds_itself_no_more(id);
    }
    while s.busy.swap(true, Ordering::AcqRel) {
        std::thread::sleep(Duration::from_millis(2));
    }
    s.drop_decoder();
    s.text.lock().take();
    #[cfg(target_os = "macos")]
    {
        let mut d = s.drawn.lock();
        if !d.tex.is_null() {
            unsafe {
                sys::obs_enter_graphics();
                sys::gs_texture_destroy(d.tex);
                sys::obs_leave_graphics();
            }
            d.tex = std::ptr::null_mut();
        }
        d.buffer = None;
    }
    unsafe { sys::obs_weak_source_release(s.weak as *mut sys::obs_weak_source_t) };
}

/// The size asked of the phone stands in until a picture comes.
unsafe extern "C" fn update(data: *mut c_void, settings: *mut sys::obs_data_t) {
    let s = shared(data);
    let get = |k: &CStr| unsafe { sys::obs_data_get_int(settings, k.as_ptr()) };
    let (w, h) = (get(c"width"), get(c"height"));
    let (w, h) = if w > 0 && h > 0 { (w as u32, h as u32) } else { DEFAULT_SIZE };
    if !s.feed.live() {
        s.size.store(((w as u64) << 32) | h as u64, Ordering::Relaxed);
    }
}

unsafe extern "C" fn video_tick(data: *mut c_void, _seconds: f32) {
    let s = shared(data);
    let generation = s.feed.generation.load(Ordering::Acquire);
    if s.seen.load(Ordering::Acquire) != generation {
        s.on_worker(move |s| s.renew(generation));
        return;
    }
    let message = s.feed.message.lock().clone();
    let said = s.text.try_lock().map(|t| t.as_ref().map(|(_, m)| m.clone()));
    if let Some(said) = said
        && said.as_deref() != Some(message.as_str())
    {
        s.on_worker(move |s| s.retext(message));
    }
}

/// Fills `w` × `h` with a colour (r, g, b, a in 0–1).
unsafe fn fill(w: u32, h: u32, rgba: [f32; 4]) {
    unsafe {
        let solid = sys::obs_get_base_effect(sys::OBS_EFFECT_SOLID);
        let colour = sys::gs_effect_get_param_by_name(solid, c"color".as_ptr());
        let tech = sys::gs_effect_get_technique(solid, c"Solid".as_ptr());
        let v = sys::vec4 { __bindgen_anon_1: sys::vec4__bindgen_ty_1 { ptr: rgba } };
        sys::gs_effect_set_vec4(colour, &v);
        sys::gs_technique_begin(tech);
        sys::gs_technique_begin_pass(tech, 0);
        sys::gs_draw_sprite(std::ptr::null_mut(), 0, w, h);
        sys::gs_technique_end_pass(tech);
        sys::gs_technique_end(tech);
    }
}

/// Pushes a matrix that turns a `w` × `h` picture clockwise by `rotation` degrees, its corner
/// staying at the origin (the caller pops it).
unsafe fn push_turn(rotation: u32, w: f32, h: f32) {
    unsafe {
        sys::gs_matrix_push();
        match rotation {
            90 => {
                sys::gs_matrix_translate3f(h, 0.0, 0.0);
                sys::gs_matrix_rotaa4f(0.0, 0.0, 1.0, std::f32::consts::FRAC_PI_2);
            }
            180 => {
                sys::gs_matrix_translate3f(w, h, 0.0);
                sys::gs_matrix_rotaa4f(0.0, 0.0, 1.0, std::f32::consts::PI);
            }
            270 => {
                sys::gs_matrix_translate3f(0.0, w, 0.0);
                sys::gs_matrix_rotaa4f(0.0, 0.0, 1.0, 3.0 * std::f32::consts::FRAC_PI_2);
            }
            _ => {}
        }
    }
}

unsafe extern "C" fn video_render(data: *mut c_void, _effect: *mut sys::gs_effect_t) {
    let s = shared(data);
    let (pw, ph) = s.picture_size();
    let live = s.feed.live();
    // the compositor takes the pictures: the same surfaces drawn here, turned upright
    #[cfg(target_os = "macos")]
    {
        let taken = if live { s.feed.newest_taken() } else { None };
        match taken {
            Some(buffer) => {
                unsafe {
                    push_turn(s.feed.rotation(), pw as f32, ph as f32);
                    s.draw_taken(buffer, (pw, ph));
                    sys::gs_matrix_pop();
                }
                s.feed.on_draw();
                return;
            }
            None => {
                if let Some(mut d) = s.drawn.try_lock() {
                    d.buffer = None;
                }
            }
        }
    }
    if live
        && let Some(d) = s.decoder.try_lock()
        && let Some(Child(c)) = d.as_ref()
        && c.width() > 0
    {
        unsafe {
            push_turn(s.feed.rotation(), pw as f32, ph as f32);
            sys::obs_source_video_render(c.as_ptr());
            sys::gs_matrix_pop();
        }
        s.feed.on_draw();
        return;
    }
    // the placeholder: a dark slate, the line in the middle (at most 80 % of the width)
    let (w, h) = s.shown_size();
    if w == 0 || h == 0 {
        return;
    }
    unsafe { fill(w, h, [0.07, 0.07, 0.08, 1.0]) };
    if let Some(t) = s.text.try_lock()
        && let Some((Child(t), _)) = t.as_ref()
    {
        let (tw, th) = (t.width() as f32, t.height() as f32);
        if tw > 0.0 && th > 0.0 {
            // the line about a twentieth of the height, never wider than 80 % of the picture
            let scale = (h as f32 * 0.05 / th).min(w as f32 * 0.8 / tw);
            unsafe {
                sys::gs_matrix_push();
                sys::gs_matrix_translate3f((w as f32 - tw * scale) / 2.0, (h as f32 - th * scale) / 2.0, 0.0);
                sys::gs_matrix_scale3f(scale, scale, 1.0);
                sys::obs_source_video_render(t.as_ptr());
                sys::gs_matrix_pop();
            }
        }
    }
}

unsafe extern "C" fn get_width(data: *mut c_void) -> u32 {
    shared(data).shown_size().0
}

unsafe extern "C" fn get_height(data: *mut c_void) -> u32 {
    shared(data).shown_size().1
}

/// Registers the Phone camera source type with libobs (once; after the modules load).
pub fn register() {
    static DONE: std::sync::Once = std::sync::Once::new();
    DONE.call_once(|| unsafe {
        let mut info: sys::obs_source_info = std::mem::zeroed();
        info.id = ID.as_ptr();
        info.type_ = sys::OBS_SOURCE_TYPE_INPUT;
        info.output_flags = sys::OBS_SOURCE_VIDEO
            | sys::OBS_SOURCE_AUDIO
            | sys::OBS_SOURCE_CUSTOM_DRAW
            | sys::OBS_SOURCE_DO_NOT_DUPLICATE;
        info.get_name = Some(get_name);
        info.create = Some(create);
        info.destroy = Some(destroy);
        info.update = Some(update);
        info.video_tick = Some(video_tick);
        info.video_render = Some(video_render);
        info.get_width = Some(get_width);
        info.get_height = Some(get_height);
        info.icon_type = sys::OBS_ICON_TYPE_CAMERA;
        sys::obs_register_source_s(&info, std::mem::size_of::<sys::obs_source_info>());
    });
}

/// The placeholder line for a feed: what it waits for.
pub fn waiting_for(phone: Option<&str>) -> String {
    match phone {
        Some(p) if !p.is_empty() => format!("Waiting for {p}"),
        _ => "Pair a phone in the Inspector".to_string(),
    }
}

