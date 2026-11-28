//! `lightair-sandbox <profile file> <program> [arguments…]`: applies the seatbelt profile in the
//! file to this process, then replaces itself with the program, which keeps the sandbox (and the
//! responsibility Lightair disclaimed when it started this launcher).
//!
//! On Windows, `lightair-sandbox.exe bridge <pipe>` is a helper's network bridge: Lightair starts
//! it in the helper's AppContainer, where it listens on 127.0.0.1 and carries each connection
//! over the named pipe to the helper's proxy lane (lightair-helpers' `windows` module).

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

#[cfg(windows)]
fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() != 3 || args[1] != "bridge" {
        eprintln!("usage: lightair-sandbox bridge <pipe>");
        std::process::exit(64);
    }
    if let Err(e) = lightair_helpers::windows::bridge_main(&args[2]) {
        eprintln!("lightair-sandbox bridge: {e}");
        std::process::exit(71);
    }
}

#[cfg(not(any(target_os = "macos", windows)))]
fn main() {
    eprintln!("lightair-sandbox is the macOS launcher");
    std::process::exit(64);
}
