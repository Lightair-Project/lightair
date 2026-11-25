//! Without libobs: a preview's frames come from Lightair's own compositor (`compose`), which
//! runs on macOS only today. Here the preview keeps what it's asked to show (the canvas, a scene
//! in the program's place, the size, the rate, the window's layer) and has no frames until the
//! compositor runs on this platform (docs/plans/no-libobs.md, step 10); asking for the program
//! says so in the log, once.

use std::ffi::c_void;
use std::sync::Arc;
use std::sync::Once;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

use lightair_model::Id;
use parking_lot::Mutex;

use super::Frame;

struct Shared {
    /// The canvas previewed; `None`: the primary canvas.
    #[allow(dead_code, reason = "the compositor reads it once it runs here")]
    canvas: Option<Id>,
    /// The scene shown instead of the program, and the canvas it's laid out in.
    source: Mutex<Option<(Id, Id)>>,
    /// Requested size, packed as (width << 32 | height). 0 = follow the canvas size.
    size: AtomicU64,
    /// The most frames a second asked for; 0: the canvas's rate.
    rate: AtomicU32,
    own_size: AtomicBool,
    active: AtomicBool,
    latest: Mutex<Option<Frame>>,
    frames: AtomicU64,
    /// Windows: the swap chain frames are presented to (the window's picture).
    #[cfg(windows)]
    layer: Mutex<Option<Arc<crate::Layer>>>,
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
            source: Mutex::new(None),
            size: AtomicU64::new(0),
            rate: AtomicU32::new(0),
            own_size: AtomicBool::new(false),
            active: AtomicBool::new(true),
            latest: Mutex::new(None),
            frames: AtomicU64::new(0),
            #[cfg(windows)]
            layer: Mutex::new(None),
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
        *self.0.source.lock() = source;
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
        *self.0.source.lock()
    }

    /// Pauses or resumes rendering.
    pub fn set_active(&self, active: bool) {
        self.0.active.store(active, Ordering::Relaxed);
    }

    /// The newest finished frame, if any.
    pub fn latest_frame(&self) -> Option<Frame> {
        self.0.latest.lock().clone()
    }

    /// Frames shown since attach (diagnostics).
    pub fn frames(&self) -> u64 {
        self.0.frames.load(Ordering::Relaxed)
    }
}
