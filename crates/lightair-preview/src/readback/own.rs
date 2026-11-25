//! Without libobs: a preview's frames come from Lightair's own compositor (`compose`), which
//! runs on macOS only today (docs/plans/no-libobs.md, step 10). Here the preview keeps what it's
//! asked to show (the canvas, a scene in the program's place, the size, the rate, the window's
//! layer); asking for the program says "not yet" in the log, once, and the preview stays empty.
//!
//! On Windows the compositor's side is ready for it, with the same calls as macOS's (`mac.rs`):
//! the compositor feeds each finished frame (a shared texture, `lightair_host::buffers::Buffer`)
//! to [`Shared::feed`] or [`Shared::feed_scene`]; the frame is presented to the window's swap
//! chain on the GPU ([`crate::Layer::present_buffer`]) and read back to memory only while
//! something asks for frames ([`Preview::latest_frame`]: thumbnails, screenshots, the colour
//! picker) or there's no swap chain.

use std::ffi::c_void;
use std::sync::Arc;
use std::sync::Once;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::time::Instant;

use lightair_model::Id;
use parking_lot::Mutex;

use super::Frame;

/// Frames are read back while asked for within this long.
const ASKED_FOR_MS: u64 = 2000;

fn now_ms() -> u64 {
    static EPOCH: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
    EPOCH.get_or_init(Instant::now).elapsed().as_millis() as u64 + 1
}

/// The scene shown instead of the program, if the source shown is one.
#[derive(Default)]
struct Shown {
    /// The scene and the canvas it's laid out in.
    at: Option<(Id, Id)>,
    /// Counts every change of what's shown, so a frame drawn before one is never given after it.
    generation: u64,
}

/// A preview's state, shared with the compositor.
pub(crate) struct Shared {
    /// The canvas previewed; `None`: the primary canvas.
    #[cfg_attr(not(windows), allow(dead_code))]
    canvas: Option<Id>,
    scene: Mutex<Shown>,
    /// Requested size, packed as (width << 32 | height). 0 = follow the canvas size.
    size: AtomicU64,
    /// The most frames a second asked for; 0: the canvas's rate.
    rate: AtomicU32,
    own_size: AtomicBool,
    active: AtomicBool,
    latest: Mutex<Option<Frame>>,
    frames: AtomicU64,
    /// When `latest_frame` was last asked (ms since `EPOCH`).
    asked: AtomicU64,
    /// Windows: the swap chain frames are presented to (the window's picture).
    #[cfg(windows)]
    layer: Mutex<Option<Arc<crate::Layer>>>,
    /// Windows: where frames are read back through (a staging texture of the frames' size).
    #[cfg(windows)]
    staging: Mutex<Option<windows::Win32::Graphics::Direct3D11::ID3D11Texture2D>>,
}

// SAFETY: every field is atomic or behind a mutex; the Direct3D 11 objects are free-threaded.
unsafe impl Send for Shared {}
unsafe impl Sync for Shared {}

/// A scene a preview shows instead of the program, as Lightair's own compositor draws it.
#[cfg(windows)]
#[derive(Clone, Copy)]
#[allow(dead_code, reason = "Lightair's own compositor reads it once it runs on Windows")]
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

/// Handle to a preview of a canvas. Cheap to clone.
#[derive(Clone)]
pub struct Preview(Arc<Shared>);

impl Preview {
    /// A preview of the primary canvas.
    pub fn attach() -> Self {
        Self::attach_to(None)
    }

    /// Previews another canvas (an additional format), by its id.
    pub fn attach_canvas(canvas: Id) -> Self {
        Self::attach_to(Some(canvas))
    }

    fn attach_to(canvas: Option<Id>) -> Self {
        Self(Arc::new(Shared {
            canvas,
            scene: Mutex::new(Shown::default()),
            size: AtomicU64::new(0),
            rate: AtomicU32::new(0),
            own_size: AtomicBool::new(false),
            active: AtomicBool::new(true),
            latest: Mutex::new(None),
            frames: AtomicU64::new(0),
            asked: AtomicU64::new(0),
            #[cfg(windows)]
            layer: Mutex::new(None),
            #[cfg(windows)]
            staging: Mutex::new(None),
        }))
    }

    /// Shows the canvas's program as Lightair's own compositor draws it. False: the compositor
    /// doesn't run on this platform yet.
    pub fn compose_program(&self) -> bool {
        static SAID: Once = Once::new();
        SAID.call_once(|| {
            tracing::warn!("not yet: Lightair's own compositor on this platform (the preview stays empty)")
        });
        false
    }

    /// Renders at this pixel size; `None` follows the canvas resolution.
    pub fn set_size(&self, size: Option<(u32, u32)>) {
        let packed = size.map_or(0, |(w, h)| (((w.max(2) & !1) as u64) << 32) | (h.max(2) & !1) as u64);
        self.0.size.store(packed, Ordering::Relaxed);
    }

    /// Shows a scene (and the canvas it's laid out in) instead of the program, or the program
    /// again with `None`.
    pub fn set_source(&self, source: Option<(Id, Id)>) {
        let mut shown = self.0.scene.lock();
        shown.at = source;
        shown.generation += 1;
        *self.0.latest.lock() = None;
    }

    /// The most frames a second a scene shown here is drawn; `None`: the canvas's rate.
    pub fn set_rate(&self, fps: Option<u32>) {
        self.0.rate.store(fps.unwrap_or(0), Ordering::Relaxed);
    }

    /// Draws the source at its own size rather than where it sits on the canvas.
    pub fn set_own_size(&self, own: bool) {
        self.0.own_size.store(own, Ordering::Relaxed);
    }

    /// Presents each frame to `layer` too (Windows: a `Layer`, as `Arc::as_ptr` gives it; a
    /// reference is taken here), or stops with `None`.
    pub fn set_layer(&self, layer: Option<*mut c_void>) {
        #[cfg(windows)]
        {
            let new = layer.filter(|l| !l.is_null()).map(|l| unsafe {
                let l = l as *const crate::Layer;
                Arc::increment_strong_count(l);
                Arc::from_raw(l)
            });
            let old = std::mem::replace(&mut *self.0.layer.lock(), new);
            drop(old);
        }
        #[cfg(not(windows))]
        let _ = layer;
    }

    /// The scene shown instead of the program, if any, and its canvas.
    pub fn source(&self) -> Option<(Id, Id)> {
        self.0.scene.lock().at
    }

    /// Pauses or resumes rendering.
    pub fn set_active(&self, active: bool) {
        self.0.active.store(active, Ordering::Relaxed);
    }

    /// The newest finished frame, if any. Asking keeps frames read back for a while.
    pub fn latest_frame(&self) -> Option<Frame> {
        self.0.asked.store(now_ms(), Ordering::Relaxed);
        self.0.latest.lock().clone()
    }

    /// Frames shown since attach (diagnostics).
    pub fn frames(&self) -> u64 {
        self.0.frames.load(Ordering::Relaxed)
    }
}

/// What the compositor calls, as on macOS (`mac::Shared`).
#[cfg(windows)]
#[allow(dead_code, reason = "Lightair's own compositor calls these once it runs on Windows")]
impl Shared {
    /// Whether the compositor's program frames are wanted now: the preview is active and shows
    /// the program (not a scene in its place).
    pub(crate) fn wants_program(&self) -> bool {
        self.active.load(Ordering::Relaxed) && self.scene.lock().at.is_none()
    }

    /// The canvas previewed; `None`: the primary canvas.
    pub(crate) fn canvas(&self) -> Option<Id> {
        self.canvas
    }

    /// The compositor's newest finished program frame. Ignored while a scene is shown instead.
    pub(crate) fn feed(&self, frame: &lightair_host::buffers::Buffer) {
        if self.scene.lock().at.is_some() {
            return;
        }
        self.show(frame);
    }

    /// The compositor stopped.
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

    /// The compositor's newest frame of the scene shown, drawn for `generation`. Ignored when
    /// what's shown changed since.
    pub(crate) fn feed_scene(&self, frame: &lightair_host::buffers::Buffer, generation: u64) {
        let shown = self.scene.lock();
        if shown.generation != generation || shown.at.is_none() {
            return;
        }
        drop(shown);
        self.show(frame);
    }

    /// The compositor no longer draws the scene shown.
    pub(crate) fn unfeed_scene(&self) {}

    /// The size asked for (`Preview::set_size`); `None`: the canvas's.
    pub(crate) fn size(&self) -> Option<(u32, u32)> {
        let packed = self.size.load(Ordering::Relaxed);
        (packed != 0).then_some(((packed >> 32) as u32, packed as u32))
    }

    /// Whether a source is drawn at its own size.
    pub(crate) fn own_size(&self) -> bool {
        self.own_size.load(Ordering::Relaxed)
    }

    /// Presents `frame` to the window's swap chain, and reads it back while frames are asked
    /// for (or there's no swap chain to show it).
    fn show(&self, frame: &lightair_host::buffers::Buffer) {
        let layer = self.layer.lock().clone();
        let presented = layer.as_ref().is_some_and(|l| l.present_buffer(frame));
        let asked = now_ms().saturating_sub(self.asked.load(Ordering::Relaxed)) <= ASKED_FOR_MS;
        if layer.is_some() && !asked {
            *self.latest.lock() = None;
            if presented {
                self.frames.fetch_add(1, Ordering::Relaxed);
            }
            return;
        }
        if let Some(read) = self.read_back(frame) {
            *self.latest.lock() = Some(Arc::new(read));
            self.frames.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// `frame`'s pixels, copied through a staging texture on its device (waits for the copy).
    fn read_back(&self, frame: &lightair_host::buffers::Buffer) -> Option<super::CpuFrame> {
        use windows::Win32::Graphics::Direct3D11::{
            D3D11_CPU_ACCESS_READ, D3D11_MAP_READ, D3D11_MAPPED_SUBRESOURCE, D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING,
        };
        use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};

        let (w, h) = frame.size();
        let mut staging = self.staging.lock();
        let fits = staging.as_ref().is_some_and(|s| {
            let mut desc = D3D11_TEXTURE2D_DESC::default();
            unsafe { s.GetDesc(&mut desc) };
            let same_device = unsafe { s.GetDevice() }.is_ok_and(|d| d == *frame.device());
            (desc.Width, desc.Height) == (w, h) && same_device
        });
        if !fits {
            let desc = D3D11_TEXTURE2D_DESC {
                Width: w,
                Height: h,
                MipLevels: 1,
                ArraySize: 1,
                Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                Usage: D3D11_USAGE_STAGING,
                BindFlags: 0,
                CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
                MiscFlags: 0,
            };
            let mut made = None;
            unsafe { frame.device().CreateTexture2D(&desc, None, Some(&mut made)) }.ok()?;
            *staging = made;
        }
        let staging = staging.as_ref()?;
        let _held = frame.lock();
        let context = frame.context();
        unsafe { context.CopyResource(staging, frame.texture()) };
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        // SAFETY: a staging texture of `w`×`h` BGRA, mapped for reading and unmapped below.
        unsafe { context.Map(staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped)) }.ok()?;
        let (w, h) = (w as usize, h as usize);
        let mut bgra = vec![0u8; w * h * 4];
        for row in 0..h {
            let src = unsafe {
                std::slice::from_raw_parts((mapped.pData as *const u8).add(row * mapped.RowPitch as usize), w * 4)
            };
            bgra[row * w * 4..(row + 1) * w * 4].copy_from_slice(src);
        }
        unsafe { context.Unmap(staging, 0) };
        Some(super::CpuFrame { width: w, height: h, bgra })
    }
}
