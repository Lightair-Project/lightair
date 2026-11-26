//! macOS: posting mouse and key events (CGEvent, needs Accessibility), watching for the human
//! (a listen-only event tap; Lightair's own events carry a tag), what's under the pointer and what
//! has the keyboard (Accessibility), and pictures of the screen.

#![allow(non_upper_case_globals, non_snake_case)]

use std::ffi::{CStr, CString, c_void};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// The tag on every event Lightair posts for PiAir (`kCGEventSourceUserData`): "PiAi".
pub const TAG: i64 = 0x5069_4169;

type CFTypeRef = *const c_void;
type CGEventRef = *mut c_void;
type CGEventSourceRef = *mut c_void;
type AXUIElementRef = *const c_void;

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct CGPoint {
    pub x: f64,
    pub y: f64,
}
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct CGSize {
    pub width: f64,
    pub height: f64,
}
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct CGRect {
    pub origin: CGPoint,
    pub size: CGSize,
}

const kCGEventLeftMouseDown: u32 = 1;
const kCGEventLeftMouseUp: u32 = 2;
const kCGEventRightMouseDown: u32 = 3;
const kCGEventRightMouseUp: u32 = 4;
const kCGEventMouseMoved: u32 = 5;
const kCGEventLeftMouseDragged: u32 = 6;
const kCGEventRightMouseDragged: u32 = 7;
const kCGEventKeyDown: u32 = 10;
const kCGEventKeyUp: u32 = 11;
const kCGEventFlagsChanged: u32 = 12;
const kCGEventScrollWheel: u32 = 22;
const kCGEventOtherMouseDown: u32 = 25;
const kCGEventOtherMouseUp: u32 = 26;
const kCGEventOtherMouseDragged: u32 = 27;
const kCGEventTapDisabledByTimeout: u32 = 0xFFFF_FFFE;
const kCGEventTapDisabledByUserInput: u32 = 0xFFFF_FFFF;
const kCGMouseEventClickState: u32 = 1;
const kCGEventSourceUserData: u32 = 42;
const kCGHIDEventTap: u32 = 0;
const kCGSessionEventTap: u32 = 1;
const kCGHeadInsertEventTap: u32 = 0;
const kCGEventTapOptionListenOnly: u32 = 1;
const kCGEventSourceStatePrivate: i32 = -1;
const kCGScrollEventUnitLine: u32 = 1;

pub const FLAG_SHIFT: u64 = 0x0002_0000;
pub const FLAG_CONTROL: u64 = 0x0004_0000;
pub const FLAG_ALT: u64 = 0x0008_0000;
pub const FLAG_COMMAND: u64 = 0x0010_0000;

type TapCallback = extern "C" fn(proxy: *mut c_void, kind: u32, event: CGEventRef, info: *mut c_void) -> CGEventRef;

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGEventSourceCreate(state: i32) -> CGEventSourceRef;
    fn CGEventCreateMouseEvent(source: CGEventSourceRef, kind: u32, at: CGPoint, button: u32) -> CGEventRef;
    fn CGEventCreateKeyboardEvent(source: CGEventSourceRef, key: u16, down: bool) -> CGEventRef;
    fn CGEventCreateScrollWheelEvent2(
        source: CGEventSourceRef,
        units: u32,
        count: u32,
        w1: i32,
        w2: i32,
        w3: i32,
    ) -> CGEventRef;
    fn CGEventKeyboardSetUnicodeString(event: CGEventRef, len: usize, chars: *const u16);
    fn CGEventSetFlags(event: CGEventRef, flags: u64);
    fn CGEventSetIntegerValueField(event: CGEventRef, field: u32, value: i64);
    fn CGEventGetIntegerValueField(event: CGEventRef, field: u32) -> i64;
    fn CGEventPost(tap: u32, event: CGEventRef);
    fn CGEventCreate(source: CGEventSourceRef) -> CGEventRef;
    fn CGEventGetLocation(event: CGEventRef) -> CGPoint;
    fn CGEventTapCreate(
        tap: u32,
        place: u32,
        options: u32,
        mask: u64,
        callback: TapCallback,
        info: *mut c_void,
    ) -> *mut c_void;
    fn CGEventTapEnable(tap: *mut c_void, enable: bool);
    fn CGGetDisplaysWithPoint(point: CGPoint, max: u32, displays: *mut u32, count: *mut u32) -> i32;
    fn CGGetActiveDisplayList(max: u32, displays: *mut u32, count: *mut u32) -> i32;
    fn CGDisplayBounds(display: u32) -> CGRect;
    fn CGMainDisplayID() -> u32;
    fn CGPreflightListenEventAccess() -> bool;
    fn CGWindowListCopyWindowInfo(option: u32, relative_to: u32) -> CFTypeRef;
    fn CGRectMakeWithDictionaryRepresentation(dict: CFTypeRef, rect: *mut CGRect) -> bool;
    static kCGWindowOwnerPID: CFTypeRef;
    static kCGWindowBounds: CFTypeRef;
    static kCGWindowLayer: CFTypeRef;
    static kCGWindowAlpha: CFTypeRef;
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFRelease(cf: CFTypeRef);
    fn CFMachPortCreateRunLoopSource(alloc: *const c_void, port: *mut c_void, order: isize) -> *mut c_void;
    fn CFRunLoopGetCurrent() -> *mut c_void;
    fn CFRunLoopAddSource(rl: *mut c_void, source: *mut c_void, mode: *const c_void);
    fn CFRunLoopRun();
    fn CFStringCreateWithCString(alloc: *const c_void, s: *const i8, encoding: u32) -> CFTypeRef;
    fn CFStringGetCString(s: CFTypeRef, buf: *mut i8, size: isize, encoding: u32) -> bool;
    fn CFGetTypeID(cf: CFTypeRef) -> usize;
    fn CFArrayGetCount(a: CFTypeRef) -> isize;
    fn CFArrayGetValueAtIndex(a: CFTypeRef, i: isize) -> CFTypeRef;
    fn CFDictionaryGetValue(d: CFTypeRef, key: CFTypeRef) -> CFTypeRef;
    fn CFNumberGetValue(n: CFTypeRef, kind: isize, out: *mut c_void) -> bool;
    fn CFStringGetTypeID() -> usize;
    static kCFRunLoopCommonModes: *const c_void;
    static kCFBooleanTrue: CFTypeRef;
    fn CFDictionaryCreate(
        alloc: *const c_void,
        keys: *const CFTypeRef,
        values: *const CFTypeRef,
        n: isize,
        kcb: *const c_void,
        vcb: *const c_void,
    ) -> CFTypeRef;
    static kCFTypeDictionaryKeyCallBacks: c_void;
    static kCFTypeDictionaryValueCallBacks: c_void;
}

#[link(name = "ApplicationServices", kind = "framework")]
unsafe extern "C" {
    fn AXIsProcessTrusted() -> bool;
    fn AXIsProcessTrustedWithOptions(options: CFTypeRef) -> bool;
    fn AXUIElementCreateSystemWide() -> AXUIElementRef;
    fn AXUIElementCopyAttributeValue(e: AXUIElementRef, attr: CFTypeRef, value: *mut CFTypeRef) -> i32;
    fn AXUIElementCopyElementAtPosition(app: AXUIElementRef, x: f32, y: f32, e: *mut AXUIElementRef) -> i32;
    fn AXUIElementGetPid(e: AXUIElementRef, pid: *mut i32) -> i32;
    static kAXTrustedCheckOptionPrompt: CFTypeRef;
}

#[link(name = "Carbon", kind = "framework")]
unsafe extern "C" {
    fn IsSecureEventInputEnabled() -> bool;
}

const UTF8: u32 = 0x0800_0100;

fn cfstr(s: &str) -> CFTypeRef {
    let c = CString::new(s).unwrap_or_default();
    unsafe { CFStringCreateWithCString(std::ptr::null(), c.as_ptr(), UTF8) }
}

fn string_of(cf: CFTypeRef) -> Option<String> {
    if cf.is_null() || unsafe { CFGetTypeID(cf) != CFStringGetTypeID() } {
        return None;
    }
    let mut buf = vec![0i8; 1024];
    unsafe { CFStringGetCString(cf, buf.as_mut_ptr(), buf.len() as isize, UTF8) }
        .then(|| unsafe { CStr::from_ptr(buf.as_ptr()) }.to_string_lossy().into_owned())
}

/// An attribute of an accessibility element, as text.
fn ax_text(e: AXUIElementRef, attr: &str) -> Option<String> {
    let name = cfstr(attr);
    let mut v: CFTypeRef = std::ptr::null();
    let err = unsafe { AXUIElementCopyAttributeValue(e, name, &mut v) };
    unsafe { CFRelease(name) };
    if err != 0 || v.is_null() {
        return None;
    }
    let s = string_of(v);
    unsafe { CFRelease(v) };
    s
}

// ------------------------------------------------------------------ permissions

/// Lightair may post events (Accessibility). `prompt` asks macOS to show its dialog.
pub fn can_post(prompt: bool) -> bool {
    if !prompt {
        return unsafe { AXIsProcessTrusted() };
    }
    unsafe {
        let keys = [kAXTrustedCheckOptionPrompt];
        let values = [kCFBooleanTrue];
        let d = CFDictionaryCreate(
            std::ptr::null(),
            keys.as_ptr(),
            values.as_ptr(),
            1,
            &kCFTypeDictionaryKeyCallBacks as *const _,
            &kCFTypeDictionaryValueCallBacks as *const _,
        );
        let ok = AXIsProcessTrustedWithOptions(d);
        CFRelease(d);
        ok
    }
}

/// Lightair may watch input (Input Monitoring), which "the human always wins" needs.
pub fn can_watch() -> bool {
    unsafe { CGPreflightListenEventAccess() }
}

// ------------------------------------------------------------------ the human

static HUMAN_AT: AtomicU64 = AtomicU64::new(0);
static WATCHING: AtomicBool = AtomicBool::new(false);
static TAP: std::sync::atomic::AtomicPtr<c_void> = std::sync::atomic::AtomicPtr::new(std::ptr::null_mut());

fn epoch() -> Instant {
    static E: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
    *E.get_or_init(Instant::now)
}

extern "C" fn on_event(_: *mut c_void, kind: u32, event: CGEventRef, _: *mut c_void) -> CGEventRef {
    if kind == kCGEventTapDisabledByTimeout || kind == kCGEventTapDisabledByUserInput {
        let tap = TAP.load(Ordering::Relaxed);
        if !tap.is_null() {
            unsafe { CGEventTapEnable(tap, true) };
        }
        return event;
    }
    if !event.is_null() && unsafe { CGEventGetIntegerValueField(event, kCGEventSourceUserData) } != TAG {
        HUMAN_AT.store(epoch().elapsed().as_millis() as u64 + 1, Ordering::Relaxed);
    }
    event
}

/// Starts watching mouse and keyboard (once). False when macOS doesn't allow it.
pub fn watch() -> bool {
    if WATCHING.load(Ordering::Relaxed) {
        return true;
    }
    if !can_watch() {
        return false;
    }
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::Builder::new()
        .name("piair-human".into())
        .spawn(move || unsafe {
            let kinds = [
                kCGEventLeftMouseDown,
                kCGEventLeftMouseUp,
                kCGEventRightMouseDown,
                kCGEventRightMouseUp,
                kCGEventMouseMoved,
                kCGEventLeftMouseDragged,
                kCGEventRightMouseDragged,
                kCGEventKeyDown,
                kCGEventKeyUp,
                kCGEventFlagsChanged,
                kCGEventScrollWheel,
                kCGEventOtherMouseDown,
                kCGEventOtherMouseUp,
                kCGEventOtherMouseDragged,
            ];
            let mask = kinds.iter().fold(0u64, |m, k| m | (1u64 << k));
            let tap = CGEventTapCreate(
                kCGSessionEventTap,
                kCGHeadInsertEventTap,
                kCGEventTapOptionListenOnly,
                mask,
                on_event,
                std::ptr::null_mut(),
            );
            if tap.is_null() {
                let _ = tx.send(false);
                return;
            }
            TAP.store(tap, Ordering::Relaxed);
            let src = CFMachPortCreateRunLoopSource(std::ptr::null(), tap, 0);
            CFRunLoopAddSource(CFRunLoopGetCurrent(), src, kCFRunLoopCommonModes);
            CGEventTapEnable(tap, true);
            let _ = tx.send(true);
            CFRunLoopRun();
        })
        .ok();
    let ok = rx.recv_timeout(Duration::from_secs(2)).unwrap_or(false);
    WATCHING.store(ok, Ordering::Relaxed);
    ok
}

/// How long since the human last touched the mouse or keyboard (`None`: not since watching).
pub fn since_human() -> Option<Duration> {
    let at = HUMAN_AT.load(Ordering::Relaxed);
    (at > 0).then(|| epoch().elapsed().saturating_sub(Duration::from_millis(at - 1)))
}

// ------------------------------------------------------------------ posting

struct Source(CGEventSourceRef);
unsafe impl Send for Source {}
unsafe impl Sync for Source {}

fn source() -> CGEventSourceRef {
    static S: std::sync::OnceLock<Source> = std::sync::OnceLock::new();
    S.get_or_init(|| Source(unsafe { CGEventSourceCreate(kCGEventSourceStatePrivate) })).0
}

fn post(e: CGEventRef) {
    if e.is_null() {
        return;
    }
    unsafe {
        CGEventSetIntegerValueField(e, kCGEventSourceUserData, TAG);
        CGEventPost(kCGHIDEventTap, e);
        CFRelease(e);
    }
}

/// Where the pointer is (global display points, origin top left of the main display).
pub fn pointer() -> (f32, f32) {
    unsafe {
        let e = CGEventCreate(std::ptr::null_mut());
        if e.is_null() {
            return (0.0, 0.0);
        }
        let p = CGEventGetLocation(e);
        CFRelease(e);
        (p.x as f32, p.y as f32)
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Button {
    Left,
    Right,
    Middle,
}

/// Moves the pointer (a held button drags).
pub fn move_to(at: (f32, f32), held: Option<Button>) {
    let kind = match held {
        None => kCGEventMouseMoved,
        Some(Button::Left) => kCGEventLeftMouseDragged,
        Some(Button::Right) => kCGEventRightMouseDragged,
        Some(Button::Middle) => kCGEventOtherMouseDragged,
    };
    let b = match held {
        Some(Button::Right) => 1,
        Some(Button::Middle) => 2,
        _ => 0,
    };
    post(unsafe { CGEventCreateMouseEvent(source(), kind, CGPoint { x: at.0 as f64, y: at.1 as f64 }, b) });
}

pub fn button(at: (f32, f32), button: Button, down: bool, clicks: i64, flags: u64) {
    let (kind, b) = match (button, down) {
        (Button::Left, true) => (kCGEventLeftMouseDown, 0),
        (Button::Left, false) => (kCGEventLeftMouseUp, 0),
        (Button::Right, true) => (kCGEventRightMouseDown, 1),
        (Button::Right, false) => (kCGEventRightMouseUp, 1),
        (Button::Middle, true) => (kCGEventOtherMouseDown, 2),
        (Button::Middle, false) => (kCGEventOtherMouseUp, 2),
    };
    let e = unsafe { CGEventCreateMouseEvent(source(), kind, CGPoint { x: at.0 as f64, y: at.1 as f64 }, b) };
    if !e.is_null() {
        unsafe {
            CGEventSetIntegerValueField(e, kCGMouseEventClickState, clicks);
            if flags != 0 {
                CGEventSetFlags(e, flags);
            }
        }
    }
    post(e);
}

pub fn scroll(dx: i32, dy: i32) {
    // CG: positive is up/left
    post(unsafe { CGEventCreateScrollWheelEvent2(source(), kCGScrollEventUnitLine, 2, -dy, -dx, 0) });
}

pub fn key(code: u16, down: bool, flags: u64) {
    let e = unsafe { CGEventCreateKeyboardEvent(source(), code, down) };
    if !e.is_null() {
        unsafe { CGEventSetFlags(e, flags) };
    }
    post(e);
}

/// Types text as Unicode (no key codes, any layout), a few characters per event.
pub fn type_text(text: &str, stop: &dyn Fn() -> bool) {
    let units: Vec<u16> = text.encode_utf16().collect();
    for chunk in units.chunks(4) {
        if stop() {
            return;
        }
        for down in [true, false] {
            let e = unsafe { CGEventCreateKeyboardEvent(source(), 0, down) };
            if e.is_null() {
                continue;
            }
            unsafe {
                CGEventSetFlags(e, 0);
                CGEventKeyboardSetUnicodeString(e, chunk.len(), chunk.as_ptr());
            }
            post(e);
        }
        std::thread::sleep(Duration::from_millis(12));
    }
}

/// Virtual key codes (US layout positions; macOS maps them to the current layout).
pub fn key_code(name: &str) -> Option<u16> {
    Some(match name {
        "a" => 0,
        "s" => 1,
        "d" => 2,
        "f" => 3,
        "h" => 4,
        "g" => 5,
        "z" => 6,
        "x" => 7,
        "c" => 8,
        "v" => 9,
        "b" => 11,
        "q" => 12,
        "w" => 13,
        "e" => 14,
        "r" => 15,
        "y" => 16,
        "t" => 17,
        "1" => 18,
        "2" => 19,
        "3" => 20,
        "4" => 21,
        "6" => 22,
        "5" => 23,
        "=" => 24,
        "9" => 25,
        "7" => 26,
        "-" => 27,
        "8" => 28,
        "0" => 29,
        "]" => 30,
        "o" => 31,
        "u" => 32,
        "[" => 33,
        "i" => 34,
        "p" => 35,
        "l" => 37,
        "j" => 38,
        "'" => 39,
        "k" => 40,
        ";" => 41,
        "\\" => 42,
        "," => 43,
        "/" => 44,
        "n" => 45,
        "m" => 46,
        "." => 47,
        "`" => 50,
        "return" | "enter" => 36,
        "tab" => 48,
        "space" => 49,
        "delete" | "backspace" => 51,
        "escape" | "esc" => 53,
        "forwarddelete" => 117,
        "home" => 115,
        "end" => 119,
        "pageup" => 116,
        "pagedown" => 121,
        "left" => 123,
        "right" => 124,
        "down" => 125,
        "up" => 126,
        "f1" => 122,
        "f2" => 120,
        "f3" => 99,
        "f4" => 118,
        "f5" => 96,
        "f6" => 97,
        "f7" => 98,
        "f8" => 100,
        "f9" => 101,
        "f10" => 109,
        "f11" => 103,
        "f12" => 111,
        _ => return None,
    })
}

pub fn modifier_flag(name: &str) -> Option<u64> {
    Some(match name {
        "cmd" | "command" | "meta" | "super" | "win" => FLAG_COMMAND,
        "ctrl" | "control" => FLAG_CONTROL,
        "alt" | "option" | "opt" => FLAG_ALT,
        "shift" => FLAG_SHIFT,
        _ => return None,
    })
}

// ------------------------------------------------------------------ what's where

/// Secure keyboard entry is on (a password field has the keyboard), or the focused element is a
/// secure text field.
pub fn password_focused() -> bool {
    if unsafe { IsSecureEventInputEnabled() } {
        return true;
    }
    unsafe {
        let system = AXUIElementCreateSystemWide();
        if system.is_null() {
            return false;
        }
        let attr = cfstr("AXFocusedUIElement");
        let mut focused: CFTypeRef = std::ptr::null();
        let err = AXUIElementCopyAttributeValue(system, attr, &mut focused);
        CFRelease(attr);
        CFRelease(system);
        if err != 0 || focused.is_null() {
            return false;
        }
        let secure = [ax_text(focused, "AXSubrole"), ax_text(focused, "AXRole")]
            .iter()
            .flatten()
            .any(|r| r == "AXSecureTextField");
        CFRelease(focused);
        secure
    }
}

/// What's at a point: the app's process and bundle id, and the element's title and description
/// (for spotting "Delete", "Send", …).
pub fn element_at(at: (f32, f32)) -> (i32, Option<String>, String) {
    unsafe {
        let system = AXUIElementCreateSystemWide();
        if system.is_null() {
            return (0, None, String::new());
        }
        let mut e: AXUIElementRef = std::ptr::null();
        let err = AXUIElementCopyElementAtPosition(system, at.0, at.1, &mut e);
        CFRelease(system);
        if err != 0 || e.is_null() {
            return (0, None, String::new());
        }
        let mut pid = 0;
        AXUIElementGetPid(e, &mut pid);
        let words = ["AXTitle", "AXDescription", "AXValue", "AXHelp"]
            .iter()
            .filter_map(|a| ax_text(e, a))
            .collect::<Vec<_>>()
            .join(" ");
        CFRelease(e);
        (pid, bundle_of_pid(pid), words)
    }
}

fn bundle_of_pid(pid: i32) -> Option<String> {
    use objc2::msg_send;
    use objc2::runtime::{AnyClass, AnyObject};
    if pid <= 0 {
        return None;
    }
    unsafe {
        let cls = AnyClass::get(c"NSRunningApplication")?;
        let app: *mut AnyObject = msg_send![cls, runningApplicationWithProcessIdentifier: pid];
        if app.is_null() {
            return None;
        }
        let id: *mut AnyObject = msg_send![app, bundleIdentifier];
        ns_string(id)
    }
}

unsafe fn ns_string(s: *mut objc2::runtime::AnyObject) -> Option<String> {
    use objc2::msg_send;
    if s.is_null() {
        return None;
    }
    let utf8: *const i8 = unsafe { msg_send![s, UTF8String] };
    (!utf8.is_null()).then(|| unsafe { CStr::from_ptr(utf8) }.to_string_lossy().into_owned())
}

/// The frontmost app's process, bundle id and name.
pub fn frontmost() -> Option<(i32, String, String)> {
    use objc2::msg_send;
    use objc2::runtime::{AnyClass, AnyObject};
    unsafe {
        let cls = AnyClass::get(c"NSWorkspace")?;
        let ws: *mut AnyObject = msg_send![cls, sharedWorkspace];
        let app: *mut AnyObject = msg_send![ws, frontmostApplication];
        if app.is_null() {
            return None;
        }
        let id: *mut AnyObject = msg_send![app, bundleIdentifier];
        let name: *mut AnyObject = msg_send![app, localizedName];
        let pid: i32 = msg_send![app, processIdentifier];
        Some((pid, ns_string(id)?, ns_string(name).unwrap_or_default()))
    }
}

/// Running apps with windows: (bundle id, name), for the allowlist picker.
pub fn running_apps() -> Vec<(String, String)> {
    use objc2::msg_send;
    use objc2::runtime::{AnyClass, AnyObject};
    let mut out = Vec::new();
    unsafe {
        let Some(cls) = AnyClass::get(c"NSWorkspace") else { return out };
        let ws: *mut AnyObject = msg_send![cls, sharedWorkspace];
        let apps: *mut AnyObject = msg_send![ws, runningApplications];
        if apps.is_null() {
            return out;
        }
        let n: usize = msg_send![apps, count];
        for i in 0..n {
            let app: *mut AnyObject = msg_send![apps, objectAtIndex: i];
            // regular apps only (Dock icon)
            let policy: isize = msg_send![app, activationPolicy];
            if policy != 0 {
                continue;
            }
            let id: *mut AnyObject = msg_send![app, bundleIdentifier];
            let name: *mut AnyObject = msg_send![app, localizedName];
            if let Some(id) = ns_string(id) {
                out.push((id, ns_string(name).unwrap_or_default()));
            }
        }
    }
    out.sort_by_key(|a| a.1.to_lowercase());
    out.dedup_by(|a, b| a.0 == b.0);
    out
}

// ------------------------------------------------------------------ the screen

/// A window on screen: its process, frame (global points) and layer (0 = a normal window).
#[derive(Clone, Copy, Debug)]
pub struct Window {
    pub pid: i32,
    pub frame: CGRect,
    pub layer: i32,
}

/// The windows on screen, front to back (the menu bar, the Dock and panels included; the desktop
/// picture left out). Transparent windows are skipped.
pub fn windows_on_screen() -> Vec<Window> {
    // kCGWindowListOptionOnScreenOnly | kCGWindowListExcludeDesktopElements
    const OPTIONS: u32 = (1 << 0) | (1 << 4);
    let mut out = Vec::new();
    unsafe {
        let list = CGWindowListCopyWindowInfo(OPTIONS, 0);
        if list.is_null() {
            return out;
        }
        let n = CFArrayGetCount(list);
        let int = |d: CFTypeRef, k: CFTypeRef| -> Option<i64> {
            let v = CFDictionaryGetValue(d, k);
            let mut x: i64 = 0;
            // kCFNumberSInt64Type
            (!v.is_null() && CFNumberGetValue(v, 4, &mut x as *mut i64 as *mut c_void)).then_some(x)
        };
        let float = |d: CFTypeRef, k: CFTypeRef| -> Option<f64> {
            let v = CFDictionaryGetValue(d, k);
            let mut x: f64 = 0.0;
            // kCFNumberFloat64Type
            (!v.is_null() && CFNumberGetValue(v, 6, &mut x as *mut f64 as *mut c_void)).then_some(x)
        };
        for i in 0..n {
            let d = CFArrayGetValueAtIndex(list, i);
            if d.is_null() || float(d, kCGWindowAlpha).is_some_and(|a| a <= 0.01) {
                continue;
            }
            let mut frame = CGRect { origin: CGPoint { x: 0.0, y: 0.0 }, size: CGSize { width: 0.0, height: 0.0 } };
            let b = CFDictionaryGetValue(d, kCGWindowBounds);
            if b.is_null() || !CGRectMakeWithDictionaryRepresentation(b, &mut frame) {
                continue;
            }
            out.push(Window {
                pid: int(d, kCGWindowOwnerPID).unwrap_or(0) as i32,
                frame,
                layer: int(d, kCGWindowLayer).unwrap_or(0) as i32,
            });
        }
        CFRelease(list);
    }
    out
}

/// The part of a display (its frame in global points, top-left origin) not taken by the menu bar
/// and the Dock, in the same coordinates. `None` when it can't be told.
pub fn visible_frame(display: CGRect) -> Option<CGRect> {
    use objc2::msg_send;
    use objc2::runtime::{AnyClass, AnyObject};
    use objc2_foundation::NSRect;
    unsafe {
        let cls = AnyClass::get(c"NSScreen")?;
        let screens: *mut AnyObject = msg_send![cls, screens];
        if screens.is_null() {
            return None;
        }
        let n: usize = msg_send![screens, count];
        // Cocoa's y goes up from the bottom of the main (first) screen
        let first: *mut AnyObject = msg_send![screens, objectAtIndex: 0usize];
        let main: NSRect = msg_send![first, frame];
        let main_h = main.size.height;
        for i in 0..n {
            let sc: *mut AnyObject = msg_send![screens, objectAtIndex: i];
            let f: NSRect = msg_send![sc, frame];
            let top = main_h - (f.origin.y + f.size.height);
            let same = (f.origin.x - display.origin.x).abs() < 1.0
                && (top - display.origin.y).abs() < 1.0
                && (f.size.width - display.size.width).abs() < 1.0;
            if same {
                let v: NSRect = msg_send![sc, visibleFrame];
                let vtop = main_h - (v.origin.y + v.size.height);
                return Some(CGRect {
                    origin: CGPoint { x: v.origin.x, y: vtop },
                    size: CGSize { width: v.size.width, height: v.size.height },
                });
            }
        }
        None
    }
}

/// An app's bundle id from its process.
pub fn bundle_id(pid: i32) -> Option<String> {
    bundle_of_pid(pid)
}

/// A display: its index for `screencapture -D` (1-based, active list order) and bounds in points.
pub fn display_at(at: (f32, f32)) -> (u32, CGRect) {
    unsafe {
        let mut id = 0u32;
        let mut n = 0u32;
        if CGGetDisplaysWithPoint(CGPoint { x: at.0 as f64, y: at.1 as f64 }, 1, &mut id, &mut n) != 0 || n == 0 {
            id = CGMainDisplayID();
        }
        let mut list = [0u32; 16];
        let mut count = 0u32;
        CGGetActiveDisplayList(16, list.as_mut_ptr(), &mut count);
        let index = list[..count as usize].iter().position(|d| *d == id).map_or(1, |i| i as u32 + 1);
        (index, CGDisplayBounds(id))
    }
}

pub fn display_by_index(index: u32) -> Option<CGRect> {
    unsafe {
        let mut list = [0u32; 16];
        let mut count = 0u32;
        CGGetActiveDisplayList(16, list.as_mut_ptr(), &mut count);
        list.get(index.checked_sub(1)? as usize).filter(|_| index <= count).map(|d| CGDisplayBounds(*d))
    }
}

/// A JPEG of display `index` (screencapture, which runs under Lightair's Screen Recording
/// permission), at its full resolution.
pub fn capture(index: u32) -> Result<Vec<u8>, String> {
    let path = std::env::temp_dir().join(format!("lightair-piair-{}.jpg", std::process::id()));
    let out = std::process::Command::new("/usr/sbin/screencapture")
        .args(["-x", "-C", "-t", "jpg", "-D", &index.to_string()])
        .arg(&path)
        .output()
        .map_err(|e| e.to_string())?;
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).trim().to_string());
    }
    let bytes = std::fs::read(&path).map_err(|e| e.to_string())?;
    let _ = std::fs::remove_file(&path);
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::Ordering;

    use super::{CGEventCreate, CGEventSetIntegerValueField, HUMAN_AT, TAG, kCGEventSourceUserData, on_event, since_human};

    /// Events Lightair posts carry its tag and aren't the human's; any other event is, and pauses
    /// the AI (`since_human` restarts).
    #[test]
    fn an_event_is_the_humans_unless_it_carries_lightairs_tag() {
        unsafe {
            let ai = CGEventCreate(std::ptr::null_mut());
            CGEventSetIntegerValueField(ai, kCGEventSourceUserData, TAG);
            let before = HUMAN_AT.load(Ordering::Relaxed);
            on_event(std::ptr::null_mut(), 1, ai, std::ptr::null_mut());
            assert_eq!(HUMAN_AT.load(Ordering::Relaxed), before, "Lightair's own event isn't the human");

            let human = CGEventCreate(std::ptr::null_mut());
            on_event(std::ptr::null_mut(), 1, human, std::ptr::null_mut());
            assert!(since_human().is_some_and(|d| d.as_secs() < 1), "any other event is the human, just now");
        }
    }
}
