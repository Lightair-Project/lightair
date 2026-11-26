//! NDI sources (`lightair_ndi`) for the compositor, with `LIGHTAIR_ENGINE=own`.
//!
//! - **Received once.** The libobs NDI source already receives its NDI source, picture and sound,
//!   from creation on, and carries the sound into the mix. This provider connects no receiver of
//!   its own: while a compositor shows a source it taps that receiver (`lightair_obs::ndi::tap`)
//!   and copies each picture it gets into a buffer the compositor isn't holding. libobs keeps its
//!   own picture of the source, so thumbnails, Studio mode's libobs preview and a source with an
//!   effect only libobs draws stay right.
//! - **Pictures.** NDI gives BGRA (BGRX for senders without alpha); the copy multiplies the
//!   colours by alpha, or makes them opaque, in one pass (Accelerate's vImage; on Windows in a
//!   plain loop, then uploaded into a Lightair buffer).
//! - **Letting go.** A source no compositor has shown for a second is copied no more; its tap
//!   goes the next time a compositor says what it shows.

use std::collections::{HashMap, HashSet};
#[cfg(target_os = "macos")]
use std::ffi::c_void;
use std::sync::Arc;
use std::time::{Duration, Instant};

use lightair_model::{Id, Source};
use lightair_host::{bridge, ndi};
#[cfg(target_os = "macos")]
use objc2_core_video::{
    CVPixelBuffer, CVPixelBufferGetBaseAddress, CVPixelBufferGetBytesPerRow, CVPixelBufferGetHeight,
    CVPixelBufferGetWidth, CVPixelBufferLockBaseAddress, CVPixelBufferLockFlags, CVPixelBufferUnlockBaseAddress,
    kCVPixelFormatType_32BGRA,
};
use parking_lot::Mutex;

use crate::providers::{Frame, Provider};

/// How long pictures are still copied once no compositor shows their source.
const LINGER: Duration = Duration::from_secs(1);

/// Takes the pictures of the NDI sources the compositors show.
#[derive(Default)]
pub(crate) struct Ndi {
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    /// What each canvas's compositor shows.
    wanted: HashMap<Id, HashSet<Id>>,
    /// The sources tapped, by document id.
    taken: HashMap<Id, Arc<Taken>>,
}

/// One tapped source.
struct Taken {
    /// The NDI name it receives: a new name starts afresh (no picture of the old one).
    name: String,
    /// Until when pictures are copied (pushed on while shown).
    until: Mutex<Instant>,
    latest: Mutex<Option<Latest>>,
    copier: Mutex<Copier>,
}

#[cfg(target_os = "macos")]
#[derive(Default)]
struct Copier {
    pool: Pool,
    serial: u64,
}

/// Windows: each picture uploaded into a Lightair buffer (premultiplied, or made opaque, on the
/// way into `scratch`).
#[cfg(windows)]
#[derive(Default)]
struct Copier {
    buffers: lightair_host::buffers::Copier,
    scratch: Vec<u8>,
    serial: u64,
}

impl Taken {
    /// A received picture (on the source's receiving thread): copied while wanted.
    #[cfg(windows)]
    fn take(&self, picture: &ndi::Picture) {
        if Instant::now() > *self.until.lock() {
            return;
        }
        let (w, h) = (picture.width as usize, picture.height as usize);
        if w == 0 || h == 0 || picture.stride < w * 4 || picture.data.len() < picture.stride * h {
            return;
        }
        let mut copier = self.copier.lock();
        let c = &mut *copier;
        c.scratch.clear();
        c.scratch.reserve(w * h * 4);
        for row in picture.data.chunks(picture.stride).take(h) {
            for px in row[..w * 4].as_chunks::<4>().0 {
                let a = if picture.opaque { 255 } else { px[3] };
                let mul = |v: u8| ((v as u16 * a as u16 + 127) / 255) as u8;
                if a == 255 {
                    c.scratch.extend_from_slice(&[px[0], px[1], px[2], 255]);
                } else {
                    c.scratch.extend_from_slice(&[mul(px[0]), mul(px[1]), mul(px[2]), a]);
                }
            }
        }
        if let Some(buffer) = c.buffers.from_memory(&c.scratch, w * 4, (w as u32, h as u32)) {
            c.serial += 1;
            *self.latest.lock() = Some(Latest { buffer, serial: c.serial });
        }
    }

    /// A received picture (on the source's receiving thread): copied while wanted.
    #[cfg(target_os = "macos")]
    fn take(&self, picture: &ndi::Picture) {
        if Instant::now() > *self.until.lock() {
            return;
        }
        let (w, h) = (picture.width, picture.height);
        if w == 0 || h == 0 || picture.data.len() < picture.stride * h as usize {
            return;
        }
        let alpha = if picture.opaque { Alpha::Opaque } else { Alpha::Straight };
        let mut copier = self.copier.lock();
        let Some(out) = copier.pool.take(w, h) else { return };
        // SAFETY: `data` holds `h` rows of `stride` bytes (checked above); `copy_bgra` checks
        // that each holds `w` pixels.
        if unsafe { copy_bgra(picture.data.as_ptr(), picture.stride, w, h, &out, alpha) } {
            copier.serial += 1;
            *self.latest.lock() = Some(Latest { buffer: out, serial: copier.serial });
        }
    }
}

/// The NDI name a source receives (its `ndi_name` setting).
fn ndi_name(source: &Source) -> Option<&str> {
    source.settings.get("ndi_name").and_then(|v| v.as_str()).filter(|n| !n.is_empty())
}

impl Provider for Ndi {
    fn handles(&self, source: &Source) -> bool {
        source.kind == ndi::KIND && ndi_name(source).is_some() && ndi::available()
    }

    fn want(&self, canvas: Id, shown: &[&Source]) {
        let mut st = self.state.lock();
        let names: HashMap<Id, &str> = shown.iter().filter_map(|s| Some((s.id, ndi_name(s)?))).collect();
        if names.is_empty() {
            st.wanted.remove(&canvas);
        } else {
            st.wanted.insert(canvas, names.keys().copied().collect());
        }
        let now = Instant::now();
        for (&id, &name) in &names {
            match st.taken.get(&id) {
                Some(t) if t.name == name => *t.until.lock() = now + LINGER,
                // newly shown, or receiving another NDI source now
                _ => {
                    let taken = Arc::new(Taken {
                        name: name.to_owned(),
                        until: Mutex::new(now + LINGER),
                        latest: Mutex::new(None),
                        copier: Mutex::new(Copier::default()),
                    });
                    let me = taken.clone();
                    ndi::tap(&id.to_string(), Arc::new(move |p: &ndi::Picture| me.take(p)));
                    st.taken.insert(id, taken);
                }
            }
        }
        // shown by no compositor for a moment: let go
        let all: HashSet<Id> = st.wanted.values().flatten().copied().collect();
        st.taken.retain(|id, t| {
            let keep = all.contains(id) || now <= *t.until.lock();
            if !keep {
                ndi::untap(&id.to_string());
            }
            keep
        });
    }

    fn frame(&self, source: Id) -> Option<Frame> {
        let taken = self.state.lock().taken.get(&source)?.clone();
        taken.latest.lock().as_ref().map(Latest::frame)
    }
}

// ---------------------------------------------------------------- shared with the Syphon provider

/// A provider's newest picture of a source, and its number.
pub(crate) struct Latest {
    pub(crate) buffer: bridge::Buffer,
    pub(crate) serial: u64,
}

// SAFETY: pixel buffers are reference-counted and usable from any thread; a provider never
// writes into one it has handed on.
unsafe impl Send for Latest {}
unsafe impl Sync for Latest {}

impl Latest {
    pub(crate) fn frame(&self) -> Frame {
        Frame { buffer: self.buffer.clone(), serial: self.serial }
    }
}

/// BGRA buffers a provider fills, reused once nothing but the pool holds them (the compositor
/// holds the one it draws until its next frame; the provider holds its newest).
#[cfg(target_os = "macos")]
#[derive(Default)]
pub(crate) struct Pool {
    kept: Vec<bridge::Buffer>,
}

// SAFETY: pixel buffers are reference-counted and usable from any thread.
#[cfg(target_os = "macos")]
unsafe impl Send for Pool {}

/// Buffers a pool keeps for reuse.
#[cfg(target_os = "macos")]
const KEPT: usize = 8;

#[cfg(target_os = "macos")]
impl Pool {
    /// A `w`×`h` BGRA buffer nothing else holds.
    pub(crate) fn take(&mut self, w: u32, h: u32) -> Option<bridge::Buffer> {
        self.kept.retain(|b| (CVPixelBufferGetWidth(b) as u32, CVPixelBufferGetHeight(b) as u32) == (w, h));
        if let Some(free) = self.kept.iter().find(|b| b.retain_count() == 1) {
            return Some(free.clone());
        }
        let new = crate::mac::pixel_buffer(w, h, kCVPixelFormatType_32BGRA)?;
        if self.kept.len() < KEPT {
            self.kept.push(new.clone());
        }
        Some(new)
    }
}

/// What a BGRA picture's alpha is.
#[cfg(target_os = "macos")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Alpha {
    /// Colours not multiplied by alpha: they're multiplied while copied.
    Straight,
    /// Drawn opaque whatever its alpha: alpha is set to 1 while copied.
    Opaque,
}

/// `vImage_Buffer`.
#[cfg(target_os = "macos")]
#[repr(C)]
struct VImageBuffer {
    data: *mut c_void,
    height: usize,
    width: usize,
    row_bytes: usize,
}

#[cfg(target_os = "macos")]
#[link(name = "Accelerate", kind = "framework")]
unsafe extern "C" {
    /// Multiplies the first three channels by the fourth (alpha last, as in BGRA).
    fn vImagePremultiplyData_RGBA8888(src: *const VImageBuffer, dest: *const VImageBuffer, flags: u32) -> isize;
    /// Copies `src` to `dest`, writing `scalar` into the channels `copy_mask` names (0x8 the
    /// first byte of each pixel … 0x1 the fourth).
    fn vImageOverwriteChannelsWithScalar_ARGB8888(
        scalar: u8,
        src: *const VImageBuffer,
        dest: *const VImageBuffer,
        copy_mask: u8,
        flags: u32,
    ) -> isize;
}

/// Copies a `w`×`h` BGRA picture (rows `src_row` bytes apart) into `out` (a BGRA buffer of the
/// same size) with premultiplied colours, in one pass. Whether it worked.
///
/// # Safety
///
/// `src` must hold `h` rows of `src_row` bytes, each with at least `w` BGRA pixels.
#[cfg(target_os = "macos")]
pub(crate) unsafe fn copy_bgra(
    src: *const u8,
    src_row: usize,
    w: u32,
    h: u32,
    out: &CVPixelBuffer,
    alpha: Alpha,
) -> bool {
    if src_row < w as usize * 4 {
        return false;
    }
    // SAFETY: `out` is locked while written and its rows are its own; `src` is as the caller says.
    unsafe {
        if CVPixelBufferLockBaseAddress(out, CVPixelBufferLockFlags(0)) != 0 {
            return false;
        }
        let base = CVPixelBufferGetBaseAddress(out);
        let row = CVPixelBufferGetBytesPerRow(out);
        let fits = !base.is_null()
            && row >= w as usize * 4
            && (CVPixelBufferGetWidth(out) as u32, CVPixelBufferGetHeight(out) as u32) == (w, h);
        let rc = if fits {
            let from =
                VImageBuffer { data: src as *mut c_void, height: h as usize, width: w as usize, row_bytes: src_row };
            let to = VImageBuffer { data: base, height: h as usize, width: w as usize, row_bytes: row };
            match alpha {
                Alpha::Straight => vImagePremultiplyData_RGBA8888(&from, &to, 0),
                // BGRA's alpha is each pixel's fourth byte
                Alpha::Opaque => vImageOverwriteChannelsWithScalar_ARGB8888(255, &from, &to, 0x1, 0),
            }
        } else {
            -1
        };
        CVPixelBufferUnlockBaseAddress(out, CVPixelBufferLockFlags(0));
        rc == 0
    }
}
