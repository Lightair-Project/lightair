//! The web page source's libobs side (the page itself is `lightair_host::web`, re-exported
//! here): `lightair_web`, a shell over a [`WebPage`] that libobs shows and hides, draws
//! Chromium's shared textures where libobs shows the page (macOS: the page's own frames, not
//! while Lightair's compositor draws it; Windows: Direct3D 11 shared textures), and gives libobs
//! the frames painted into memory and the sound no track takes.

use std::ffi::{CStr, c_char, c_void};
use std::sync::Arc;

use parking_lot::Mutex;

pub use lightair_host::web::*;

use crate::sys;

/// The source a page's sinks write into, cleared when the source goes (a paint after that is
/// dropped).
#[derive(Clone)]
struct Target(Arc<Mutex<Option<usize>>>);

impl Elsewhere for Target {
    fn frame(&self, bgra: &[u8], w: u32, h: u32) {
        let held = self.0.lock();
        let Some(source) = *held else { return };
        unsafe {
            let mut frame: sys::obs_source_frame = std::mem::zeroed();
            frame.data[0] = bgra.as_ptr() as *mut u8;
            frame.linesize[0] = w * 4;
            frame.width = w;
            frame.height = h;
            frame.format = sys::VIDEO_FORMAT_BGRA;
            frame.full_range = true;
            frame.timestamp = crate::clock::now_ns();
            // (libobs copies it)
            sys::obs_source_output_video(source as *mut sys::obs_source_t, &frame);
        }
    }

    fn audio(&self, planes: &[&[f32]], rate: u32) {
        let held = self.0.lock();
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
            audio.timestamp = crate::clock::now_ns();
            // (libobs copies it)
            sys::obs_source_output_audio(source as *mut sys::obs_source_t, &audio);
        }
    }
}

struct Web {
    page: WebPage,
    target: Target,
    /// macOS: the texture libobs draws the page's shown surface with, and its size.
    #[cfg(target_os = "macos")]
    drawn: Mutex<(Option<Texture>, (u32, u32))>,
    #[cfg(windows)]
    shared: Arc<shared::Frames>,
}

#[cfg(target_os = "macos")]
struct Texture(*mut sys::gs_texture_t);
// SAFETY: only touched on libobs's graphics thread (destroyed there too).
#[cfg(target_os = "macos")]
unsafe impl Send for Texture {}

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

/// Runs `f` on a web page source's page (`source` is its libobs source; any other kind:
/// nothing).
fn with_page(source: *mut sys::obs_source_t, f: impl FnOnce(&WebPage)) {
    let web = unsafe {
        let id = sys::obs_source_get_id(source);
        if id.is_null() || CStr::from_ptr(id).to_bytes() != KIND.as_bytes() {
            return;
        }
        (sys::obs_obj_get_data(source as *mut c_void) as *const Web).as_ref()
    };
    if let Some(web) = web {
        f(&web.page);
    }
}

/// Loads a web page source's page again.
pub(crate) fn reload(source: *mut sys::obs_source_t) {
    with_page(source, |p| p.reload());
}

/// Restyles a web page source's page now (custom CSS being typed, not yet its setting).
pub(crate) fn restyle(source: *mut sys::obs_source_t, css: String) {
    with_page(source, |p| p.restyle(css));
}

unsafe extern "C" fn get_name(_: *mut c_void) -> *const c_char {
    c"Browser".as_ptr()
}

unsafe extern "C" fn create(settings: *mut sys::obs_data_t, source: *mut sys::obs_source_t) -> *mut c_void {
    // a page paints only when it changes, irregularly: its newest frame shows at once (libobs's
    // buffering, made for steady video, would hold it for a second or more)
    unsafe { sys::obs_source_set_async_unbuffered(source, true) };
    let target = Target(Arc::new(Mutex::new(Some(source as usize))));
    let id = crate::obs::doc_id(source).unwrap_or_default();
    #[cfg(windows)]
    let shared = Arc::new(shared::Frames::default());
    #[cfg(windows)]
    let texture: Option<lightair_web::TextureSink> = {
        let frames = shared.clone();
        // SAFETY: Chromium hands its surface over during its paint
        Some(Arc::new(move |surface, _w, _h| unsafe { frames.put(surface) }))
    };
    #[cfg(not(windows))]
    let texture = None;
    let elsewhere: Arc<dyn Elsewhere> = Arc::new(target.clone());
    let web = Web {
        page: WebPage::new(id, options(settings), texture, Some(elsewhere)),
        target,
        #[cfg(target_os = "macos")]
        drawn: Mutex::new((None, (0, 0))),
        #[cfg(windows)]
        shared,
    };
    Box::into_raw(Box::new(web)) as *mut c_void
}

unsafe extern "C" fn destroy(data: *mut c_void) {
    let web = unsafe { Box::from_raw(data as *mut Web) };
    // no frame after this
    web.target.0.lock().take();
    #[cfg(target_os = "macos")]
    if let Some(Texture(t)) = web.drawn.lock().0.take() {
        unsafe {
            sys::obs_enter_graphics();
            sys::gs_texture_destroy(t);
            sys::obs_leave_graphics();
        }
    }
    drop(web);
}

unsafe extern "C" fn update(data: *mut c_void, settings: *mut sys::obs_data_t) {
    let web = unsafe { &*(data as *const Web) };
    web.page.set_options(options(settings));
}

unsafe extern "C" fn show(data: *mut c_void) {
    unsafe { &*(data as *const Web) }.page.show();
}

unsafe extern "C" fn hide(data: *mut c_void) {
    unsafe { &*(data as *const Web) }.page.hide();
}

unsafe extern "C" fn video_tick(data: *mut c_void, _seconds: f32) {
    unsafe { &*(data as *const Web) }.page.tick();
}

unsafe extern "C" fn get_defaults(settings: *mut sys::obs_data_t) {
    unsafe {
        sys::obs_data_set_default_int(settings, c"width".as_ptr(), 1920);
        sys::obs_data_set_default_int(settings, c"height".as_ptr(), 1080);
        sys::obs_data_set_default_int(settings, c"fps".as_ptr(), 30);
        sys::obs_data_set_default_int(settings, c"zoom".as_ptr(), 100);
    }
}

/// Draws the page where libobs shows it (macOS): the newest painted surface, not while Lightair's
/// compositor draws the page (its frames would go unseen; the newest waits for libobs's next draw).
#[cfg(target_os = "macos")]
unsafe extern "C" fn video_render(data: *mut c_void, _effect: *mut sys::gs_effect_t) {
    let web = unsafe { &*(data as *const Web) };
    let frames = web.page.frames();
    if frames.taken() {
        return;
    }
    let mut drawn = web.drawn.lock();
    if let Some(next) = frames.take_next() {
        let size = next.size();
        let rebound = match &drawn.0 {
            Some(Texture(t)) if drawn.1 == size => unsafe { sys::gs_texture_rebind_iosurface(*t, next.ptr()) },
            _ => false,
        };
        if !rebound {
            if let Some(Texture(t)) = drawn.0.take() {
                unsafe { sys::gs_texture_destroy(t) };
            }
            let t = unsafe { sys::gs_texture_create_from_iosurface(next.ptr()) };
            drawn.0 = (!t.is_null()).then_some(Texture(t));
        }
        drawn.1 = size;
        // the previous surface goes back to Chromium now that the texture shows this one
        frames.set_shown(next);
    }
    let Some(Texture(tex)) = &drawn.0 else { return };
    let (w, h) = drawn.1;
    unsafe {
        // an IOSurface is a rectangle texture; Chromium's pixels are premultiplied (the blend
        // says so) (Metal has no rectangle textures)
        let rect = sys::gs_get_device_type() as u32 != sys::GS_DEVICE_METAL;
        let effect =
            sys::obs_get_base_effect(if rect { sys::OBS_EFFECT_DEFAULT_RECT } else { sys::OBS_EFFECT_DEFAULT });
        let image = sys::gs_effect_get_param_by_name(effect, c"image".as_ptr());
        sys::gs_effect_set_texture(image, *tex);
        sys::gs_blend_state_push();
        sys::gs_blend_function(sys::GS_BLEND_ONE, sys::GS_BLEND_INVSRCALPHA);
        while sys::gs_effect_loop(effect, c"Draw".as_ptr()) {
            sys::gs_draw_sprite(*tex, 0, w, h);
        }
        sys::gs_blend_state_pop();
    }
}

#[cfg(target_os = "macos")]
fn size(data: *mut c_void) -> (u32, u32) {
    let web = unsafe { &*(data as *const Web) };
    let shown = web.page.frames().shown_size();
    if shown.0 > 0 {
        shown
    } else {
        let o = web.page.options();
        (o.width, o.height)
    }
}

#[cfg(target_os = "macos")]
unsafe extern "C" fn get_width(data: *mut c_void) -> u32 {
    size(data).0
}

#[cfg(target_os = "macos")]
unsafe extern "C" fn get_height(data: *mut c_void) -> u32 {
    size(data).1
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
        // macOS and Windows: Chromium's shared textures, drawn as they are
        #[cfg(target_os = "macos")]
        {
            info.output_flags = sys::OBS_SOURCE_VIDEO
                | sys::OBS_SOURCE_AUDIO
                | sys::OBS_SOURCE_DO_NOT_DUPLICATE
                | sys::OBS_SOURCE_CUSTOM_DRAW;
            info.video_render = Some(video_render);
            info.get_width = Some(get_width);
            info.get_height = Some(get_height);
        }
        #[cfg(windows)]
        {
            info.output_flags = sys::OBS_SOURCE_VIDEO
                | sys::OBS_SOURCE_AUDIO
                | sys::OBS_SOURCE_DO_NOT_DUPLICATE
                | sys::OBS_SOURCE_CUSTOM_DRAW;
            info.video_render = Some(shared::video_render);
            info.get_width = Some(shared::get_width);
            info.get_height = Some(shared::get_height);
        }
        info.get_name = Some(get_name);
        info.create = Some(create);
        info.destroy = Some(destroy);
        info.update = Some(update);
        info.show = Some(show);
        info.hide = Some(hide);
        info.video_tick = Some(video_tick);
        info.get_defaults = Some(get_defaults);
        sys::obs_register_source_s(&info, std::mem::size_of::<sys::obs_source_info>());
    });
}

/// Chromium's frames as Direct3D 11 shared textures (Windows): Chromium hands each paint's
/// texture over as an NT handle valid during its paint call, so it's duplicated there and
/// opened by libobs (on its own device) when it next draws; the texture opened last is drawn
/// until a newer one arrives.
#[cfg(windows)]
mod shared {
    use std::ffi::c_void;

    use parking_lot::Mutex;
    use windows_sys::Win32::Foundation::{CloseHandle, DUPLICATE_SAME_ACCESS, DuplicateHandle, HANDLE};
    use windows_sys::Win32::System::Threading::GetCurrentProcess;

    use super::Web;
    use crate::sys;

    /// A handle of our own to a shared texture, closed when dropped.
    struct Handle(HANDLE);

    impl Drop for Handle {
        fn drop(&mut self) {
            unsafe { CloseHandle(self.0) };
        }
    }

    // SAFETY: a kernel handle, usable from any thread.
    unsafe impl Send for Handle {}

    struct Texture(*mut sys::gs_texture_t);
    // SAFETY: only touched on libobs's graphics thread (destroyed there too).
    unsafe impl Send for Texture {}

    #[derive(Default)]
    pub struct Frames {
        /// Painted, not yet opened.
        next: Mutex<Option<Handle>>,
        /// The texture drawn, and its size.
        shown: Mutex<(Option<Texture>, (u32, u32))>,
    }

    impl Frames {
        /// A new frame (Chromium's thread, during its paint): `handle` is Chromium's.
        pub fn put(&self, handle: *mut c_void) {
            if handle.is_null() {
                return;
            }
            let mut own: HANDLE = std::ptr::null_mut();
            let ok = unsafe {
                let me = GetCurrentProcess();
                DuplicateHandle(me, handle as HANDLE, me, &mut own, 0, 0, DUPLICATE_SAME_ACCESS)
            };
            if ok != 0 && !own.is_null() {
                // the one it replaces (never drawn) is let go
                *self.next.lock() = Some(Handle(own));
            }
        }
    }

    impl Drop for Frames {
        fn drop(&mut self) {
            if let Some(Texture(t)) = self.shown.get_mut().0.take() {
                unsafe {
                    sys::obs_enter_graphics();
                    sys::gs_texture_destroy(t);
                    sys::obs_leave_graphics();
                }
            }
        }
    }

    fn frames<'a>(data: *mut c_void) -> &'a Frames {
        unsafe { &(*(data as *const Web)).shared }
    }

    pub unsafe extern "C" fn video_render(data: *mut c_void, _effect: *mut sys::gs_effect_t) {
        let f = frames(data);
        let mut shown = f.shown.lock();
        if let Some(next) = f.next.lock().take() {
            // (NT handles are small numbers: libobs takes them as 32 bits)
            let t = unsafe { sys::gs_texture_open_nt_shared(next.0 as usize as u32) };
            if !t.is_null() {
                if let Some(Texture(old)) = shown.0.take() {
                    unsafe { sys::gs_texture_destroy(old) };
                }
                shown.1 = unsafe { (sys::gs_texture_get_width(t), sys::gs_texture_get_height(t)) };
                shown.0 = Some(Texture(t));
            }
        }
        let Some(Texture(tex)) = &shown.0 else { return };
        let (w, h) = shown.1;
        unsafe {
            // Chromium's pixels are premultiplied (the blend says so)
            let effect = sys::obs_get_base_effect(sys::OBS_EFFECT_DEFAULT);
            let image = sys::gs_effect_get_param_by_name(effect, c"image".as_ptr());
            sys::gs_effect_set_texture(image, *tex);
            sys::gs_blend_state_push();
            sys::gs_blend_function(sys::GS_BLEND_ONE, sys::GS_BLEND_INVSRCALPHA);
            while sys::gs_effect_loop(effect, c"Draw".as_ptr()) {
                sys::gs_draw_sprite(*tex, 0, w, h);
            }
            sys::gs_blend_state_pop();
        }
    }

    fn size(data: *mut c_void) -> (u32, u32) {
        let web = unsafe { &*(data as *const Web) };
        let shown = web.shared.shown.lock().1;
        if shown.0 > 0 {
            shown
        } else {
            let o = web.page.options();
            (o.width, o.height)
        }
    }

    pub unsafe extern "C" fn get_width(data: *mut c_void) -> u32 {
        size(data).0
    }

    pub unsafe extern "C" fn get_height(data: *mut c_void) -> u32 {
        size(data).1
    }
}
