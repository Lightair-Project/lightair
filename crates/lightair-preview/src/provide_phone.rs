//! The phone camera (`lightair_phone_camera`) as a picture provider, with `LIGHTAIR_ENGINE=own`.
//!
//! - **The phone's own decoder.** A phone's stream is received and decoded as before
//!   (`lightair_obs::phone_camera`: VideoToolbox, the moment each frame arrives); with the switch
//!   on the decoder makes BGRA IOSurfaces. While a canvas's compositor shows the source, this
//!   provider takes the feed's pictures ([`phone_camera::Feed::take`]): each one comes here instead
//!   of being copied into libobs, and libobs's wrapper draws the same surfaces where libobs still
//!   shows the source (thumbnails, Studio mode). Once no compositor shows it, the pictures go
//!   back to libobs a second later.
//! - **Upright.** The phone encodes as its sensor sees and says which way is up: a picture that
//!   needs turning is turned by VideoToolbox into a buffer of a pool (one the compositor isn't
//!   holding), on the decoder's thread.
//! - **Placeholder.** While no pictures come (no phone, or it dropped) this provider has no
//!   frame, so libobs's wrapper, with its line saying what it waits for, stands in.
//! - **Sound** is the phone's microphone, as before: the decoder hands it to the wrapper's
//!   private source, which passes it on to the wrapper and so into the mix.

use std::collections::{HashMap, HashSet};
use std::ptr::NonNull;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use lightair_model::{Id, Source};
use lightair_host::{bridge, phone_camera};
use objc2_core_foundation::{CFBoolean, CFDictionary, CFNumber, CFRetained, CFString, CFType, Type};
use objc2_core_video::{
    CVPixelBuffer, CVPixelBufferGetHeight, CVPixelBufferGetIOSurface, CVPixelBufferGetPixelFormatType,
    CVPixelBufferGetWidth, CVPixelBufferPool, kCVPixelBufferHeightKey, kCVPixelBufferIOSurfacePropertiesKey,
    kCVPixelBufferMetalCompatibilityKey, kCVPixelBufferPixelFormatTypeKey, kCVPixelBufferWidthKey,
    kCVPixelFormatType_32BGRA, kCVReturnSuccess,
};
use objc2_video_toolbox::{
    VTPixelRotationSession, VTSessionSetProperty, kVTPixelRotationPropertyKey_Rotation, kVTRotation_180,
    kVTRotation_CCW90, kVTRotation_CW90,
};
use parking_lot::Mutex;

use crate::providers::{Frame, Provider};

/// How long the pictures stay taken once no compositor shows the source.
const LINGER: Duration = Duration::from_secs(1);

/// Milliseconds on a clock of this module's own (for "when last wanted", kept in an atomic).
fn now_ms() -> u64 {
    static START: OnceLock<Instant> = OnceLock::new();
    START.get_or_init(Instant::now).elapsed().as_millis() as u64 + 1
}

/// A picture handed to the compositor and its serial.
struct Shown(bridge::Buffer, u64);

// SAFETY: CoreVideo's buffers are reference counted thread-safely, and a picture isn't written
// again once it's handed over.
unsafe impl Send for Shown {}

/// What a taken feed's pictures go into: the newest, upright.
struct Slot {
    newest: Mutex<Option<Shown>>,
    serial: AtomicU64,
    /// When a compositor last showed the source (`now_ms`): a decoder still handing pictures
    /// long after gives them back to libobs itself, in case no compositor runs any more.
    wanted_at: AtomicU64,
    turner: Mutex<Turner>,
}

impl Slot {
    /// A decoded picture, turned upright and kept as the newest; whether it was taken.
    fn take(&self, image: &CVPixelBuffer, rotation: u32) -> bool {
        if now_ms().saturating_sub(self.wanted_at.load(Ordering::Acquire)) > 2 * LINGER.as_millis() as u64 {
            return false;
        }
        // only what the compositor can draw: BGRA in an IOSurface
        if CVPixelBufferGetPixelFormatType(image) != kCVPixelFormatType_32BGRA
            || CVPixelBufferGetIOSurface(Some(image)).is_none()
        {
            return false;
        }
        let upright = if rotation == 0 { Some(image.retain()) } else { self.turner.lock().turn(image, rotation) };
        let Some(upright) = upright else { return false };
        let serial = self.serial.fetch_add(1, Ordering::AcqRel) + 1;
        // (the one replaced is let go outside the lock)
        let old = self.newest.lock().replace(Shown(upright, serial));
        drop(old);
        true
    }
}

/// Turns pictures by quarter turns with VideoToolbox, into buffers of a pool sized for the
/// turned picture (the pool hands out only buffers nobody holds).
struct Turner {
    session: Option<CFRetained<VTPixelRotationSession>>,
    /// The rotation the session is set to (degrees clockwise).
    set: u32,
    pool: Option<(CFRetained<CVPixelBufferPool>, (usize, usize))>,
}

// SAFETY: the session and pool are used under the slot's lock, one thread at a time.
unsafe impl Send for Turner {}

impl Turner {
    fn new() -> Turner {
        Turner { session: None, set: 0, pool: None }
    }

    fn turn(&mut self, image: &CVPixelBuffer, rotation: u32) -> Option<bridge::Buffer> {
        if self.session.is_none() {
            let mut session: *mut VTPixelRotationSession = std::ptr::null_mut();
            // SAFETY: `session` is a valid place for the new session
            let rc = unsafe { VTPixelRotationSession::create(None, NonNull::from(&mut session)) };
            if rc != 0 || session.is_null() {
                tracing::warn!(rc, "phone camera: no VideoToolbox session to turn pictures");
                return None;
            }
            // SAFETY: the session was just made, one reference ours
            self.session = Some(unsafe { CFRetained::from_raw(NonNull::new_unchecked(session)) });
            self.set = 0;
        }
        let session = self.session.as_ref()?;
        if self.set != rotation {
            // SAFETY: VideoToolbox's constant keys and values, read only
            let value = unsafe {
                match rotation {
                    90 => kVTRotation_CW90,
                    180 => kVTRotation_180,
                    _ => kVTRotation_CCW90,
                }
            };
            // SAFETY: the rotation key takes one of the rotation constants
            let rc =
                unsafe { VTSessionSetProperty(session, kVTPixelRotationPropertyKey_Rotation, Some(value.as_ref())) };
            if rc != 0 {
                return None;
            }
            self.set = rotation;
        }
        let (w, h) = (CVPixelBufferGetWidth(image), CVPixelBufferGetHeight(image));
        let size = if rotation % 180 == 90 { (h, w) } else { (w, h) };
        if self.pool.as_ref().is_none_or(|(_, s)| *s != size) {
            self.pool = pool(size).map(|p| (p, size));
        }
        let (pool, _) = self.pool.as_ref()?;
        let mut out: *mut CVPixelBuffer = std::ptr::null_mut();
        // SAFETY: `out` is a valid place for the new buffer
        let rc = unsafe { CVPixelBufferPool::create_pixel_buffer(None, pool, NonNull::from(&mut out)) };
        if rc != kCVReturnSuccess || out.is_null() {
            return None;
        }
        // SAFETY: the pool gave one reference, ours
        let turned = unsafe { CFRetained::from_raw(NonNull::new_unchecked(out)) };
        // SAFETY: both are valid BGRA buffers, the destination the source's size turned
        (unsafe { session.rotate_image(image, &turned) } == 0).then_some(turned)
    }
}

/// A pool of BGRA IOSurface-backed buffers Metal can use, `size` each.
fn pool(size: (usize, usize)) -> Option<CFRetained<CVPixelBufferPool>> {
    let empty: CFRetained<CFDictionary<CFString, CFType>> = CFDictionary::from_slices(&[], &[]);
    let (format, w, h) = (
        CFNumber::new_i32(kCVPixelFormatType_32BGRA as i32),
        CFNumber::new_i64(size.0 as i64),
        CFNumber::new_i64(size.1 as i64),
    );
    // SAFETY: CoreVideo's constant keys, read only
    let keys: [&CFString; 5] = unsafe {
        [
            kCVPixelBufferPixelFormatTypeKey,
            kCVPixelBufferWidthKey,
            kCVPixelBufferHeightKey,
            kCVPixelBufferIOSurfacePropertiesKey,
            kCVPixelBufferMetalCompatibilityKey,
        ]
    };
    let values: [&CFType; 5] = [format.as_ref(), w.as_ref(), h.as_ref(), empty.as_ref(), CFBoolean::new(true).as_ref()];
    let attrs = CFDictionary::from_slices(&keys, &values);
    let mut out: *mut CVPixelBufferPool = std::ptr::null_mut();
    // SAFETY: `out` is a valid place for the new pool; the attributes outlive the call
    let rc = unsafe { CVPixelBufferPool::create(None, None, Some(attrs.as_opaque()), NonNull::from(&mut out)) };
    if rc != kCVReturnSuccess || out.is_null() {
        tracing::warn!(rc, ?size, "phone camera: no pool for turned pictures");
        return None;
    }
    // SAFETY: the pool was just made, one reference ours
    Some(unsafe { CFRetained::from_raw(NonNull::new_unchecked(out)) })
}

/// A source whose feed this provider takes.
struct Taking {
    feed: Arc<phone_camera::Feed>,
    slot: Arc<Slot>,
    unwanted_since: Option<Instant>,
}

#[derive(Default)]
struct State {
    /// The phone cameras each canvas shows now.
    shown: HashMap<Id, HashSet<Id>>,
    taking: HashMap<Id, Taking>,
}

#[derive(Default)]
pub(crate) struct Phone {
    state: Mutex<State>,
}

impl Provider for Phone {
    fn handles(&self, source: &Source) -> bool {
        source.kind == phone_camera::KIND
    }

    fn want(&self, canvas: Id, shown: &[&Source]) {
        let mut state = self.state.lock();
        if shown.is_empty() {
            state.shown.remove(&canvas);
        } else {
            state.shown.insert(canvas, shown.iter().map(|s| s.id).collect());
        }
        let all: HashSet<Id> = state.shown.values().flatten().copied().collect();
        let now = Instant::now();
        let at = now_ms();
        for id in &all {
            let taking = state.taking.entry(*id).or_insert_with(|| {
                // the feed by the source's id (its libobs name), made here if the wrapper hasn't yet
                let feed = phone_camera::feed(&id.to_string());
                let slot = Arc::new(Slot {
                    newest: Mutex::new(None),
                    serial: AtomicU64::new(0),
                    wanted_at: AtomicU64::new(at),
                    turner: Mutex::new(Turner::new()),
                });
                let into = slot.clone();
                feed.take(Some(Arc::new(move |image, rotation| into.take(image, rotation))));
                Taking { feed, slot, unwanted_since: None }
            });
            taking.unwanted_since = None;
            taking.slot.wanted_at.store(at, Ordering::Release);
        }
        // a second after no compositor shows it, the pictures go back to libobs
        state.taking.retain(|id, t| {
            if all.contains(id) {
                return true;
            }
            let since = *t.unwanted_since.get_or_insert(now);
            if now.duration_since(since) < LINGER {
                return true;
            }
            t.feed.take(None);
            false
        });
    }

    fn frame(&self, source: Id) -> Option<Frame> {
        let slot = {
            let state = self.state.lock();
            let t = state.taking.get(&source)?;
            // no pictures coming: libobs's placeholder stands in
            if !t.feed.live() {
                return None;
            }
            t.slot.clone()
        };
        let newest = slot.newest.lock();
        let Shown(buffer, serial) = newest.as_ref()?;
        Some(Frame { buffer: buffer.clone(), serial: *serial })
    }
}
