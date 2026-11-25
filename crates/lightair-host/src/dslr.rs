//! A mirrorless or DSLR camera over USB (docs/13, "Built-in replacements"): libgphoto2 when it's
//! installed (Homebrew's `libgphoto2`, MacPorts', or a Linux distribution's), loaded at run time
//! and never shipped.

use std::ffi::{c_char, c_int, c_ulong, c_void};
use std::sync::OnceLock;

pub const KIND: &str = "lightair_dslr";

/// libgphoto2's objects (contexts, cameras, files).
pub type Ptr = *mut c_void;

/// libgphoto2's functions Lightair calls.
pub struct Lib {
    _lib: libloading::Library,
    pub context_new: unsafe extern "C" fn() -> Ptr,
    pub camera_new: unsafe extern "C" fn(*mut Ptr) -> c_int,
    pub camera_init: unsafe extern "C" fn(Ptr, Ptr) -> c_int,
    pub camera_exit: unsafe extern "C" fn(Ptr, Ptr) -> c_int,
    pub camera_unref: unsafe extern "C" fn(Ptr) -> c_int,
    pub file_new: unsafe extern "C" fn(*mut Ptr) -> c_int,
    pub file_unref: unsafe extern "C" fn(Ptr) -> c_int,
    pub capture_preview: unsafe extern "C" fn(Ptr, Ptr, Ptr) -> c_int,
    pub file_get_data_and_size: unsafe extern "C" fn(Ptr, *mut *const c_char, *mut c_ulong) -> c_int,
}

unsafe impl Send for Lib {}
unsafe impl Sync for Lib {}

/// libgphoto2, loaded the first time it's asked for (`None`: not installed).
pub fn lib() -> Option<&'static Lib> {
    static LIB: OnceLock<Option<Lib>> = OnceLock::new();
    LIB.get_or_init(|| {
        let names = if cfg!(target_os = "macos") {
            vec![
                "/opt/homebrew/lib/libgphoto2.6.dylib",
                "/usr/local/lib/libgphoto2.6.dylib",
                "/opt/local/lib/libgphoto2.6.dylib",
                "libgphoto2.6.dylib",
            ]
        } else if cfg!(windows) {
            vec!["libgphoto2-6.dll"]
        } else {
            vec!["libgphoto2.so.6", "libgphoto2.so"]
        };
        for name in names {
            let Ok(l) = (unsafe { libloading::Library::new(name) }) else { continue };
            let lib = unsafe {
                macro_rules! sym {
                    ($n:literal) => {
                        match l.get($n) {
                            Ok(s) => *s,
                            Err(_) => continue,
                        }
                    };
                }
                Lib {
                    context_new: sym!(b"gp_context_new\0"),
                    camera_new: sym!(b"gp_camera_new\0"),
                    camera_init: sym!(b"gp_camera_init\0"),
                    camera_exit: sym!(b"gp_camera_exit\0"),
                    camera_unref: sym!(b"gp_camera_unref\0"),
                    file_new: sym!(b"gp_file_new\0"),
                    file_unref: sym!(b"gp_file_unref\0"),
                    capture_preview: sym!(b"gp_camera_capture_preview\0"),
                    file_get_data_and_size: sym!(b"gp_file_get_data_and_size\0"),
                    _lib: l,
                }
            };
            tracing::info!(name, "libgphoto2 loaded");
            return Some(lib);
        }
        None
    })
    .as_ref()
}

/// libgphoto2 is installed.
pub fn available() -> bool {
    lib().is_some()
}

/// A camera over USB's live view as a source's pictures ([`crate::frames`]): the first camera
/// libgphoto2 finds, found again when it goes away, until dropped. macOS.
#[cfg(target_os = "macos")]
pub struct LiveView {
    id: lightair_model::Id,
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

#[cfg(target_os = "macos")]
impl LiveView {
    pub fn start(id: lightair_model::Id) -> LiveView {
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let s = stop.clone();
        let thread = std::thread::Builder::new().name("lightair-dslr".into()).spawn(move || run(id, s)).ok();
        LiveView { id, stop, thread }
    }
}

#[cfg(target_os = "macos")]
impl Drop for LiveView {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
        crate::frames::forget(self.id);
    }
}

/// The live view's loop: a camera, then its previews as pictures, until stopped.
#[cfg(target_os = "macos")]
fn run(id: lightair_model::Id, stop: std::sync::Arc<std::sync::atomic::AtomicBool>) {
    use std::sync::atomic::Ordering;
    use std::time::Duration;
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
            if ok
                && unsafe { (l.file_get_data_and_size)(file, &mut data, &mut size) } >= 0
                && !data.is_null()
                && size > 0
            {
                let jpeg = unsafe { std::slice::from_raw_parts(data as *const u8, size as usize) };
                match image::load_from_memory_with_format(jpeg, image::ImageFormat::Jpeg) {
                    Ok(img) => {
                        failures = 0;
                        let rgba = img.to_rgba8();
                        let (w, h) = rgba.dimensions();
                        if let Some(buffer) = crate::buffers::from_rgba(&rgba, w, h) {
                            crate::frames::publish(id, buffer);
                        }
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
