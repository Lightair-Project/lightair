//! Web pages without copies (macOS): the compositor draws Chromium's own surface, held in use
//! while anything draws it and given back to Chromium once nothing does.
#![cfg(target_os = "macos")]

use std::ffi::c_void;

use lightair_host::buffers::{Buffer, bgra_buffer, surface_of};
use lightair_host::web::Frames;

#[link(name = "IOSurface", kind = "framework")]
unsafe extern "C" {
    fn IOSurfaceGetUseCount(surface: *mut c_void) -> i32;
}

/// A painted page frame: an IOSurface (kept alive by its own pixel buffer) and its use count
/// before anything of the page holds it.
fn painted() -> (Buffer, *mut c_void, i32) {
    let own = bgra_buffer(64, 36).expect("a BGRA pixel buffer");
    let surface = surface_of(&own).expect("an IOSurface");
    let idle = unsafe { IOSurfaceGetUseCount(surface) };
    (own, surface, idle)
}

fn in_use(surface: *mut c_void, idle: i32) -> bool {
    unsafe { IOSurfaceGetUseCount(surface) > idle }
}

/// The compositor draws Chromium's own surface, not a copy: the frame it's handed is a pixel
/// buffer over the very surface painted.
#[test]
fn the_compositor_gets_the_painted_surface_itself() {
    let frames = Frames::default();
    frames.want(true);
    let (_own, surface, _) = painted();
    unsafe { frames.put(surface) };
    let (buffer, _) = frames.frame(true).expect("a frame for the compositor");
    assert_eq!(surface_of(&buffer), Some(surface), "no copy: the same IOSurface");
}

/// A frame stays in use (Chromium doesn't paint into it) while the compositor still draws it,
/// however many frames are painted meanwhile, and goes back to Chromium once nothing draws it;
/// all of them go back when the compositor stops wanting the page.
#[test]
fn a_frame_is_held_while_drawn_and_given_back_after() {
    let frames = Frames::default();
    frames.want(true);
    let (_a_own, a, a_idle) = painted();
    unsafe { frames.put(a) };
    let drawn = frames.frame(true).expect("frame A");
    let later: Vec<_> = (0..3).map(|_| painted()).collect();
    for (_, s, _) in &later {
        unsafe { frames.put(*s) };
    }
    assert!(in_use(a, a_idle), "A is still drawn: Chromium mustn't paint into it");
    drop(drawn);
    let (_d_own, d, _) = painted();
    unsafe { frames.put(d) };
    assert!(!in_use(a, a_idle), "nothing draws A now: it's Chromium's again");
    let (_, b, b_idle) = &later[0];
    assert!(!in_use(*b, *b_idle), "B was never drawn and is no longer the newest");

    frames.want(false);
    let (_, c, c_idle) = &later[2];
    assert!(!in_use(*c, *c_idle), "the compositor's frames go back when it stops");
}
