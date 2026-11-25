//! Display identifiers as screen captures name them, what there is to capture (displays,
//! windows, apps) and small captures of displays for thumbnails.
//!
//! What a screen source's settings hold: on macOS a display's UUID, a window's id and an app's
//! bundle id; on Windows a monitor's device path, and a window, app or game as libobs's window
//! capture named it (`title:class:executable`, each part with `#` written `#22` and `:` written
//! `#3A`), which stays the same across restarts.

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

/// Windows: the primary monitor's device path, as `screen_options` lists displays.
#[cfg(windows)]
pub fn main_display_uuid() -> Option<String> {
    lightair_capture::screen::displays().into_iter().find(|d| d.main).map(|d| d.uuid)
}

#[cfg(not(any(target_os = "macos", windows)))]
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

/// What a screen capture can show for `key` ("display", "window" or "app") in `mode`: labels
/// and the values its setting takes. Displays read "Name: 1920x1080 @ 0,0" (" (Primary
/// Monitor)" for the primary one), windows "[App] Title", apps and games their name. A game
/// (`mode` game, `key` "window") is the largest window of each program running.
#[cfg(windows)]
pub fn screen_options(mode: i64, key: &str) -> Vec<(String, serde_json::Value)> {
    use lightair_capture::{game, screen};
    use serde_json::Value;
    match key {
        "display" => screen::displays()
            .into_iter()
            .map(|d| {
                let main = if d.main { " (Primary Monitor)" } else { "" };
                let (x, y) = (d.origin.0 as i64, d.origin.1 as i64);
                let label = format!("{}: {}x{} @ {x},{y}{main}", d.name, d.pixel_size.0, d.pixel_size.1);
                (label, Value::from(d.uuid))
            })
            .collect(),
        "window" if mode == lightair_model::kinds::GAME => game::running()
            .into_iter()
            .map(|g| (g.name, Value::from(window_string(&g.title, &class_of(g.window), &g.exe))))
            .collect(),
        "window" => screen::shareable()
            .map(|s| {
                s.windows
                    .into_iter()
                    .filter(|w| w.on_screen && !w.title.is_empty())
                    .map(|w| {
                        let label = match &w.app_name {
                            Some(app) => format!("[{app}] {}", w.title),
                            None => w.title.clone(),
                        };
                        let exe = w.app.as_deref().unwrap_or("");
                        (label, Value::from(window_string(&w.title, &class_of(w.id), exe)))
                    })
                    .collect()
            })
            .unwrap_or_default(),
        // each app by its largest window
        "app" => screen::shareable()
            .map(|s| {
                s.apps
                    .into_iter()
                    .filter_map(|a| {
                        let w = s
                            .windows
                            .iter()
                            .filter(|w| w.on_screen && w.app.as_deref() == Some(a.bundle.as_str()))
                            .max_by_key(|w| w.size.0 as u64 * w.size.1 as u64)?;
                        Some((a.name, Value::from(window_string(&w.title, &class_of(w.id), &a.bundle))))
                    })
                    .collect()
            })
            .unwrap_or_default(),
        _ => Vec::new(),
    }
}

#[cfg(not(any(target_os = "macos", windows)))]
pub fn screen_options(_mode: i64, _key: &str) -> Vec<(String, serde_json::Value)> {
    Vec::new()
}

/// Windows: a window as libobs's window capture names it, `title:class:executable` (the
/// executable's file name; `exe` may be its full path), each part with `#` written `#22` and
/// `:` written `#3A`.
#[cfg(windows)]
pub fn window_string(title: &str, class: &str, exe: &str) -> String {
    let encode = |s: &str| s.replace('#', "#22").replace(':', "#3A");
    let exe = exe.rsplit(['\\', '/']).next().unwrap_or(exe);
    format!("{}:{}:{}", encode(title), encode(class), encode(exe))
}

/// Windows: the title, class and executable (its file name) of a window string
/// ([`window_string`]); `None` when it isn't one.
#[cfg(windows)]
pub fn parse_window_string(value: &str) -> Option<(String, String, String)> {
    let decode = |s: &str| s.replace("#3A", ":").replace("#22", "#");
    let mut parts = value.split(':');
    let (title, class, exe) = (parts.next()?, parts.next()?, parts.next()?);
    if parts.next().is_some() {
        return None;
    }
    Some((decode(title), decode(class), decode(exe)))
}

/// Windows: the window a window string ([`window_string`]) names now, by its id (`HWND`): of
/// the windows of its executable, the one with its title and class, else its class, else its
/// title, else the frontmost. Blocks a moment (it lists the windows): off the UI thread.
#[cfg(windows)]
pub fn find_window(value: &str) -> Option<u32> {
    let (title, class, exe) = parse_window_string(value)?;
    let windows = lightair_capture::screen::shareable().ok()?.windows;
    let name = |path: &str| path.rsplit(['\\', '/']).next().unwrap_or(path).to_ascii_lowercase();
    let ours: Vec<_> = windows
        .into_iter()
        .filter(|w| exe.is_empty() || w.app.as_deref().is_some_and(|a| name(a) == exe.to_ascii_lowercase()))
        .map(|w| {
            let class_now = class_of(w.id);
            (w, class_now)
        })
        .collect();
    let rank = |(w, c): &(lightair_capture::screen::Window, String)| match (w.title == title, *c == class) {
        (true, true) => 0,
        (false, true) => 1,
        (true, false) => 2,
        (false, false) => 3,
    };
    ours.iter().min_by_key(|w| rank(w)).map(|(w, _)| w.id)
}

/// Windows: window `id`'s class name ("" when it's gone).
#[cfg(windows)]
fn class_of(id: u32) -> String {
    use windows_sys::Win32::UI::WindowsAndMessaging::GetClassNameW;
    // a window handle's significant 32 bits, sign-extended as Windows does
    let hwnd = id as i32 as isize as *mut std::ffi::c_void;
    let mut name = [0u16; 256];
    // SAFETY: writes at most `name.len()` characters into `name`.
    let len = unsafe { GetClassNameW(hwnd, name.as_mut_ptr(), name.len() as i32) }.max(0) as usize;
    String::from_utf16_lossy(&name[..len])
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

/// The newest picture of a thumbnail's capture.
#[cfg(windows)]
#[derive(Default)]
struct Latest(parking_lot::Mutex<Option<crate::buffers::Buffer>>);

/// A display captured for a thumbnail, small and slow: its newest picture, until dropped.
#[cfg(windows)]
pub struct Capture {
    _running: lightair_capture::screen::ScreenCapture,
    latest: std::sync::Arc<Latest>,
}

#[cfg(windows)]
impl Capture {
    /// The newest picture (a BGRA shared texture).
    pub fn latest(&self) -> Option<crate::buffers::Buffer> {
        self.latest.0.lock().clone()
    }

    /// Its picture's size (0×0 before the first one).
    pub fn size(&self) -> (u32, u32) {
        self.latest.0.lock().as_ref().map_or((0, 0), |b| b.size())
    }
}

/// A capture of display `value` (its device path) at most `fit` big, for a thumbnail; `None`
/// without the display. Blocks a moment: off the UI thread.
#[cfg(windows)]
pub fn screen_capture(value: &serde_json::Value, fit: (u32, u32)) -> Option<Capture> {
    use lightair_capture::Image;
    use lightair_capture::{PixelFormat, screen};

    let uuid = value.as_str()?.to_string();
    let display = screen::displays().into_iter().find(|d| d.uuid.eq_ignore_ascii_case(&uuid))?;
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
    // the capture's textures on the buffers' graphics adapter, so they can be copied there
    lightair_capture::set_adapter(crate::buffers::adapter());
    let latest = std::sync::Arc::new(Latest::default());
    let into = latest.clone();
    let mut copier = crate::buffers::Copier::new();
    let sink = move |frame: lightair_capture::Frame| {
        if frame.format != PixelFormat::Bgra {
            return;
        }
        let buffer = match frame.image() {
            Image::Texture(t) => copier.copy(t.shared_handle(), frame.size, t.fence().map(|f| (f.handle, f.value))),
            Image::Memory(m) => copier.from_memory(&m.data, m.stride, frame.size),
        };
        drop(frame);
        if let Some(buffer) = buffer {
            *into.0.lock() = Some(buffer);
        }
    };
    let running = screen::ScreenCapture::start(&screen::Target::Display(display.uuid), &config, sink).ok()?;
    Some(Capture { _running: running, latest })
}

/// A display captured for a thumbnail. None is made here (see [`screen_capture`]).
#[cfg(not(any(target_os = "macos", windows)))]
pub struct Capture(std::convert::Infallible);

#[cfg(not(any(target_os = "macos", windows)))]
impl Capture {
    /// Its picture's size.
    pub fn size(&self) -> (u32, u32) {
        match self.0 {}
    }
}

/// No thumbnails of displays here: `None`, said in the log once.
#[cfg(not(any(target_os = "macos", windows)))]
pub fn screen_capture(_value: &serde_json::Value, _fit: (u32, u32)) -> Option<Capture> {
    static SAID: std::sync::Once = std::sync::Once::new();
    SAID.call_once(|| tracing::info!("not yet: display thumbnails on this platform without libobs"));
    None
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
