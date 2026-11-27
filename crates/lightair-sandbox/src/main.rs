//! `lightair-sandbox <profile file> <program> [arguments…]`: applies the seatbelt profile in the
//! file to this process, then replaces itself with the program, which keeps the sandbox (and the
//! responsibility Lightair disclaimed when it started this launcher).

#[cfg(target_os = "macos")]
fn main() {
    use std::ffi::{CStr, CString, c_char, c_int};
    use std::os::unix::process::CommandExt;

    unsafe extern "C" {
        fn sandbox_init(profile: *const c_char, flags: u64, errorbuf: *mut *mut c_char) -> c_int;
        fn sandbox_free_error(errorbuf: *mut c_char);
    }

    let args: Vec<std::ffi::OsString> = std::env::args_os().collect();
    if args.len() < 3 {
        eprintln!("usage: lightair-sandbox <profile file> <program> [arguments…]");
        std::process::exit(64);
    }
    let profile = match std::fs::read_to_string(&args[1]) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("lightair-sandbox: can't read the profile: {e}");
            std::process::exit(66);
        }
    };
    let Ok(profile) = CString::new(profile) else {
        eprintln!("lightair-sandbox: the profile holds a NUL byte");
        std::process::exit(65);
    };
    let mut error: *mut c_char = std::ptr::null_mut();
    // Flags 0: the profile is the text of a profile, not a name.
    let rc = unsafe { sandbox_init(profile.as_ptr(), 0, &mut error) };
    if rc != 0 {
        let message = if error.is_null() {
            "unknown error".to_string()
        } else {
            let m = unsafe { CStr::from_ptr(error) }.to_string_lossy().into_owned();
            unsafe { sandbox_free_error(error) };
            m
        };
        eprintln!("lightair-sandbox: the profile wasn't applied: {message}");
        std::process::exit(71);
    }
    let err = std::process::Command::new(&args[2]).args(&args[3..]).exec();
    eprintln!("lightair-sandbox: couldn't start {}: {err}", args[2].to_string_lossy());
    std::process::exit(72);
}

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("lightair-sandbox is the macOS launcher; Windows helpers are started in an AppContainer instead");
    std::process::exit(64);
}
