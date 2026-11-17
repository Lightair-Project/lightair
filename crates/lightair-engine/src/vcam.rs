//! The virtual camera's installation on Windows.
//!
//! Lightair's camera there is OBS's DirectShow camera ("OBS Virtual Camera", `win-dshow`'s
//! `virtualcam_output`): frames go to it through shared memory, and other apps open it as a
//! camera. The output is registered only where the camera is installed (a COM class in the
//! registry), which OBS's installer does, or Lightair here: the camera's DLLs ship with OBS's
//! module (`core\win-dshow\data`), and registering them is `regsvr32`, as administrator.
//! libobs registers the output as it loads its modules, so a camera installed now is there the
//! next time Lightair starts.

/// The error when the camera isn't installed (the UI offers to install it).
pub const NOT_INSTALLED: &str = "The virtual camera isn't installed on this PC.";

/// Whether the camera is installed (Windows; elsewhere it's macOS's own camera extension).
pub fn installed() -> bool {
    #[cfg(windows)]
    {
        use windows_sys::Win32::System::Registry::{HKEY, HKEY_CLASSES_ROOT, KEY_READ, KEY_WOW64_64KEY, RegCloseKey, RegOpenKeyExW};
        let key: Vec<u16> = r"CLSID\{A3FCE0F5-3493-419F-958A-ABA1250EC20B}".encode_utf16().chain([0]).collect();
        let mut out: HKEY = std::ptr::null_mut();
        let ok = unsafe { RegOpenKeyExW(HKEY_CLASSES_ROOT, key.as_ptr(), 0, KEY_READ | KEY_WOW64_64KEY, &mut out) } == 0;
        if ok {
            unsafe { RegCloseKey(out) };
        }
        ok
    }
    #[cfg(not(windows))]
    true
}

/// Installs the camera: registers its 64- and 32-bit DLLs from libobs's install, as
/// administrator (Windows asks once). Returns once the prompt is shown; whether it worked is
/// `installed()` afterwards (Lightair restarts to use it).
#[cfg(windows)]
pub fn install() -> Result<(), String> {
    use windows_sys::Win32::UI::Shell::ShellExecuteW;
    use windows_sys::Win32::UI::WindowsAndMessaging::SW_HIDE;
    let runtime = crate::runtime::Runtime::detect()?;
    let root = runtime.frameworks.parent().and_then(std::path::Path::parent).ok_or("libobs has no install folder")?;
    let data = root.join(r"core\win-dshow\data");
    let dlls: Vec<_> = ["obs-virtualcam-module64.dll", "obs-virtualcam-module32.dll"]
        .into_iter()
        .map(|d| data.join(d))
        .filter(|d| d.exists())
        .collect();
    if dlls.is_empty() {
        return Err(format!("The virtual camera's files aren't in {}.", data.display()));
    }
    let commands: Vec<String> = dlls.iter().map(|d| format!("regsvr32.exe /i /s \"{}\"", d.display())).collect();
    let args = format!("/c {}", commands.join(" & "));
    let wide = |s: &str| -> Vec<u16> { s.encode_utf16().chain([0]).collect() };
    let (verb, file, params) = (wide("runas"), wide("cmd.exe"), wide(&args));
    // (above 32: started; the user may still say no to the prompt)
    let r = unsafe { ShellExecuteW(std::ptr::null_mut(), verb.as_ptr(), file.as_ptr(), params.as_ptr(), std::ptr::null(), SW_HIDE) };
    if r as isize > 32 { Ok(()) } else { Err("Windows didn't start the installation.".into()) }
}

#[cfg(not(windows))]
pub fn install() -> Result<(), String> {
    Err("The virtual camera installs with Lightair on this system.".into())
}
