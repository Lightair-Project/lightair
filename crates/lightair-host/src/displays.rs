//! Display identifiers as screen captures name them.

/// UUID of the main display, as used by `screen_capture`'s `display_uuid` setting.
#[cfg(target_os = "macos")]
pub fn main_display_uuid() -> Option<String> {
    use std::ffi::{c_char, c_void};

    type CFTypeRef = *const c_void;
    #[link(name = "CoreGraphics", kind = "framework")]
    unsafe extern "C" {
        fn CGMainDisplayID() -> u32;
    }
    #[link(name = "ColorSync", kind = "framework")]
    unsafe extern "C" {
        fn CGDisplayCreateUUIDFromDisplayID(display: u32) -> CFTypeRef;
    }
    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        fn CFUUIDCreateString(alloc: CFTypeRef, uuid: CFTypeRef) -> CFTypeRef;
        fn CFStringGetCString(s: CFTypeRef, buf: *mut c_char, size: isize, encoding: u32) -> bool;
        fn CFRelease(cf: CFTypeRef);
    }
    const UTF8: u32 = 0x0800_0100;

    unsafe {
        let uuid = CGDisplayCreateUUIDFromDisplayID(CGMainDisplayID());
        if uuid.is_null() {
            return None;
        }
        let text = CFUUIDCreateString(std::ptr::null(), uuid);
        CFRelease(uuid);
        if text.is_null() {
            return None;
        }
        let mut buf = [0 as c_char; 64];
        let ok = CFStringGetCString(text, buf.as_mut_ptr(), buf.len() as isize, UTF8);
        CFRelease(text);
        ok.then(|| std::ffi::CStr::from_ptr(buf.as_ptr()).to_string_lossy().into_owned())
    }
}

#[cfg(not(target_os = "macos"))]
pub fn main_display_uuid() -> Option<String> {
    None
}

/// What a screen capture can show for `key` ("display", "window" or "app") in `mode`: labels
/// and the values its setting takes. Displays read "Name: 1512x982 @ 0,0" (" (Primary Monitor)"
/// for the main one), windows "[App] Title", apps their name.
#[cfg(target_os = "macos")]
pub fn screen_options(mode: i64, key: &str) -> Vec<(String, serde_json::Value)> {
    use lightair_capture::screen;
    use serde_json::Value;
    let _ = mode;
    match key {
        "display" => screen::displays()
            .into_iter()
            .map(|d| {
                let main = if d.main { " (Primary Monitor)" } else { "" };
                let label = format!("{}: {}x{} @ {},{}{main}", d.name, d.size.0, d.size.1, d.origin.0, d.origin.1);
                (label, Value::from(d.uuid))
            })
            .collect(),
        "window" => screen::shareable()
            .map(|s| {
                s.windows
                    .into_iter()
                    .filter(|w| w.on_screen && w.layer == 0 && !w.title.is_empty())
                    .map(|w| {
                        let label = match &w.app_name {
                            Some(app) => format!("[{app}] {}", w.title),
                            None => w.title.clone(),
                        };
                        (label, Value::from(w.id))
                    })
                    .collect()
            })
            .unwrap_or_default(),
        "app" => screen::shareable()
            .map(|s| {
                s.apps.into_iter().filter(|a| !a.bundle.is_empty()).map(|a| (a.name, Value::from(a.bundle))).collect()
            })
            .unwrap_or_default(),
        _ => Vec::new(),
    }
}

#[cfg(not(target_os = "macos"))]
pub fn screen_options(_mode: i64, _key: &str) -> Vec<(String, serde_json::Value)> {
    Vec::new()
}

/// The newest picture of a thumbnail's capture.
#[cfg(target_os = "macos")]
#[derive(Default)]
struct Latest(parking_lot::Mutex<Option<crate::buffers::Buffer>>);

// SAFETY: CoreVideo's buffers are reference counted thread-safely, and a captured picture isn't
// written again once it's handed over.
#[cfg(target_os = "macos")]
unsafe impl Send for Latest {}
#[cfg(target_os = "macos")]
unsafe impl Sync for Latest {}

/// A display captured for a thumbnail, small and slow: its newest picture, until dropped.
#[cfg(target_os = "macos")]
pub struct Capture {
    _running: lightair_capture::screen::ScreenCapture,
    latest: std::sync::Arc<Latest>,
}

#[cfg(target_os = "macos")]
impl Capture {
    /// The newest picture (BGRA, IOSurface-backed).
    pub fn latest(&self) -> Option<crate::buffers::Buffer> {
        self.latest.0.lock().clone()
    }

    /// Its picture's size (0×0 before the first one).
    pub fn size(&self) -> (u32, u32) {
        use objc2_core_video::{CVPixelBufferGetHeight, CVPixelBufferGetWidth};
        self.latest
            .0
            .lock()
            .as_ref()
            .map_or((0, 0), |b| (CVPixelBufferGetWidth(b) as u32, CVPixelBufferGetHeight(b) as u32))
    }
}

/// A capture of display `value` (its UUID) at most `fit` big, for a thumbnail; `None` without
/// access or the display. Blocks a moment: off the UI thread.
#[cfg(target_os = "macos")]
pub fn screen_capture(value: &serde_json::Value, fit: (u32, u32)) -> Option<Capture> {
    use std::ptr::NonNull;

    use lightair_capture::{PixelFormat, screen};
    use objc2_core_foundation::CFRetained;
    use objc2_core_video::CVPixelBuffer;

    if !screen::access() {
        return None;
    }
    let uuid = value.as_str()?.to_string();
    let display = screen::displays().into_iter().find(|d| d.uuid == uuid)?;
    let k = (fit.0 as f32 / display.pixel_size.0.max(1) as f32)
        .min(fit.1 as f32 / display.pixel_size.1.max(1) as f32)
        .min(1.0);
    let size = (
        ((display.pixel_size.0 as f32 * k) as u32).max(2) & !1,
        ((display.pixel_size.1 as f32 * k) as u32).max(2) & !1,
    );
    let config = screen::Config {
        size: Some(size),
        fps: 5,
        format: PixelFormat::Bgra,
        cursor: true,
        queue_depth: 3,
        ..Default::default()
    };
    let latest = std::sync::Arc::new(Latest::default());
    let into = latest.clone();
    let sink = move |frame: lightair_capture::Frame| {
        if frame.format != PixelFormat::Bgra {
            return;
        }
        let Some(raw) = NonNull::new(frame.into_raw().cast::<CVPixelBuffer>()) else { return };
        // SAFETY: `into_raw` gave up the frame's reference, now this one's
        let buffer = unsafe { CFRetained::from_raw(raw) };
        *into.0.lock() = Some(buffer);
    };
    let running = screen::ScreenCapture::start(&screen::Target::Display(uuid), &config, sink).ok()?;
    Some(Capture { _running: running, latest })
}
