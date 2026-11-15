//! Web pages as sources (`lightair_web`): a page Chromium renders off-screen
//! (`lightair_web`, one Chromium on every platform, bundled with Lightair), its frames given to
//! libobs as they're painted, like a camera's.
//!
//! Settings: `url`; `width`, `height` (the page's size in CSS pixels, and the frame's); `fps`.

use std::ffi::{CStr, c_char, c_void};
use std::sync::Arc;

use parking_lot::Mutex;

use crate::sys;

pub const KIND: &str = "lightair_web";

/// The source a page paints into, cleared when the source goes (a paint after that is dropped).
#[derive(Clone)]
struct Target(Arc<Mutex<Option<usize>>>);

struct Web {
    page: Option<lightair_web::Page>,
    target: Target,
    options: lightair_web::PageOptions,
}

fn options(settings: *mut sys::obs_data_t) -> lightair_web::PageOptions {
    unsafe {
        let url = sys::obs_data_get_string(settings, c"url".as_ptr());
        let url = if url.is_null() { String::new() } else { CStr::from_ptr(url).to_string_lossy().into_owned() };
        lightair_web::PageOptions {
            url,
            width: sys::obs_data_get_int(settings, c"width".as_ptr()).clamp(16, 7680) as u32,
            height: sys::obs_data_get_int(settings, c"height".as_ptr()).clamp(16, 7680) as u32,
            fps: sys::obs_data_get_int(settings, c"fps".as_ptr()).clamp(1, 60) as u32,
        }
    }
}

fn open(o: &lightair_web::PageOptions, target: &Target) -> Option<lightair_web::Page> {
    if o.url.trim().is_empty() {
        return None;
    }
    let target = target.clone();
    let sink: lightair_web::FrameSink = Arc::new(move |bgra: &[u8], w: u32, h: u32| {
        let held = target.0.lock();
        let Some(source) = *held else { return };
        unsafe {
            let mut frame: sys::obs_source_frame = std::mem::zeroed();
            frame.data[0] = bgra.as_ptr() as *mut u8;
            frame.linesize[0] = w * 4;
            frame.width = w;
            frame.height = h;
            frame.format = sys::VIDEO_FORMAT_BGRA;
            frame.full_range = true;
            frame.timestamp = sys::os_gettime_ns();
            // (libobs copies it)
            sys::obs_source_output_video(source as *mut sys::obs_source_t, &frame);
        }
    });
    Some(lightair_web::Page::open(o.clone(), sink))
}

unsafe extern "C" fn get_name(_: *mut c_void) -> *const c_char {
    c"Web page".as_ptr()
}

unsafe extern "C" fn create(settings: *mut sys::obs_data_t, source: *mut sys::obs_source_t) -> *mut c_void {
    let target = Target(Arc::new(Mutex::new(Some(source as usize))));
    let o = options(settings);
    let page = open(&o, &target);
    Box::into_raw(Box::new(Web { page, target, options: o })) as *mut c_void
}

unsafe extern "C" fn destroy(data: *mut c_void) {
    let web = unsafe { Box::from_raw(data as *mut Web) };
    // no frame after this
    *web.target.0.lock() = None;
    drop(web);
}

unsafe extern "C" fn update(data: *mut c_void, settings: *mut sys::obs_data_t) {
    let web = unsafe { &mut *(data as *mut Web) };
    let o = options(settings);
    if o == web.options {
        return;
    }
    match &web.page {
        Some(page) if !o.url.trim().is_empty() => page.update(o.clone()),
        _ => web.page = open(&o, &web.target),
    }
    web.options = o;
}

unsafe extern "C" fn get_defaults(settings: *mut sys::obs_data_t) {
    unsafe {
        sys::obs_data_set_default_int(settings, c"width".as_ptr(), 1920);
        sys::obs_data_set_default_int(settings, c"height".as_ptr(), 1080);
        sys::obs_data_set_default_int(settings, c"fps".as_ptr(), 30);
    }
}

/// Registers the source with libobs (once; after startup).
pub fn register() {
    static DONE: std::sync::Once = std::sync::Once::new();
    DONE.call_once(|| unsafe {
        let mut info: sys::obs_source_info = std::mem::zeroed();
        info.id = c"lightair_web".as_ptr();
        info.type_ = sys::OBS_SOURCE_TYPE_INPUT;
        // frames as they're painted, kept until the next (a still page paints once)
        info.output_flags = sys::OBS_SOURCE_ASYNC_VIDEO | sys::OBS_SOURCE_DO_NOT_DUPLICATE;
        info.get_name = Some(get_name);
        info.create = Some(create);
        info.destroy = Some(destroy);
        info.update = Some(update);
        info.get_defaults = Some(get_defaults);
        sys::obs_register_source_s(&info, std::mem::size_of::<sys::obs_source_info>());
    });
}
