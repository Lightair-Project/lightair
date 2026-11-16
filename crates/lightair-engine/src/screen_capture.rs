//! Screen captures that run only while they're shown (macOS): `lightair_screen_capture` wraps
//! OBS's `screen_capture` (ScreenCaptureKit), which captures from the moment it's made whether
//! anything shows it or not. Each running capture has the window server compose a copy of the
//! whole display every frame: a workspace with a display capture in each of five scenes kept
//! the window server at twice its load, the machine sluggish, with one of them on air.
//!
//! The wrapper makes its capture when something shows it (on air, a preview, a thumbnail) and
//! lets it go a few seconds after nothing does (so cutting back and forth doesn't restart it).
//! While stopped it keeps its last size, so layouts don't jump.

use std::ffi::{CStr, CString, c_char, c_void};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use parking_lot::Mutex;

use crate::sys;

/// The libobs source type.
pub const ID: &CStr = c"lightair_screen_capture";
/// The capture it runs.
const INNER: &CStr = c"screen_capture";
/// How long a capture keeps running once nothing shows it.
const LINGER: Duration = Duration::from_secs(5);

struct Child(*mut sys::obs_source_t);
// SAFETY: libobs sources are reference counted and thread-safe.
unsafe impl Send for Child {}

struct Shared {
    source: usize,
    /// Its current settings (a reference held here).
    settings: Mutex<usize>,
    child: Mutex<Option<Child>>,
    /// A capture is being made or let go of (on a thread of its own).
    busy: AtomicBool,
    hidden_since: Mutex<Option<Instant>>,
    /// The last size seen (width << 32 | height).
    size: AtomicU64,
}

impl Shared {
    fn settings(&self) -> *mut sys::obs_data_t {
        *self.settings.lock() as *mut sys::obs_data_t
    }

    /// Makes the capture (any thread).
    fn start(self: &Arc<Self>) {
        let parent = self.source as *mut sys::obs_source_t;
        let name = unsafe { CStr::from_ptr(sys::obs_source_get_name(parent)) }.to_string_lossy().into_owned();
        let name = CString::new(format!("{name} (capture)")).unwrap_or_default();
        // (held while it's used: an update may replace them meanwhile)
        let settings = {
            let s = self.settings.lock();
            unsafe { sys::obs_data_addref(*s as *mut sys::obs_data_t) };
            *s as *mut sys::obs_data_t
        };
        let child = unsafe { sys::obs_source_create_private(INNER.as_ptr(), name.as_ptr(), settings) };
        unsafe { sys::obs_data_release(settings) };
        if !child.is_null() {
            // it draws only while it counts as shown
            unsafe { sys::obs_source_inc_showing(child) };
            *self.child.lock() = Some(Child(child));
        }
        self.busy.store(false, Ordering::Release);
    }

    /// Lets the capture go (any thread).
    fn stop(self: &Arc<Self>) {
        let child = self.child.lock().take();
        if let Some(Child(c)) = child {
            unsafe {
                sys::obs_source_dec_showing(c);
                sys::obs_source_release(c);
            }
        }
        self.busy.store(false, Ordering::Release);
    }

    fn on_worker(self: &Arc<Self>, start: bool) {
        if self.busy.swap(true, Ordering::AcqRel) {
            return;
        }
        let me = self.clone();
        // (making or releasing a source from inside libobs's tick isn't safe: a thread of its own)
        let spawned = std::thread::Builder::new().name("lightair-screen-capture".into()).spawn(move || {
            if start { me.start() } else { me.stop() }
        });
        if spawned.is_err() {
            self.busy.store(false, Ordering::Release);
        }
    }
}

fn shared<'a>(data: *mut c_void) -> &'a Arc<Shared> {
    unsafe { &*(data as *const Arc<Shared>) }
}

unsafe extern "C" fn get_name(_: *mut c_void) -> *const c_char {
    c"Screen capture".as_ptr()
}

unsafe extern "C" fn create(settings: *mut sys::obs_data_t, source: *mut sys::obs_source_t) -> *mut c_void {
    unsafe { sys::obs_data_addref(settings) };
    let s = Arc::new(Shared {
        source: source as usize,
        settings: Mutex::new(settings as usize),
        child: Mutex::new(None),
        busy: AtomicBool::new(false),
        hidden_since: Mutex::new(None),
        size: AtomicU64::new(0),
    });
    Box::into_raw(Box::new(s)) as *mut c_void
}

unsafe extern "C" fn destroy(data: *mut c_void) {
    let s = unsafe { Box::from_raw(data as *mut Arc<Shared>) };
    // a capture being made finishes first
    while s.busy.load(Ordering::Acquire) {
        std::thread::sleep(Duration::from_millis(2));
    }
    s.busy.store(true, Ordering::Release);
    s.stop();
    unsafe { sys::obs_data_release(s.settings()) };
}

unsafe extern "C" fn update(data: *mut c_void, settings: *mut sys::obs_data_t) {
    let s = shared(data);
    unsafe { sys::obs_data_addref(settings) };
    let old = std::mem::replace(&mut *s.settings.lock(), settings as usize);
    unsafe { sys::obs_data_release(old as *mut sys::obs_data_t) };
    if let Some(Child(c)) = s.child.lock().as_ref() {
        unsafe { sys::obs_source_update(*c, settings) };
    }
}

/// Starts the capture when something shows it, stops it a while after nothing does.
unsafe extern "C" fn video_tick(data: *mut c_void, _seconds: f32) {
    let s = shared(data);
    let shown = unsafe { sys::obs_source_showing(s.source as *mut sys::obs_source_t) };
    let running = s.child.lock().is_some();
    if shown {
        *s.hidden_since.lock() = None;
        if !running {
            s.on_worker(true);
        }
    } else if running {
        let mut since = s.hidden_since.lock();
        let at = *since.get_or_insert_with(Instant::now);
        if at.elapsed() >= LINGER {
            *since = None;
            drop(since);
            s.on_worker(false);
        }
    }
}

unsafe extern "C" fn video_render(data: *mut c_void, _effect: *mut sys::gs_effect_t) {
    let s = shared(data);
    // (never waits on a capture being made)
    let Some(child) = s.child.try_lock() else { return };
    if let Some(Child(c)) = child.as_ref() {
        unsafe { sys::obs_source_video_render(*c) };
    }
}

fn size(data: *mut c_void) -> (u32, u32) {
    let s = shared(data);
    if let Some(child) = s.child.try_lock()
        && let Some(Child(c)) = child.as_ref()
    {
        let (w, h) = unsafe { (sys::obs_source_get_width(*c), sys::obs_source_get_height(*c)) };
        if w > 0 && h > 0 {
            s.size.store(((w as u64) << 32) | h as u64, Ordering::Relaxed);
        }
    }
    let packed = s.size.load(Ordering::Relaxed);
    ((packed >> 32) as u32, packed as u32)
}

unsafe extern "C" fn get_width(data: *mut c_void) -> u32 {
    size(data).0
}

unsafe extern "C" fn get_height(data: *mut c_void) -> u32 {
    size(data).1
}

/// Registers the source with libobs (once; after the modules load).
pub fn register() {
    static DONE: std::sync::Once = std::sync::Once::new();
    DONE.call_once(|| unsafe {
        let mut info: sys::obs_source_info = std::mem::zeroed();
        info.id = ID.as_ptr();
        info.type_ = sys::OBS_SOURCE_TYPE_INPUT;
        info.output_flags = sys::OBS_SOURCE_VIDEO | sys::OBS_SOURCE_CUSTOM_DRAW | sys::OBS_SOURCE_DO_NOT_DUPLICATE;
        info.get_name = Some(get_name);
        info.create = Some(create);
        info.destroy = Some(destroy);
        info.update = Some(update);
        info.video_tick = Some(video_tick);
        info.video_render = Some(video_render);
        info.get_width = Some(get_width);
        info.get_height = Some(get_height);
        info.icon_type = sys::OBS_ICON_TYPE_DESKTOP_CAPTURE;
        sys::obs_register_source_s(&info, std::mem::size_of::<sys::obs_source_info>());
    });
}
