//! macOS preview path: Lightair's own compositor (`compose`) draws each frame into an
//! IOSurface-backed pixel buffer (BGRA); the preview hands the newest to the UI and to the
//! window's Core Animation layer. GPUI's `surface()` element draws NV12, so a frame drawn with it
//! is converted on the GPU by VideoToolbox (`nv12`), once per frame.
//!
//! No pixel ever crosses to the CPU.

use std::ffi::c_void;

use std::ptr::{self, NonNull};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

use lightair_model::Id;
use objc2_core_foundation::{CFBoolean, CFDictionary, CFRetained, CFString, CFType};
use objc2_core_video::{
    CVAttachmentMode, CVPixelBuffer, CVPixelBufferCreate, CVPixelBufferGetIOSurface,
    kCVImageBufferYCbCrMatrix_ITU_R_601_4, kCVImageBufferYCbCrMatrixKey, kCVPixelBufferIOSurfacePropertiesKey,
    kCVPixelBufferMetalCompatibilityKey, kCVPixelFormatType_420YpCbCr8BiPlanarFullRange,
    kCVReturnSuccess,
};
use parking_lot::Mutex;

/// A finished preview frame (NV12 full range), ready for GPUI's `surface()` element.
pub type Frame = CFRetained<CVPixelBuffer>;

// ---------------------------------------------------------------- buffers
pub(crate) fn pixel_buffer(width: u32, height: u32, format: u32) -> Option<CFRetained<CVPixelBuffer>> {
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

// ---------------------------------------------------------------- shared state
pub(crate) struct Shared {
    /// Newest finished frame for the UI.
    latest: Mutex<Option<Frame>>,
    /// Requested size, packed as (width << 32 | height). 0 = follow the canvas size.
    size: AtomicU64,
    active: AtomicBool,
    frames: AtomicU64,
    /// `None`: the main canvas (program). `Some`: another canvas (an additional format), by id.
    canvas: Option<Id>,
    /// When set, shows this scene and the canvas it's laid out in (a scene that isn't on air:
    /// Studio mode's preview) instead of the canvas's program.
    source: Mutex<Option<(Id, Id)>>,
    /// Draw the source at its own size (a player), not in the canvas's coordinates.
    own_size: AtomicBool,
    /// A Core Animation layer each finished frame is shown in as well (the window's preview,
    /// drawn under its UI without the UI redrawing; `Preview::set_layer`). Retained.
    layer: Mutex<Option<LayerRef>>,
    /// The scene shown instead of the program, if any (Lightair's own compositor draws it).
    scene: Mutex<Shown>,
    /// The most frames a second the compositor draws a scene shown here (0: none set).
    rate: AtomicU32,
}

/// The scene shown instead of the program.
#[derive(Default)]
struct Shown {
    /// The scene and the canvas it's laid out in.
    at: Option<(Id, Id)>,
    /// Counts every change of what's shown, so a frame drawn before one is never given after it.
    generation: u64,
}

/// A scene a preview shows instead of the program, as Lightair's own compositor draws it.
#[derive(Clone, Copy)]
pub(crate) struct SceneView {
    pub(crate) scene: Id,
    pub(crate) canvas: Id,
    /// What `Shared::feed_scene` is given with the frame.
    pub(crate) generation: u64,
    /// The size asked for (`Preview::set_size`); `None`: the canvas's.
    pub(crate) size: Option<(u32, u32)>,
    /// The most frames a second asked for (`Preview::set_rate`).
    pub(crate) rate: Option<u32>,
}

impl Shared {
    /// Whether the compositor's program frames are wanted now: the preview is active and shows
    /// the program (not a source in its place).
    pub(crate) fn wants_program(&self) -> bool {
        self.active.load(Ordering::Relaxed) && self.source.lock().is_none()
    }

    /// The compositor's newest finished program frame (BGRA): to the UI and the layer. Ignored
    /// while a scene is shown instead of the program.
    pub(crate) fn feed(&self, frame: &Frame) {
        if self.source.lock().is_some() {
            return;
        }
        // a buffer drawn into again: the NV12 copy of its last picture goes (see `nv12`)
        frame.remove_attachment(&nv12_key());
        *self.latest.lock() = Some(frame.clone());
        self.frames.fetch_add(1, Ordering::Relaxed);
        if let Some(layer) = self.layer.lock().as_ref()
            && let Some(surface) = CVPixelBufferGetIOSurface(Some(frame))
        {
            unsafe { ca::show(layer.0, CFRetained::as_ptr(&surface).as_ptr() as *mut c_void) };
        }
    }

    /// The compositor stopped giving this preview the program: nothing else draws it, so
    /// nothing to do.
    pub(crate) fn unfeed(&self) {}

    /// The scene shown instead of the program, while the preview is active and shows one.
    pub(crate) fn scene_view(&self) -> Option<SceneView> {
        if !self.active.load(Ordering::Relaxed) {
            return None;
        }
        let shown = self.scene.lock();
        let (scene, canvas) = shown.at?;
        let packed = self.size.load(Ordering::Relaxed);
        let rate = self.rate.load(Ordering::Relaxed);
        Some(SceneView {
            scene,
            canvas,
            generation: shown.generation,
            size: (packed != 0).then_some(((packed >> 32) as u32, packed as u32)),
            rate: (rate != 0).then_some(rate),
        })
    }

    /// The compositor's newest frame (BGRA) of the scene shown, drawn for `generation`: to the
    /// UI and the layer. Ignored when what's shown changed since.
    pub(crate) fn feed_scene(&self, frame: &Frame, generation: u64) {
        let shown = self.scene.lock();
        if shown.generation != generation || shown.at.is_none() {
            return;
        }
        frame.remove_attachment(&nv12_key());
        *self.latest.lock() = Some(frame.clone());
        self.frames.fetch_add(1, Ordering::Relaxed);
        if let Some(layer) = self.layer.lock().as_ref()
            && let Some(surface) = CVPixelBufferGetIOSurface(Some(frame))
        {
            unsafe { ca::show(layer.0, CFRetained::as_ptr(&surface).as_ptr() as *mut c_void) };
        }
    }

    /// The compositor no longer draws the scene shown: nothing else draws it, so nothing to do.
    pub(crate) fn unfeed_scene(&self) {}
}

/// A retained `CALayer`.
struct LayerRef(*mut c_void);

impl Drop for LayerRef {
    fn drop(&mut self) {
        unsafe { ca::CFRelease(self.0) };
    }
}

/// Core Animation, through the Objective-C runtime.
mod ca {
    use std::ffi::{c_char, c_void};

    #[link(name = "QuartzCore", kind = "framework")]
    unsafe extern "C" {}

    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        pub fn CFRetain(cf: *const c_void) -> *const c_void;
        pub fn CFRelease(cf: *const c_void);
    }
    #[link(name = "objc")]
    unsafe extern "C" {
        fn objc_getClass(name: *const c_char) -> *mut c_void;
        fn sel_registerName(name: *const c_char) -> *mut c_void;
        fn objc_msgSend();
        fn objc_autoreleasePoolPush() -> *mut c_void;
        fn objc_autoreleasePoolPop(pool: *mut c_void);
    }

    /// `layer.contents = surface` in a transaction of its own, without animation (any thread).
    pub unsafe fn show(layer: *mut c_void, surface: *mut c_void) {
        unsafe {
            let pool = objc_autoreleasePoolPush();
            let send0: unsafe extern "C" fn(*mut c_void, *mut c_void) = std::mem::transmute(objc_msgSend as unsafe extern "C" fn());
            let send_bool: unsafe extern "C" fn(*mut c_void, *mut c_void, bool) = std::mem::transmute(objc_msgSend as unsafe extern "C" fn());
            let send_obj: unsafe extern "C" fn(*mut c_void, *mut c_void, *mut c_void) = std::mem::transmute(objc_msgSend as unsafe extern "C" fn());
            let tx = objc_getClass(c"CATransaction".as_ptr());
            send0(tx, sel_registerName(c"begin".as_ptr()));
            send_bool(tx, sel_registerName(c"setDisableActions:".as_ptr()), true);
            send_obj(layer, sel_registerName(c"setContents:".as_ptr()), surface);
            send0(tx, sel_registerName(c"commit".as_ptr()));
            objc_autoreleasePoolPop(pool);
        }
    }
}

// SAFETY: `Shared` is used by the compositor's thread and the UI thread. Every field is either
// atomic or behind a mutex; the CoreVideo objects inside are reference-counted CoreFoundation
// types that may be retained and released on any thread, and the layer is only given a surface
// inside a Core Animation transaction.
unsafe impl Send for Shared {}
unsafe impl Sync for Shared {}

/// Handle to a preview of the program canvas. Cheap to clone; rendering stops when the last
/// clone is dropped.
#[derive(Clone)]
pub struct Preview(Arc<Handle>);

struct Handle {
    shared: Arc<Shared>,
    /// Lightair's own compositor gives this preview its frames (`compose_program`).
    composed: AtomicBool,
    /// Lightair's own compositor draws the scenes shown in the program's place (`set_source`).
    scenes: AtomicBool,
}

impl Preview {
    /// A preview of the main canvas.
    pub fn attach() -> Self {
        Self::attach_to(None)
    }

    /// Previews another canvas (an additional format), by its id.
    pub fn attach_canvas(canvas: Id) -> Self {
        Self::attach_to(Some(canvas))
    }

    fn attach_to(canvas: Option<Id>) -> Self {
        let shared = Arc::new(Shared {
            latest: Mutex::new(None),
            size: AtomicU64::new(0),
            active: AtomicBool::new(true),
            frames: AtomicU64::new(0),
            canvas,
            source: Mutex::new(None),
            own_size: AtomicBool::new(false),
            layer: Mutex::new(None),
            scene: Mutex::new(Shown::default()),
            rate: AtomicU32::new(0),
        });
        Self(Arc::new(Handle { shared, composed: AtomicBool::new(false), scenes: AtomicBool::new(false) }))
    }

    /// Shows the canvas's program as Lightair's own compositor draws it (`lightair-render`). The
    /// main canvas's preview (`attach`) follows the primary canvas; another canvas's
    /// (`attach_canvas`), that canvas. False when the compositor couldn't start.
    pub fn compose_program(&self) -> bool {
        if !lightair_host::bridge::own() {
            return false;
        }
        let target = match &self.0.shared.canvas {
            None => crate::compose::Target::Primary,
            Some(c) => crate::compose::Target::Canvas(c.to_string()),
        };
        if !crate::compose::start() {
            return false;
        }
        crate::compose::watch(target, &self.0.shared);
        self.0.composed.store(true, Ordering::Relaxed);
        true
    }

    /// Renders at this pixel size (e.g. the on-screen size of the canvas view, rounded to even).
    /// `None` follows the canvas resolution.
    pub fn set_size(&self, size: Option<(u32, u32)>) {
        let packed = size.map_or(0, |(w, h)| (((w.max(2) & !1) as u64) << 32) | (h.max(2) & !1) as u64);
        self.0.shared.size.store(packed, Ordering::Relaxed);
    }

    /// Shows a scene (and the canvas it's laid out in) instead of the program, or the program
    /// again with `None`, as Lightair's own compositor draws it.
    pub fn set_source(&self, scene: Option<(Id, Id)>) {
        let shared = &self.0.shared;
        // what's shown changes first, so a frame of the old scene isn't taken after it
        {
            let mut shown = shared.scene.lock();
            shown.at = scene;
            shown.generation += 1;
        }
        *shared.source.lock() = scene;
        if scene.is_some()
            && lightair_host::bridge::own()
            && crate::compose::start()
            && !self.0.scenes.swap(true, Ordering::Relaxed)
        {
            crate::compose::watch_scenes(shared);
        }
    }

    /// The most frames a second Lightair's own compositor draws a scene shown here
    /// (`set_source`), for thumbnails and other views that needn't move smoothly. `None`: the
    /// canvas's rate, or at most 30 for a view a quarter of the canvas's area or smaller.
    pub fn set_rate(&self, fps: Option<u32>) {
        self.0.shared.rate.store(fps.unwrap_or(0), Ordering::Relaxed);
    }

    /// Draws the source at its own size (a player: the whole picture, whatever the canvas),
    /// rather than where it sits on the canvas.
    pub fn set_own_size(&self, own: bool) {
        self.0.shared.own_size.store(own, Ordering::Relaxed);
    }

    /// Shows each finished frame in `layer` (a `CALayer`, retained here) too, or stops with
    /// `None`. The window draws its preview this way, under its UI, so a new frame doesn't make
    /// the UI redraw.
    pub fn set_layer(&self, layer: Option<*mut c_void>) {
        let new = layer.filter(|l| !l.is_null()).map(|l| {
            unsafe { ca::CFRetain(l) };
            LayerRef(l)
        });
        let old = std::mem::replace(&mut *self.0.shared.layer.lock(), new);
        drop(old);
    }

    /// The scene shown instead of the program, if any, and its canvas.
    pub fn source(&self) -> Option<(Id, Id)> {
        *self.0.shared.source.lock()
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
        // the compositor gives it no more frames
        if self.composed.load(Ordering::Relaxed) || self.scenes.load(Ordering::Relaxed) {
            crate::compose::unwatch(&self.shared);
        }
    }
}

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
    if let Some(bgra) = bgra_pixels(frame) {
        let (row, data) = bgra;
        let mut rgba = Vec::with_capacity(ow * oh * 4);
        for oy in 0..oh {
            for ox in 0..ow {
                let p = &data[oy * step * row + ox * step * 4..][..4];
                rgba.extend_from_slice(&[p[2], p[1], p[0], 255]);
            }
        }
        return Some((ow, oh, rgba));
    }
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

/// Reads the (Y, Cb, Cr) values at a point of a frame, given as fractions of its size
/// (diagnostics and tests).
pub fn sample_ycbcr(frame: &Frame, fx: f32, fy: f32) -> Option<(u8, u8, u8)> {
    use objc2_core_video::{
        CVPixelBufferGetBaseAddressOfPlane, CVPixelBufferGetBytesPerRowOfPlane, CVPixelBufferGetHeight,
        CVPixelBufferGetWidth, CVPixelBufferLockBaseAddress, CVPixelBufferLockFlags, CVPixelBufferUnlockBaseAddress,
    };
    let (w, h) = (CVPixelBufferGetWidth(frame), CVPixelBufferGetHeight(frame));
    let (x, y) = (((w as f32 * fx) as usize).min(w - 1), ((h as f32 * fy) as usize).min(h - 1));
    if let Some((row, data)) = bgra_pixels(frame) {
        // the same full-range BT.601 the NV12 frames carry
        let p = &data[y * row + x * 4..][..4];
        let (b, g, r) = (p[0] as f32, p[1] as f32, p[2] as f32);
        let q = |v: f32| v.round().clamp(0.0, 255.0) as u8;
        return Some((
            q(0.299 * r + 0.587 * g + 0.114 * b),
            q(-0.168_736 * r - 0.331_264 * g + 0.5 * b + 128.0),
            q(0.5 * r - 0.418_688 * g - 0.081_312 * b + 128.0),
        ));
    }
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

/// A BGRA frame's rows (bytes per row, the pixels), copied out; `None` for NV12 frames.
fn bgra_pixels(frame: &Frame) -> Option<(usize, Vec<u8>)> {
    use objc2_core_video::{
        CVPixelBufferGetBaseAddress, CVPixelBufferGetBytesPerRow, CVPixelBufferGetHeight, CVPixelBufferGetPixelFormatType,
        CVPixelBufferLockBaseAddress, CVPixelBufferLockFlags, CVPixelBufferUnlockBaseAddress,
    };
    if CVPixelBufferGetPixelFormatType(frame) != objc2_core_video::kCVPixelFormatType_32BGRA {
        return None;
    }
    unsafe {
        if CVPixelBufferLockBaseAddress(frame, CVPixelBufferLockFlags::ReadOnly) != 0 {
            return None;
        }
        let base = CVPixelBufferGetBaseAddress(frame) as *const u8;
        let row = CVPixelBufferGetBytesPerRow(frame);
        let out = (!base.is_null()).then(|| (row, std::slice::from_raw_parts(base, row * CVPixelBufferGetHeight(frame)).to_vec()));
        CVPixelBufferUnlockBaseAddress(frame, CVPixelBufferLockFlags::ReadOnly);
        out
    }
}

/// The frame is NV12 (what GPUI's `surface()` draws), not BGRA (the compositor's).
pub fn frame_is_nv12(frame: &Frame) -> bool {
    objc2_core_video::CVPixelBufferGetPixelFormatType(frame) == kCVPixelFormatType_420YpCbCr8BiPlanarFullRange
}

/// The attachment on a BGRA frame holding its NV12 copy (`nv12`), dropped when the buffer is
/// drawn into again.
fn nv12_key() -> CFRetained<CFString> {
    CFString::from_static_str("LightairNV12")
}

/// NV12 copies the UI has made, reused once nothing else holds them (GPUI keeps the one it's
/// drawing until its next frame).
const NV12_KEPT: usize = 12;

/// The frame as NV12, for GPUI's `surface()`: itself when it's NV12; a BGRA frame's copy
/// converted on the GPU by VideoToolbox, made once per frame (whichever views draw it) and only
/// when something draws it with GPUI (the canvas's Core Animation layer takes BGRA as it is).
/// Call on the UI thread.
pub fn nv12(frame: &Frame) -> Option<Frame> {
    use objc2_core_video::{CVPixelBufferGetHeight, CVPixelBufferGetWidth};
    use objc2_video_toolbox::VTPixelTransferSession;
    thread_local! {
        static SESSION: Option<CFRetained<VTPixelTransferSession>> = {
            let mut session: *mut VTPixelTransferSession = ptr::null_mut();
            let rc = unsafe { VTPixelTransferSession::create(None, NonNull::from(&mut session)) };
            if rc != 0 || session.is_null() {
                tracing::error!(rc, "VTPixelTransferSessionCreate failed");
                None
            } else {
                let session = unsafe { CFRetained::from_raw(NonNull::new_unchecked(session)) };
                // GPUI's surface shader decodes BT.601 (VideoToolbox would pick BT.709)
                unsafe {
                    objc2_video_toolbox::VTSessionSetProperty(
                        &session,
                        objc2_video_toolbox::kVTPixelTransferPropertyKey_DestinationYCbCrMatrix,
                        Some(kCVImageBufferYCbCrMatrix_ITU_R_601_4.as_ref()),
                    )
                };
                Some(session)
            }
        };
        static KEPT: std::cell::RefCell<Vec<Frame>> = const { std::cell::RefCell::new(Vec::new()) };
    }
    if frame_is_nv12(frame) {
        return Some(frame.clone());
    }
    let key = nv12_key();
    if let Some(done) = unsafe { frame.attachment(&key, ptr::null_mut()) } {
        // SAFETY: only `nv12` sets this attachment, always to a CVPixelBuffer
        return Some(unsafe { CFRetained::cast_unchecked::<CVPixelBuffer>(done) });
    }
    let (w, h) = (CVPixelBufferGetWidth(frame), CVPixelBufferGetHeight(frame));
    let out = KEPT.with_borrow_mut(|kept| {
        kept.retain(|b| (CVPixelBufferGetWidth(b), CVPixelBufferGetHeight(b)) == (w, h));
        // held only here: free (an attachment or GPUI holding it counts)
        if let Some(free) = kept.iter().find(|b| b.retain_count() == 1) {
            return Some(free.clone());
        }
        let new = pixel_buffer(w as u32, h as u32, kCVPixelFormatType_420YpCbCr8BiPlanarFullRange)?;
        if kept.len() < NV12_KEPT {
            kept.push(new.clone());
        }
        Some(new)
    })?;
    let ok = SESSION.with(|s| s.as_ref().is_some_and(|s| unsafe { s.transfer_image(frame, &out) } == 0));
    if !ok {
        return None;
    }
    unsafe { frame.set_attachment(&key, out.as_ref(), CVAttachmentMode::ShouldNotPropagate) };
    Some(out)
}
