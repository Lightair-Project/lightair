//! NDI sources (`lightair_ndi`) received for the compositor, with `LIGHTAIR_ENGINE=own`.
//!
//! - **One receiver per source while shown.** A source a canvas's compositor shows gets a
//!   receiver of its own on a thread of its own; once no compositor shows it, the receiver stops
//!   a second later (so a scene being rebuilt or a switch's hand-over doesn't reconnect).
//! - **Pictures.** The receiver asks NDI for UYVY when the sender's picture is opaque and BGRA
//!   when it carries alpha. UYVY is copied into a 4:2:2 IOSurface and converted to BGRA by
//!   VideoToolbox; BGRA is copied with its colours multiplied by alpha in the same pass
//!   (Accelerate's vImage). Each picture goes into a buffer the compositor isn't holding.
//! - **Sound** isn't taken here: the receiver drops it. The libobs NDI source keeps receiving the
//!   source, sound and picture, and carries the sound into the mix as before. That means a shown
//!   NDI source is received twice while the switch is on, until the libobs source asks NDI for
//!   its sound only (NDI's "audio only" bandwidth) whenever this provider draws the picture.
//! - **NDI's runtime** is loaded at run time from where it's installed (the same places the
//!   libobs source looks), never shipped. Without it this provider handles nothing.

use std::collections::HashMap;
use std::ffi::{CString, c_char, c_void};
use std::ptr::NonNull;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use lightair_model::{Id, Source};
use lightair_obs::bridge;
use objc2_core_foundation::CFRetained;
use objc2_core_video::{
    CVAttachmentMode, CVPixelBuffer, CVPixelBufferGetBaseAddress, CVPixelBufferGetBytesPerRow, CVPixelBufferGetHeight,
    CVPixelBufferGetWidth, CVPixelBufferLockBaseAddress, CVPixelBufferLockFlags, CVPixelBufferUnlockBaseAddress,
    kCVImageBufferYCbCrMatrix_ITU_R_601_4, kCVImageBufferYCbCrMatrix_ITU_R_709_2, kCVImageBufferYCbCrMatrixKey,
    kCVPixelFormatType_32BGRA, kCVPixelFormatType_422YpCbCr8,
};
use objc2_video_toolbox::VTPixelTransferSession;
use parking_lot::Mutex;

use crate::providers::{Frame, Provider};

/// How long a receiver keeps running once no compositor shows its source.
const LINGER: Duration = Duration::from_secs(1);
/// How long one wait for a frame may take: the receiver looks at whether it's still wanted at
/// least this often.
const CAPTURE_TIMEOUT_MS: u32 = 100;

// ---------------------------------------------------------------- NDI's runtime (receiving)

#[repr(C)]
#[derive(Clone, Copy)]
struct NdiSource {
    p_ndi_name: *const c_char,
    p_url_address: *const c_char,
}

#[repr(C)]
struct RecvCreate {
    source_to_connect_to: NdiSource,
    color_format: i32,
    bandwidth: i32,
    allow_video_fields: bool,
    p_ndi_recv_name: *const c_char,
}

#[repr(C)]
struct VideoFrame {
    xres: i32,
    yres: i32,
    four_cc: u32,
    frame_rate_n: i32,
    frame_rate_d: i32,
    picture_aspect_ratio: f32,
    frame_format_type: i32,
    timecode: i64,
    p_data: *mut u8,
    line_stride_in_bytes: i32,
    p_metadata: *const c_char,
    timestamp: i64,
}

/// `NDIlib_frame_type_e`.
const FRAME_VIDEO: i32 = 1;
const FRAME_ERROR: i32 = 4;
/// `NDIlib_recv_color_format_UYVY_BGRA`: opaque pictures as UYVY, pictures with alpha as BGRA.
const COLOR_UYVY_BGRA: i32 = 1;
const BANDWIDTH_HIGHEST: i32 = 100;
const FOURCC_UYVY: u32 = u32::from_le_bytes(*b"UYVY");
const FOURCC_BGRA: u32 = u32::from_le_bytes(*b"BGRA");
const FOURCC_BGRX: u32 = u32::from_le_bytes(*b"BGRX");

type Ptr = *mut c_void;

struct Lib {
    _lib: libloading::Library,
    recv_create_v3: unsafe extern "C" fn(*const RecvCreate) -> Ptr,
    recv_destroy: unsafe extern "C" fn(Ptr),
    recv_capture_v2: unsafe extern "C" fn(Ptr, *mut VideoFrame, *mut c_void, *mut c_void, u32) -> i32,
    recv_free_video_v2: unsafe extern "C" fn(Ptr, *const VideoFrame),
}

// SAFETY: NDI's functions may be called from any thread; the library handle is only kept.
unsafe impl Send for Lib {}
unsafe impl Sync for Lib {}

/// Where NDI's runtime may be installed, in the order the libobs NDI source looks.
fn candidates() -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    for var in ["NDI_RUNTIME_DIR_V6", "NDI_RUNTIME_DIR_V5"] {
        if let Ok(dir) = std::env::var(var) {
            out.push(std::path::Path::new(&dir).join("libndi.dylib"));
        }
    }
    for p in [
        "/usr/local/lib/libndi.dylib",
        "/Library/NDI SDK for Apple/lib/macOS/libndi.dylib",
        "/Library/Application Support/NDI/lib/libndi.dylib",
        "/Applications/NDI Video Monitor.app/Contents/Frameworks/libndi_advanced.dylib",
    ] {
        out.push(p.into());
    }
    out
}

/// NDI's runtime, loaded the first time an NDI source is shown (by then the libobs source has
/// usually loaded the same library, so this only takes another reference to it).
fn runtime() -> Option<&'static Lib> {
    static LIB: OnceLock<Option<Lib>> = OnceLock::new();
    LIB.get_or_init(|| {
        for path in candidates().into_iter().filter(|p| p.exists()) {
            // SAFETY: NDI's runtime runs no code on load that needs anything from Lightair.
            let Ok(l) = (unsafe { libloading::Library::new(&path) }) else { continue };
            // SAFETY: the symbols have the types NDI documents for them.
            let lib = unsafe {
                macro_rules! sym {
                    ($name:literal) => {
                        match l.get($name) {
                            Ok(s) => *s,
                            Err(_) => continue,
                        }
                    };
                }
                let initialize: unsafe extern "C" fn() -> bool = sym!(b"NDIlib_initialize\0");
                if !initialize() {
                    continue;
                }
                Lib {
                    recv_create_v3: sym!(b"NDIlib_recv_create_v3\0"),
                    recv_destroy: sym!(b"NDIlib_recv_destroy\0"),
                    recv_capture_v2: sym!(b"NDIlib_recv_capture_v2\0"),
                    recv_free_video_v2: sym!(b"NDIlib_recv_free_video_v2\0"),
                    _lib: l,
                }
            };
            return Some(lib);
        }
        None
    })
    .as_ref()
}

// ---------------------------------------------------------------- the provider

/// Receives the NDI sources the compositors show.
#[derive(Default)]
pub(crate) struct Ndi {
    state: Arc<Mutex<State>>,
}

#[derive(Default)]
struct State {
    /// What each canvas's compositor shows: source → NDI name.
    wanted: HashMap<Id, HashMap<Id, String>>,
    receivers: HashMap<Id, Entry>,
}

struct Entry {
    receiver: Arc<Receiver>,
    /// When the receiver stops unless a compositor shows its source again (`None`: shown).
    stop_at: Option<Instant>,
}

struct Receiver {
    name: String,
    /// The newest picture and its number.
    latest: Mutex<Option<Latest>>,
}

/// The NDI name a source receives (its `ndi_name` setting).
fn ndi_name(source: &Source) -> Option<&str> {
    source.settings.get("ndi_name").and_then(|v| v.as_str()).filter(|n| !n.is_empty())
}

impl Provider for Ndi {
    fn handles(&self, source: &Source) -> bool {
        source.kind == lightair_obs::ndi::KIND && ndi_name(source).is_some() && runtime().is_some()
    }

    fn want(&self, canvas: Id, shown: &[&Source]) {
        let mut st = self.state.lock();
        let names: HashMap<Id, String> = shown.iter().filter_map(|s| Some((s.id, ndi_name(s)?.to_owned()))).collect();
        if names.is_empty() {
            st.wanted.remove(&canvas);
        } else {
            st.wanted.insert(canvas, names);
        }
        let all: HashMap<Id, String> =
            st.wanted.values().flat_map(|m| m.iter().map(|(id, n)| (*id, n.clone()))).collect();
        let now = Instant::now();
        // not shown any more: stops after a moment (the receiver's thread lets itself go)
        for (id, entry) in st.receivers.iter_mut() {
            if !all.contains_key(id) && entry.stop_at.is_none() {
                entry.stop_at = Some(now + LINGER);
            }
        }
        for (id, name) in all {
            match st.receivers.get_mut(&id) {
                Some(entry) if entry.receiver.name == name => entry.stop_at = None,
                // new, or another NDI source now: a new receiver (the old one sees it's replaced)
                _ => {
                    let Some(lib) = runtime() else { return };
                    let receiver = Arc::new(Receiver { name, latest: Mutex::new(None) });
                    let (state, me) = (self.state.clone(), receiver.clone());
                    let spawned = std::thread::Builder::new()
                        .name("lightair-ndi-provider".into())
                        .spawn(move || receive(lib, state, id, me));
                    if spawned.is_ok() {
                        st.receivers.insert(id, Entry { receiver, stop_at: None });
                    }
                }
            }
        }
    }

    fn frame(&self, source: Id) -> Option<Frame> {
        let receiver = self.state.lock().receivers.get(&source)?.receiver.clone();
        receiver.latest.lock().as_ref().map(Latest::frame)
    }
}

/// Whether `me` should keep receiving `id`: it's still the source's receiver and either shown
/// or within its moment after. Lets go of the entry when its moment is over.
fn still_wanted(state: &Mutex<State>, id: Id, me: &Arc<Receiver>) -> bool {
    let mut st = state.lock();
    let Some(entry) = st.receivers.get(&id) else { return false };
    if !Arc::ptr_eq(&entry.receiver, me) {
        return false;
    }
    if entry.stop_at.is_some_and(|t| Instant::now() >= t) {
        st.receivers.remove(&id);
        return false;
    }
    true
}

/// A receiver's thread: connects to the NDI source `me.name` and turns each picture into a BGRA
/// IOSurface until it isn't wanted any more.
fn receive(lib: &'static Lib, state: Arc<Mutex<State>>, id: Id, me: Arc<Receiver>) {
    let name = CString::new(me.name.as_str()).unwrap_or_default();
    let receiver_name = CString::new("Lightair").unwrap_or_default();
    let create = RecvCreate {
        source_to_connect_to: NdiSource { p_ndi_name: name.as_ptr(), p_url_address: std::ptr::null() },
        color_format: COLOR_UYVY_BGRA,
        bandwidth: BANDWIDTH_HIGHEST,
        allow_video_fields: false,
        p_ndi_recv_name: receiver_name.as_ptr(),
    };
    // SAFETY: `create` and the strings it points to outlive the call.
    let recv = unsafe { (lib.recv_create_v3)(&create) };
    if recv.is_null() {
        tracing::warn!(source = %me.name, "NDI provider: couldn't receive");
        // keep the entry until it's let go, so it isn't tried again every frame
        while still_wanted(&state, id, &me) {
            std::thread::sleep(Duration::from_millis(CAPTURE_TIMEOUT_MS as u64));
        }
        return;
    }
    let mut converter = Converter::new();
    let mut serial = 0u64;
    while still_wanted(&state, id, &me) {
        // SAFETY: a zeroed frame is what NDI expects to fill in.
        let mut video: VideoFrame = unsafe { std::mem::zeroed() };
        // sound and metadata aren't asked for (null): NDI drops them for this receiver
        // SAFETY: `recv` is this thread's receiver; `video` is a valid place for a frame.
        let kind = unsafe {
            (lib.recv_capture_v2)(recv, &mut video, std::ptr::null_mut(), std::ptr::null_mut(), CAPTURE_TIMEOUT_MS)
        };
        match kind {
            FRAME_VIDEO => {
                let picture = converter.convert(&video);
                // SAFETY: the frame came from this receiver and is given back once.
                unsafe { (lib.recv_free_video_v2)(recv, &video) };
                if let Some(buffer) = picture {
                    serial += 1;
                    *me.latest.lock() = Some(Latest { buffer, serial });
                }
            }
            // the connection was lost: NDI reconnects by itself; don't spin meanwhile
            FRAME_ERROR => std::thread::sleep(Duration::from_millis(CAPTURE_TIMEOUT_MS as u64)),
            _ => {}
        }
    }
    // SAFETY: made above, used only on this thread, destroyed once.
    unsafe { (lib.recv_destroy)(recv) };
}

// ---------------------------------------------------------------- pictures

/// Turns NDI's frames into BGRA IOSurfaces with premultiplied colours.
struct Converter {
    session: Option<CFRetained<VTPixelTransferSession>>,
    /// The 4:2:2 IOSurface UYVY frames are copied into before VideoToolbox converts them
    /// (reused: the conversion is done when the transfer returns).
    staging: Option<bridge::Buffer>,
    pool: Pool,
    warned: bool,
}

impl Converter {
    fn new() -> Converter {
        let mut session: *mut VTPixelTransferSession = std::ptr::null_mut();
        // SAFETY: `session` is a valid place for the new session.
        let rc = unsafe { VTPixelTransferSession::create(None, NonNull::from(&mut session)) };
        let session = (rc == 0 && !session.is_null()).then(|| {
            // SAFETY: the session was just made, one reference ours.
            unsafe { CFRetained::from_raw(NonNull::new_unchecked(session)) }
        });
        if session.is_none() {
            tracing::warn!(rc, "NDI provider: no VideoToolbox session; opaque NDI pictures stay libobs's");
        }
        Converter { session, staging: None, pool: Pool::default(), warned: false }
    }

    fn convert(&mut self, video: &VideoFrame) -> Option<bridge::Buffer> {
        let (w, h, stride) = (video.xres, video.yres, video.line_stride_in_bytes);
        if video.p_data.is_null() || w <= 0 || h <= 0 || stride <= 0 {
            return None;
        }
        let (w, h, stride) = (w as u32, h as u32, stride as usize);
        match video.four_cc {
            FOURCC_UYVY => self.uyvy(video.p_data, stride, w, h),
            FOURCC_BGRA | FOURCC_BGRX => {
                let alpha = if video.four_cc == FOURCC_BGRA { Alpha::Straight } else { Alpha::Opaque };
                let out = self.pool.take(w, h)?;
                // SAFETY: NDI's frame holds `h` rows of `stride` bytes, each with `w` BGRA pixels.
                unsafe { copy_bgra(video.p_data, stride, w, h, &out, alpha) }.then_some(out)
            }
            other => {
                if !std::mem::replace(&mut self.warned, true) {
                    tracing::warn!(four_cc = other, "NDI provider: a picture format it doesn't take");
                }
                None
            }
        }
    }

    /// A UYVY frame, converted to BGRA by VideoToolbox.
    fn uyvy(&mut self, data: *const u8, stride: usize, w: u32, h: u32) -> Option<bridge::Buffer> {
        let session = self.session.as_ref()?;
        if w % 2 != 0 || stride < w as usize * 2 {
            return None;
        }
        let fits = |b: &bridge::Buffer| (CVPixelBufferGetWidth(b) as u32, CVPixelBufferGetHeight(b) as u32) == (w, h);
        if !self.staging.as_ref().is_some_and(fits) {
            let staging = crate::mac::pixel_buffer(w, h, kCVPixelFormatType_422YpCbCr8)?;
            // NDI's YUV is BT.709 from HD up, BT.601 below (video range, as 4:2:2 here is)
            // SAFETY: CoreVideo's constants; the matrix attachment takes one of them.
            unsafe {
                let matrix = if h >= 720 {
                    kCVImageBufferYCbCrMatrix_ITU_R_709_2
                } else {
                    kCVImageBufferYCbCrMatrix_ITU_R_601_4
                };
                staging.set_attachment(
                    kCVImageBufferYCbCrMatrixKey,
                    matrix.as_ref(),
                    CVAttachmentMode::ShouldNotPropagate,
                );
            }
            self.staging = Some(staging);
        }
        let staging = self.staging.as_ref()?;
        // SAFETY: the staging buffer is locked while written, within its rows; NDI's frame
        // holds `h` rows of `stride` bytes, each with `w * 2` bytes of UYVY.
        unsafe {
            if CVPixelBufferLockBaseAddress(staging, CVPixelBufferLockFlags(0)) != 0 {
                return None;
            }
            let base = CVPixelBufferGetBaseAddress(staging) as *mut u8;
            let row = CVPixelBufferGetBytesPerRow(staging);
            let ok = !base.is_null() && row >= w as usize * 2;
            if ok {
                for y in 0..h as usize {
                    std::ptr::copy_nonoverlapping(data.add(y * stride), base.add(y * row), w as usize * 2);
                }
            }
            CVPixelBufferUnlockBaseAddress(staging, CVPixelBufferLockFlags(0));
            if !ok {
                return None;
            }
        }
        let out = self.pool.take(w, h)?;
        // SAFETY: both are valid IOSurface-backed pixel buffers of the same size.
        (unsafe { session.transfer_image(staging, &out) } == 0).then_some(out)
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
#[derive(Default)]
pub(crate) struct Pool {
    kept: Vec<bridge::Buffer>,
}

// SAFETY: pixel buffers are reference-counted and usable from any thread.
unsafe impl Send for Pool {}

/// Buffers a pool keeps for reuse.
const KEPT: usize = 8;

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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Alpha {
    /// Colours not multiplied by alpha: they're multiplied while copied.
    Straight,
    /// Drawn opaque whatever its alpha: alpha is set to 1 while copied.
    Opaque,
}

/// `vImage_Buffer`.
#[repr(C)]
struct VImageBuffer {
    data: *mut c_void,
    height: usize,
    width: usize,
    row_bytes: usize,
}

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
