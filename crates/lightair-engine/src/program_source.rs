//! macOS, with `LIGHTAIR_ENGINE=own`: `lightair_program`, a private libobs source that draws the
//! newest picture Lightair's own compositor made of a canvas's program (a BGRA IOSurface), so
//! libobs's encoders and outputs can take the compositor's picture until they're replaced.
//!
//! The compositor hands each finished frame over with [`publish`]; the source draws the newest
//! one as a libobs texture over its IOSurface (made once, then rebound to each new surface), at
//! the canvas's size. It holds the buffer it draws, so the compositor doesn't paint over it.

use std::collections::HashMap;
use std::ffi::{CStr, c_char, c_void};

use lightair_model::Id;
use objc2_core_foundation::CFRetained;
use objc2_core_video::{CVPixelBuffer, CVPixelBufferGetHeight, CVPixelBufferGetWidth};
use parking_lot::Mutex;

use crate::bridge::surface_of;
use crate::obs::{Data, ObsSource, SourceRef};
use crate::sys;

/// The libobs source type.
pub const ID: &CStr = c"lightair_program";

type Buffer = CFRetained<CVPixelBuffer>;

/// The newest finished program picture of each canvas.
struct Programs(Option<HashMap<Id, Buffer>>);

// SAFETY: CoreVideo's buffers are reference counted thread-safely, and a finished picture isn't
// written while it's handed over.
unsafe impl Send for Programs {}
unsafe impl Sync for Programs {}

static PROGRAMS: Mutex<Programs> = Mutex::new(Programs(None));

/// Hands `frame` (BGRA, IOSurface-backed, complete on the GPU) over as `canvas`'s newest program
/// picture. Any thread.
pub fn publish(canvas: Id, frame: &Buffer) {
    PROGRAMS.lock().0.get_or_insert_with(HashMap::new).insert(canvas, frame.clone());
}

/// Forgets `canvas`'s picture (its compositor stopped): its sources draw nothing new.
pub fn withdraw(canvas: Id) {
    if let Some(p) = PROGRAMS.lock().0.as_mut() {
        p.remove(&canvas);
    }
}

fn newest(canvas: Id) -> Option<Buffer> {
    PROGRAMS.lock().0.as_ref()?.get(&canvas).cloned()
}

/// A private `lightair_program` source drawing `canvas`'s compositor picture (registered with
/// libobs first if it isn't yet). Engine thread.
pub fn create(canvas: Id) -> Option<SourceRef> {
    register();
    let mut settings = Data::new();
    settings.set_str("canvas", &canvas.to_string());
    ObsSource::create_private(ID.to_str().ok()?, "Lightair program", Some(&settings))?.new_ref()
}

// ------------------------------------------------------------------ the libobs source

/// What the source shows: the buffer it draws and a libobs texture over its surface.
struct Shown {
    buffer: Option<Buffer>,
    tex: *mut sys::gs_texture_t,
    size: (u32, u32),
}

struct Program {
    canvas: Mutex<Option<Id>>,
    shown: Mutex<Shown>,
}

// SAFETY: the texture is only touched on libobs's graphics thread (or inside its context when
// the source is destroyed); the pixel buffers are reference counted thread-safely.
unsafe impl Send for Program {}
unsafe impl Sync for Program {}

fn program<'a>(data: *mut c_void) -> &'a Program {
    unsafe { &*(data as *const Program) }
}

fn canvas_of(settings: *mut sys::obs_data_t) -> Option<Id> {
    let s = unsafe { sys::obs_data_get_string(settings, c"canvas".as_ptr()) };
    if s.is_null() {
        return None;
    }
    uuid::Uuid::parse_str(&unsafe { CStr::from_ptr(s) }.to_string_lossy()).ok().map(Id)
}

unsafe extern "C" fn get_name(_: *mut c_void) -> *const c_char {
    c"Lightair program".as_ptr()
}

unsafe extern "C" fn create_cb(settings: *mut sys::obs_data_t, _source: *mut sys::obs_source_t) -> *mut c_void {
    let p = Program {
        canvas: Mutex::new(canvas_of(settings)),
        shown: Mutex::new(Shown { buffer: None, tex: std::ptr::null_mut(), size: (0, 0) }),
    };
    Box::into_raw(Box::new(p)) as *mut c_void
}

unsafe extern "C" fn destroy(data: *mut c_void) {
    let p = unsafe { Box::from_raw(data as *mut Program) };
    let mut shown = p.shown.lock();
    if !shown.tex.is_null() {
        unsafe {
            sys::obs_enter_graphics();
            sys::gs_texture_destroy(shown.tex);
            sys::obs_leave_graphics();
        }
        shown.tex = std::ptr::null_mut();
    }
    shown.buffer = None;
}

unsafe extern "C" fn update(data: *mut c_void, settings: *mut sys::obs_data_t) {
    *program(data).canvas.lock() = canvas_of(settings);
}

unsafe extern "C" fn video_render(data: *mut c_void, _effect: *mut sys::gs_effect_t) {
    let p = program(data);
    let Some(canvas) = *p.canvas.lock() else { return };
    let mut shown = p.shown.lock();
    let newest = newest(canvas);
    let changed = match (&newest, &shown.buffer) {
        (Some(n), Some(s)) => CFRetained::as_ptr(n) != CFRetained::as_ptr(s),
        (Some(_), None) => true,
        (None, _) => false,
    };
    if changed && let Some(buffer) = newest {
        let size = (CVPixelBufferGetWidth(&buffer) as u32, CVPixelBufferGetHeight(&buffer) as u32);
        let Some(surface) = surface_of(&buffer) else { return };
        unsafe {
            // the texture follows the newest surface: rebound while the size holds, else made again
            let rebound =
                !shown.tex.is_null() && shown.size == size && sys::gs_texture_rebind_iosurface(shown.tex, surface);
            if !rebound {
                if !shown.tex.is_null() {
                    sys::gs_texture_destroy(shown.tex);
                }
                shown.tex = sys::gs_texture_create_from_iosurface(surface);
            }
        }
        shown.size = size;
        shown.buffer = Some(buffer);
    }
    if shown.tex.is_null() || shown.buffer.is_none() {
        return;
    }
    let (w, h) = canvas_size(shown.size);
    unsafe {
        // an IOSurface is a rectangle texture on OpenGL (Metal has none)
        let rect = sys::gs_get_device_type() as u32 != sys::GS_DEVICE_METAL;
        let effect =
            sys::obs_get_base_effect(if rect { sys::OBS_EFFECT_DEFAULT_RECT } else { sys::OBS_EFFECT_DEFAULT });
        let image = sys::gs_effect_get_param_by_name(effect, c"image".as_ptr());
        sys::gs_effect_set_texture(image, shown.tex);
        while sys::gs_effect_loop(effect, c"Draw".as_ptr()) {
            sys::gs_draw_sprite(shown.tex, 0, w, h);
        }
    }
}

/// The size the source draws at: libobs's main canvas, else the picture's own.
fn canvas_size(picture: (u32, u32)) -> (u32, u32) {
    let mut ovi = sys::obs_video_info::default();
    if unsafe { sys::obs_get_video_info(&mut ovi) } && ovi.base_width > 0 && ovi.base_height > 0 {
        (ovi.base_width, ovi.base_height)
    } else {
        picture
    }
}

unsafe extern "C" fn get_width(data: *mut c_void) -> u32 {
    canvas_size(program(data).shown.lock().size).0
}

unsafe extern "C" fn get_height(data: *mut c_void) -> u32 {
    canvas_size(program(data).shown.lock().size).1
}

/// Registers the source type with libobs (once; after startup). Hidden from the source types
/// the app lists.
pub fn register() {
    static DONE: std::sync::Once = std::sync::Once::new();
    DONE.call_once(|| unsafe {
        let mut info: sys::obs_source_info = std::mem::zeroed();
        info.id = ID.as_ptr();
        info.type_ = sys::OBS_SOURCE_TYPE_INPUT;
        info.output_flags = sys::OBS_SOURCE_VIDEO
            | sys::OBS_SOURCE_CUSTOM_DRAW
            | sys::OBS_SOURCE_DO_NOT_DUPLICATE
            | sys::OBS_SOURCE_CAP_DISABLED;
        info.get_name = Some(get_name);
        info.create = Some(create_cb);
        info.destroy = Some(destroy);
        info.update = Some(update);
        info.video_render = Some(video_render);
        info.get_width = Some(get_width);
        info.get_height = Some(get_height);
        sys::obs_register_source_s(&info, std::mem::size_of::<sys::obs_source_info>());
    });
}
