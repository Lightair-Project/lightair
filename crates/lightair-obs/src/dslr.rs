//! A mirrorless or DSLR camera over USB (docs/13, "Built-in replacements"): its live view as a
//! source (`lightair_dslr`), through libgphoto2 when it's installed (Homebrew's `libgphoto2`,
//! MacPorts', or a Linux distribution's). Lightair loads it at run time and never ships it; the
//! first camera libgphoto2 finds is used. On a Mac, macOS's own photo daemon (ptpcamerad) may
//! hold the camera first: quitting Image Capture and Photos lets libgphoto2 have it.

use std::ffi::{c_char, c_ulong, c_void};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use crate::sys;

pub use lightair_host::dslr::{KIND, available};
use lightair_host::dslr::{Ptr, lib};

struct Dslr {
    source: *mut sys::obs_source_t,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

struct SrcPtr(*mut sys::obs_source_t);
unsafe impl Send for SrcPtr {}

fn run(src: SrcPtr, stop: Arc<AtomicBool>) {
    let Some(l) = lib() else { return };
    while !stop.load(Ordering::Relaxed) {
        // a camera (again, after one goes away)
        let ctx = unsafe { (l.context_new)() };
        let mut cam: Ptr = std::ptr::null_mut();
        if unsafe { (l.camera_new)(&mut cam) } < 0 || cam.is_null() {
            std::thread::sleep(Duration::from_secs(2));
            continue;
        }
        if unsafe { (l.camera_init)(cam, ctx) } < 0 {
            unsafe { (l.camera_unref)(cam) };
            std::thread::sleep(Duration::from_secs(2));
            continue;
        }
        tracing::info!("a camera's live view, through libgphoto2");
        let mut failures = 0;
        while !stop.load(Ordering::Relaxed) && failures < 20 {
            let mut file: Ptr = std::ptr::null_mut();
            if unsafe { (l.file_new)(&mut file) } < 0 {
                break;
            }
            let ok = unsafe { (l.capture_preview)(cam, file, ctx) } >= 0;
            let mut data: *const c_char = std::ptr::null();
            let mut size: c_ulong = 0;
            if ok && unsafe { (l.file_get_data_and_size)(file, &mut data, &mut size) } >= 0 && !data.is_null() && size > 0 {
                let jpeg = unsafe { std::slice::from_raw_parts(data as *const u8, size as usize) };
                match image::load_from_memory_with_format(jpeg, image::ImageFormat::Jpeg) {
                    Ok(img) => {
                        failures = 0;
                        let rgba = img.to_rgba8();
                        let (w, h) = rgba.dimensions();
                        let mut frame: sys::obs_source_frame = unsafe { std::mem::zeroed() };
                        frame.data[0] = rgba.as_ptr() as *mut u8;
                        frame.linesize[0] = w * 4;
                        frame.width = w;
                        frame.height = h;
                        frame.format = sys::VIDEO_FORMAT_RGBA;
                        frame.full_range = true;
                        frame.timestamp = crate::clock::now_ns();
                        unsafe { sys::obs_source_output_video(src.0, &frame) };
                    }
                    Err(_) => failures += 1,
                }
            } else {
                failures += 1;
                std::thread::sleep(Duration::from_millis(100));
            }
            unsafe { (l.file_unref)(file) };
        }
        unsafe {
            (l.camera_exit)(cam, ctx);
            (l.camera_unref)(cam);
        }
    }
}

unsafe extern "C" fn get_name(_: *mut c_void) -> *const c_char {
    c"Camera over USB".as_ptr()
}

unsafe extern "C" fn create(_: *mut sys::obs_data_t, source: *mut sys::obs_source_t) -> *mut c_void {
    let stop = Arc::new(AtomicBool::new(false));
    let s = stop.clone();
    let p = SrcPtr(source);
    let thread = std::thread::Builder::new().name("lightair-dslr".into()).spawn(move || run(p, s)).ok();
    Box::into_raw(Box::new(Dslr { source, stop, thread })) as *mut c_void
}

unsafe extern "C" fn destroy(data: *mut c_void) {
    let mut d = unsafe { Box::from_raw(data as *mut Dslr) };
    let _ = d.source;
    d.stop.store(true, Ordering::Relaxed);
    if let Some(t) = d.thread.take() {
        let _ = t.join();
    }
}

pub fn register() {
    static DONE: std::sync::Once = std::sync::Once::new();
    DONE.call_once(|| unsafe {
        let mut info: sys::obs_source_info = std::mem::zeroed();
        info.id = c"lightair_dslr".as_ptr();
        info.type_ = sys::OBS_SOURCE_TYPE_INPUT;
        info.output_flags = sys::OBS_SOURCE_ASYNC_VIDEO | sys::OBS_SOURCE_DO_NOT_DUPLICATE;
        info.get_name = Some(get_name);
        info.create = Some(create);
        info.destroy = Some(destroy);
        sys::obs_register_source_s(&info, std::mem::size_of::<sys::obs_source_info>());
    });
}
