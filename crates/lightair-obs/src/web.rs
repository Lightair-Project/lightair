//! Web pages as sources (`lightair_web`): a page Chromium renders off-screen
//! (`lightair_web`, one Chromium on every platform, bundled with Lightair), its frames given to
//! libobs as they're painted, like a camera's; its sound goes to libobs too (Chromium doesn't
//! play it), so it reaches the mixer like any other source's.
//!
//! Settings: `url`; `width`, `height` (the frame's size: the page's in CSS pixels at 100%);
//! `fps`; `zoom` (percent, as a browser's); `css` (added to the page each time it loads).
//!
//! On macOS Chromium shares its frames as IOSurfaces, and on Windows as Direct3D 11 shared
//! textures, which libobs draws as they are (as OBS's browser source does): no frame is read
//! back to memory, copied or uploaded again. Elsewhere frames arrive as pixels, given to libobs
//! like a camera's.
//!
//! With Lightair's own compositor (macOS, `LIGHTAIR_ENGINE=own`) the same page gives it its
//! frames too ([`feed`]): while the compositor wants a page, each frame Chromium paints is also
//! copied on the GPU into a BGRA IOSurface from a small ring of the source's, and the compositor
//! takes the newest finished copy. While it takes them, libobs stops drawing the page itself
//! (its draw would be thrown away). With the audio engine too, the page's sound goes straight
//! into its track in the mixer (`audio_host::feeds_itself`) while a track takes it, not
//! through libobs.

use std::ffi::{CStr, c_char, c_void};
use std::sync::Arc;

use parking_lot::Mutex;

use crate::sys;

pub const KIND: &str = "lightair_web";

/// The source a page paints into, cleared when the source goes (a paint after that is dropped),
/// and the mixer's feed its sound goes into instead of libobs while a track takes it.
#[derive(Clone)]
struct Target(Arc<Mutex<Option<usize>>>, Arc<Mutex<Option<crate::audio_host::InputFeed>>>);

/// The audio engine's side of a page: a track taking its sound hands over the feed.
#[cfg(target_os = "macos")]
struct Sound(Arc<Mutex<Option<crate::audio_host::InputFeed>>>);

#[cfg(target_os = "macos")]
impl crate::audio_host::SoundFeeder for Sound {
    fn attach(&self, feed: Option<crate::audio_host::InputFeed>) {
        // the old feed (if any) is dropped here, on the engine thread
        drop(std::mem::replace(&mut *self.0.lock(), feed));
    }
}

/// How long a page stays open once nothing shows it (a scene being rebuilt, a transition's
/// hand-over).
const LINGER: std::time::Duration = std::time::Duration::from_secs(1);

/// A page is open only while something shows it (on air, a preview): off screen it's closed,
/// costing nothing (OBS's "shut down when not visible"), and opened again when shown.
struct Web {
    page: Mutex<Option<lightair_web::Page>>,
    /// On screen somewhere (libobs's show/hide).
    showing: std::sync::atomic::AtomicBool,
    hidden_since: Mutex<Option<std::time::Instant>>,
    target: Target,
    options: Mutex<lightair_web::PageOptions>,
    #[cfg(any(target_os = "macos", windows))]
    shared: Arc<shared::Frames>,
}

fn string(settings: *mut sys::obs_data_t, key: &CStr) -> String {
    let s = unsafe { sys::obs_data_get_string(settings, key.as_ptr()) };
    if s.is_null() { String::new() } else { unsafe { CStr::from_ptr(s) }.to_string_lossy().into_owned() }
}

fn options(settings: *mut sys::obs_data_t) -> lightair_web::PageOptions {
    unsafe {
        lightair_web::PageOptions {
            url: string(settings, c"url"),
            width: sys::obs_data_get_int(settings, c"width".as_ptr()).clamp(16, 7680) as u32,
            height: sys::obs_data_get_int(settings, c"height".as_ptr()).clamp(16, 7680) as u32,
            fps: sys::obs_data_get_int(settings, c"fps".as_ptr()).clamp(1, 60) as u32,
            zoom: sys::obs_data_get_int(settings, c"zoom".as_ptr()).clamp(25, 500) as f32 / 100.0,
            css: string(settings, c"css"),
        }
    }
}

fn open(
    o: &lightair_web::PageOptions,
    target: &Target,
    #[cfg(any(target_os = "macos", windows))] shared: Option<Arc<shared::Frames>>,
) -> Option<lightair_web::Page> {
    if o.url.trim().is_empty() {
        return None;
    }
    let audio_target = target.clone();
    let target = target.clone();
    let sink: lightair_web::FrameSink = Arc::new(move |bgra: &[u8], w: u32, h: u32| {
        let held = target.0.lock();
        let Some(source) = *held else { return };
        unsafe {
            let mut frame: sys::obs_source_frame = std::mem::zeroed();
            frame.data[0] = bgra.as_ptr() as *mut u8;
            frame.linesize[0] = w * 4;
            frame.width = w;
            frame.height = h;
            frame.format = sys::VIDEO_FORMAT_BGRA;
            frame.full_range = true;
            frame.timestamp = sys::os_gettime_ns();
            // (libobs copies it)
            sys::obs_source_output_video(source as *mut sys::obs_source_t, &frame);
        }
    });
    let target = audio_target;
    let audio: lightair_web::AudioSink = Arc::new(move |planes: &[&[f32]], rate: u32| {
        // a track takes the sound itself: into its feed (a block arriving while the feed is
        // swapped is dropped, never waited for)
        match target.1.try_lock() {
            Some(mut feed) => {
                if let Some(feed) = feed.as_mut() {
                    feed.push_planar(planes, rate);
                    return;
                }
            }
            None => return,
        }
        let held = target.0.lock();
        let Some(source) = *held else { return };
        let Some(frames) = planes.first().map(|p| p.len()) else { return };
        unsafe {
            let mut audio: sys::obs_source_audio = std::mem::zeroed();
            for (slot, plane) in audio.data.iter_mut().zip(planes) {
                *slot = plane.as_ptr() as *const u8;
            }
            audio.frames = frames as u32;
            audio.speakers = match planes.len() {
                1 => sys::SPEAKERS_MONO,
                2 => sys::SPEAKERS_STEREO,
                3 => sys::SPEAKERS_2POINT1,
                4 => sys::SPEAKERS_4POINT0,
                5 => sys::SPEAKERS_4POINT1,
                6 => sys::SPEAKERS_5POINT1,
                _ => sys::SPEAKERS_7POINT1,
            };
            audio.format = sys::AUDIO_FORMAT_FLOAT_PLANAR;
            audio.samples_per_sec = rate;
            audio.timestamp = sys::os_gettime_ns();
            // (libobs copies it)
            sys::obs_source_output_audio(source as *mut sys::obs_source_t, &audio);
        }
    });
    #[cfg(any(target_os = "macos", windows))]
    if let Some(frames) = shared {
        let texture: lightair_web::TextureSink = Arc::new(move |surface, _w, _h| frames.put(surface));
        return Some(lightair_web::Page::open_shared(o.clone(), texture, sink, audio));
    }
    Some(lightair_web::Page::open(o.clone(), sink, audio))
}

/// Runs `f` on a web page source's open page (`source` is its libobs source; any other kind,
/// or a page closed off screen: nothing).
fn with_page(source: *mut sys::obs_source_t, f: impl FnOnce(&lightair_web::Page)) {
    let web = unsafe {
        let id = sys::obs_source_get_id(source);
        if id.is_null() || CStr::from_ptr(id).to_bytes() != KIND.as_bytes() {
            return;
        }
        (sys::obs_obj_get_data(source as *mut c_void) as *const Web).as_ref()
    };
    if let Some(web) = web
        && let Some(page) = web.page.lock().as_ref()
    {
        f(page);
    }
}

/// Loads a web page source's page again.
pub(crate) fn reload(source: *mut sys::obs_source_t) {
    with_page(source, |p| p.reload());
}

/// Restyles a web page source's page now (custom CSS being typed, not yet its setting).
pub(crate) fn restyle(source: *mut sys::obs_source_t, css: String) {
    with_page(source, |p| p.restyle(css));
}

impl Web {
    /// Opens the page if it isn't (shown again).
    fn ensure_open(&self) {
        let mut page = self.page.lock();
        if page.is_none() {
            let o = self.options.lock().clone();
            #[cfg(any(target_os = "macos", windows))]
            {
                *page = open(&o, &self.target, Some(self.shared.clone()));
            }
            #[cfg(not(any(target_os = "macos", windows)))]
            {
                *page = open(&o, &self.target);
            }
        }
        if let Some(p) = page.as_ref() {
            p.set_visible(true);
        }
    }
}

unsafe extern "C" fn get_name(_: *mut c_void) -> *const c_char {
    c"Browser".as_ptr()
}

/// The web page sources' frames for Lightair's own compositor, by libobs source name (the
/// document source's id), while the switch is on.
#[cfg(target_os = "macos")]
static FEEDS: std::sync::LazyLock<Mutex<std::collections::HashMap<String, std::sync::Weak<shared::Frames>>>> =
    std::sync::LazyLock::new(Default::default);

/// A web page source's frames, as Lightair's own compositor takes them (macOS).
#[cfg(target_os = "macos")]
pub struct Feed(Arc<shared::Frames>);

/// The frames of the web page source named `source` (the document source's id), when there is
/// one and Lightair's own compositor is on.
#[cfg(target_os = "macos")]
pub fn feed(source: &str) -> Option<Feed> {
    FEEDS.lock().get(source).and_then(std::sync::Weak::upgrade).map(Feed)
}

#[cfg(target_os = "macos")]
impl Feed {
    /// Whether a compositor shows the page: while it does, each frame painted is copied for it
    /// (and the page is asked to paint once more when there's no copy yet); when it stops, the
    /// copies go.
    pub fn want(&self, on: bool) {
        self.0.want(on);
    }

    /// The newest finished copy of the page's frame, and a count that changes with each new
    /// one. `take`: the compositor draws it in libobs's place, so libobs stops drawing the page
    /// for as long as frames keep being taken.
    pub fn frame(&self, take: bool) -> Option<(crate::bridge::Buffer, u64)> {
        self.0.frame(take)
    }
}

unsafe extern "C" fn create(settings: *mut sys::obs_data_t, source: *mut sys::obs_source_t) -> *mut c_void {
    // a page paints only when it changes, irregularly: its newest frame shows at once (libobs's
    // buffering, made for steady video, would hold it for a second or more)
    unsafe { sys::obs_source_set_async_unbuffered(source, true) };
    let target = Target(Arc::new(Mutex::new(Some(source as usize))), Arc::new(Mutex::new(None)));
    #[cfg(target_os = "macos")]
    if crate::system_audio::mixer_captures() {
        crate::audio_host::feeds_itself(source, Arc::new(Sound(target.1.clone())));
    }
    // (opened when something first shows it)
    let web = Web {
        page: Mutex::new(None),
        showing: Default::default(),
        hidden_since: Mutex::new(None),
        target,
        options: Mutex::new(options(settings)),
        #[cfg(any(target_os = "macos", windows))]
        shared: Arc::new(shared::Frames::default()),
    };
    #[cfg(target_os = "macos")]
    if crate::bridge::own() {
        let name = unsafe { sys::obs_source_get_name(source) };
        if !name.is_null() {
            let name = unsafe { CStr::from_ptr(name) }.to_string_lossy().into_owned();
            FEEDS.lock().insert(name, Arc::downgrade(&web.shared));
        }
    }
    Box::into_raw(Box::new(web)) as *mut c_void
}

unsafe extern "C" fn destroy(data: *mut c_void) {
    let web = unsafe { Box::from_raw(data as *mut Web) };
    // no frame after this
    let source = web.target.0.lock().take();
    #[cfg(target_os = "macos")]
    if let Some(source) = source {
        crate::audio_host::feeds_itself_no_more(source as *mut sys::obs_source_t);
    }
    #[cfg(not(target_os = "macos"))]
    let _ = source;
    *web.target.1.lock() = None;
    #[cfg(target_os = "macos")]
    FEEDS.lock().retain(|_, f| !std::ptr::eq(f.as_ptr(), Arc::as_ptr(&web.shared)));
    drop(web);
}

unsafe extern "C" fn update(data: *mut c_void, settings: *mut sys::obs_data_t) {
    let web = unsafe { &*(data as *const Web) };
    let o = options(settings);
    if o == *web.options.lock() {
        return;
    }
    *web.options.lock() = o.clone();
    let mut page = web.page.lock();
    match page.as_ref() {
        Some(p) if !o.url.trim().is_empty() => p.update(o),
        // no address now: closed; a closed page opens with these when shown
        _ => {
            *page = None;
            drop(page);
            if web.showing.load(std::sync::atomic::Ordering::Relaxed) {
                web.ensure_open();
            }
        }
    }
}

unsafe extern "C" fn show(data: *mut c_void) {
    let web = unsafe { &*(data as *const Web) };
    web.showing.store(true, std::sync::atomic::Ordering::Relaxed);
    *web.hidden_since.lock() = None;
    web.ensure_open();
}

unsafe extern "C" fn hide(data: *mut c_void) {
    let web = unsafe { &*(data as *const Web) };
    web.showing.store(false, std::sync::atomic::Ordering::Relaxed);
    *web.hidden_since.lock() = Some(std::time::Instant::now());
    if let Some(page) = web.page.lock().as_ref() {
        page.set_visible(false);
    }
}

/// Closes the page once nothing has shown it for a moment.
unsafe extern "C" fn video_tick(data: *mut c_void, _seconds: f32) {
    let web = unsafe { &*(data as *const Web) };
    // the compositor wants a frame the page hasn't painted since (a still page paints once)
    #[cfg(target_os = "macos")]
    if web.shared.repaint_asked()
        && let Some(page) = web.page.lock().as_ref()
    {
        page.repaint();
    }
    if web.showing.load(std::sync::atomic::Ordering::Relaxed) {
        return;
    }
    let mut since = web.hidden_since.lock();
    if since.is_some_and(|t| t.elapsed() >= LINGER) {
        *since = None;
        drop(since);
        // (closing only asks Chromium's thread to)
        web.page.lock().take();
    }
}

unsafe extern "C" fn get_defaults(settings: *mut sys::obs_data_t) {
    unsafe {
        sys::obs_data_set_default_int(settings, c"width".as_ptr(), 1920);
        sys::obs_data_set_default_int(settings, c"height".as_ptr(), 1080);
        sys::obs_data_set_default_int(settings, c"fps".as_ptr(), 30);
        sys::obs_data_set_default_int(settings, c"zoom".as_ptr(), 100);
    }
}

/// Registers the source with libobs (once; after startup).
pub fn register() {
    static DONE: std::sync::Once = std::sync::Once::new();
    DONE.call_once(|| unsafe {
        let mut info: sys::obs_source_info = std::mem::zeroed();
        info.id = c"lightair_web".as_ptr();
        info.type_ = sys::OBS_SOURCE_TYPE_INPUT;
        // frames as they're painted, kept until the next (a still page paints once); its sound
        info.output_flags = sys::OBS_SOURCE_ASYNC_VIDEO | sys::OBS_SOURCE_AUDIO | sys::OBS_SOURCE_DO_NOT_DUPLICATE;
        // macOS and Windows: Chromium's shared textures, drawn as they are
        #[cfg(any(target_os = "macos", windows))]
        {
            info.output_flags = sys::OBS_SOURCE_VIDEO | sys::OBS_SOURCE_AUDIO | sys::OBS_SOURCE_DO_NOT_DUPLICATE | sys::OBS_SOURCE_CUSTOM_DRAW;
            info.video_render = Some(shared::video_render);
            info.get_width = Some(shared::get_width);
            info.get_height = Some(shared::get_height);
        }
        info.get_name = Some(get_name);
        info.create = Some(create);
        info.destroy = Some(destroy);
        info.update = Some(update);
        info.show = Some(show);
        info.hide = Some(hide);
        info.video_tick = Some(video_tick);
        info.get_defaults = Some(get_defaults);
        sys::obs_register_source_s(&info, std::mem::size_of::<sys::obs_source_info>());
    });
}

/// Chromium's frames as shared IOSurfaces (macOS): the newest one is kept (retained, and marked
/// in use so Chromium doesn't paint into it again meanwhile) until libobs draws the next.
///
/// For Lightair's own compositor, each frame painted while it wants the page is also copied into
/// a buffer of the source's own ring (Chromium's surface held in use until the GPU has copied
/// it), so the compositor holds a picture no one paints into.
#[cfg(target_os = "macos")]
mod shared {
    use std::collections::VecDeque;
    use std::ffi::c_void;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::sync::LazyLock;
    use std::time::{Duration, Instant};

    use parking_lot::Mutex;

    use super::Web;
    use crate::bridge;
    use crate::sys;

    /// Copy buffers per page at most: the newest copy, one each compositor may still hold until
    /// its next frame, one being copied into, one free.
    const RING: usize = 6;

    /// How long after the compositor last took a frame libobs still leaves the page undrawn
    /// (less than an output's wait before it falls back to libobs's own picture).
    const TAKEN_FOR: Duration = Duration::from_millis(250);

    /// What [`now`] counts from.
    static EPOCH: LazyLock<Instant> = LazyLock::new(Instant::now);

    /// Nanoseconds since `EPOCH`, never 0 (0 stands for "never").
    fn now() -> u64 {
        EPOCH.elapsed().as_nanos() as u64 + 1
    }

    #[link(name = "IOSurface", kind = "framework")]
    unsafe extern "C" {
        fn IOSurfaceIncrementUseCount(surface: *mut c_void);
        fn IOSurfaceDecrementUseCount(surface: *mut c_void);
        fn IOSurfaceGetWidth(surface: *mut c_void) -> usize;
        fn IOSurfaceGetHeight(surface: *mut c_void) -> usize;
    }
    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        fn CFRetain(cf: *const c_void) -> *const c_void;
        fn CFRelease(cf: *const c_void);
    }

    /// A retained IOSurface, in use until dropped.
    struct Surface(*mut c_void);

    impl Surface {
        fn hold(surface: *mut c_void) -> Self {
            unsafe {
                CFRetain(surface);
                IOSurfaceIncrementUseCount(surface);
            }
            Surface(surface)
        }
        fn size(&self) -> (u32, u32) {
            unsafe { (IOSurfaceGetWidth(self.0) as u32, IOSurfaceGetHeight(self.0) as u32) }
        }
    }

    impl Drop for Surface {
        fn drop(&mut self) {
            unsafe {
                IOSurfaceDecrementUseCount(self.0);
                CFRelease(self.0);
            }
        }
    }

    // SAFETY: IOSurfaces are reference-counted kernel objects usable from any thread.
    unsafe impl Send for Surface {}

    struct Texture(*mut sys::gs_texture_t);
    // SAFETY: only touched on libobs's graphics thread (destroyed there too).
    unsafe impl Send for Texture {}

    /// The surface on screen, the texture over it, its size.
    type Shown = (Option<Surface>, Option<Texture>, (u32, u32));

    /// A buffer of the ring, and the GPU's view of it that copies go into.
    struct Slot {
        buffer: bridge::Buffer,
        target: lightair_web::copy::Target,
    }

    /// A copy the GPU is making: into `buffer`, from Chromium's surface (held until it's done).
    struct Copying {
        copy: lightair_web::copy::Copy,
        buffer: bridge::Buffer,
        _from: Surface,
    }

    /// The compositor's copies of the page's frames.
    #[derive(Default)]
    struct Copies {
        ring: Vec<Slot>,
        /// Oldest first (one queue: they finish in order).
        copying: VecDeque<Copying>,
        /// The newest finished copy, and its count.
        latest: Option<(bridge::Buffer, u64)>,
        serial: u64,
    }

    // SAFETY: CoreVideo pixel buffers are reference-counted and usable from any thread; the
    // targets and copies are Metal objects only read once made; the surfaces IOSurfaces.
    unsafe impl Send for Copies {}

    impl Copies {
        /// Hands on the copies the GPU has finished.
        fn collect(&mut self) {
            while let Some(done) = self.copying.front().and_then(|c| c.copy.finished()) {
                let Some(c) = self.copying.pop_front() else { break };
                if done {
                    self.serial += 1;
                    self.latest = Some((c.buffer, self.serial));
                }
            }
        }

        /// Starts copying `surface` (Chromium's, being painted now) into a free buffer of its
        /// size; `false` when every buffer is still held (the frame is skipped).
        fn start(&mut self, surface: *mut c_void) -> bool {
            self.collect();
            let size = unsafe { (IOSurfaceGetWidth(surface) as u32, IOSurfaceGetHeight(surface) as u32) };
            // buffers of another size go once nothing holds them (the page was resized)
            self.ring.retain(|s| s.target.size() == size || s.buffer.retain_count() > 1);
            let free = self.ring.iter().position(|s| s.target.size() == size && s.buffer.retain_count() == 1);
            let slot = match free {
                Some(i) => i,
                None if self.ring.len() < RING => {
                    let Some(buffer) = bridge::bgra_buffer(size.0, size.1) else { return true };
                    let Some(target) = bridge::surface_of(&buffer).and_then(|s| unsafe { lightair_web::copy::Target::new(s) })
                    else {
                        return true;
                    };
                    self.ring.push(Slot { buffer, target });
                    self.ring.len() - 1
                }
                None => return false,
            };
            let slot = &self.ring[slot];
            // SAFETY: `surface` is Chromium's, live during its paint; it's held below until the
            // copy has run.
            if let Some(copy) = unsafe { lightair_web::copy::copy(surface, &slot.target) } {
                self.copying.push_back(Copying { copy, buffer: slot.buffer.clone(), _from: Surface::hold(surface) });
            }
            true
        }
    }

    #[derive(Default)]
    pub struct Frames {
        /// Painted, not yet drawn.
        next: Mutex<Option<Surface>>,
        /// What the texture shows (kept in use while it does), the texture, its size.
        shown: Mutex<Shown>,
        /// Copies for Lightair's own compositor, made while it wants the page.
        copies: Mutex<Copies>,
        wanted: AtomicBool,
        /// The page should paint again (the compositor has no copy of what it shows).
        repaint: AtomicBool,
        /// When the compositor last took a frame to draw in libobs's place ([`now`]; 0: never).
        taken: AtomicU64,
    }

    impl Frames {
        /// A new frame (main thread, during Chromium's paint).
        pub fn put(&self, surface: *mut c_void) {
            if surface.is_null() {
                return;
            }
            if self.wanted.load(Ordering::Relaxed) && !self.copies.lock().start(surface) {
                // every copy is still held: the next paint brings this one's picture
                self.repaint.store(true, Ordering::Relaxed);
            }
            let held = Surface::hold(surface);
            // the one it replaces (never drawn) goes back to Chromium
            *self.next.lock() = Some(held);
        }

        /// Whether a compositor shows the page (see `Feed::want`).
        pub fn want(&self, on: bool) {
            let was = self.wanted.swap(on, Ordering::Relaxed);
            if on && !was {
                // the newest frame Chromium painted, copied now; and another asked for, in case
                // the page has repainted that surface since
                // (one lock at a time: libobs's draw takes `shown`, then `next`)
                let next = self.next.lock().as_ref().map(|s| Surface::hold(s.0));
                let newest = next.or_else(|| self.shown.lock().0.as_ref().map(|s| Surface::hold(s.0)));
                if let Some(s) = newest {
                    self.copies.lock().start(s.0);
                }
                self.repaint.store(true, Ordering::Relaxed);
            } else if !on && was {
                let old = std::mem::take(&mut *self.copies.lock());
                self.taken.store(0, Ordering::Relaxed);
                drop(old);
            }
        }

        /// The newest finished copy (see `Feed::frame`).
        pub fn frame(&self, take: bool) -> Option<(bridge::Buffer, u64)> {
            let latest = {
                let mut copies = self.copies.lock();
                copies.collect();
                copies.latest.clone()
            }?;
            if take {
                self.taken.store(now(), Ordering::Relaxed);
            }
            Some(latest)
        }

        /// Whether the page was asked to paint again since last asked (and forgets it).
        pub fn repaint_asked(&self) -> bool {
            self.repaint.swap(false, Ordering::Relaxed)
        }

        /// Whether the compositor draws the page in libobs's place now.
        fn taken(&self) -> bool {
            let at = self.taken.load(Ordering::Relaxed);
            at != 0 && now().saturating_sub(at) < TAKEN_FOR.as_nanos() as u64
        }
    }

    impl Drop for Frames {
        fn drop(&mut self) {
            if let Some(Texture(t)) = self.shown.get_mut().1.take() {
                unsafe {
                    sys::obs_enter_graphics();
                    sys::gs_texture_destroy(t);
                    sys::obs_leave_graphics();
                }
            }
        }
    }

    fn frames<'a>(data: *mut c_void) -> &'a Frames {
        unsafe { &(*(data as *const Web)).shared }
    }

    pub unsafe extern "C" fn video_render(data: *mut c_void, _effect: *mut sys::gs_effect_t) {
        let f = frames(data);
        // Lightair's compositor draws the page from its own copies: libobs's draw would go unseen
        // (the newest frame waits in `next` for when libobs draws it again)
        if f.taken() {
            return;
        }
        let mut shown = f.shown.lock();
        if let Some(next) = f.next.lock().take() {
            let size = next.size();
            let rebound = match &shown.1 {
                Some(Texture(t)) if shown.2 == size => unsafe { sys::gs_texture_rebind_iosurface(*t, next.0) },
                _ => false,
            };
            if !rebound {
                if let Some(Texture(t)) = shown.1.take() {
                    unsafe { sys::gs_texture_destroy(t) };
                }
                let t = unsafe { sys::gs_texture_create_from_iosurface(next.0) };
                shown.1 = (!t.is_null()).then_some(Texture(t));
            }
            shown.2 = size;
            // the previous surface goes back to Chromium now that the texture shows this one
            shown.0 = Some(next);
        }
        let Some(Texture(tex)) = &shown.1 else { return };
        let (w, h) = shown.2;
        unsafe {
            // an IOSurface is a rectangle texture; Chromium's pixels are premultiplied (the
            // blend says so)
            // (Metal has no rectangle textures)
            let rect = sys::gs_get_device_type() as u32 != sys::GS_DEVICE_METAL;
            let effect = sys::obs_get_base_effect(if rect { sys::OBS_EFFECT_DEFAULT_RECT } else { sys::OBS_EFFECT_DEFAULT });
            let image = sys::gs_effect_get_param_by_name(effect, c"image".as_ptr());
            sys::gs_effect_set_texture(image, *tex);
            sys::gs_blend_state_push();
            sys::gs_blend_function(sys::GS_BLEND_ONE, sys::GS_BLEND_INVSRCALPHA);
            while sys::gs_effect_loop(effect, c"Draw".as_ptr()) {
                sys::gs_draw_sprite(*tex, 0, w, h);
            }
            sys::gs_blend_state_pop();
        }
    }

    fn size(data: *mut c_void) -> (u32, u32) {
        let web = unsafe { &*(data as *const Web) };
        let shown = web.shared.shown.lock().2;
        if shown.0 > 0 {
            shown
        } else {
            let o = web.options.lock();
            (o.width, o.height)
        }
    }

    pub unsafe extern "C" fn get_width(data: *mut c_void) -> u32 {
        size(data).0
    }

    pub unsafe extern "C" fn get_height(data: *mut c_void) -> u32 {
        size(data).1
    }
}

/// Chromium's frames as Direct3D 11 shared textures (Windows): Chromium hands each paint's
/// texture over as an NT handle valid during its paint call, so it's duplicated there and
/// opened by libobs (on its own device) when it next draws; the texture opened last is drawn
/// until a newer one arrives.
#[cfg(windows)]
mod shared {
    use std::ffi::c_void;

    use parking_lot::Mutex;
    use windows_sys::Win32::Foundation::{CloseHandle, DUPLICATE_SAME_ACCESS, DuplicateHandle, HANDLE};
    use windows_sys::Win32::System::Threading::GetCurrentProcess;

    use super::Web;
    use crate::sys;

    /// A handle of our own to a shared texture, closed when dropped.
    struct Handle(HANDLE);

    impl Drop for Handle {
        fn drop(&mut self) {
            unsafe { CloseHandle(self.0) };
        }
    }

    // SAFETY: a kernel handle, usable from any thread.
    unsafe impl Send for Handle {}

    struct Texture(*mut sys::gs_texture_t);
    // SAFETY: only touched on libobs's graphics thread (destroyed there too).
    unsafe impl Send for Texture {}

    #[derive(Default)]
    pub struct Frames {
        /// Painted, not yet opened.
        next: Mutex<Option<Handle>>,
        /// The texture drawn, and its size.
        shown: Mutex<(Option<Texture>, (u32, u32))>,
    }

    impl Frames {
        /// A new frame (Chromium's thread, during its paint): `handle` is Chromium's.
        pub fn put(&self, handle: *mut c_void) {
            if handle.is_null() {
                return;
            }
            let mut own: HANDLE = std::ptr::null_mut();
            let ok = unsafe {
                let me = GetCurrentProcess();
                DuplicateHandle(me, handle as HANDLE, me, &mut own, 0, 0, DUPLICATE_SAME_ACCESS)
            };
            if ok != 0 && !own.is_null() {
                // the one it replaces (never drawn) is let go
                *self.next.lock() = Some(Handle(own));
            }
        }
    }

    impl Drop for Frames {
        fn drop(&mut self) {
            if let Some(Texture(t)) = self.shown.get_mut().0.take() {
                unsafe {
                    sys::obs_enter_graphics();
                    sys::gs_texture_destroy(t);
                    sys::obs_leave_graphics();
                }
            }
        }
    }

    fn frames<'a>(data: *mut c_void) -> &'a Frames {
        unsafe { &(*(data as *const Web)).shared }
    }

    pub unsafe extern "C" fn video_render(data: *mut c_void, _effect: *mut sys::gs_effect_t) {
        let f = frames(data);
        let mut shown = f.shown.lock();
        if let Some(next) = f.next.lock().take() {
            // (NT handles are small numbers: libobs takes them as 32 bits)
            let t = unsafe { sys::gs_texture_open_nt_shared(next.0 as usize as u32) };
            if !t.is_null() {
                if let Some(Texture(old)) = shown.0.take() {
                    unsafe { sys::gs_texture_destroy(old) };
                }
                shown.1 = unsafe { (sys::gs_texture_get_width(t), sys::gs_texture_get_height(t)) };
                shown.0 = Some(Texture(t));
            }
        }
        let Some(Texture(tex)) = &shown.0 else { return };
        let (w, h) = shown.1;
        unsafe {
            // Chromium's pixels are premultiplied (the blend says so)
            let effect = sys::obs_get_base_effect(sys::OBS_EFFECT_DEFAULT);
            let image = sys::gs_effect_get_param_by_name(effect, c"image".as_ptr());
            sys::gs_effect_set_texture(image, *tex);
            sys::gs_blend_state_push();
            sys::gs_blend_function(sys::GS_BLEND_ONE, sys::GS_BLEND_INVSRCALPHA);
            while sys::gs_effect_loop(effect, c"Draw".as_ptr()) {
                sys::gs_draw_sprite(*tex, 0, w, h);
            }
            sys::gs_blend_state_pop();
        }
    }

    fn size(data: *mut c_void) -> (u32, u32) {
        let web = unsafe { &*(data as *const Web) };
        let shown = web.shared.shown.lock().1;
        if shown.0 > 0 {
            shown
        } else {
            let o = web.options.lock();
            (o.width, o.height)
        }
    }

    pub unsafe extern "C" fn get_width(data: *mut c_void) -> u32 {
        size(data).0
    }

    pub unsafe extern "C" fn get_height(data: *mut c_void) -> u32 {
        size(data).1
    }
}
