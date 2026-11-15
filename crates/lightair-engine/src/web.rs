//! Web pages as sources (`lightair_web`): a page Chromium renders off-screen
//! (`lightair_web`, one Chromium on every platform, bundled with Lightair), its frames given to
//! libobs as they're painted, like a camera's; its sound goes to libobs too (Chromium doesn't
//! play it), so it reaches the mixer like any other source's.
//!
//! Settings: `url`; `width`, `height` (the frame's size: the page's in CSS pixels at 100%);
//! `fps`; `zoom` (percent, as a browser's); `css` (added to the page each time it loads).

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

fn string(settings: *mut sys::obs_data_t, key: &CStr) -> String {
    let s = unsafe { sys::obs_data_get_string(settings, key.as_ptr()) };
    if s.is_null() { String::new() } else { unsafe { CStr::from_ptr(s) }.to_string_lossy().into_owned() }
}

fn options(settings: *mut sys::obs_data_t) -> lightair_web::PageOptions {
    unsafe {
        lightair_web::PageOptions {
            url: string(settings, c"url"),
            width: sys::obs_data_get_int(settings, c"width".as_ptr()).clamp(16, 7680) as u32,
            height: sys::obs_data_get_int(settings, c"height".as_ptr()).clamp(16, 7680) as u32,
            fps: sys::obs_data_get_int(settings, c"fps".as_ptr()).clamp(1, 60) as u32,
            zoom: sys::obs_data_get_int(settings, c"zoom".as_ptr()).clamp(25, 500) as f32 / 100.0,
            css: string(settings, c"css"),
        }
    }
}

fn open(o: &lightair_web::PageOptions, target: &Target) -> Option<lightair_web::Page> {
    if o.url.trim().is_empty() {
        return None;
    }
    let audio_target = target.clone();
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
    let target = audio_target;
    let audio: lightair_web::AudioSink = Arc::new(move |planes: &[&[f32]], rate: u32| {
        let held = target.0.lock();
        let Some(source) = *held else { return };
        let Some(frames) = planes.first().map(|p| p.len()) else { return };
        unsafe {
            let mut audio: sys::obs_source_audio = std::mem::zeroed();
            for (slot, plane) in audio.data.iter_mut().zip(planes) {
                *slot = plane.as_ptr() as *const u8;
            }
            audio.frames = frames as u32;
            audio.speakers = match planes.len() {
                1 => sys::SPEAKERS_MONO,
                2 => sys::SPEAKERS_STEREO,
                3 => sys::SPEAKERS_2POINT1,
                4 => sys::SPEAKERS_4POINT0,
                5 => sys::SPEAKERS_4POINT1,
                6 => sys::SPEAKERS_5POINT1,
                _ => sys::SPEAKERS_7POINT1,
            };
            audio.format = sys::AUDIO_FORMAT_FLOAT_PLANAR;
            audio.samples_per_sec = rate;
            audio.timestamp = sys::os_gettime_ns();
            // (libobs copies it)
            sys::obs_source_output_audio(source as *mut sys::obs_source_t, &audio);
        }
    });
    Some(lightair_web::Page::open(o.clone(), sink, audio))
}

/// The page of a web page source (`source` is its libobs source; any other kind: none).
fn page_of<'a>(source: *mut sys::obs_source_t) -> Option<&'a lightair_web::Page> {
    unsafe {
        let id = sys::obs_source_get_id(source);
        if id.is_null() || CStr::from_ptr(id).to_bytes() != KIND.as_bytes() {
            return None;
        }
        let data = sys::obs_obj_get_data(source as *mut c_void) as *const Web;
        data.as_ref().and_then(|web| web.page.as_ref())
    }
}

/// Loads a web page source's page again.
pub(crate) fn reload(source: *mut sys::obs_source_t) {
    if let Some(page) = page_of(source) {
        page.reload();
    }
}

/// Restyles a web page source's page now (custom CSS being typed, not yet its setting).
pub(crate) fn restyle(source: *mut sys::obs_source_t, css: String) {
    if let Some(page) = page_of(source) {
        page.restyle(css);
    }
}

unsafe extern "C" fn get_name(_: *mut c_void) -> *const c_char {
    c"Browser".as_ptr()
}

unsafe extern "C" fn create(settings: *mut sys::obs_data_t, source: *mut sys::obs_source_t) -> *mut c_void {
    // a page paints only when it changes, irregularly: its newest frame shows at once (libobs's
    // buffering, made for steady video, would hold it for a second or more)
    unsafe { sys::obs_source_set_async_unbuffered(source, true) };
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
        sys::obs_data_set_default_int(settings, c"zoom".as_ptr(), 100);
    }
}

/// Registers the source with libobs (once; after startup).
pub fn register() {
    static DONE: std::sync::Once = std::sync::Once::new();
    DONE.call_once(|| unsafe {
        let mut info: sys::obs_source_info = std::mem::zeroed();
        info.id = c"lightair_web".as_ptr();
        info.type_ = sys::OBS_SOURCE_TYPE_INPUT;
        // frames as they're painted, kept until the next (a still page paints once); its sound
        info.output_flags = sys::OBS_SOURCE_ASYNC_VIDEO | sys::OBS_SOURCE_AUDIO | sys::OBS_SOURCE_DO_NOT_DUPLICATE;
        info.get_name = Some(get_name);
        info.create = Some(create);
        info.destroy = Some(destroy);
        info.update = Some(update);
        info.get_defaults = Some(get_defaults);
        sys::obs_register_source_s(&info, std::mem::size_of::<sys::obs_source_info>());
    });
}
