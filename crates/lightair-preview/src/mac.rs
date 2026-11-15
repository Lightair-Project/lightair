//! macOS preview path (OpenGL libobs backend):
//!
//! 1. libobs renders the program canvas, scaled to the preview size, into its own BGRA render
//!    target (libobs can't render into IOSurface textures directly).
//! 2. A GL framebuffer blit copies that into a BGRA IOSurface (GPU only).
//! 3. VideoToolbox converts the BGRA IOSurface into full-range BT.601 NV12 (`420f`), the only
//!    format GPUI's `surface()` element draws, into a ring of three buffers.
//! 4. The UI picks up the newest NV12 buffer each frame.
//!
//! No pixel ever crosses to the CPU.

use std::ffi::c_void;

use crate::render::{draw_scaled, draw_with};
use std::ptr::{self, NonNull};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicIsize, AtomicU64, Ordering};

use lightair_engine::sys;
use objc2_core_foundation::{CFBoolean, CFDictionary, CFRetained, CFString, CFType};
use objc2_core_video::{
    CVAttachmentMode, CVPixelBuffer, CVPixelBufferCreate, CVPixelBufferGetIOSurface,
    kCVImageBufferYCbCrMatrix_ITU_R_601_4, kCVImageBufferYCbCrMatrixKey, kCVPixelBufferIOSurfacePropertiesKey,
    kCVPixelBufferMetalCompatibilityKey, kCVPixelFormatType_32BGRA, kCVPixelFormatType_420YpCbCr8BiPlanarFullRange,
    kCVReturnSuccess,
};
use objc2_video_toolbox::VTPixelTransferSession;
use parking_lot::Mutex;

const RING: usize = 3;

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
    pub const RGBA8: GLenum = 0x8058;
    pub const BGRA: GLenum = 0x80E1;
    pub const UNSIGNED_INT_8_8_8_8_REV: GLenum = 0x8367;
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
        pub fn glFinish();
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

/// Everything sized to one preview resolution. Owned by the graphics thread except `ring`,
/// whose buffers the UI also retains.
struct Pool {
    width: u32,
    height: u32,
    /// libobs render target the canvas is drawn into.
    target: *mut sys::gs_texture_t,
    /// BGRA IOSurface the target is blitted into, and its GL texture + framebuffers.
    bgra: CFRetained<CVPixelBuffer>,
    bgra_tex: gl::GLuint,
    read_fbo: gl::GLuint,
    draw_fbo: gl::GLuint,
    ring: Vec<CFRetained<CVPixelBuffer>>,
    next: usize,
    latest: AtomicIsize,
}

// SAFETY: GL/libobs handles are only used on libobs's graphics thread; the CoreVideo buffers
// are thread-safe reference-counted objects.
unsafe impl Send for Pool {}
unsafe impl Sync for Pool {}

impl Pool {
    /// Must run inside the libobs graphics context (GL context current).
    unsafe fn new(width: u32, height: u32) -> Option<Self> {
        let bgra = pixel_buffer(width, height, kCVPixelFormatType_32BGRA)?;
        let ring = (0..RING)
            .map(|_| pixel_buffer(width, height, kCVPixelFormatType_420YpCbCr8BiPlanarFullRange))
            .collect::<Option<Vec<_>>>()?;
        let target =
            unsafe { sys::gs_texture_create(width, height, sys::GS_BGRA, 1, ptr::null_mut(), sys::GS_RENDER_TARGET) };
        if target.is_null() {
            return None;
        }
        let surface = CVPixelBufferGetIOSurface(Some(&bgra))?;
        let mut pool = Self {
            width,
            height,
            target,
            bgra,
            bgra_tex: 0,
            read_fbo: 0,
            draw_fbo: 0,
            ring,
            next: 0,
            latest: AtomicIsize::new(-1),
        };
        unsafe {
            let mut prev_rect: gl::GLint = 0;
            gl::glGetIntegerv(gl::TEXTURE_BINDING_RECTANGLE, &mut prev_rect);
            gl::glGenTextures(1, &mut pool.bgra_tex);
            gl::glBindTexture(gl::TEXTURE_RECTANGLE, pool.bgra_tex);
            let err = gl::CGLTexImageIOSurface2D(
                gl::CGLGetCurrentContext(),
                gl::TEXTURE_RECTANGLE,
                gl::RGBA8,
                width as i32,
                height as i32,
                gl::BGRA,
                gl::UNSIGNED_INT_8_8_8_8_REV,
                CFRetained::as_ptr(&surface).as_ptr() as *const c_void,
                0,
            );
            gl::glBindTexture(gl::TEXTURE_RECTANGLE, prev_rect as gl::GLuint);
            if err != 0 {
                tracing::error!(err, "CGLTexImageIOSurface2D failed");
                pool.destroy();
                return None;
            }
            gl::glGenFramebuffers(1, &mut pool.read_fbo);
            gl::glGenFramebuffers(1, &mut pool.draw_fbo);
        }
        Some(pool)
    }

    /// Must run inside the libobs graphics context.
    unsafe fn destroy(&mut self) {
        unsafe {
            if self.read_fbo != 0 {
                gl::glDeleteFramebuffers(1, &self.read_fbo);
            }
            if self.draw_fbo != 0 {
                gl::glDeleteFramebuffers(1, &self.draw_fbo);
            }
            if self.bgra_tex != 0 {
                gl::glDeleteTextures(1, &self.bgra_tex);
            }
            if !self.target.is_null() {
                sys::gs_texture_destroy(self.target);
            }
        }
        self.read_fbo = 0;
        self.draw_fbo = 0;
        self.bgra_tex = 0;
        self.target = ptr::null_mut();
    }

    /// Copies the libobs render target into the BGRA IOSurface with a GL blit.
    unsafe fn blit_to_surface(&self) -> bool {
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
                self.bgra_tex,
                0,
            );
            let ok = gl::glCheckFramebufferStatus(gl::READ_FRAMEBUFFER) == gl::FRAMEBUFFER_COMPLETE
                && gl::glCheckFramebufferStatus(gl::DRAW_FRAMEBUFFER) == gl::FRAMEBUFFER_COMPLETE;
            if ok {
                let (w, h) = (self.width as i32, self.height as i32);
                gl::glBlitFramebuffer(0, 0, w, h, 0, 0, w, h, gl::COLOR_BUFFER_BIT, gl::NEAREST);
            }
            gl::glBindFramebuffer(gl::READ_FRAMEBUFFER, prev_read as gl::GLuint);
            gl::glBindFramebuffer(gl::DRAW_FRAMEBUFFER, prev_draw as gl::GLuint);
            // VideoToolbox reads the IOSurface on another queue next: the blit must be finished.
            gl::glFinish();
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
    transfer: Mutex<Option<CFRetained<VTPixelTransferSession>>>,
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
        let mut session: *mut VTPixelTransferSession = ptr::null_mut();
        let rc = unsafe { VTPixelTransferSession::create(None, NonNull::from(&mut session)) };
        let transfer =
            (rc == 0 && !session.is_null()).then(|| unsafe { CFRetained::from_raw(NonNull::new_unchecked(session)) });
        if transfer.is_none() {
            tracing::error!(rc, "VTPixelTransferSessionCreate failed; preview disabled");
        }
        let shared = Arc::new(Shared {
            pool: Mutex::new(None),
            latest: Mutex::new(None),
            size: AtomicU64::new(0),
            active: AtomicBool::new(true),
            frames: AtomicU64::new(0),
            transfer: Mutex::new(transfer),
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

    let source = shared.source.lock();
    match (&*source, &shared.canvas) {
        (Some(src), _) => unsafe {
            let (sw, sh) = (sys::obs_source_get_width(src.0), sys::obs_source_get_height(src.0));
            let (cw, ch) =
                if shared.own_size.load(Ordering::Relaxed) && sw > 0 && sh > 0 { (sw, sh) } else { (base_w, base_h) };
            draw_with(cw, ch, pool.target, w, h, || sys::obs_source_video_render(src.0))
        },
        (None, None) => unsafe { draw_scaled(main, base_w, base_h, pool.target, w, h) },
        (None, Some(c)) => unsafe { draw_with(base_w, base_h, pool.target, w, h, || sys::obs_canvas_render(c.0)) },
    }
    drop(source);
    if !unsafe { pool.blit_to_surface() } {
        return;
    }
    let idx = pool.next;
    pool.next = (pool.next + 1) % RING;
    let dst = &pool.ring[idx];
    let ok = shared.transfer.lock().as_ref().is_some_and(|t| unsafe { t.transfer_image(&pool.bgra, dst) } == 0);
    if !ok {
        return;
    }
    pool.latest.store(idx as isize, Ordering::Release);
    *shared.latest.lock() = Some(dst.clone());
    shared.frames.fetch_add(1, Ordering::Relaxed);
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
