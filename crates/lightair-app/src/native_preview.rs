//! The canvas preview drawn by Core Animation, under the window's UI (macOS).
//!
//! GPUI redraws its whole window whenever anything in it moves, so a live picture drawn by GPUI
//! cost a full redraw 60 times a second. Instead a view of Lightair's own sits below GPUI's in
//! the window: its layer paints the window's background, the canvas well, and in it one layer per
//! canvas showing that canvas's preview frames (handed over by the preview straight from libobs's
//! graphics thread, `Preview::set_layer`). The window is transparent where nothing is painted,
//! and GPUI leaves the canvas well unpainted, so the picture shows through; everything GPUI draws
//! (selection, guides, menus, any panel over the canvas) is drawn over it as before.

#[cfg(target_os = "macos")]
pub use mac::NativePreview;

/// Whether the preview is drawn under the UI (macOS; `LIGHTAIR_GPUI_PREVIEW=1` draws it with
/// GPUI as before).
pub fn on() -> bool {
    cfg!(target_os = "macos") && std::env::var_os("LIGHTAIR_GPUI_PREVIEW").is_none()
}

#[cfg(target_os = "macos")]
mod mac {
use std::collections::HashMap;
use std::ffi::c_void;

use std::ffi::{CStr, c_char};

use gpui_kit::{Bounds, Hsla, Pixels, Window};
use lightair_core::Id;
use raw_window_handle::{HasWindowHandle, RawWindowHandle};

#[link(name = "QuartzCore", kind = "framework")]
unsafe extern "C" {}

#[link(name = "objc")]
unsafe extern "C" {
    fn objc_getClass(name: *const c_char) -> *mut c_void;
    fn sel_registerName(name: *const c_char) -> *mut c_void;
    fn objc_msgSend();
}

#[repr(C)]
#[derive(Clone, Copy)]
struct NSRect {
    x: f64,
    y: f64,
    w: f64,
    h: f64,
}

type Id_ = *mut c_void;

/// `[target sel]` → id
unsafe fn send(target: Id_, sel: &CStr) -> Id_ {
    let f: unsafe extern "C" fn(Id_, *mut c_void) -> Id_ = unsafe { std::mem::transmute(objc_msgSend as unsafe extern "C" fn()) };
    unsafe { f(target, sel_registerName(sel.as_ptr())) }
}

/// `[target sel: arg]` with a pointer-sized argument
unsafe fn send_p(target: Id_, sel: &CStr, arg: *mut c_void) -> Id_ {
    let f: unsafe extern "C" fn(Id_, *mut c_void, *mut c_void) -> Id_ = unsafe { std::mem::transmute(objc_msgSend as unsafe extern "C" fn()) };
    unsafe { f(target, sel_registerName(sel.as_ptr()), arg) }
}

unsafe fn send_bool(target: Id_, sel: &CStr, arg: bool) {
    let f: unsafe extern "C" fn(Id_, *mut c_void, bool) = unsafe { std::mem::transmute(objc_msgSend as unsafe extern "C" fn()) };
    unsafe { f(target, sel_registerName(sel.as_ptr()), arg) }
}

unsafe fn send_u64(target: Id_, sel: &CStr, arg: u64) {
    let f: unsafe extern "C" fn(Id_, *mut c_void, u64) = unsafe { std::mem::transmute(objc_msgSend as unsafe extern "C" fn()) };
    unsafe { f(target, sel_registerName(sel.as_ptr()), arg) }
}

unsafe fn send_rect(target: Id_, sel: &CStr, r: NSRect) -> Id_ {
    let f: unsafe extern "C" fn(Id_, *mut c_void, NSRect) -> Id_ = unsafe { std::mem::transmute(objc_msgSend as unsafe extern "C" fn()) };
    unsafe { f(target, sel_registerName(sel.as_ptr()), r) }
}

unsafe fn get_rect(target: Id_, sel: &CStr) -> NSRect {
    let f: unsafe extern "C" fn(Id_, *mut c_void) -> NSRect = unsafe { std::mem::transmute(objc_msgSend as unsafe extern "C" fn()) };
    unsafe { f(target, sel_registerName(sel.as_ptr())) }
}

/// `[content addSubview: view positioned: NSWindowBelow relativeTo: other]`
unsafe fn add_below(content: Id_, view: Id_, other: Id_) {
    let f: unsafe extern "C" fn(Id_, *mut c_void, Id_, isize, Id_) = unsafe { std::mem::transmute(objc_msgSend as unsafe extern "C" fn()) };
    unsafe { f(content, sel_registerName(c"addSubview:positioned:relativeTo:".as_ptr()), view, -1, other) }
}

fn class(name: &CStr) -> Id_ {
    unsafe { objc_getClass(name.as_ptr()) }
}

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGColorCreateSRGB(r: f64, g: f64, b: f64, a: f64) -> *mut c_void;
    fn CGColorRelease(c: *mut c_void);
}

fn rect(b: Bounds<Pixels>) -> NSRect {
    NSRect {
        x: f32::from(b.origin.x) as f64,
        y: f32::from(b.origin.y) as f64,
        w: f32::from(b.size.width).max(0.0) as f64,
        h: f32::from(b.size.height).max(0.0) as f64,
    }
}

unsafe fn set_colour(layer: Id_, colour: Hsla) {
    let c = colour.to_rgb();
    unsafe {
        let cg = CGColorCreateSRGB(c.r as f64, c.g as f64, c.b as f64, c.a as f64);
        send_p(layer, c"setBackgroundColor:", cg);
        CGColorRelease(cg);
    }
}

unsafe fn new_layer() -> Id_ {
    unsafe { send(class(c"CALayer"), c"new") }
}

/// Changes layers without Core Animation's implicit animation.
fn quietly(f: impl FnOnce()) {
    unsafe {
        let tx = class(c"CATransaction");
        send(tx, c"begin");
        send_bool(tx, c"setDisableActions:", true);
        f();
        send(tx, c"commit");
    }
}

/// The well and each canvas's picture, as last placed.
type Placed = (Option<Bounds<Pixels>>, Vec<(Id, Bounds<Pixels>)>);

/// The view under the UI, the well's view in it, and each canvas's picture: its view and the
/// layer it hosts (which the preview shows frames in).
pub struct NativePreview {
    view: Id_,
    host: Id_,
    well: Id_,
    pictures: HashMap<Id, (Id_, Id_)>,
    /// Where things were last placed (shown again as they were when the canvas comes back).
    last: Placed,
    visible: bool,
}

impl NativePreview {
    /// Puts the view under GPUI's in `window` (which must be transparent), painting `background`
    /// everywhere and `well` where the canvas well is.
    pub fn attach(window: &Window, background: Hsla, well: Hsla) -> Option<Self> {
        let handle = HasWindowHandle::window_handle(window).ok()?;
        let RawWindowHandle::AppKit(h) = handle.as_raw() else { return None };
        let gpui_view = h.ns_view.as_ptr() as Id_;
        unsafe {
            let content = send(gpui_view, c"superview");
            if content.is_null() {
                return None;
            }
            let bounds = get_rect(content, c"bounds");
            let view = send_rect(send(class(c"NSView"), c"alloc"), c"initWithFrame:", bounds);
            // width and height follow the window
            send_u64(view, c"setAutoresizingMask:", 2 | 16);
            let host = new_layer();
            set_colour(host, background);
            send_p(view, c"setLayer:", host);
            send_bool(view, c"setWantsLayer:", true);
            // the well: a view of its own (AppKit draws views; layers added under a view's layer
            // weren't), its pictures views in it
            let well_view = send_rect(send(class(c"NSView"), c"alloc"), c"initWithFrame:", NSRect { x: 0.0, y: 0.0, w: 0.0, h: 0.0 });
            let well_layer = new_layer();
            set_colour(well_layer, well);
            send_bool(well_layer, c"setMasksToBounds:", true);
            send_p(well_view, c"setLayer:", well_layer);
            send_bool(well_view, c"setWantsLayer:", true);
            send_bool(well_view, c"setHidden:", true);
            send_p(view, c"addSubview:", well_view);
            add_below(content, view, gpui_view);
            Some(Self { view, host, well: well_view, pictures: HashMap::new(), last: (None, vec![]), visible: true })
        }
    }

    /// The layer a canvas's preview shows its frames in (made the first time), for
    /// `Preview::set_layer`.
    pub fn picture(&mut self, canvas: Id) -> *mut c_void {
        let well = self.well;
        self.pictures
            .entry(canvas)
            .or_insert_with(|| unsafe {
                let view = send_rect(send(class(c"NSView"), c"alloc"), c"initWithFrame:", NSRect { x: 0.0, y: 0.0, w: 0.0, h: 0.0 });
                let layer = new_layer();
                let gravity = send_p(class(c"NSString"), c"stringWithUTF8String:", c"resize".as_ptr() as *mut c_void);
                send_p(layer, c"setContentsGravity:", gravity);
                send_p(view, c"setLayer:", layer);
                send_bool(view, c"setWantsLayer:", true);
                send_bool(view, c"setHidden:", true);
                send_p(well, c"addSubview:", view);
                (view, layer)
            })
            .1
    }

    /// Where the well is (window points), and each shown canvas's picture in it; `None`: not
    /// on screen (another place, the Library, Editor).
    pub fn place(&mut self, well: Option<Bounds<Pixels>>, pictures: &[(Id, Bounds<Pixels>)]) {
        self.last = (well, pictures.to_vec());
        let well = well.filter(|_| self.visible);
        let (view, well_view, pictures_now) = (self.view, self.well, &self.pictures);
        quietly(|| unsafe {
            match well {
                None => send_bool(well_view, c"setHidden:", true),
                Some(w) => {
                    // AppKit's views count up from the bottom
                    let host_h = get_rect(view, c"bounds").h;
                    let mut r = rect(w);
                    r.y = host_h - r.y - r.h;
                    send_rect(well_view, c"setFrame:", r);
                    send_bool(well_view, c"setHidden:", false);
                    for (id, (picture, _)) in pictures_now {
                        match pictures.iter().find(|(c, _)| c == id) {
                            Some((_, b)) => {
                                let mut p = rect(*b);
                                p.x -= rect(w).x;
                                p.y = (rect(w).y + rect(w).h) - (p.y + p.h);
                                send_rect(*picture, c"setFrame:", p);
                                send_bool(*picture, c"setHidden:", false);
                            }
                            None => send_bool(*picture, c"setHidden:", true),
                        }
                    }
                }
            }
        });
    }

    /// The canvas is on screen (the Live place) or not; placed again as it last was.
    pub fn set_visible(&mut self, visible: bool) {
        if visible != self.visible {
            self.visible = visible;
            let (well, pictures) = self.last.clone();
            self.place(well, &pictures);
        }
    }

    /// A canvas gone: its layer goes too.
    pub fn forget(&mut self, canvas: Id) {
        if let Some((view, layer)) = self.pictures.remove(&canvas) {
            unsafe {
                send(view, c"removeFromSuperview");
                send(view, c"release");
                send(layer, c"release");
            }
        }
    }
}

impl Drop for NativePreview {
    fn drop(&mut self) {
        for (_, (view, layer)) in self.pictures.drain() {
            unsafe {
                send(view, c"removeFromSuperview");
                send(view, c"release");
                send(layer, c"release");
            }
        }
        unsafe {
            send(self.view, c"removeFromSuperview");
            send(self.view, c"release");
            send(self.well, c"release");
            send(self.host, c"release");
        }
    }
}
}

/// Elsewhere GPUI draws the preview.
#[cfg(not(target_os = "macos"))]
pub struct NativePreview;

#[cfg(not(target_os = "macos"))]
impl NativePreview {
    pub fn attach(_: &gpui_kit::Window, _: gpui_kit::Hsla, _: gpui_kit::Hsla) -> Option<Self> {
        None
    }
    pub fn picture(&mut self, _: lightair_core::Id) -> *mut std::ffi::c_void {
        std::ptr::null_mut()
    }
    pub fn place(&mut self, _: Option<gpui_kit::Bounds<gpui_kit::Pixels>>, _: &[(lightair_core::Id, gpui_kit::Bounds<gpui_kit::Pixels>)]) {}
    pub fn set_visible(&mut self, _: bool) {}
    pub fn forget(&mut self, _: lightair_core::Id) {}
}
