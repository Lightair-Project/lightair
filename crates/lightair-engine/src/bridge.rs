//! macOS, with `LIGHTAIR_ENGINE=own`: what Lightair's own compositor (`lightair-render`'s
//! `SceneRenderer`, driven by `lightair-preview`) needs from libobs while libobs still captures.
//!
//! - **Pictures:** the compositor says which document sources it wants ([`want`]). On libobs's
//!   graphics thread, after each frame, every wanted source is drawn at its own size into a
//!   libobs render target and copied into a BGRA IOSurface from a small ring per source (libobs
//!   can't render into an IOSurface directly on OpenGL: the copy is a framebuffer blit there, a
//!   texture copy on Metal). A GPU fence says when a copy is done; only finished buffers are
//!   handed on, and [`picture`] gives the newest one from any thread. Sources nobody wants
//!   aren't drawn. A source is drawn through its libobs filters, so its effects are already in
//!   the picture.
//! - **Sources:** the engine thread keeps a map from document source to libobs source, updated
//!   as the reconciler makes and removes them ([`source_added`], [`source_removed`]), so the
//!   graphics thread never touches the reconciler.
//! - **The document:** after each applied change the engine thread publishes the workspace and
//!   the scene switch under way, if any ([`snapshot`]).
//!
//! With the switch off every entry point returns at once and nothing is registered with libobs.

use std::collections::{HashMap, HashSet, VecDeque};
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

/// Buffers per source: the one handed on, one the compositor may still be drawing from, one
/// the GPU is filling, and one free.
const RING: usize = 4;

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
type Buffer = CFRetained<CVPixelBuffer>;

struct Bridge {
    /// Document source → its libobs source (engine thread writes; graphics thread reads).
    sources: Mutex<HashMap<Id, SourceRef>>,
    /// The sources each compositor (by canvas) wants pictures of.
    wanted: Mutex<HashMap<Id, HashSet<Id>>>,
    /// The newest finished picture of each source drawn.
    latest: Mutex<HashMap<Id, Buffer>>,
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
/// what it wanted before; empty when it stops).
pub fn want(canvas: Id, sources: HashSet<Id>) {
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

/// The newest finished picture of `source`, if it's wanted and has been drawn. Any thread.
pub fn picture(source: Id) -> Option<lightair_render::texture::PixelBuffer> {
    let buffer = BRIDGE.latest.lock().get(&source)?.clone();
    // SAFETY: the reference made by `clone` is given up to the pixel buffer, which releases it.
    unsafe { lightair_render::texture::PixelBuffer::from_raw(CFRetained::into_raw(buffer).as_ptr().cast()) }
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
        let mut slot = Slot { buffer, gl: 0, tex: ptr::null_mut(), fence: None };
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

    /// A slot nothing else holds (neither handed on nor drawn from by the compositor) and the
    /// GPU isn't filling; a new one while the ring isn't full. Inside libobs's graphics context.
    unsafe fn free_slot(&mut self) -> Option<usize> {
        let free =
            (0..self.slots.len()).find(|&i| !self.pending.contains(&i) && self.slots[i].buffer.retain_count() == 1);
        match free {
            Some(i) => Some(i),
            None if self.slots.len() < RING => unsafe { self.add_slot() },
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

/// Draws `source` at its own size (`w`×`h`) into `target`, on transparent black, its colours
/// multiplied by its alpha (as libobs draws a source for its filters). Inside libobs's graphics
/// context.
unsafe fn draw_source(source: *mut sys::obs_source_t, w: u32, h: u32, target: *mut sys::gs_texture_t) {
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
        sys::obs_source_video_render(source);
        sys::gs_blend_state_pop();
        sys::gs_set_render_target(prev_rt, prev_zs);
        sys::gs_matrix_pop();
        sys::gs_projection_pop();
        sys::gs_viewport_pop();
    }
}

// ---------------------------------------------------------------- graphics thread

/// Runs on libobs's graphics thread after each frame (graphics context held): hands on the
/// copies the GPU has finished and draws each wanted source into a free buffer.
unsafe extern "C" fn on_rendered(_: *mut c_void) {
    let b = &*BRIDGE;
    let wanted: HashSet<Id> = b.wanted.lock().values().flatten().copied().collect();
    let mut rings = b.rings.lock();
    if wanted.is_empty() && rings.is_empty() {
        return;
    }
    // the sources stay alive while drawn: the engine thread waits to remove one
    let sources = b.sources.lock();
    rings.retain(|id, ring| {
        let keep = wanted.contains(id) && sources.contains_key(id);
        if !keep {
            unsafe { ring.destroy() };
            b.latest.lock().remove(id);
        }
        keep
    });
    for id in &wanted {
        let Some(src) = sources.get(id) else { continue };
        let src = src.as_ptr();
        let (w, h) = unsafe { (sys::obs_source_get_width(src), sys::obs_source_get_height(src)) };
        if w == 0 || h == 0 {
            continue;
        }
        if rings.get(id).is_some_and(|r| r.size != (w, h))
            && let Some(mut old) = rings.remove(id)
        {
            unsafe { old.destroy() };
        }
        if !rings.contains_key(id) {
            let Some(ring) = (unsafe { Ring::new((w, h)) }) else { continue };
            rings.insert(*id, ring);
        }
        let Some(ring) = rings.get_mut(id) else { continue };
        if let Some(i) = unsafe { ring.finished() } {
            b.latest.lock().insert(*id, ring.slots[i].buffer.clone());
        }
        // every buffer in use: this frame of the source is skipped rather than waited for
        let Some(slot) = (unsafe { ring.free_slot() }) else { continue };
        unsafe {
            draw_source(src, w, h, ring.target);
            ring.copy(slot);
        }
    }
}
