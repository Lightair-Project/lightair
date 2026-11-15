//! Web pages in Lightair: one Chromium on every platform (CEF's official build, pinned in this
//! crate's Cargo.toml and bundled with Lightair), so a page renders the same on macOS and
//! Windows.
//!
//! - Chromium starts the first time a page is opened, on the main thread, and is driven from
//!   Lightair's own loop (CEF's external message pump: [`pump`], called by the app). No window:
//!   each page renders off-screen at its own size (one CSS pixel a pixel) and every frame it
//!   paints (BGRA) goes to the page's sink, which hands it to the scene.
//! - Pages are asked for from any thread ([`Page::open`]); the main thread makes them.
//! - Chromium's helper processes are Lightair's own: the `lightair-web-helper` binary, which
//!   runs [`helper_main`] (on macOS, as Lightair's four helper apps inside its bundle).
//!
//! Where Chromium isn't bundled (tests run outside Lightair.app, the Windows check made from a
//! Mac), pages simply never paint ([`available`] says so).

use std::sync::Arc;

/// Where a page's frames go: premultiplied BGRA rows, `width × 4` bytes each, and the size.
pub type FrameSink = Arc<dyn Fn(&[u8], u32, u32) + Send + Sync>;

/// Where a page's sound goes: one plane of 32-bit float samples per channel (all the same
/// length), and the sample rate. Chromium sends sound here instead of playing it.
pub type AudioSink = Arc<dyn Fn(&[&[f32]], u32) + Send + Sync>;

/// A page's settings.
#[derive(Debug, Clone, PartialEq)]
pub struct PageOptions {
    pub url: String,
    pub width: u32,
    pub height: u32,
    /// Frames a second at most (Chromium paints only what changes).
    pub fps: u32,
    /// Page zoom, as a browser's (1.0: 100%): the page lays out `width / zoom` CSS pixels
    /// wide, drawn sharp at `zoom` pixels each, so the frame stays `width × height`.
    pub zoom: f32,
}

impl PageOptions {
    /// The page's view in CSS pixels (what it lays out in).
    pub fn view(&self) -> (u32, u32) {
        let z = self.zoom.clamp(0.25, 5.0);
        (((self.width as f32 / z).round() as u32).max(1), ((self.height as f32 / z).round() as u32).max(1))
    }
}

/// A web page rendering off-screen; closed when dropped.
pub struct Page {
    id: u64,
}

impl Page {
    /// Opens `options` (made on the main thread soon after); its frames go to `sink`, its sound
    /// to `audio`.
    pub fn open(options: PageOptions, sink: FrameSink, audio: AudioSink) -> Page {
        let id = imp::next_id();
        imp::request(imp::Request::Open { id, options, sink, audio });
        Page { id }
    }

    /// New settings: another address, size or rate.
    pub fn update(&self, options: PageOptions) {
        imp::request(imp::Request::Update { id: self.id, options });
    }

    /// Loads the page again, skipping the cache.
    pub fn reload(&self) {
        imp::request(imp::Request::Reload { id: self.id });
    }
}

impl Drop for Page {
    fn drop(&mut self) {
        imp::request(imp::Request::Close { id: self.id });
    }
}

/// Whether Chromium is bundled here (pages can paint).
pub fn available() -> bool {
    imp::available()
}

/// Runs Chromium's helper process when this process is one (`--type=` on its command line):
/// its exit code; `None` for any other process. The helper binary's `main` calls it first.
pub fn helper_main() -> Option<i32> {
    imp::helper_main()
}

/// Drives Chromium: makes, changes and closes the pages asked for, and does its pending work.
/// Main thread only, every few milliseconds (cheap while there's nothing to do).
pub fn pump() {
    imp::pump()
}

/// Whether [`pump`] should come back soon (pages are open, or about to be).
pub fn busy() -> bool {
    imp::busy()
}

#[cfg(any(target_os = "macos", all(windows, not(lightair_cross_check))))]
mod imp {
    use super::{AudioSink, FrameSink, PageOptions};
    use cef::*;
    use parking_lot::Mutex;
    use std::cell::RefCell;
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::sync::Arc;

    pub enum Request {
        Open { id: u64, options: PageOptions, sink: FrameSink, audio: AudioSink },
        Update { id: u64, options: PageOptions },
        Reload { id: u64 },
        Close { id: u64 },
    }

    static NEXT: AtomicU64 = AtomicU64::new(1);
    static REQUESTS: Mutex<Vec<Request>> = Mutex::new(Vec::new());
    static OPEN_PAGES: AtomicU64 = AtomicU64::new(0);
    static STARTED: AtomicBool = AtomicBool::new(false);
    /// Chromium's browser context is set up (pages can be made; before that, they wait).
    static READY: AtomicBool = AtomicBool::new(false);

    pub fn next_id() -> u64 {
        NEXT.fetch_add(1, Ordering::Relaxed)
    }

    pub fn request(r: Request) {
        if matches!(r, Request::Open { .. }) {
            OPEN_PAGES.fetch_add(1, Ordering::Relaxed);
        }
        REQUESTS.lock().push(r);
    }

    pub fn busy() -> bool {
        OPEN_PAGES.load(Ordering::Relaxed) > 0 || !REQUESTS.lock().is_empty()
    }

    // ------------------------------------------------------------ where Chromium is

    /// Chromium's framework (macOS): Lightair.app/Contents/Frameworks, next to the executable's
    /// folder; from a helper app, three folders up.
    #[cfg(target_os = "macos")]
    fn framework(helper: bool) -> Option<std::path::PathBuf> {
        let exe = std::env::current_exe().ok()?;
        let up = if helper { "../../.." } else { "../Frameworks" };
        let path = exe.parent()?.join(up).join("Chromium Embedded Framework.framework");
        path.canonicalize().ok().filter(|p| p.exists())
    }

    pub fn available() -> bool {
        #[cfg(target_os = "macos")]
        return framework(false).is_some();
        #[cfg(windows)]
        return std::env::current_exe().ok().and_then(|e| e.parent().map(|d| d.join("libcef.dll").exists())).unwrap_or(false);
    }

    /// Loads Chromium's library (macOS loads it at run time, from the bundle), then declares the
    /// CEF API version this was built against, which CEF needs before anything else.
    fn load(helper: bool) -> bool {
        #[cfg(target_os = "macos")]
        let loaded = {
            use std::os::unix::ffi::OsStrExt;
            let Some(dir) = framework(helper) else { return false };
            let lib = dir.join("Chromium Embedded Framework");
            let Ok(name) = std::ffi::CString::new(lib.as_os_str().as_bytes()) else { return false };
            // (loaded once; kept for the process's life)
            unsafe { load_library(Some(&*name.as_ptr().cast())) == 1 }
        };
        #[cfg(windows)]
        let loaded = {
            let _ = helper;
            true
        };
        if loaded {
            let _ = api_hash(sys::CEF_API_VERSION_LAST, 0);
        }
        loaded
    }

    pub fn helper_main() -> Option<i32> {
        if !std::env::args().any(|a| a.starts_with("--type=")) {
            return None;
        }
        if !load(true) {
            return Some(1);
        }
        let args = args::Args::new();
        let code = execute_process(Some(args.as_main_args()), None, std::ptr::null_mut());
        Some(code.max(0))
    }

    // ------------------------------------------------------------ the browser process

    wrap_browser_process_handler! {
        struct LightairBrowserProcess;
        impl BrowserProcessHandler {
            fn on_context_initialized(&self) {
                tracing::debug!("Chromium ready for pages");
                READY.store(true, Ordering::Relaxed);
            }
        }
    }

    wrap_app! {
        struct LightairApp;
        impl App {
            fn browser_process_handler(&self) -> Option<BrowserProcessHandler> {
                Some(LightairBrowserProcess::new())
            }
            fn on_before_command_line_processing(&self, _process_type: Option<&CefString>, command_line: Option<&mut CommandLine>) {
                let Some(line) = command_line else { return };
                // no window of its own, no keychain prompts, no first-run UI; sound plays without
                // a click (nobody clicks a page in a show)
                for switch in [
                    "use-mock-keychain",
                    "disable-features=MacAppCodeSignClone",
                    "no-first-run",
                    "no-default-browser-check",
                    "autoplay-policy=no-user-gesture-required",
                ] {
                    match switch.split_once('=') {
                        Some((name, value)) => line.append_switch_with_value(Some(&CefString::from(name)), Some(&CefString::from(value))),
                        None => line.append_switch(Some(&CefString::from(switch))),
                    }
                }
            }
        }
    }

    /// Chromium's cache (cookies, storage): one folder, which only one Chromium can use at a
    /// time. Another Lightair running alongside (a second copy, a test) gets one of its own,
    /// removed by a later start once that copy has gone.
    fn cache_dir() -> std::path::PathBuf {
        let temp = std::env::temp_dir();
        let shared = temp.join("lightair-web");
        #[cfg(target_os = "macos")]
        {
            let alive = |pid: i32| pid > 0 && unsafe { libc::kill(pid, 0) } == 0;
            let me = std::process::id() as i32;
            // (Chromium's lock: a link to "host-pid")
            let holder = std::fs::read_link(shared.join("SingletonLock"))
                .ok()
                .and_then(|t| t.to_string_lossy().rsplit('-').next().and_then(|p| p.parse::<i32>().ok()));
            if holder.is_some_and(|pid| pid != me && alive(pid)) {
                for entry in std::fs::read_dir(&temp).into_iter().flatten().flatten() {
                    let name = entry.file_name().to_string_lossy().into_owned();
                    if let Some(pid) = name.strip_prefix("lightair-web-").and_then(|p| p.parse::<i32>().ok())
                        && pid != me
                        && !alive(pid)
                    {
                        let _ = std::fs::remove_dir_all(entry.path());
                    }
                }
                return temp.join(format!("lightair-web-{me}"));
            }
        }
        shared
    }

    /// Starts Chromium (the first time a page is opened): false where it isn't bundled.
    fn start() -> bool {
        if STARTED.load(Ordering::Relaxed) {
            return true;
        }
        if !available() || !load(false) {
            tracing::warn!("Chromium isn't bundled here: web pages won't paint");
            return false;
        }
        #[cfg(target_os = "macos")]
        mac::adopt_app_protocol();
        let args = args::Args::new();
        let mut settings = Settings {
            no_sandbox: 1,
            external_message_pump: 1,
            windowless_rendering_enabled: 1,
            log_severity: LogSeverity::WARNING,
            // transparent where a page doesn't paint (overlays)
            background_color: 0x0000_0000,
            ..Default::default()
        };
        #[cfg(target_os = "macos")]
        {
            let exe = std::env::current_exe().unwrap_or_default();
            let frameworks = exe.parent().map(|d| d.join("../Frameworks")).unwrap_or_default();
            let bundle = exe.parent().and_then(|d| d.parent()).and_then(|d| d.parent()).map(|p| p.to_path_buf()).unwrap_or_default();
            let helper = frameworks.join("Lightair Helper.app/Contents/MacOS/Lightair Helper");
            settings.browser_subprocess_path = CefString::from(helper.to_string_lossy().as_ref());
            settings.framework_dir_path = CefString::from(frameworks.join("Chromium Embedded Framework.framework").to_string_lossy().as_ref());
            settings.main_bundle_path = CefString::from(bundle.to_string_lossy().as_ref());
        }
        let cache = cache_dir();
        settings.root_cache_path = CefString::from(cache.to_string_lossy().as_ref());
        let mut app = LightairApp::new();
        let ok = initialize(Some(args.as_main_args()), Some(&settings), Some(&mut app), std::ptr::null_mut()) == 1;
        if !ok {
            tracing::warn!(code = get_exit_code(), "Chromium didn't start");
            return false;
        }
        STARTED.store(true, Ordering::Relaxed);
        tracing::info!("Chromium started");
        true
    }

    // ------------------------------------------------------------ pages

    /// A page on the main thread: its browser (once made), its view (CSS size and zoom, read by
    /// the renderer).
    struct Live {
        browser: Option<Browser>,
        view: Arc<Mutex<((u32, u32), f32)>>,
        options: PageOptions,
    }

    fn view_of(o: &PageOptions) -> ((u32, u32), f32) {
        (o.view(), o.zoom.clamp(0.25, 5.0))
    }

    thread_local! {
        static PAGES: RefCell<HashMap<u64, Live>> = RefCell::new(HashMap::new());
    }

    wrap_render_handler! {
        struct PageRender {
            view: Arc<Mutex<((u32, u32), f32)>>,
            sink: FrameSink,
        }
        impl RenderHandler {
            fn view_rect(&self, _browser: Option<&mut Browser>, rect: Option<&mut Rect>) {
                if let Some(rect) = rect {
                    let ((w, h), _) = *self.view.lock();
                    *rect = Rect { x: 0, y: 0, width: w.max(1) as i32, height: h.max(1) as i32 };
                }
            }
            fn screen_info(&self, _browser: Option<&mut Browser>, info: Option<&mut ScreenInfo>) -> ::std::os::raw::c_int {
                // pixels a CSS pixel: the page's zoom, whatever display Lightair is on
                if let Some(info) = info {
                    info.device_scale_factor = self.view.lock().1;
                    return 1;
                }
                0
            }
            fn on_paint(&self, _browser: Option<&mut Browser>, type_: PaintElementType, _dirty: Option<&[Rect]>, buffer: *const u8, width: ::std::os::raw::c_int, height: ::std::os::raw::c_int) {
                if type_ != PaintElementType::VIEW || buffer.is_null() || width <= 0 || height <= 0 {
                    return;
                }
                let bytes = unsafe { std::slice::from_raw_parts(buffer, width as usize * height as usize * 4) };
                (self.sink)(bytes, width as u32, height as u32);
            }
        }
    }

    wrap_life_span_handler! {
        struct PageLife {
            id: u64,
        }
        impl LifeSpanHandler {
            fn on_after_created(&self, browser: Option<&mut Browser>) {
                let Some(browser) = browser else { return };
                let id = self.id;
                tracing::debug!(id, "web page made");
                let browser = browser.clone();
                PAGES.with(|pages| match pages.borrow_mut().get_mut(&id) {
                    Some(live) => live.browser = Some(browser),
                    // closed before it was made
                    None => {
                        if let Some(host) = browser.host() {
                            host.close_browser(1);
                        }
                    }
                });
            }
        }
    }

    // A page's sound: asked for as 48 kHz stereo; Chromium says what it sends when a stream
    // starts (rate, channels), then sends planar float packets.
    wrap_audio_handler! {
        struct PageAudio {
            format: Arc<Mutex<(u32, usize)>>,
            sink: AudioSink,
        }
        impl AudioHandler {
            fn audio_parameters(&self, _browser: Option<&mut Browser>, params: Option<&mut AudioParameters>) -> ::std::os::raw::c_int {
                let Some(params) = params else { return 0 };
                params.channel_layout = ChannelLayout::LAYOUT_STEREO;
                params.sample_rate = 48_000;
                if params.frames_per_buffer <= 0 {
                    params.frames_per_buffer = 1024;
                }
                1
            }
            fn on_audio_stream_started(&self, _browser: Option<&mut Browser>, params: Option<&AudioParameters>, channels: ::std::os::raw::c_int) {
                let rate = params.map_or(48_000, |p| p.sample_rate.max(1) as u32);
                *self.format.lock() = (rate, channels.clamp(1, 8) as usize);
            }
            fn on_audio_stream_packet(&self, _browser: Option<&mut Browser>, data: *mut *const f32, frames: ::std::os::raw::c_int, _pts: i64) {
                let (rate, channels) = *self.format.lock();
                if data.is_null() || frames <= 0 {
                    return;
                }
                let mut planes: [&[f32]; 8] = [&[]; 8];
                for (c, plane) in planes.iter_mut().enumerate().take(channels) {
                    let ptr = unsafe { *data.add(c) };
                    if ptr.is_null() {
                        return;
                    }
                    *plane = unsafe { std::slice::from_raw_parts(ptr, frames as usize) };
                }
                (self.sink)(&planes[..channels], rate);
            }
        }
    }

    wrap_client! {
        struct PageClient {
            render: RenderHandler,
            life: LifeSpanHandler,
            audio: AudioHandler,
        }
        impl Client {
            fn audio_handler(&self) -> Option<AudioHandler> {
                Some(self.audio.clone())
            }
            fn render_handler(&self) -> Option<RenderHandler> {
                Some(self.render.clone())
            }
            fn life_span_handler(&self) -> Option<LifeSpanHandler> {
                Some(self.life.clone())
            }
        }
    }

    fn open(id: u64, options: PageOptions, sink: FrameSink, audio: AudioSink) {
        let view = Arc::new(Mutex::new(view_of(&options)));
        let format = Arc::new(Mutex::new((48_000, 2)));
        let mut client = PageClient::new(PageRender::new(view.clone(), sink), PageLife::new(id), PageAudio::new(format, audio));
        let window = WindowInfo { windowless_rendering_enabled: 1, ..Default::default() };
        let browser_settings = BrowserSettings {
            windowless_frame_rate: options.fps.clamp(1, 60) as i32,
            background_color: 0x0000_0000,
            ..Default::default()
        };
        let url = CefString::from(options.url.as_str());
        PAGES.with(|pages| pages.borrow_mut().insert(id, Live { browser: None, view, options }));
        if browser_host_create_browser(Some(&window), Some(&mut client), Some(&url), Some(&browser_settings), None, None) != 1 {
            tracing::warn!("Chromium didn't open a page");
        }
    }

    fn update(id: u64, options: PageOptions) {
        PAGES.with(|pages| {
            let mut pages = pages.borrow_mut();
            let Some(live) = pages.get_mut(&id) else { return };
            let before = std::mem::replace(&mut live.options, options.clone());
            *live.view.lock() = view_of(&options);
            let Some(browser) = &live.browser else { return };
            if let Some(host) = browser.host() {
                if before.zoom != options.zoom {
                    host.notify_screen_info_changed();
                }
                if (before.width, before.height, before.zoom) != (options.width, options.height, options.zoom) {
                    host.was_resized();
                }
                if before.fps != options.fps {
                    host.set_windowless_frame_rate(options.fps.clamp(1, 60) as i32);
                }
            }
            if before.url != options.url
                && let Some(frame) = browser.main_frame()
            {
                frame.load_url(Some(&CefString::from(options.url.as_str())));
            }
        });
    }

    fn reload(id: u64) {
        PAGES.with(|pages| {
            if let Some(browser) = pages.borrow().get(&id).and_then(|live| live.browser.clone()) {
                browser.reload_ignore_cache();
            }
        });
    }

    fn close(id: u64) {
        OPEN_PAGES.fetch_sub(1, Ordering::Relaxed);
        PAGES.with(|pages| {
            if let Some(live) = pages.borrow_mut().remove(&id)
                && let Some(host) = live.browser.and_then(|b| b.host())
            {
                host.close_browser(1);
            }
        });
    }

    pub fn pump() {
        if !REQUESTS.lock().is_empty() && !start() {
            // not bundled: forget them (their pages stay blank)
            REQUESTS.lock().clear();
            return;
        }
        if STARTED.load(Ordering::Relaxed) {
            do_message_loop_work();
        }
        // pages are made once Chromium's context is ready (macOS insists)
        if !READY.load(Ordering::Relaxed) {
            return;
        }
        let requests = std::mem::take(&mut *REQUESTS.lock());
        for r in requests {
            match r {
                Request::Open { id, options, sink, audio } => open(id, options, sink, audio),
                Request::Update { id, options } => update(id, options),
                Request::Reload { id } => reload(id),
                Request::Close { id } => close(id),
            }
        }
    }

    /// Chromium on macOS expects the application object to say whether it's in the middle of
    /// sending an event (CrAppControlProtocol); Lightair's window toolkit owns that object, so
    /// the two methods are added to its class.
    #[cfg(target_os = "macos")]
    mod mac {
        use objc2::runtime::{AnyClass, AnyObject, Bool, Sel};
        use objc2::{msg_send, sel};
        use std::sync::atomic::{AtomicBool, Ordering};

        static HANDLING: AtomicBool = AtomicBool::new(false);

        extern "C" fn is_handling(_: &AnyObject, _: Sel) -> Bool {
            Bool::new(HANDLING.load(Ordering::Relaxed))
        }

        extern "C" fn set_handling(_: &AnyObject, _: Sel, on: Bool) {
            HANDLING.store(on.as_bool(), Ordering::Relaxed);
        }

        unsafe extern "C" {
            fn class_addMethod(cls: *const AnyClass, name: Sel, imp: *const std::ffi::c_void, types: *const std::ffi::c_char) -> Bool;
            fn objc_getProtocol(name: *const std::ffi::c_char) -> *const std::ffi::c_void;
            fn class_addProtocol(cls: *const AnyClass, protocol: *const std::ffi::c_void) -> Bool;
        }

        pub fn adopt_app_protocol() {
            let Some(app_class) = AnyClass::get(c"NSApplication") else { return };
            let app: *mut AnyObject = unsafe { msg_send![app_class, sharedApplication] };
            if app.is_null() {
                return;
            }
            let class: *const AnyClass = unsafe { msg_send![app, class] };
            unsafe {
                class_addMethod(class, sel!(isHandlingSendEvent), is_handling as *const _, c"c@:".as_ptr());
                class_addMethod(class, sel!(setHandlingSendEvent:), set_handling as *const _, c"v@:c".as_ptr());
                for name in [c"CrAppProtocol", c"CrAppControlProtocol", c"CefAppProtocol"] {
                    let p = objc_getProtocol(name.as_ptr());
                    let added = !p.is_null() && class_addProtocol(class, p).as_bool();
                    let conforms: Bool = if p.is_null() { Bool::NO } else { msg_send![app, conformsToProtocol: p as *const AnyObject] };
                    tracing::debug!(protocol = ?name, found = !p.is_null(), added, conforms = conforms.as_bool(), class = ?(*class).name(), "Chromium app protocol");
                }
            }
        }
    }
}

/// No Chromium here (the Windows check made from a Mac): pages never paint.
#[cfg(not(any(target_os = "macos", all(windows, not(lightair_cross_check)))))]
mod imp {
    use super::{AudioSink, FrameSink, PageOptions};

    #[allow(dead_code)] // (asked for, never made here)
    pub enum Request {
        Open { id: u64, options: PageOptions, sink: FrameSink, audio: AudioSink },
        Update { id: u64, options: PageOptions },
        Reload { id: u64 },
        Close { id: u64 },
    }

    pub fn next_id() -> u64 {
        0
    }

    pub fn request(_: Request) {}

    pub fn available() -> bool {
        false
    }

    pub fn helper_main() -> Option<i32> {
        None
    }

    pub fn pump() {}

    pub fn busy() -> bool {
        false
    }
}
