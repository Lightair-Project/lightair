//! Where the engine's binaries live: libobs, graphics backends and OBS plugins.
//!
//! The location is discovered from the libobs image that is actually loaded, so the same code
//! works inside `Lightair.app` (Contents/Frameworks + Contents/PlugIns) and in development,
//! where libobs comes from an installed OBS.

use std::ffi::{CStr, c_void};
use std::path::{Path, PathBuf};

use libobs_sys as sys;

#[derive(Debug, Clone)]
pub struct Runtime {
    /// Directory holding libobs.framework and the graphics backends.
    pub frameworks: PathBuf,
    /// Directory holding `<module>.plugin` bundles.
    pub plugins: PathBuf,
    /// True when the running executable is inside the same bundle as libobs.
    pub in_bundle: bool,
}

impl Runtime {
    pub fn detect() -> Result<Self, String> {
        let lib = loaded_libobs_path().ok_or("could not locate the loaded libobs image")?;
        let frameworks = lib
            .ancestors()
            .find(|p| p.file_name().is_some_and(|n| n == "libobs.framework"))
            .and_then(Path::parent)
            .map(Path::to_path_buf)
            .ok_or_else(|| format!("libobs is not inside a libobs.framework: {}", lib.display()))?;
        let contents = frameworks.parent().unwrap_or(&frameworks).to_path_buf();
        let plugins = contents.join("PlugIns");
        let in_bundle = std::env::current_exe()
            .ok()
            .and_then(|exe| exe.parent().map(Path::to_path_buf))
            .is_some_and(|dir| dir == contents.join("MacOS"));
        Ok(Self { frameworks, plugins, in_bundle })
    }

    pub fn graphics_module(&self, metal: bool) -> PathBuf {
        self.frameworks.join(if metal { "libobs-metal.dylib" } else { "libobs-opengl.dylib" })
    }
}

fn loaded_libobs_path() -> Option<PathBuf> {
    let mut info: libc::Dl_info = unsafe { std::mem::zeroed() };
    let addr = sys::obs_startup as *const c_void;
    // SAFETY: dladdr only reads the address and fills `info`.
    if unsafe { libc::dladdr(addr, &mut info) } == 0 || info.dli_fname.is_null() {
        return None;
    }
    let path = unsafe { CStr::from_ptr(info.dli_fname) };
    Some(PathBuf::from(path.to_string_lossy().into_owned()))
}
