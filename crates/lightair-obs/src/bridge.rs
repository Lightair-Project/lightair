//! macOS, with `LIGHTAIR_ENGINE=own`: what Lightair's own compositor (`lightair-render`'s
//! `SceneRenderer`, driven by `lightair-preview`) needs from libobs while libobs still captures.
//!
//! - **Pictures:** the compositor says which document sources it wants ([`want`]). On libobs's
//!   graphics thread, after each frame, every wanted source is drawn at its own size into a
//!   libobs render target and copied into a BGRA IOSurface from a small ring per source (libobs
//!   can't render into an IOSurface directly on OpenGL: the copy is a framebuffer blit there, a
//!   texture copy on Metal). A GPU fence says when a copy is done; only finished buffers are
//!   handed on, and [`picture`] gives the newest one from any thread. Sources nobody wants
//!   aren't drawn. A source is drawn through its libobs filters (its effects already in the
//!   picture), or, when every compositor drawing it asks, without them: the compositor then
//!   draws its effects itself. Each picture says which it is.
//! - **Sources:** the engine thread keeps a map from document source to libobs source, updated
//!   as the reconciler makes and removes them ([`source_added`], [`source_removed`]), so the
//!   graphics thread never touches the reconciler.
//! - **The document:** after each applied change the engine thread publishes the workspace and
//!   the scene switch under way, if any ([`snapshot`]).
//!
//! With the switch off every entry point returns at once and nothing is registered with libobs.

use std::collections::{HashMap, VecDeque};
use std::ffi::c_void;
use std::ptr::{self, NonNull};
use std::sync::{Arc, LazyLock, OnceLock};
use std::time::Instant;

use lightair_model::{Id, TransitionSettings, Workspace};
use objc2_core_foundation::{CFBoolean, CFDictionary, CFRetained, CFString, CFType};
use objc2_core_video::{
    CVPixelBuffer, CVPixelBufferCreate, CVPixelBufferGetIOSurface, kCVPixelBufferIOSurfacePropertiesKey,
    kCVPixelBufferMetalCompatibilityKey, kCVPixelFormatType_32BGRA, kCVReturnSuccess,
};
use parking_lot::Mutex;

use crate::obs::{ObsSource, SourceRef};
use crate::sys;

/// Buffers per source besides one for each compositor drawing it (which may hold one until its
/// next frame): the one handed on, one the GPU is filling, and one free.
const RING: usize = 3;

/// Whether Lightair's own compositor draws the program (`LIGHTAIR_ENGINE=own`). Read once.
pub fn own() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var("LIGHTAIR_ENGINE").is_ok_and(|v| v.trim().eq_ignore_ascii_case("own")))
}

// ---------------------------------------------------------------- the document

/// A scene switch under way: what the compositor needs to draw it.
#[derive(Debug, Clone, Copy)]
pub struct Switch {
    pub from: Id,
    pub to: Id,
    pub settings: TransitionSettings,
    pub started: Instant,
}

impl Switch {
    /// How far through it is at `now` (0..1; 1 once it's over).
    pub fn progress(&self, now: Instant) -> f32 {
        let ms = now.saturating_duration_since(self.started).as_secs_f32() * 1000.0;
        (ms / self.settings.duration_ms.max(1) as f32).clamp(0.0, 1.0)
    }
}

/// The document as the engine last applied it.
pub struct Snapshot {
    pub workspace: Arc<Workspace>,
    /// The switch the last program change started (cleared by a cut).
    pub switch: Option<Switch>,
}

static SNAPSHOT: Mutex<Option<Arc<Snapshot>>> = Mutex::new(None);
static SWITCH: Mutex<Option<Switch>> = Mutex::new(None);

/// The document as the engine last applied it (`None` before the first change, or with the
/// switch off). The lock is held only to clone an `Arc`.
pub fn snapshot() -> Option<Arc<Snapshot>> {
    SNAPSHOT.lock().clone()
}

/// Engine thread, after applying a change: `workspace` is what libobs now shows.
pub(crate) fn publish(workspace: &Workspace) {
    if !own() {
        return;
    }
    let switch = *SWITCH.lock();
    *SNAPSHOT.lock() = Some(Arc::new(Snapshot { workspace: Arc::new(workspace.clone()), switch }));
}

/// Engine thread, when the program changes: the switch it starts (`None`: a cut).
pub(crate) fn set_switch(switch: Option<Switch>) {
    if own() {
        *SWITCH.lock() = switch;
    }
}

// ---------------------------------------------------------------- sources

/// A retained `CVPixelBuffer`: BGRA, IOSurface-backed.
pub type Buffer = CFRetained<CVPixelBuffer>;

struct Bridge {
    /// Document source → its libobs source (engine thread writes; graphics thread reads).
    sources: Mutex<HashMap<Id, SourceRef>>,
    /// The sources each compositor (by canvas) wants pictures of, each with whether it wants
    /// it without its libobs filters.
    wanted: Mutex<HashMap<Id, HashMap<Id, bool>>>,
    /// The newest finished picture of each source drawn, whether it was drawn without its
    /// filters, and the libobs frame that drew it.
    latest: Mutex<HashMap<Id, (Buffer, bool, u64)>>,
    /// Each drawn source's ring (graphics thread only).
    rings: Mutex<HashMap<Id, Ring>>,
}

// SAFETY: the CoreVideo buffers are reference counted thread-safely and aren't written once
// handed on; the rings' libobs and GL handles are only used on libobs's graphics thread (inside
// its graphics context); `SourceRef` is Send and Sync itself.
unsafe impl Send for Bridge {}
unsafe impl Sync for Bridge {}

static BRIDGE: LazyLock<Bridge> = LazyLock::new(|| Bridge {
    sources: Mutex::new(HashMap::new()),
    wanted: Mutex::new(HashMap::new()),
    latest: Mutex::new(HashMap::new()),
    rings: Mutex::new(HashMap::new()),
});

/// Engine thread: the reconciler made `source`'s libobs source.
pub(crate) fn source_added(source: Id, obs: &ObsSource) {
    if !own() {
        return;
    }
    if let Some(r) = obs.new_ref() {
        BRIDGE.sources.lock().insert(source, r);
    }
}

/// Engine thread: the reconciler removed `source`'s libobs source.
pub(crate) fn source_removed(source: Id) {
    if !own() {
        return;
    }
    // released outside the lock (the graphics thread draws under it)
    let gone = BRIDGE.sources.lock().remove(&source);
    drop(gone);
    BRIDGE.latest.lock().remove(&source);
}

/// Engine thread: the reconciler let go of everything (another workspace, shutdown).
pub(crate) fn clear() {
    if !own() {
        return;
    }
    let gone = std::mem::take(&mut *BRIDGE.sources.lock());
    drop(gone);
    BRIDGE.latest.lock().clear();
    *SWITCH.lock() = None;
    *SNAPSHOT.lock() = None;
}

/// The compositor for `canvas` wants pictures of `sources` from the next frame on (replacing
/// what it wanted before; empty when it stops). `true`: without the source's libobs filters
/// (drawn so only when every compositor drawing it asks, and the source has no filter that
/// isn't one of its effects).
pub fn want(canvas: Id, sources: HashMap<Id, bool>) {
    if !own() {
        return;
    }
    static REGISTERED: std::sync::Once = std::sync::Once::new();
    if !sources.is_empty() {
        REGISTERED.call_once(|| unsafe { sys::obs_add_main_rendered_callback(Some(on_rendered), ptr::null_mut()) });
    }
    let mut wanted = BRIDGE.wanted.lock();
    if sources.is_empty() {
        wanted.remove(&canvas);
    } else {
        wanted.insert(canvas, sources);
    }
}

/// A source's picture: a BGRA, IOSurface-backed pixel buffer, colours multiplied by alpha.
pub struct Picture {
    pub buffer: Buffer,
    /// Drawn without the source's libobs filters: its effects aren't in it.
    pub unfiltered: bool,
    /// The video time of the libobs frame that drew it: a different value is a new picture
    /// (buffers are reused).
    pub serial: u64,
}

impl Picture {
    /// The buffer as the renderer takes it.
    pub fn pixel_buffer(&self) -> Option<lightair_render::texture::PixelBuffer> {
        let buffer = self.buffer.clone();
        // SAFETY: the reference made by `clone` is given up to the pixel buffer, which releases it.
        unsafe { lightair_render::texture::PixelBuffer::from_raw(CFRetained::into_raw(buffer).as_ptr().cast()) }
    }
}

/// The newest finished picture of `source`, if it's wanted and has been drawn. Any thread.
pub fn picture(source: Id) -> Option<Picture> {
    let (buffer, unfiltered, serial) = BRIDGE.latest.lock().get(&source)?.clone();
    Some(Picture { buffer, unfiltered, serial })
}

// ---------------------------------------------------------------- timers

/// What each timer shows now (the engine's timers write it once a second, while the switch is
/// on), so the compositor can draw them itself.
static TIMER_TEXTS: LazyLock<Mutex<HashMap<Id, String>>> = LazyLock::new(Default::default);

/// The text the timer `source` shows now, once its first tick has made one.
pub fn timer_text(source: Id) -> Option<String> {
    TIMER_TEXTS.lock().get(&source).cloned()
}

/// The engine's timers: `source` now shows `text` (`None`: the timer is gone).
pub(crate) fn set_timer_text(source: Id, text: Option<&str>) {
    if !own() {
        return;
    }
    let mut texts = TIMER_TEXTS.lock();
    match text {
        Some(t) => {
            texts.insert(source, t.to_string());
        }
        None => {
            texts.remove(&source);
        }
    }
}

/// The engine's timers were all removed.
pub(crate) fn clear_timer_texts() {
    if own() {
        TIMER_TEXTS.lock().clear();
    }
}

// ---------------------------------------------------------------- avatars

/// What posed an avatar source on libobs's last tick, so the compositor can pose its own drawing
/// of the same avatar. The libobs source keeps the camera, the trackers, the calibration, the
/// co-host's face and mood, the mic level and push to talk; the compositor only reads this.
#[derive(Clone, Debug)]
pub struct AvatarPosing {
    /// The face seen, calibrated and mirrored as the avatar's settings say, with the co-host's
    /// face merged in when this is its avatar; `None`: none seen (the avatar settles).
    pub face: Option<lightair_avatar::Face>,
    /// The body seen (a VRM's body tracking; `None` for flat avatars or with the body off).
    pub body: Option<lightair_avatar::body::BodyFrame>,
    /// The co-host's mood over the avatar's own expression, and its weight.
    pub mood: Option<(&'static str, f32)>,
    /// A PNGtuber's mic track level (dBFS; `-inf` for silence or no mic).
    pub mic_db: f32,
    /// A PNGtuber's push to talk is held.
    pub push_to_talk_held: bool,
    /// With `show_tracking`: the face's points in the trackers' picture (0..1 across and down).
    pub view_face: Vec<[f32; 2]>,
    /// The anchors something is pinned to: each one's place in the avatar's picture at rest and
    /// now, as the libobs source moves its pinned scene items.
    pub pins: Vec<(String, crate::avatar_pins::Spot, crate::avatar_pins::Spot)>,
    /// The avatar's picture size the pins' places are measured in.
    pub size: (u32, u32),
    /// When it was published.
    pub at: Instant,
}

/// The trackers' picture an avatar source last handed its trackers (with `show_tracking`).
#[derive(Clone, Debug)]
pub struct TrackingPicture {
    /// BGRA, rows `width * 4` bytes apart, top row first.
    pub bgra: Arc<Vec<u8>>,
    pub width: u32,
    pub height: u32,
}

#[derive(Default)]
struct AvatarFeed {
    posing: Option<Arc<AvatarPosing>>,
    view: Option<TrackingPicture>,
}

/// Each avatar source's last posing and tracking picture, by libobs source.
static AVATARS: LazyLock<Mutex<HashMap<usize, AvatarFeed>>> = LazyLock::new(Default::default);

/// The avatar source `source` (libobs's) was posed this tick with `posing`. Its video thread.
pub(crate) fn set_avatar_posing(source: usize, posing: AvatarPosing) {
    if !own() {
        return;
    }
    AVATARS.lock().entry(source).or_default().posing = Some(Arc::new(posing));
}

/// The avatar source `source` handed its trackers this picture (`show_tracking` on). Graphics
/// thread.
pub(crate) fn set_avatar_view(source: usize, bgra: Arc<Vec<u8>>, size: (u32, u32)) {
    if !own() {
        return;
    }
    let view = TrackingPicture { bgra, width: size.0, height: size.1 };
    AVATARS.lock().entry(source).or_default().view = Some(view);
}

/// The avatar source `source` is gone.
pub(crate) fn forget_avatar(source: usize) {
    if !own() {
        return;
    }
    // (freed outside the lock)
    let gone = AVATARS.lock().remove(&source);
    drop(gone);
}

/// The document avatar source `source`'s last posing, and its trackers' picture when it shows
/// one; `None` before its libobs source has ticked. Any thread.
pub fn avatar_posing(source: Id) -> Option<(Arc<AvatarPosing>, Option<TrackingPicture>)> {
    let obs = BRIDGE.sources.lock().get(&source)?.as_ptr() as usize;
    let feeds = AVATARS.lock();
    let feed = feeds.get(&obs)?;
    Some((feed.posing.clone()?, feed.view.clone()))
}

// ---------------------------------------------------------------- buffers

/// A BGRA, IOSurface-backed pixel buffer (Metal and OpenGL can use its surface).
pub(crate) fn bgra_buffer(width: u32, height: u32) -> Option<Buffer> {
    let empty: CFRetained<CFDictionary<CFString, CFType>> = CFDictionary::from_slices(&[], &[]);
    // SAFETY: CoreVideo's constant keys, read only.
    let keys: [&CFString; 2] = unsafe { [kCVPixelBufferIOSurfacePropertiesKey, kCVPixelBufferMetalCompatibilityKey] };
    let values: [&CFType; 2] = [empty.as_ref(), CFBoolean::new(true).as_ref()];
    let attrs = CFDictionary::from_slices(&keys, &values);
    let mut out: *mut CVPixelBuffer = ptr::null_mut();
    // SAFETY: `out` is a valid place for the new buffer; the attributes outlive the call.
    let rc = unsafe {
        CVPixelBufferCreate(
            None,
            width as usize,
            height as usize,
            kCVPixelFormatType_32BGRA,
            Some(attrs.as_opaque()),
            NonNull::from(&mut out),
        )
    };
    if rc != kCVReturnSuccess || out.is_null() {
        tracing::error!(rc, width, height, "bridge: CVPixelBufferCreate failed");
        return None;
    }
    // SAFETY: CVPixelBufferCreate returned a buffer we own one reference to.
    Some(unsafe { CFRetained::from_raw(NonNull::new_unchecked(out)) })
}

/// The buffer's IOSurface, borrowed (as an `IOSurfaceRef`).
pub(crate) fn surface_of(buffer: &Buffer) -> Option<*mut c_void> {
    let surface = CVPixelBufferGetIOSurface(Some(buffer))?;
    // the buffer keeps its surface alive; the pointer is used while the buffer is held
    Some(CFRetained::as_ptr(&surface).as_ptr().cast())
}

// ---------------------------------------------------------------- OpenGL (libobs's context)
mod gl {
    #![allow(non_upper_case_globals)]
    use std::ffi::c_void;
    pub type GLuint = u32;
    pub type GLint = i32;
    pub type GLenum = u32;
    pub type GLsync = *mut c_void;
    pub const FRAMEBUFFER_COMPLETE: GLenum = 0x8CD5;
    pub const READ_FRAMEBUFFER: GLenum = 0x8CA8;
    pub const DRAW_FRAMEBUFFER: GLenum = 0x8CA9;
    pub const READ_FRAMEBUFFER_BINDING: GLenum = 0x8CAA;
    pub const DRAW_FRAMEBUFFER_BINDING: GLenum = 0x8CA6;
    pub const COLOR_ATTACHMENT0: GLenum = 0x8CE0;
    pub const TEXTURE_2D: GLenum = 0x0DE1;
    pub const TEXTURE_RECTANGLE: GLenum = 0x84F5;
    pub const TEXTURE_BINDING_RECTANGLE: GLenum = 0x84F6;
    pub const RGBA: GLenum = 0x1908;
    pub const BGRA: GLenum = 0x80E1;
    pub const UNSIGNED_INT_8_8_8_8_REV: GLenum = 0x8367;
    pub const SYNC_GPU_COMMANDS_COMPLETE: GLenum = 0x9117;
    pub const ALREADY_SIGNALED: GLenum = 0x911A;
    pub const CONDITION_SATISFIED: GLenum = 0x911C;
    pub const COLOR_BUFFER_BIT: u32 = 0x4000;
    pub const NEAREST: GLenum = 0x2600;

    #[link(name = "OpenGL", kind = "framework")]
    unsafe extern "C" {
        pub fn glGenFramebuffers(n: i32, ids: *mut GLuint);
        pub fn glDeleteFramebuffers(n: i32, ids: *const GLuint);
        pub fn glBindFramebuffer(target: GLenum, fb: GLuint);
        pub fn glFramebufferTexture2D(target: GLenum, attachment: GLenum, textarget: GLenum, tex: GLuint, level: GLint);
        pub fn glCheckFramebufferStatus(target: GLenum) -> GLenum;
        pub fn glBlitFramebuffer(
            sx0: GLint,
            sy0: GLint,
            sx1: GLint,
            sy1: GLint,
            dx0: GLint,
            dy0: GLint,
            dx1: GLint,
            dy1: GLint,
            mask: u32,
            filter: GLenum,
        );
        pub fn glGenTextures(n: i32, ids: *mut GLuint);
        pub fn glDeleteTextures(n: i32, ids: *const GLuint);
        pub fn glBindTexture(target: GLenum, tex: GLuint);
        pub fn glGetIntegerv(pname: GLenum, out: *mut GLint);
        pub fn glFlush();
        pub fn glFenceSync(condition: GLenum, flags: u32) -> GLsync;
        pub fn glClientWaitSync(sync: GLsync, flags: u32, timeout: u64) -> GLenum;
        pub fn glDeleteSync(sync: GLsync);
        pub fn CGLGetCurrentContext() -> *mut c_void;
        pub fn CGLTexImageIOSurface2D(
            ctx: *mut c_void,
            target: GLenum,
            internal_format: GLenum,
            width: i32,
            height: i32,
            format: GLenum,
            kind: GLenum,
            surface: *const c_void,
            plane: u32,
        ) -> i32;
    }
}

// ---------------------------------------------------------------- a source's ring

/// One BGRA buffer and what libobs writes it through: a rectangle texture over its surface
/// (OpenGL), or a libobs texture over it (Metal).
struct Slot {
    buffer: Buffer,
    gl: gl::GLuint,
    tex: *mut sys::gs_texture_t,
    /// Set while the GPU is still filling it (OpenGL).
    fence: Option<gl::GLsync>,
    /// What's in it was drawn without the source's filters.
    unfiltered: bool,
    /// The video time of the libobs frame that drew it.
    frame: u64,
}

/// A source's buffers at one size. Graphics thread only.
struct Ring {
    size: (u32, u32),
    /// The libobs render target the source is drawn into before the copy.
    target: *mut sys::gs_texture_t,
    read_fbo: gl::GLuint,
    draw_fbo: gl::GLuint,
    slots: Vec<Slot>,
    /// Slots the GPU is filling, oldest first.
    pending: VecDeque<usize>,
    /// libobs renders with Metal: copies are libobs's own, handed on a frame later (Metal gives
    /// this no fence to wait on).
    metal: bool,
}

impl Ring {
    /// Inside libobs's graphics context.
    unsafe fn new(size: (u32, u32)) -> Option<Ring> {
        let metal = unsafe { sys::gs_get_device_type() } as u32 == sys::GS_DEVICE_METAL;
        let target =
            unsafe { sys::gs_texture_create(size.0, size.1, sys::GS_BGRA, 1, ptr::null_mut(), sys::GS_RENDER_TARGET) };
        if target.is_null() {
            return None;
        }
        let mut ring =
            Ring { size, target, read_fbo: 0, draw_fbo: 0, slots: Vec::new(), pending: VecDeque::new(), metal };
        if !metal {
            unsafe {
                gl::glGenFramebuffers(1, &mut ring.read_fbo);
                gl::glGenFramebuffers(1, &mut ring.draw_fbo);
            }
        }
        Some(ring)
    }

    /// Inside libobs's graphics context.
    unsafe fn destroy(&mut self) {
        unsafe {
            for slot in self.slots.drain(..) {
                if let Some(f) = slot.fence {
                    gl::glDeleteSync(f);
                }
                if slot.gl != 0 {
                    gl::glDeleteTextures(1, &slot.gl);
                }
                if !slot.tex.is_null() {
                    sys::gs_texture_destroy(slot.tex);
                }
            }
            if self.read_fbo != 0 {
                gl::glDeleteFramebuffers(1, &self.read_fbo);
            }
            if self.draw_fbo != 0 {
                gl::glDeleteFramebuffers(1, &self.draw_fbo);
            }
            if !self.target.is_null() {
                sys::gs_texture_destroy(self.target);
            }
        }
        self.read_fbo = 0;
        self.draw_fbo = 0;
        self.target = ptr::null_mut();
        self.pending.clear();
    }

    /// A new slot at the ring's size. Inside libobs's graphics context.
    unsafe fn add_slot(&mut self) -> Option<usize> {
        let (w, h) = self.size;
        let buffer = bgra_buffer(w, h)?;
        let surface = surface_of(&buffer)?;
        let mut slot = Slot { buffer, gl: 0, tex: ptr::null_mut(), fence: None, unfiltered: false, frame: 0 };
        if self.metal {
            slot.tex = unsafe { sys::gs_texture_create_from_iosurface(surface) };
            if slot.tex.is_null() {
                tracing::error!("bridge: no libobs texture over a source's IOSurface");
                return None;
            }
        } else {
            unsafe {
                let mut prev: gl::GLint = 0;
                gl::glGetIntegerv(gl::TEXTURE_BINDING_RECTANGLE, &mut prev);
                gl::glGenTextures(1, &mut slot.gl);
                gl::glBindTexture(gl::TEXTURE_RECTANGLE, slot.gl);
                let err = gl::CGLTexImageIOSurface2D(
                    gl::CGLGetCurrentContext(),
                    gl::TEXTURE_RECTANGLE,
                    gl::RGBA,
                    w as i32,
                    h as i32,
                    gl::BGRA,
                    gl::UNSIGNED_INT_8_8_8_8_REV,
                    surface,
                    0,
                );
                gl::glBindTexture(gl::TEXTURE_RECTANGLE, prev as gl::GLuint);
                if err != 0 {
                    tracing::error!(err, "bridge: CGLTexImageIOSurface2D failed");
                    gl::glDeleteTextures(1, &slot.gl);
                    return None;
                }
            }
        }
        self.slots.push(slot);
        Some(self.slots.len() - 1)
    }

    /// The newest slot the GPU has finished since last asked, if any.
    unsafe fn finished(&mut self) -> Option<usize> {
        let mut done = None;
        while let Some(&i) = self.pending.front() {
            let Some(fence) = self.slots[i].fence else {
                // Metal: queued last frame, done by now
                self.pending.pop_front();
                done = Some(i);
                continue;
            };
            let r = unsafe { gl::glClientWaitSync(fence, 0, 0) };
            if r != gl::ALREADY_SIGNALED && r != gl::CONDITION_SATISFIED {
                break;
            }
            unsafe { gl::glDeleteSync(fence) };
            self.slots[i].fence = None;
            self.pending.pop_front();
            done = Some(i);
        }
        done
    }

    /// A slot nothing else holds (neither handed on nor drawn from by a compositor) and the GPU
    /// isn't filling; a new one while the ring has fewer than `most`. Inside libobs's graphics
    /// context.
    unsafe fn free_slot(&mut self, most: usize) -> Option<usize> {
        let free =
            (0..self.slots.len()).find(|&i| !self.pending.contains(&i) && self.slots[i].buffer.retain_count() == 1);
        match free {
            Some(i) => Some(i),
            None if self.slots.len() < most => unsafe { self.add_slot() },
            None => None,
        }
    }

    /// Copies the render target into `slot` and starts its fence. Inside libobs's graphics
    /// context.
    unsafe fn copy(&mut self, slot: usize) -> bool {
        if self.metal {
            unsafe { sys::gs_copy_texture(self.slots[slot].tex, self.target) };
            self.pending.push_back(slot);
            return true;
        }
        let (w, h) = (self.size.0 as i32, self.size.1 as i32);
        unsafe {
            let src = *(sys::gs_texture_get_obj(self.target) as *const gl::GLuint);
            let (mut prev_read, mut prev_draw) = (0, 0);
            gl::glGetIntegerv(gl::READ_FRAMEBUFFER_BINDING, &mut prev_read);
            gl::glGetIntegerv(gl::DRAW_FRAMEBUFFER_BINDING, &mut prev_draw);
            gl::glBindFramebuffer(gl::READ_FRAMEBUFFER, self.read_fbo);
            gl::glFramebufferTexture2D(gl::READ_FRAMEBUFFER, gl::COLOR_ATTACHMENT0, gl::TEXTURE_2D, src, 0);
            gl::glBindFramebuffer(gl::DRAW_FRAMEBUFFER, self.draw_fbo);
            gl::glFramebufferTexture2D(
                gl::DRAW_FRAMEBUFFER,
                gl::COLOR_ATTACHMENT0,
                gl::TEXTURE_RECTANGLE,
                self.slots[slot].gl,
                0,
            );
            let ok = gl::glCheckFramebufferStatus(gl::READ_FRAMEBUFFER) == gl::FRAMEBUFFER_COMPLETE
                && gl::glCheckFramebufferStatus(gl::DRAW_FRAMEBUFFER) == gl::FRAMEBUFFER_COMPLETE;
            if ok {
                gl::glBlitFramebuffer(0, 0, w, h, 0, 0, w, h, gl::COLOR_BUFFER_BIT, gl::NEAREST);
            }
            gl::glBindFramebuffer(gl::READ_FRAMEBUFFER, prev_read as gl::GLuint);
            gl::glBindFramebuffer(gl::DRAW_FRAMEBUFFER, prev_draw as gl::GLuint);
            if ok {
                self.slots[slot].fence = Some(gl::glFenceSync(gl::SYNC_GPU_COMMANDS_COMPLETE, 0));
                // sent to the GPU now, not when libobs next flushes
                gl::glFlush();
                self.pending.push_back(slot);
            }
            ok
        }
    }
}

/// Draws `source` at `w`×`h` into `target`, on transparent black, its colours multiplied by its
/// alpha (as libobs draws a source for its filters): through its filters, or, given its
/// innermost filter (`skip`), without any of them (libobs draws the filter's target as the
/// filter would see it, which for the innermost one is the source itself). Inside libobs's
/// graphics context.
unsafe fn draw_source(
    source: *mut sys::obs_source_t,
    skip: Option<*mut sys::obs_source_t>,
    w: u32,
    h: u32,
    target: *mut sys::gs_texture_t,
) {
    unsafe {
        let prev_rt = sys::gs_get_render_target();
        let prev_zs = sys::gs_get_zstencil_target();
        sys::gs_viewport_push();
        sys::gs_projection_push();
        sys::gs_matrix_push();
        sys::gs_matrix_identity();
        sys::gs_set_render_target(target, ptr::null_mut());
        sys::gs_set_viewport(0, 0, w as i32, h as i32);
        sys::gs_ortho(0.0, w as f32, 0.0, h as f32, -100.0, 100.0);
        let clear = sys::vec4::default();
        sys::gs_clear(sys::GS_CLEAR_COLOR, &clear, 1.0, 0);
        sys::gs_blend_state_push();
        sys::gs_enable_blending(true);
        sys::gs_blend_function_separate(
            sys::GS_BLEND_SRCALPHA,
            sys::GS_BLEND_INVSRCALPHA,
            sys::GS_BLEND_ONE,
            sys::GS_BLEND_INVSRCALPHA,
        );
        match skip {
            Some(filter) => sys::obs_source_skip_video_filter(filter),
            None => sys::obs_source_video_render(source),
        }
        sys::gs_blend_state_pop();
        sys::gs_set_render_target(prev_rt, prev_zs);
        sys::gs_matrix_pop();
        sys::gs_projection_pop();
        sys::gs_viewport_pop();
    }
}

/// How `source` can be drawn without its filters: `Unfiltered::Plain` it has none,
/// `Unfiltered::Skip(f)` by skipping its innermost filter `f` (a reference the caller releases),
/// `Unfiltered::No` when one of its filters isn't one of its document effects (the screen look's
/// pointer filter, which the compositor doesn't draw yet). Inside libobs's graphics context.
enum Unfiltered {
    Plain,
    Skip(*mut sys::obs_source_t),
    No,
}

unsafe fn unfiltered(source: *mut sys::obs_source_t) -> Unfiltered {
    struct Found {
        filters: usize,
        innermost: *mut sys::obs_source_t,
        other: bool,
    }
    unsafe extern "C" fn each(parent: *mut sys::obs_source_t, child: *mut sys::obs_source_t, param: *mut c_void) {
        let found = unsafe { &mut *(param as *mut Found) };
        found.filters += 1;
        let id = unsafe { sys::obs_source_get_id(child) };
        if !id.is_null() {
            let id = unsafe { std::ffi::CStr::from_ptr(id) }.to_bytes();
            if id == lightair_model::catalog::POINTER_KIND.as_bytes()
                || id == lightair_model::catalog::OLD_POINTER_KIND.as_bytes()
            {
                found.other = true;
            }
        }
        if found.innermost.is_null() && unsafe { sys::obs_filter_get_target(child) } == parent {
            // held while drawn: the engine thread may take the filter off meanwhile
            found.innermost = unsafe { sys::obs_source_get_ref(child) };
        }
    }
    let mut found = Found { filters: 0, innermost: ptr::null_mut(), other: false };
    unsafe { sys::obs_source_enum_filters(source, Some(each), (&mut found as *mut Found).cast()) };
    match found {
        Found { filters: 0, .. } => Unfiltered::Plain,
        Found { other: false, innermost, .. } if !innermost.is_null() => Unfiltered::Skip(innermost),
        Found { innermost, .. } => {
            if !innermost.is_null() {
                unsafe { sys::obs_source_release(innermost) };
            }
            Unfiltered::No
        }
    }
}

// ---------------------------------------------------------------- graphics thread

/// Runs on libobs's graphics thread after each canvas's frame is drawn (graphics context held);
/// once a frame, the first time: hands on the copies the GPU has finished and draws each wanted
/// source into a free buffer.
unsafe extern "C" fn on_rendered(_: *mut c_void) {
    // every canvas's frame calls it; the sources are drawn once
    static LAST: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(u64::MAX);
    let frame = unsafe { sys::obs_get_video_frame_time() };
    if LAST.swap(frame, std::sync::atomic::Ordering::Relaxed) == frame {
        return;
    }
    let b = &*BRIDGE;
    // each source, how many compositors want it, and whether all of them want it unfiltered
    let mut wanted: HashMap<Id, (usize, bool)> = HashMap::new();
    for (id, bare) in b.wanted.lock().values().flatten() {
        let e = wanted.entry(*id).or_insert((0, true));
        e.0 += 1;
        e.1 &= *bare;
    }
    let mut rings = b.rings.lock();
    if wanted.is_empty() && rings.is_empty() {
        return;
    }
    // the sources stay alive while drawn: the engine thread waits to remove one
    let sources = b.sources.lock();
    rings.retain(|id, ring| {
        let keep = wanted.contains_key(id) && sources.contains_key(id);
        if !keep {
            unsafe { ring.destroy() };
            b.latest.lock().remove(id);
        }
        keep
    });
    for (id, &(compositors, bare)) in &wanted {
        let Some(src) = sources.get(id) else { continue };
        let src = src.as_ptr();
        let route = if bare { unsafe { unfiltered(src) } } else { Unfiltered::No };
        let skip = match route {
            Unfiltered::Skip(f) => Some(f),
            _ => None,
        };
        let unfiltered = !matches!(route, Unfiltered::No);
        // unfiltered: the source's own size (its filters may crop or scale it)
        let (w, h) = unsafe {
            if unfiltered {
                (sys::obs_source_get_base_width(src), sys::obs_source_get_base_height(src))
            } else {
                (sys::obs_source_get_width(src), sys::obs_source_get_height(src))
            }
        };
        let release = || {
            if let Some(f) = skip {
                unsafe { sys::obs_source_release(f) };
            }
        };
        if w == 0 || h == 0 {
            release();
            continue;
        }
        if rings.get(id).is_some_and(|r| r.size != (w, h))
            && let Some(mut old) = rings.remove(id)
        {
            unsafe { old.destroy() };
        }
        if !rings.contains_key(id) {
            let Some(ring) = (unsafe { Ring::new((w, h)) }) else {
                release();
                continue;
            };
            rings.insert(*id, ring);
        }
        let Some(ring) = rings.get_mut(id) else {
            release();
            continue;
        };
        if let Some(i) = unsafe { ring.finished() } {
            let slot = &ring.slots[i];
            b.latest.lock().insert(*id, (slot.buffer.clone(), slot.unfiltered, slot.frame));
        }
        // every buffer in use: this frame of the source is skipped rather than waited for
        let Some(slot) = (unsafe { ring.free_slot(RING + compositors) }) else {
            release();
            continue;
        };
        unsafe {
            draw_source(src, skip, w, h, ring.target);
            ring.slots[slot].unfiltered = unfiltered;
            ring.slots[slot].frame = frame;
            ring.copy(slot);
        }
        release();
    }
}

// ---------------------------------------------------------------- the camera looks' trackers

/// Points in the faces given to the looks: the face landmark model's 478 (0–467 the mesh,
/// 468–477 the irises, each iris's centre first).
pub const FACE_POINTS: usize = 478;

/// The tracker's regions (`ft_region`) the looks' points are taken from.
const FT_LEFT_PUPIL: usize = 10;
const FT_RIGHT_PUPIL: usize = 11;

/// The Effect Stack's face tracker and segmentation (Core ML on macOS), for a host that tracks
/// and smooths their results itself: Lightair's own compositor drawing the camera looks. One per
/// camera; it analyses BGRA frames on threads of its own and keeps the newest results.
pub struct FaceTracker {
    tracker: NonNull<c_void>,
}

// SAFETY: the tracker's calls lock what they share inside the module; any thread may call them.
unsafe impl Send for FaceTracker {}

/// One analysed frame's landmarks, before smoothing.
pub struct RawFace {
    /// The time it was submitted with.
    pub time_ns: i64,
    /// A face was found: `points` holds its [`FACE_POINTS`] points, normalised 0–1, y down.
    pub found: bool,
    pub points: Vec<[f32; 2]>,
    /// The analysed frame's size.
    pub size: (u32, u32),
}

/// A mask the tracker made, before temporal smoothing (the multiclass one) or as it keeps it
/// (the person one).
pub struct RawMask {
    pub time_ns: i64,
    pub bytes: Vec<u8>,
    pub size: (u32, u32),
}

impl FaceTracker {
    /// Whether the Effect Stack module loaded gives what the looks need (its newer procs).
    pub fn available() -> bool {
        // a call with no tracker does nothing; it fails only when the proc isn't there
        crate::avatar::tracker_call(c"lightair_face_tracker_segmentation", |cd| {
            crate::avatar::set_ptr(cd, c"tracker", ptr::null_mut())
        })
        .map(|mut cd| crate::avatar::free_cd(&mut cd))
        .is_some()
    }

    /// A new tracker (its models load in the background; frames given meanwhile are dropped).
    pub fn new() -> Option<FaceTracker> {
        let mut cd = crate::avatar::tracker_call(c"lightair_face_tracker_new", |_| {})?;
        let t = crate::avatar::get_ptr(&mut cd, c"tracker");
        crate::avatar::free_cd(&mut cd);
        Some(FaceTracker { tracker: NonNull::new(t)? })
    }

    /// Which analyses run: the segmentation (multiclass, at about 30 a second when `fast`, every
    /// frame given with `every_frame`, about 15 a second otherwise) and with `person` the finer
    /// person mask at its quality (0 fast, 1 balanced, 2 accurate).
    pub fn segmentation(&self, enabled: bool, fast: bool, every_frame: bool, person: Option<u32>) {
        let t = self.tracker.as_ptr();
        if let Some(mut cd) = crate::avatar::tracker_call(c"lightair_face_tracker_segmentation", |cd| {
            crate::avatar::set_ptr(cd, c"tracker", t);
            set_bool(cd, c"enabled", enabled);
            set_bool(cd, c"fast", fast);
            set_bool(cd, c"every_frame", every_frame);
            set_bool(cd, c"person", person.is_some());
            crate::avatar::set_int(cd, c"quality", person.unwrap_or(1).min(2) as i64);
        }) {
            crate::avatar::free_cd(&mut cd);
        }
    }

    /// Gives a BGRA frame to analyse (copied before this returns); false when the tracker is
    /// still busy with an earlier one (the frame is dropped).
    ///
    /// # Safety
    /// `data` must hold `height` rows of `linesize` bytes, each at least `width` × 4.
    pub unsafe fn submit(&self, data: *const u8, linesize: usize, width: u32, height: u32, id: u64, time_ns: i64) -> bool {
        let t = self.tracker.as_ptr();
        let Some(mut cd) = crate::avatar::tracker_call(c"lightair_face_tracker_submit", |cd| {
            crate::avatar::set_ptr(cd, c"tracker", t);
            crate::avatar::set_ptr(cd, c"data", data as *mut c_void);
            crate::avatar::set_int(cd, c"linesize", linesize as i64);
            crate::avatar::set_int(cd, c"width", width as i64);
            crate::avatar::set_int(cd, c"height", height as i64);
            crate::avatar::set_int(cd, c"frame_id", id as i64);
            crate::avatar::set_int(cd, c"timestamp_ns", time_ns);
        }) else {
            return false;
        };
        let sent = crate::avatar::get_bool(&mut cd, c"sent");
        crate::avatar::free_cd(&mut cd);
        sent
    }

    /// The newest analysed frame's landmarks, when one newer than `serial` is there (`serial`
    /// then becomes its count). The tracker's region layout is turned back into the landmark
    /// model's order: the mesh's 468 points, then each iris as its centre (the tracker keeps
    /// only the centres; the looks read only those).
    pub fn raw_face(&self, serial: &mut u64) -> Option<RawFace> {
        let t = self.tracker.as_ptr();
        // SAFETY: plain old data, filled in by the tracker
        let mut face: Box<crate::avatar::FtFace> = Box::new(unsafe { std::mem::zeroed() });
        let fp = &mut *face as *mut crate::avatar::FtFace as *mut c_void;
        let before = *serial;
        let mut cd = crate::avatar::tracker_call(c"lightair_face_tracker_get_raw", |cd| {
            crate::avatar::set_ptr(cd, c"tracker", t);
            crate::avatar::set_int(cd, c"serial", before as i64);
            crate::avatar::set_ptr(cd, c"face", fp);
        })?;
        let ok = crate::avatar::get_bool(&mut cd, c"ok");
        let found = crate::avatar::get_bool(&mut cd, c"found");
        let new = get_int(&mut cd, c"new_serial") as u64;
        let time_ns = get_int(&mut cd, c"timestamp_ns");
        let size = (get_int(&mut cd, c"width") as u32, get_int(&mut cd, c"height") as u32);
        crate::avatar::free_cd(&mut cd);
        if !ok {
            return None;
        }
        *serial = new;
        let points = if found { landmark_order(&face) } else { None };
        Some(RawFace { time_ns, found: points.is_some(), points: points.unwrap_or_default(), size })
    }

    /// The newest multiclass mask (RGBA: R skin, G face skin, B hair, A person) before temporal
    /// smoothing, or with `person` the person mask (one byte a pixel), when one newer than
    /// `serial` is there.
    pub fn raw_mask(&self, person: bool, serial: &mut u64) -> Option<RawMask> {
        let t = self.tracker.as_ptr();
        let channels = if person { 1 } else { 4 };
        // asked first with no room: the size of a newer mask, if there is one
        let mut bytes: Vec<u8> = Vec::new();
        for _ in 0..3 {
            let before = *serial;
            let (ptr, capacity) = (bytes.as_mut_ptr(), bytes.len());
            let mut cd = crate::avatar::tracker_call(c"lightair_face_tracker_copy_mask", |cd| {
                crate::avatar::set_ptr(cd, c"tracker", t);
                set_bool(cd, c"person", person);
                crate::avatar::set_int(cd, c"serial", before as i64);
                crate::avatar::set_ptr(cd, c"buffer", if capacity == 0 { ptr::null_mut() } else { ptr.cast() });
                crate::avatar::set_int(cd, c"capacity", capacity as i64);
            })?;
            let ok = crate::avatar::get_bool(&mut cd, c"ok");
            let new = get_int(&mut cd, c"new_serial") as u64;
            let (w, h) = (get_int(&mut cd, c"width") as u32, get_int(&mut cd, c"height") as u32);
            let time_ns = get_int(&mut cd, c"timestamp_ns");
            crate::avatar::free_cd(&mut cd);
            let needed = w as usize * h as usize * channels;
            if ok {
                *serial = new;
                bytes.truncate(needed);
                return Some(RawMask { time_ns, bytes, size: (w, h) });
            }
            if needed == 0 || needed <= bytes.len() {
                // nothing newer
                return None;
            }
            // a newer mask: asked again with room for it (it may have grown meanwhile)
            bytes.resize(needed, 0);
        }
        None
    }
}

impl Drop for FaceTracker {
    fn drop(&mut self) {
        let t = self.tracker.as_ptr();
        if let Some(mut cd) =
            crate::avatar::tracker_call(c"lightair_face_tracker_free", |cd| crate::avatar::set_ptr(cd, c"tracker", t))
        {
            crate::avatar::free_cd(&mut cd);
        }
    }
}

/// The tracker's face (its region layout from the landmark model: the mesh in order, each pupil
/// one point) as the landmark model's 478 points; `None` without the mesh (a tracker that gives
/// only a constellation of points).
fn landmark_order(face: &crate::avatar::FtFace) -> Option<Vec<[f32; 2]>> {
    let region = |r: usize| {
        let (s, n) = (face.region_start[r].max(0) as usize, face.region_count[r].max(0) as usize);
        face.pts.get(s..s + n).unwrap_or(&[])
    };
    let mesh = region(crate::avatar::FT_MESH);
    let (left, right) = (region(FT_LEFT_PUPIL), region(FT_RIGHT_PUPIL));
    if mesh.len() < 468 || left.is_empty() || right.is_empty() {
        return None;
    }
    let mut points = Vec::with_capacity(FACE_POINTS);
    points.extend_from_slice(&mesh[..468]);
    // 468–472 the subject's right iris (the picture's left), 473–477 the left
    points.extend(std::iter::repeat_n(right[0], 5));
    points.extend(std::iter::repeat_n(left[0], 5));
    Some(points)
}

fn set_bool(cd: *mut sys::calldata_t, name: &std::ffi::CStr, v: bool) {
    unsafe { sys::calldata_set_data(cd, name.as_ptr(), &v as *const _ as *const c_void, std::mem::size_of::<bool>()) };
}

fn get_int(cd: &mut sys::calldata_t, name: &std::ffi::CStr) -> i64 {
    let mut v: i64 = 0;
    unsafe { sys::calldata_get_data(cd, name.as_ptr(), &mut v as *mut _ as *mut c_void, std::mem::size_of::<i64>()) };
    v
}
