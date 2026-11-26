//! Web pages as sources (`lightair_web`): a page Chromium renders off screen (`lightair_web`,
//! one Chromium on every platform, bundled with Lightair), open only while something shows it.
//!
//! Settings: `url`; `width`, `height` (the frame's size: the page's in CSS pixels at 100%);
//! `fps`; `zoom` (percent, as a browser's); `css` (added to the page each time it loads).
//!
//! On macOS Chromium shares its frames as IOSurfaces ([`Frames`]): while Lightair's compositor
//! wants a page, it draws Chromium's own surface of the newest frame, held in use (Chromium paints
//! into another meanwhile) for as long as anything draws it ([`feed`]). A page's sound goes into
//! its track in the mixer while a track takes it.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use lightair_model::Id;
use parking_lot::Mutex;

pub const KIND: &str = "lightair_web";

#[cfg(target_os = "macos")]
use std::collections::HashMap;
#[cfg(target_os = "macos")]
use std::ffi::c_void;
#[cfg(target_os = "macos")]
use std::sync::atomic::AtomicU64;
#[cfg(target_os = "macos")]
use std::sync::{LazyLock, Weak};

#[cfg(target_os = "macos")]
mod mac {
    use super::*;
    /// How long after the compositor last took a frame the page still counts as drawn by it.
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
    pub struct Surface(*mut c_void);

    impl Surface {
        /// # Safety
        /// `surface` must be a live `IOSurfaceRef`.
        pub unsafe fn hold(surface: *mut c_void) -> Self {
            unsafe {
                CFRetain(surface);
                IOSurfaceIncrementUseCount(surface);
            }
            Surface(surface)
        }
        /// The surface (an `IOSurfaceRef`), borrowed.
        pub fn ptr(&self) -> *mut c_void {
            self.0
        }

        pub fn size(&self) -> (u32, u32) {
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

    /// The frames handed to Lightair's compositor: Chromium's surfaces as pixel buffers, each
    /// held in use while anything draws it.
    #[derive(Default)]
    struct Handed {
        held: Vec<(crate::buffers::Buffer, Surface)>,
        /// The newest, and its count.
        latest: Option<(crate::buffers::Buffer, u64)>,
        serial: u64,
    }

    // SAFETY: CoreVideo pixel buffers are reference counted thread-safely; the surfaces are
    // IOSurfaces.
    unsafe impl Send for Handed {}

    impl Handed {
        /// `surface` (Chromium's, being painted now) is the newest frame; the ones nothing draws
        /// any more go back to Chromium.
        fn put(&mut self, surface: *mut c_void) {
            // only this list holds them: not the newest, not drawn
            self.held.retain(|(b, _)| b.retain_count() > 1);
            // SAFETY: `surface` is Chromium's, live during its paint (or held in use).
            let Some(buffer) = (unsafe { crate::buffers::over_surface(surface) }) else { return };
            // SAFETY: as above.
            let hold = unsafe { Surface::hold(surface) };
            self.held.push((buffer.clone(), hold));
            self.serial += 1;
            self.latest = Some((buffer, self.serial));
        }
    }

    /// A page's painted frames (macOS): the newest Chromium painted (retained, and marked in use so
    /// Chromium doesn't paint into it again meanwhile) until whoever draws the page takes it, and,
    /// while a compositor wants the page, each one painted as a pixel buffer over Chromium's own
    /// surface (held in use while anything draws it).
    #[derive(Default)]
    pub struct Frames {
        /// Painted, not yet drawn.
        next: Mutex<Option<Surface>>,
        /// What a drawer of the frames shows now (kept in use while it does).
        shown: Mutex<Option<Surface>>,
        /// Frames for Lightair's own compositor, while it wants the page.
        handed: Mutex<Handed>,
        wanted: AtomicBool,
        /// The page should paint again (the compositor has no copy of what it shows).
        repaint: AtomicBool,
        /// When the compositor last took a frame to draw ([`now`]; 0: never).
        taken: AtomicU64,
    }

    impl Frames {
        /// A new frame (main thread, during Chromium's paint).
        ///
        /// # Safety
        /// `surface` must be null or a live `IOSurfaceRef` (Chromium's, during its paint).
        pub unsafe fn put(&self, surface: *mut c_void) {
            if surface.is_null() {
                return;
            }
            if self.wanted.load(Ordering::Relaxed) {
                self.handed.lock().put(surface);
            }
            let held = unsafe { Surface::hold(surface) };
            // the one it replaces (never drawn) goes back to Chromium
            *self.next.lock() = Some(held);
        }

        /// Whether a compositor shows the page: while it does, each frame painted is handed to it
        /// (and the page is asked to paint once more); when it stops, the frames held for it go
        /// back to Chromium.
        pub fn want(&self, on: bool) {
            let was = self.wanted.swap(on, Ordering::Relaxed);
            if on && !was {
                // the newest frame Chromium painted (held in use, so as painted) handed on now
                // (one lock at a time: a drawer takes `shown`, then `next`)
                let next = self.next.lock().as_ref().map(|s| unsafe { Surface::hold(s.0) });
                let newest = next.or_else(|| self.shown.lock().as_ref().map(|s| unsafe { Surface::hold(s.0) }));
                if let Some(s) = newest {
                    self.handed.lock().put(s.0);
                }
                self.repaint.store(true, Ordering::Relaxed);
            } else if !on && was {
                let old = std::mem::take(&mut *self.handed.lock());
                self.taken.store(0, Ordering::Relaxed);
                drop(old);
            }
        }

        /// The newest frame handed to the compositor, and a count that changes with each new one.
        /// `take`: the compositor draws it, and counts as drawing the page for as long as frames
        /// keep being taken.
        pub fn frame(&self, take: bool) -> Option<(crate::buffers::Buffer, u64)> {
            let latest = self.handed.lock().latest.clone()?;
            if take {
                self.taken.store(now(), Ordering::Relaxed);
            }
            Some(latest)
        }

        /// Whether the page was asked to paint again since last asked (and forgets it).
        pub fn repaint_asked(&self) -> bool {
            self.repaint.swap(false, Ordering::Relaxed)
        }

        /// Whether the compositor draws the page now.
        pub fn taken(&self) -> bool {
            let at = self.taken.load(Ordering::Relaxed);
            at != 0 && now().saturating_sub(at) < TAKEN_FOR.as_nanos() as u64
        }

        /// For a drawer other than the compositor: the newest painted frame not drawn yet.
        pub fn take_next(&self) -> Option<Surface> {
            self.next.lock().take()
        }

        /// For a drawer: it shows `surface` now (the one before goes back to Chromium).
        pub fn set_shown(&self, surface: Surface) {
            *self.shown.lock() = Some(surface);
        }

        /// The size of what's shown (0×0 before anything is).
        pub fn shown_size(&self) -> (u32, u32) {
            self.shown.lock().as_ref().map_or((0, 0), Surface::size)
        }
    }

    /// The web pages' frames for Lightair's own compositor, by document source id (as text).
    pub(super) static FEEDS: LazyLock<Mutex<HashMap<String, Weak<Frames>>>> = LazyLock::new(Default::default);

    /// A web page's frames, as Lightair's own compositor takes them.
    pub struct Feed(Arc<Frames>);

    /// The frames of the web page of document source `source` (its id as text), when it has one.
    pub fn feed(source: &str) -> Option<Feed> {
        FEEDS.lock().get(source).and_then(Weak::upgrade).map(Feed)
    }

    impl Feed {
        /// See [`Frames::want`].
        pub fn want(&self, on: bool) {
            self.0.want(on);
        }

        /// See [`Frames::frame`].
        pub fn frame(&self, take: bool) -> Option<(crate::buffers::Buffer, u64)> {
            self.0.frame(take)
        }
    }
}
#[cfg(target_os = "macos")]
use mac::FEEDS;
#[cfg(target_os = "macos")]
pub use mac::{Feed, Frames, Surface, feed};

// ---------------------------------------------------------------------------------------------
// The page

/// Where a page's frames and sound go besides the compositor and the mixer.
pub trait Elsewhere: Send + Sync {
    /// A frame painted into memory (where Chromium can't share textures).
    fn frame(&self, bgra: &[u8], width: u32, height: u32);
    /// Sound no track of the mixer took.
    fn audio(&self, planes: &[&[f32]], rate: u32);
}

/// How long a page stays open once nothing shows it (a scene being rebuilt, a transition's
/// hand-over).
const LINGER: Duration = Duration::from_secs(1);

/// A web page for document source `source`: open only while something shows it (on air, a
/// preview), closed a moment after nothing does, costing nothing off screen.
pub struct WebPage {
    source: Id,
    page: Mutex<Option<lightair_web::Page>>,
    /// On screen somewhere.
    showing: AtomicBool,
    hidden_since: Mutex<Option<Instant>>,
    options: Mutex<lightair_web::PageOptions>,
    texture: Option<lightair_web::TextureSink>,
    frame: lightair_web::FrameSink,
    audio: lightair_web::AudioSink,
    #[cfg(target_os = "macos")]
    frames: Arc<Frames>,
}

impl WebPage {
    /// A page for `source` with `options` (opened when first shown). Its shared textures go to
    /// `texture` (macOS: the page's own [`Frames`] when `None`); frames painted into memory and
    /// sound no track takes go to `elsewhere`.
    pub fn new(
        source: Id,
        options: lightair_web::PageOptions,
        texture: Option<lightair_web::TextureSink>,
        elsewhere: Option<Arc<dyn Elsewhere>>,
    ) -> WebPage {
        let slot = crate::audio_host::mixer_on().then(|| crate::audio_host::feed_slot(source));
        let to = elsewhere.clone();
        let frame: lightair_web::FrameSink = Arc::new(move |bgra: &[u8], w: u32, h: u32| {
            if let Some(e) = &to {
                e.frame(bgra, w, h);
            }
        });
        let audio: lightair_web::AudioSink = Arc::new(move |planes: &[&[f32]], rate: u32| {
            // a track takes the sound itself: into its feed
            if slot.as_ref().is_some_and(|s| s.push_planar(planes, rate)) {
                return;
            }
            if let Some(e) = &elsewhere {
                e.audio(planes, rate);
            }
        });
        #[cfg(target_os = "macos")]
        let frames = Arc::new(Frames::default());
        #[cfg(target_os = "macos")]
        let texture = texture.or_else(|| {
            let f = frames.clone();
            // SAFETY: Chromium hands its surface over during its paint
            Some(Arc::new(move |surface, _w, _h| unsafe { f.put(surface) }) as lightair_web::TextureSink)
        });
        #[cfg(target_os = "macos")]
        if crate::show::own() {
            FEEDS.lock().insert(source.to_string(), Arc::downgrade(&frames));
        }
        WebPage {
            source,
            page: Mutex::new(None),
            showing: AtomicBool::new(false),
            hidden_since: Mutex::new(None),
            options: Mutex::new(options),
            texture,
            frame,
            audio,
            #[cfg(target_os = "macos")]
            frames,
        }
    }

    /// The page's frames (macOS).
    #[cfg(target_os = "macos")]
    pub fn frames(&self) -> &Arc<Frames> {
        &self.frames
    }

    pub fn options(&self) -> lightair_web::PageOptions {
        self.options.lock().clone()
    }

    /// New settings: the open page follows them; no address closes it (it opens with these
    /// when shown).
    pub fn set_options(&self, o: lightair_web::PageOptions) {
        if o == *self.options.lock() {
            return;
        }
        *self.options.lock() = o.clone();
        let mut page = self.page.lock();
        match page.as_ref() {
            Some(p) if !o.url.trim().is_empty() => p.update(o),
            _ => {
                *page = None;
                drop(page);
                if self.showing.load(Ordering::Relaxed) {
                    self.ensure_open();
                }
            }
        }
    }

    /// Something shows the page now.
    pub fn show(&self) {
        self.showing.store(true, Ordering::Relaxed);
        *self.hidden_since.lock() = None;
        self.ensure_open();
    }

    /// Nothing shows the page now (it closes after a moment, [`WebPage::tick`]).
    pub fn hide(&self) {
        self.showing.store(false, Ordering::Relaxed);
        *self.hidden_since.lock() = Some(Instant::now());
        if let Some(page) = self.page.lock().as_ref() {
            page.set_visible(false);
        }
    }

    /// A few times a second: paints again when the compositor wants a frame the page hasn't
    /// painted since (a still page paints once), and closes the page once nothing has shown it
    /// for a moment.
    pub fn tick(&self) {
        #[cfg(target_os = "macos")]
        if self.frames.repaint_asked()
            && let Some(page) = self.page.lock().as_ref()
        {
            page.repaint();
        }
        if self.showing.load(Ordering::Relaxed) {
            return;
        }
        let mut since = self.hidden_since.lock();
        if since.is_some_and(|t| t.elapsed() >= LINGER) {
            *since = None;
            drop(since);
            // (closing only asks Chromium's thread to)
            self.page.lock().take();
        }
    }

    /// Loads the page again.
    pub fn reload(&self) {
        if let Some(p) = self.page.lock().as_ref() {
            p.reload();
        }
    }

    /// Restyles the page now (custom CSS being typed, not yet its setting).
    pub fn restyle(&self, css: String) {
        if let Some(p) = self.page.lock().as_ref() {
            p.restyle(css);
        }
    }

    fn ensure_open(&self) {
        let mut page = self.page.lock();
        if page.is_none() {
            let o = self.options.lock().clone();
            if !o.url.trim().is_empty() {
                *page = Some(match &self.texture {
                    Some(t) => lightair_web::Page::open_shared(o, t.clone(), self.frame.clone(), self.audio.clone()),
                    None => lightair_web::Page::open(o, self.frame.clone(), self.audio.clone()),
                });
            }
        }
        if let Some(p) = page.as_ref() {
            p.set_visible(true);
        }
    }
}

impl Drop for WebPage {
    fn drop(&mut self) {
        self.page.lock().take();
        if crate::audio_host::mixer_on() {
            crate::audio_host::feeds_itself_no_more(self.source);
        }
        #[cfg(target_os = "macos")]
        FEEDS.lock().retain(|_, f| !std::ptr::eq(f.as_ptr(), Arc::as_ptr(&self.frames)));
    }
}
