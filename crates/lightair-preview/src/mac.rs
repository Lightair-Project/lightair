//! macOS preview path (OpenGL libobs backend):
//!
//! 1. libobs renders the program canvas, scaled to the preview size, into its own BGRA render
//!    target (libobs can't render into IOSurface textures directly). The program at canvas size
//!    is read straight from libobs's main texture instead.
//! 2. A libobs shader converts it to full-range BT.601 NV12 (`420f`), the only format GPUI's
//!    `surface()` element draws: luma into one target, chroma at half size into another.
//! 3. GL framebuffer blits copy the two planes into an NV12 IOSurface from a ring of four.
//! 4. A GPU fence says when that's done; the frame goes to the UI at the next render once it is,
//!    so the graphics thread never waits for the GPU (it used to stop for `glFinish` and a
//!    VideoToolbox conversion every frame: most of the render time).
//!
//! No pixel ever crosses to the CPU.

use std::ffi::c_void;

use crate::render::{draw_scaled, draw_with};
use std::ptr::{self, NonNull};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use lightair_engine::sys;
use objc2_core_foundation::{CFBoolean, CFDictionary, CFRetained, CFString, CFType};
use objc2_core_video::{
    CVAttachmentMode, CVPixelBuffer, CVPixelBufferCreate, CVPixelBufferGetIOSurface,
    kCVImageBufferYCbCrMatrix_ITU_R_601_4, kCVImageBufferYCbCrMatrixKey, kCVPixelBufferIOSurfacePropertiesKey,
    kCVPixelBufferMetalCompatibilityKey, kCVPixelFormatType_420YpCbCr8BiPlanarFullRange,
    kCVReturnSuccess,
};
use parking_lot::Mutex;

/// NV12 buffers: the one on screen, up to two the GPU is still filling, one free.
const RING: usize = 4;

/// The conversion to what GPUI draws (full-range BT.601, as `pixel_buffer` tags the buffers).
const NV12_EFFECT: &str = r#"
uniform float4x4 ViewProj;
uniform texture2d image;
sampler_state def_sampler { Filter = Linear; AddressU = Clamp; AddressV = Clamp; };
struct VertInOut { float4 pos : POSITION; float2 uv : TEXCOORD0; };
VertInOut VSDefault(VertInOut vert_in)
{
    VertInOut vert_out;
    vert_out.pos = mul(float4(vert_in.pos.xyz, 1.0), ViewProj);
    vert_out.uv = vert_in.uv;
    return vert_out;
}
float4 PSLuma(VertInOut vert_in) : TARGET
{
    float3 c = image.Sample(def_sampler, vert_in.uv).rgb;
    return float4(dot(c, float3(0.299, 0.587, 0.114)), 0.0, 0.0, 1.0);
}
float4 PSChroma(VertInOut vert_in) : TARGET
{
    float3 c = image.Sample(def_sampler, vert_in.uv).rgb;
    return float4(dot(c, float3(-0.168736, -0.331264, 0.5)) + 0.5, dot(c, float3(0.5, -0.418688, -0.081312)) + 0.5, 0.0, 1.0);
}
technique Luma { pass { vertex_shader = VSDefault(vert_in); pixel_shader = PSLuma(vert_in); } }
technique Chroma { pass { vertex_shader = VSDefault(vert_in); pixel_shader = PSChroma(vert_in); } }
"#;

/// A finished preview frame (NV12 full range), ready for GPUI's `surface()` element.
pub type Frame = CFRetained<CVPixelBuffer>;

// ---------------------------------------------------------------- OpenGL (libobs's context)
mod gl {
    #![allow(non_upper_case_globals)]
    use std::ffi::c_void;
    pub type GLuint = u32;
    pub type GLint = i32;
    pub type GLenum = u32;
    pub const FRAMEBUFFER_COMPLETE: GLenum = 0x8CD5;
    pub const READ_FRAMEBUFFER: GLenum = 0x8CA8;
    pub const DRAW_FRAMEBUFFER: GLenum = 0x8CA9;
    pub const READ_FRAMEBUFFER_BINDING: GLenum = 0x8CAA;
    pub const DRAW_FRAMEBUFFER_BINDING: GLenum = 0x8CA6;
    pub const COLOR_ATTACHMENT0: GLenum = 0x8CE0;
    pub const TEXTURE_2D: GLenum = 0x0DE1;
    pub const TEXTURE_RECTANGLE: GLenum = 0x84F5;
    pub const TEXTURE_BINDING_RECTANGLE: GLenum = 0x84F6;
    pub const R8: GLenum = 0x8229;
    pub const RG8: GLenum = 0x822B;
    pub const RED: GLenum = 0x1903;
    pub const RG: GLenum = 0x8227;
    pub const UNSIGNED_BYTE: GLenum = 0x1401;
    pub const SYNC_GPU_COMMANDS_COMPLETE: GLenum = 0x9117;
    pub const ALREADY_SIGNALED: GLenum = 0x911A;
    pub const CONDITION_SATISFIED: GLenum = 0x911C;
    pub type GLsync = *mut c_void;
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

// ---------------------------------------------------------------- buffers
fn pixel_buffer(width: u32, height: u32, format: u32) -> Option<CFRetained<CVPixelBuffer>> {
    let empty: CFRetained<CFDictionary<CFString, CFType>> = CFDictionary::from_slices(&[], &[]);
    let keys: [&CFString; 2] = unsafe { [kCVPixelBufferIOSurfacePropertiesKey, kCVPixelBufferMetalCompatibilityKey] };
    let values: [&CFType; 2] = [empty.as_ref(), CFBoolean::new(true).as_ref()];
    let attrs = CFDictionary::from_slices(&keys, &values);
    let mut out: *mut CVPixelBuffer = ptr::null_mut();
    let rc = unsafe {
        CVPixelBufferCreate(
            None,
            width as usize,
            height as usize,
            format,
            Some(attrs.as_opaque()),
            NonNull::from(&mut out),
        )
    };
    if rc != kCVReturnSuccess || out.is_null() {
        tracing::error!(rc, width, height, format, "CVPixelBufferCreate failed");
        return None;
    }
    let buf = unsafe { CFRetained::from_raw(NonNull::new_unchecked(out)) };
    if format == kCVPixelFormatType_420YpCbCr8BiPlanarFullRange {
        // GPUI's surface shader decodes BT.601; tell VideoToolbox to encode with the same matrix.
        unsafe {
            buf.set_attachment(
                kCVImageBufferYCbCrMatrixKey,
                kCVImageBufferYCbCrMatrix_ITU_R_601_4.as_ref(),
                CVAttachmentMode::ShouldPropagate,
            )
        };
    }
    Some(buf)
}

/// One NV12 buffer and its planes as GL textures.
struct Slot {
    buffer: CFRetained<CVPixelBuffer>,
    luma: gl::GLuint,
    chroma: gl::GLuint,
    /// Set while the GPU is still filling it.
    fence: Option<gl::GLsync>,
}

/// Everything sized to one preview resolution. Owned by the graphics thread except the slots'
/// buffers, which the UI also retains.
struct Pool {
    width: u32,
    height: u32,
    /// libobs render target the canvas is drawn into (when it isn't the main texture as it is).
    target: *mut sys::gs_texture_t,
    /// The planes as libobs draws them, before the blit into an IOSurface.
    luma: *mut sys::gs_texture_t,
    chroma: *mut sys::gs_texture_t,
    effect: *mut sys::gs_effect_t,
    read_fbo: gl::GLuint,
    draw_fbo: gl::GLuint,
    slots: Vec<Slot>,
    /// Slots the GPU is filling, oldest first.
    pending: std::collections::VecDeque<usize>,
    /// The slot the UI has.
    shown: Option<usize>,
    next: usize,
}

// SAFETY: GL/libobs handles are only used on libobs's graphics thread; the CoreVideo buffers
// are thread-safe reference-counted objects.
unsafe impl Send for Pool {}
unsafe impl Sync for Pool {}

/// A GL rectangle texture over one plane of an IOSurface.
unsafe fn plane_texture(surface: *const c_void, plane: u32, w: u32, h: u32, internal: gl::GLenum, format: gl::GLenum) -> Option<gl::GLuint> {
    unsafe {
        let mut prev: gl::GLint = 0;
        gl::glGetIntegerv(gl::TEXTURE_BINDING_RECTANGLE, &mut prev);
        let mut tex = 0;
        gl::glGenTextures(1, &mut tex);
        gl::glBindTexture(gl::TEXTURE_RECTANGLE, tex);
        let err = gl::CGLTexImageIOSurface2D(
            gl::CGLGetCurrentContext(),
            gl::TEXTURE_RECTANGLE,
            internal,
            w as i32,
            h as i32,
            format,
            gl::UNSIGNED_BYTE,
            surface,
            plane,
        );
        gl::glBindTexture(gl::TEXTURE_RECTANGLE, prev as gl::GLuint);
        if err != 0 {
            tracing::error!(err, plane, "CGLTexImageIOSurface2D failed");
            gl::glDeleteTextures(1, &tex);
            return None;
        }
        Some(tex)
    }
}

impl Pool {
    /// Must run inside the libobs graphics context (GL context current).
    unsafe fn new(width: u32, height: u32) -> Option<Self> {
        let mut pool = Self {
            width,
            height,
            target: ptr::null_mut(),
            luma: ptr::null_mut(),
            chroma: ptr::null_mut(),
            effect: ptr::null_mut(),
            read_fbo: 0,
            draw_fbo: 0,
            slots: Vec::new(),
            pending: Default::default(),
            shown: None,
            next: 0,
        };
        unsafe {
            pool.target = sys::gs_texture_create(width, height, sys::GS_BGRA, 1, ptr::null_mut(), sys::GS_RENDER_TARGET);
            pool.luma = sys::gs_texture_create(width, height, sys::GS_R8, 1, ptr::null_mut(), sys::GS_RENDER_TARGET);
            pool.chroma = sys::gs_texture_create(width / 2, height / 2, sys::GS_R8G8, 1, ptr::null_mut(), sys::GS_RENDER_TARGET);
            let code = std::ffi::CString::new(NV12_EFFECT).unwrap();
            let name = c"lightair-preview-nv12";
            let mut error: *mut std::ffi::c_char = ptr::null_mut();
            pool.effect = sys::gs_effect_create(code.as_ptr(), name.as_ptr(), &mut error);
            if !error.is_null() {
                tracing::error!(error = %std::ffi::CStr::from_ptr(error).to_string_lossy(), "preview shader");
                sys::bfree(error as *mut c_void);
            }
            if pool.target.is_null() || pool.luma.is_null() || pool.chroma.is_null() || pool.effect.is_null() {
                pool.destroy();
                return None;
            }
            for _ in 0..RING {
                let Some(buffer) = pixel_buffer(width, height, kCVPixelFormatType_420YpCbCr8BiPlanarFullRange) else {
                    pool.destroy();
                    return None;
                };
                let Some(surface) = CVPixelBufferGetIOSurface(Some(&buffer)) else {
                    pool.destroy();
                    return None;
                };
                let raw = CFRetained::as_ptr(&surface).as_ptr() as *const c_void;
                let luma = plane_texture(raw, 0, width, height, gl::R8, gl::RED);
                let chroma = plane_texture(raw, 1, width / 2, height / 2, gl::RG8, gl::RG);
                match (luma, chroma) {
                    (Some(luma), Some(chroma)) => pool.slots.push(Slot { buffer, luma, chroma, fence: None }),
                    (l, c) => {
                        for t in [l, c].into_iter().flatten() {
                            gl::glDeleteTextures(1, &t);
                        }
                        pool.destroy();
                        return None;
                    }
                }
            }
            gl::glGenFramebuffers(1, &mut pool.read_fbo);
            gl::glGenFramebuffers(1, &mut pool.draw_fbo);
        }
        Some(pool)
    }

    /// Must run inside the libobs graphics context.
    unsafe fn destroy(&mut self) {
        unsafe {
            for slot in self.slots.drain(..) {
                if let Some(f) = slot.fence {
                    gl::glDeleteSync(f);
                }
                gl::glDeleteTextures(1, &slot.luma);
                gl::glDeleteTextures(1, &slot.chroma);
            }
            if self.read_fbo != 0 {
                gl::glDeleteFramebuffers(1, &self.read_fbo);
            }
            if self.draw_fbo != 0 {
                gl::glDeleteFramebuffers(1, &self.draw_fbo);
            }
            for t in [&mut self.target, &mut self.luma, &mut self.chroma] {
                if !t.is_null() {
                    sys::gs_texture_destroy(*t);
                    *t = ptr::null_mut();
                }
            }
            if !self.effect.is_null() {
                sys::gs_effect_destroy(self.effect);
                self.effect = ptr::null_mut();
            }
        }
        self.read_fbo = 0;
        self.draw_fbo = 0;
        self.pending.clear();
        self.shown = None;
    }

    /// The newest slot the GPU has finished, if any finished since last asked.
    unsafe fn finished(&mut self) -> Option<usize> {
        let mut done = None;
        while let Some(&i) = self.pending.front() {
            let Some(fence) = self.slots[i].fence else {
                self.pending.pop_front();
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
        if done.is_some() {
            self.shown = done;
        }
        done
    }

    /// A slot neither on screen nor being filled (the one shown longest ago), if any.
    fn free_slot(&mut self) -> Option<usize> {
        for k in 0..RING {
            let i = (self.next + k) % RING;
            if Some(i) != self.shown && !self.pending.contains(&i) {
                self.next = (i + 1) % RING;
                return Some(i);
            }
        }
        None
    }

    /// Converts `src` (`sw`×`sh`) into NV12 in `slot`, and starts its fence.
    unsafe fn convert(&mut self, src: *mut sys::gs_texture_t, slot: usize) -> bool {
        thread_local! {
            static IMAGE: std::ffi::CString = std::ffi::CString::new("image").unwrap();
            static LUMA: std::ffi::CString = std::ffi::CString::new("Luma").unwrap();
            static CHROMA: std::ffi::CString = std::ffi::CString::new("Chroma").unwrap();
        }
        let (w, h) = (self.width, self.height);
        unsafe {
            let image = IMAGE.with(|n| sys::gs_effect_get_param_by_name(self.effect, n.as_ptr()));
            let sw = sys::gs_texture_get_width(src);
            let sh = sys::gs_texture_get_height(src);
            for (dst, dw, dh, technique) in [(self.luma, w, h, &LUMA), (self.chroma, w / 2, h / 2, &CHROMA)] {
                draw_with(sw, sh, dst, dw, dh, || {
                    sys::gs_blend_state_push();
                    sys::gs_enable_blending(false);
                    sys::gs_effect_set_texture(image, src);
                    technique.with(|t| {
                        while sys::gs_effect_loop(self.effect, t.as_ptr()) {
                            sys::gs_draw_sprite(src, 0, sw, sh);
                        }
                    });
                    sys::gs_blend_state_pop();
                });
            }
            let ok = self.blit(self.luma, self.slots[slot].luma, w, h) && self.blit(self.chroma, self.slots[slot].chroma, w / 2, h / 2);
            if ok {
                self.slots[slot].fence = Some(gl::glFenceSync(gl::SYNC_GPU_COMMANDS_COMPLETE, 0));
                // sent to the GPU now, not when libobs next flushes
                gl::glFlush();
                self.pending.push_back(slot);
            }
            ok
        }
    }

    /// Copies a libobs texture into an IOSurface plane's texture with a GL blit.
    unsafe fn blit(&self, from: *mut sys::gs_texture_t, to: gl::GLuint, w: u32, h: u32) -> bool {
        unsafe {
            let src = *(sys::gs_texture_get_obj(from) as *const gl::GLuint);
            let (mut prev_read, mut prev_draw) = (0, 0);
            gl::glGetIntegerv(gl::READ_FRAMEBUFFER_BINDING, &mut prev_read);
            gl::glGetIntegerv(gl::DRAW_FRAMEBUFFER_BINDING, &mut prev_draw);
            gl::glBindFramebuffer(gl::READ_FRAMEBUFFER, self.read_fbo);
            gl::glFramebufferTexture2D(gl::READ_FRAMEBUFFER, gl::COLOR_ATTACHMENT0, gl::TEXTURE_2D, src, 0);
            gl::glBindFramebuffer(gl::DRAW_FRAMEBUFFER, self.draw_fbo);
            gl::glFramebufferTexture2D(gl::DRAW_FRAMEBUFFER, gl::COLOR_ATTACHMENT0, gl::TEXTURE_RECTANGLE, to, 0);
            let ok = gl::glCheckFramebufferStatus(gl::READ_FRAMEBUFFER) == gl::FRAMEBUFFER_COMPLETE
                && gl::glCheckFramebufferStatus(gl::DRAW_FRAMEBUFFER) == gl::FRAMEBUFFER_COMPLETE;
            if ok {
                let (w, h) = (w as i32, h as i32);
                gl::glBlitFramebuffer(0, 0, w, h, 0, 0, w, h, gl::COLOR_BUFFER_BIT, gl::NEAREST);
            }
            gl::glBindFramebuffer(gl::READ_FRAMEBUFFER, prev_read as gl::GLuint);
            gl::glBindFramebuffer(gl::DRAW_FRAMEBUFFER, prev_draw as gl::GLuint);
            ok
        }
    }
}

// ---------------------------------------------------------------- shared state
struct Shared {
    pool: Mutex<Option<Pool>>,
    /// Newest finished frame for the UI.
    latest: Mutex<Option<Frame>>,
    /// Requested size, packed as (width << 32 | height). 0 = follow the canvas size.
    size: AtomicU64,
    active: AtomicBool,
    frames: AtomicU64,
    /// `None`: the main canvas (program). `Some`: another canvas (an additional format); we
    /// own one strong reference.
    canvas: Option<CanvasRef>,
    /// When set, renders this source (a scene that isn't on air: Studio mode's preview)
    /// instead of the canvas's program. Held "showing" so its sources run.
    source: Mutex<Option<SourceRef>>,
    /// Draw the source at its own size (a player), not in the canvas's coordinates.
    own_size: AtomicBool,
}

struct CanvasRef(*mut sys::obs_canvas_t);

/// A strong, showing reference to a source.
struct SourceRef(*mut sys::obs_source_t);

impl Drop for SourceRef {
    fn drop(&mut self) {
        unsafe {
            sys::obs_source_dec_showing(self.0);
            sys::obs_source_release(self.0);
        }
    }
}

// SAFETY: `Shared` is used by libobs's graphics thread and the UI thread. Every field is either
// atomic or behind a mutex; the CoreVideo/VideoToolbox objects inside are reference-counted
// CoreFoundation types that may be retained/released on any thread, and the transfer session
// is only invoked on the graphics thread.
unsafe impl Send for Shared {}
unsafe impl Sync for Shared {}

/// Handle to a preview of the program canvas. Cheap to clone; rendering stops when the last
/// clone is dropped.
#[derive(Clone)]
pub struct Preview(Arc<Handle>);

struct Handle {
    shared: Arc<Shared>,
}

impl Preview {
    /// Starts rendering the main canvas into preview buffers. libobs must be running.
    pub fn attach() -> Self {
        Self::attach_to(None)
    }

    /// Previews another canvas (an additional format). Takes ownership of the strong reference
    /// in `canvas` (e.g. from `Reconciler::canvas_ref`).
    pub fn attach_canvas(canvas: *mut sys::obs_canvas_t) -> Self {
        Self::attach_to((!canvas.is_null()).then_some(CanvasRef(canvas)))
    }

    fn attach_to(canvas: Option<CanvasRef>) -> Self {
        let shared = Arc::new(Shared {
            pool: Mutex::new(None),
            latest: Mutex::new(None),
            size: AtomicU64::new(0),
            active: AtomicBool::new(true),
            frames: AtomicU64::new(0),
            canvas,
            source: Mutex::new(None),
            own_size: AtomicBool::new(false),
        });
        unsafe { sys::obs_add_main_rendered_callback(Some(on_rendered), Arc::as_ptr(&shared) as *mut c_void) };
        Self(Arc::new(Handle { shared }))
    }

    /// Renders at this pixel size (e.g. the on-screen size of the canvas view, rounded to even).
    /// `None` follows the canvas resolution.
    pub fn set_size(&self, size: Option<(u32, u32)>) {
        let packed = size.map_or(0, |(w, h)| (((w.max(2) & !1) as u64) << 32) | (h.max(2) & !1) as u64);
        self.0.shared.size.store(packed, Ordering::Relaxed);
    }

    /// Shows `source` (e.g. a scene that isn't on air) instead of the program, or the program
    /// again with `None`. Takes ownership of the strong reference in `source`.
    pub fn set_source(&self, source: Option<*mut sys::obs_source_t>) {
        let new = source.filter(|p| !p.is_null()).map(|p| {
            unsafe { sys::obs_source_inc_showing(p) };
            SourceRef(p)
        });
        // Swap under the lock (the graphics thread renders under it); release afterwards.
        let old = std::mem::replace(&mut *self.0.shared.source.lock(), new);
        drop(old);
    }

    /// The source shown instead of the program, if any (borrowed pointer, for comparison).
    /// Draws the source at its own size (a player: the whole picture, whatever the canvas),
    /// rather than where it sits on the canvas.
    pub fn set_own_size(&self, own: bool) {
        self.0.shared.own_size.store(own, Ordering::Relaxed);
    }

    pub fn source(&self) -> Option<*mut sys::obs_source_t> {
        self.0.shared.source.lock().as_ref().map(|s| s.0)
    }

    /// Pauses or resumes rendering (hidden views cost nothing).
    pub fn set_active(&self, active: bool) {
        self.0.shared.active.store(active, Ordering::Relaxed);
    }

    /// The newest finished frame, if any.
    pub fn latest_frame(&self) -> Option<Frame> {
        self.0.shared.latest.lock().clone()
    }

    /// Frames rendered since attach (diagnostics).
    pub fn frames(&self) -> u64 {
        self.0.shared.frames.load(Ordering::Relaxed)
    }
}

impl Drop for Handle {
    fn drop(&mut self) {
        unsafe {
            sys::obs_remove_main_rendered_callback(Some(on_rendered), Arc::as_ptr(&self.shared) as *mut c_void);
            if let Some(mut pool) = self.shared.pool.lock().take() {
                sys::obs_enter_graphics();
                pool.destroy();
                sys::obs_leave_graphics();
            }
            if let Some(c) = &self.shared.canvas {
                sys::obs_canvas_release(c.0);
            }
            self.shared.source.lock().take();
        }
    }
}

// ---------------------------------------------------------------- graphics thread
/// Runs on libobs's graphics thread after the main texture is rendered (graphics context held).
unsafe extern "C" fn on_rendered(param: *mut c_void) {
    let shared = unsafe { &*(param as *const Shared) };
    if !shared.active.load(Ordering::Relaxed) {
        return;
    }
    let main = unsafe { sys::obs_get_main_texture() };
    if main.is_null() {
        return;
    }
    let (base_w, base_h) = match &shared.canvas {
        None => unsafe { (sys::gs_texture_get_width(main), sys::gs_texture_get_height(main)) },
        Some(c) => {
            let mut ovi = sys::obs_video_info::default();
            if !unsafe { sys::obs_canvas_get_video_info(c.0, &mut ovi) } {
                return;
            }
            (ovi.base_width, ovi.base_height)
        }
    };
    let packed = shared.size.load(Ordering::Relaxed);
    let (w, h) = if packed == 0 { (base_w & !1, base_h & !1) } else { ((packed >> 32) as u32, packed as u32) };

    let mut guard = shared.pool.lock();
    if guard.as_ref().is_none_or(|p| p.width != w || p.height != h) {
        if let Some(mut old) = guard.take() {
            unsafe { old.destroy() };
        }
        *guard = unsafe { Pool::new(w, h) };
    }
    let Some(pool) = guard.as_mut() else { return };

    // the newest finished conversion goes to the UI
    if let Some(i) = unsafe { pool.finished() } {
        *shared.latest.lock() = Some(pool.slots[i].buffer.clone());
        shared.frames.fetch_add(1, Ordering::Relaxed);
    }
    // the GPU is behind: this frame is skipped rather than waited for
    let Some(slot) = pool.free_slot() else { return };

    let source = shared.source.lock();
    let src = match (&*source, &shared.canvas) {
        (Some(src), _) => unsafe {
            let (sw, sh) = (sys::obs_source_get_width(src.0), sys::obs_source_get_height(src.0));
            let (cw, ch) =
                if shared.own_size.load(Ordering::Relaxed) && sw > 0 && sh > 0 { (sw, sh) } else { (base_w, base_h) };
            draw_with(cw, ch, pool.target, w, h, || sys::obs_source_video_render(src.0));
            pool.target
        },
        // the program as it is: converted straight from libobs's own texture
        (None, None) if (base_w, base_h) == (w, h) => main,
        (None, None) => unsafe {
            draw_scaled(main, base_w, base_h, pool.target, w, h);
            pool.target
        },
        (None, Some(c)) => unsafe {
            draw_with(base_w, base_h, pool.target, w, h, || sys::obs_canvas_render(c.0));
            pool.target
        },
    };
    drop(source);
    unsafe { pool.convert(src, slot) };
}

/// Reads the (Y, Cb, Cr) values at a point of a frame, given as fractions of its size
/// (diagnostics and tests).
/// A frame's pixel size.
pub fn frame_size(frame: &Frame) -> (usize, usize) {
    use objc2_core_video::{CVPixelBufferGetHeight, CVPixelBufferGetWidth};
    (CVPixelBufferGetWidth(frame), CVPixelBufferGetHeight(frame))
}

/// A frame shrunk to at most `max_width` pixels wide, as RGBA (thumbnails). The preview's
/// YCbCr is full-range BT.601.
pub fn frame_rgba(frame: &Frame, max_width: usize) -> Option<(usize, usize, Vec<u8>)> {
    use objc2_core_video::{
        CVPixelBufferGetBaseAddressOfPlane, CVPixelBufferGetBytesPerRowOfPlane, CVPixelBufferGetHeight,
        CVPixelBufferGetWidth, CVPixelBufferLockBaseAddress, CVPixelBufferLockFlags, CVPixelBufferUnlockBaseAddress,
    };
    let (w, h) = (CVPixelBufferGetWidth(frame), CVPixelBufferGetHeight(frame));
    if w == 0 || h == 0 {
        return None;
    }
    let step = w.div_ceil(max_width.max(1)).max(1);
    let (ow, oh) = (w / step, h / step);
    unsafe {
        if CVPixelBufferLockBaseAddress(frame, CVPixelBufferLockFlags::ReadOnly) != 0 {
            return None;
        }
        let luma = CVPixelBufferGetBaseAddressOfPlane(frame, 0) as *const u8;
        let chroma = CVPixelBufferGetBaseAddressOfPlane(frame, 1) as *const u8;
        let out = (!luma.is_null() && !chroma.is_null()).then(|| {
            let (ly, cy) = (CVPixelBufferGetBytesPerRowOfPlane(frame, 0), CVPixelBufferGetBytesPerRowOfPlane(frame, 1));
            let mut rgba = Vec::with_capacity(ow * oh * 4);
            for oy in 0..oh {
                for ox in 0..ow {
                    let (x, y) = (ox * step, oy * step);
                    let yv = *luma.add(y * ly + x) as f32;
                    let c = chroma.add((y / 2) * cy + (x / 2) * 2);
                    let (cb, cr) = (*c as f32 - 128.0, *c.add(1) as f32 - 128.0);
                    let q = |v: f32| v.round().clamp(0.0, 255.0) as u8;
                    rgba.extend_from_slice(&[
                        q(yv + 1.402 * cr),
                        q(yv - 0.344_136 * cb - 0.714_136 * cr),
                        q(yv + 1.772 * cb),
                        255,
                    ]);
                }
            }
            (ow, oh, rgba)
        });
        CVPixelBufferUnlockBaseAddress(frame, CVPixelBufferLockFlags::ReadOnly);
        out
    }
}

pub fn sample_ycbcr(frame: &Frame, fx: f32, fy: f32) -> Option<(u8, u8, u8)> {
    use objc2_core_video::{
        CVPixelBufferGetBaseAddressOfPlane, CVPixelBufferGetBytesPerRowOfPlane, CVPixelBufferGetHeight,
        CVPixelBufferGetWidth, CVPixelBufferLockBaseAddress, CVPixelBufferLockFlags, CVPixelBufferUnlockBaseAddress,
    };
    let (w, h) = (CVPixelBufferGetWidth(frame), CVPixelBufferGetHeight(frame));
    let (x, y) = (((w as f32 * fx) as usize).min(w - 1), ((h as f32 * fy) as usize).min(h - 1));
    unsafe {
        if CVPixelBufferLockBaseAddress(frame, CVPixelBufferLockFlags::ReadOnly) != 0 {
            return None;
        }
        let luma = CVPixelBufferGetBaseAddressOfPlane(frame, 0) as *const u8;
        let chroma = CVPixelBufferGetBaseAddressOfPlane(frame, 1) as *const u8;
        let out = (!luma.is_null() && !chroma.is_null()).then(|| {
            let yv = *luma.add(y * CVPixelBufferGetBytesPerRowOfPlane(frame, 0) + x);
            let c = chroma.add((y / 2) * CVPixelBufferGetBytesPerRowOfPlane(frame, 1) + (x / 2) * 2);
            (yv, *c, *c.add(1))
        });
        CVPixelBufferUnlockBaseAddress(frame, CVPixelBufferLockFlags::ReadOnly);
        out
    }
}
