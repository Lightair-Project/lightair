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
use std::ptr;
use std::sync::{Arc, LazyLock};

use lightair_model::Id;
use parking_lot::Mutex;

use crate::obs::{ObsSource, SourceRef};
use crate::sys;

pub use lightair_host::show::{
    Snapshot, Switch, clear_timer_texts, link_received_by_compositor, own, publish, set_switch, set_timer_text,
    snapshot, timer_text,
};

/// Buffers per source besides one for each compositor drawing it (which may hold one until its
/// next frame): the one handed on, one the GPU is filling, and one free.
const RING: usize = 3;

/// The document scene and canvas a libobs scene stands for, from the name the reconciler gives
/// it (`<scene id>:<canvas id>`); `None` for any other source.
pub fn scene_of(source: *mut sys::obs_source_t) -> Option<(Id, Id)> {
    if source.is_null() || unsafe { sys::obs_scene_from_source(source) }.is_null() {
        return None;
    }
    let name = unsafe { sys::obs_source_get_name(source) };
    if name.is_null() {
        return None;
    }
    let name = unsafe { std::ffi::CStr::from_ptr(name) }.to_str().ok()?;
    let (scene, canvas) = name.split_once(':')?;
    let id = |s: &str| serde_json::from_value::<Id>(serde_json::Value::String(s.to_string())).ok();
    Some((id(scene)?, id(canvas)?))
}

// ---------------------------------------------------------------- sources

/// A retained `CVPixelBuffer`: BGRA, IOSurface-backed.
pub use lightair_host::buffers::Buffer;

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
    lightair_host::show::clear();
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

pub use lightair_host::bridge::Picture;

/// The newest finished picture of `source`, if it's wanted and has been drawn. Any thread.
pub fn picture(source: Id) -> Option<Picture> {
    let (buffer, unfiltered, serial) = BRIDGE.latest.lock().get(&source)?.clone();
    Some(Picture { buffer, unfiltered, serial })
}

// ---------------------------------------------------------------- avatars

pub use lightair_host::bridge::{AvatarPosing, TrackingPicture, avatar_posing};

/// The avatar source `source` (libobs's) was posed this tick with `posing`. Its video thread.
pub(crate) fn set_avatar_posing(source: usize, posing: AvatarPosing) {
    if !own() {
        return;
    }
    if let Some(id) = crate::obs::doc_id(source as *mut sys::obs_source_t) {
        lightair_host::bridge::set_avatar_posing(id, posing);
    }
}

/// The avatar source `source` handed its trackers this picture (`show_tracking` on). Graphics
/// thread.
pub(crate) fn set_avatar_view(source: usize, bgra: Arc<Vec<u8>>, size: (u32, u32)) {
    if !own() {
        return;
    }
    if let Some(id) = crate::obs::doc_id(source as *mut sys::obs_source_t) {
        lightair_host::bridge::set_avatar_view(id, bgra, size);
    }
}

/// The avatar source `source` is gone.
pub(crate) fn forget_avatar(source: usize) {
    if !own() {
        return;
    }
    if let Some(id) = crate::obs::doc_id(source as *mut sys::obs_source_t) {
        lightair_host::bridge::forget_avatar(id);
    }
}

/// Hands the compositor libobs's pictures (`lightair_host::bridge::want` and `picture` come
/// here). Once, at startup.
pub(crate) fn install() {
    struct Libobs;
    impl lightair_host::bridge::Elsewhere for Libobs {
        fn want(&self, canvas: Id, sources: HashMap<Id, bool>) {
            want(canvas, sources);
        }
        fn picture(&self, source: Id) -> Option<Picture> {
            picture(source)
        }
        fn sound(&self, source: Id, planes: &[&[f32]], rate: u32) {
            let Ok(name) = std::ffi::CString::new(source.to_string()) else { return };
            let Some(into) = (unsafe { SourceRef::from_owned(sys::obs_get_source_by_name(name.as_ptr())) }) else {
                return;
            };
            let Some(frames) = planes.first().map(|p| p.len()) else { return };
            let mut a = sys::obs_source_audio {
                frames: frames as u32,
                speakers: if planes.len() >= 2 { sys::SPEAKERS_STEREO } else { sys::SPEAKERS_MONO },
                format: sys::AUDIO_FORMAT_FLOAT_PLANAR,
                samples_per_sec: rate,
                timestamp: crate::clock::now_ns(),
                ..Default::default()
            };
            for (o, p) in a.data.iter_mut().zip(planes) {
                *o = p.as_ptr() as *const u8;
            }
            unsafe { sys::obs_source_output_audio(into.as_ptr(), &a) };
        }
    }
    lightair_host::bridge::set_elsewhere(Box::new(Libobs));
}

pub use lightair_host::buffers::{bgra_buffer, surface_of};

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

/// The Effect Stack's trackers, without libobs (`lightair_host::tracker`).
pub use lightair_host::tracker::{FACE_POINTS, FaceTracker, RawFace, RawMask};

