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
