//! The Effect Stack's trackers: the face tracker and segmentation built alone from the module's
//! sources, called through its C API (`face-tracker.h`). macOS: `liblightair-tracker.dylib` on
//! Core ML and Vision, from the app's Frameworks (a bundle) or the module's build folder
//! (development), with the compiled models beside it (`tracker-models`). Windows:
//! `lightair-tracker.dll` on ONNX Runtime with DirectML (`onnxruntime.dll` and `DirectML.dll`
//! beside it), beside the app or in the module's build folder, the ONNX models in
//! `tracker-models`; it runs on the graphics adapter Lightair's pictures are on.
//! Without the library the looks have no tracker and say so ([`FaceTracker::available`]).

use std::ffi::{CString, c_char, c_void};
use std::path::{Path, PathBuf};
use std::ptr::NonNull;
use std::sync::OnceLock;

/// Points in the faces given to the looks: the face landmark model's 478 (0–467 the mesh,
/// 468–477 the irises, each iris's centre first).
pub const FACE_POINTS: usize = 478;

/// The tracker's regions (`ft_region`).
pub const FT_LEFT_PUPIL: usize = 10;
pub const FT_RIGHT_PUPIL: usize = 11;
pub const FT_MESH: usize = 12;
const FT_REGION_COUNT: usize = 13;
const FT_MAX_POINTS: usize = 720;

/// `ft_face` (face-tracker.h).
#[repr(C)]
pub struct FtFace {
    pub valid: bool,
    pub present: bool,
    pub frame_id: u64,
    pub timestamp_ns: u64,
    pub region_start: [i32; FT_REGION_COUNT],
    pub region_count: [i32; FT_REGION_COUNT],
    pub num_points: i32,
    pub pts: [[f32; 2]; FT_MAX_POINTS],
    pub skin_valid: bool,
    pub skin_cbcr: [f32; 2],
}

type Tracker = *mut c_void;

struct Lib {
    _lib: libloading::Library,
    models: CString,
    create: unsafe extern "C" fn(*const c_char) -> Tracker,
    destroy: unsafe extern "C" fn(Tracker),
    submit_bgra: unsafe extern "C" fn(Tracker, *const u8, u32, u32, u32, u64, u64) -> bool,
    set_segmentation: unsafe extern "C" fn(Tracker, bool),
    set_segmentation_fast: unsafe extern "C" fn(Tracker, bool),
    set_segmentation_every_frame: unsafe extern "C" fn(Tracker, bool),
    set_person: unsafe extern "C" fn(Tracker, bool, i32),
    get_raw: unsafe extern "C" fn(Tracker, *mut u64, *mut FtFace, *mut bool, *mut u64, *mut u32, *mut u32) -> bool,
    copy_mask: unsafe extern "C" fn(Tracker, bool, *mut u64, *mut u8, u64, *mut u32, *mut u32, *mut u64) -> bool,
    get_at: unsafe extern "C" fn(Tracker, u64, *mut FtFace) -> bool,
    body_create: unsafe extern "C" fn(*const c_char) -> Tracker,
    body_destroy: unsafe extern "C" fn(Tracker),
    body_submit_bgra: unsafe extern "C" fn(Tracker, *const u8, u32, u32, u32, u64) -> bool,
    body_get: unsafe extern "C" fn(Tracker, *mut c_void) -> bool,
}

/// The library's file name here.
#[cfg(target_os = "macos")]
const LIBRARY: &str = "liblightair-tracker.dylib";
#[cfg(windows)]
const LIBRARY: &str = "lightair-tracker.dll";

// SAFETY: the library's functions may be called from any thread (they lock what they share).
unsafe impl Send for Lib {}
unsafe impl Sync for Lib {}

/// Where the library may be: beside the app (`Contents/Frameworks`), or the module's build folder
/// next to the running binary's `target` folder.
fn candidates() -> Vec<(PathBuf, PathBuf)> {
    let mut out = Vec::new();
    if let Ok(path) = std::env::var("LIGHTAIR_TRACKER") {
        let lib = PathBuf::from(path);
        let models = lib.parent().map(|p| p.join("tracker-models")).unwrap_or_default();
        out.push((lib, models));
    }
    if let Some(exe_dir) = std::env::current_exe().ok().and_then(|e| e.parent().map(Path::to_path_buf)) {
        // a bundle: Contents/MacOS/<app> → Contents/Frameworks, Contents/Resources
        #[cfg(target_os = "macos")]
        out.push((exe_dir.join("../Frameworks").join(LIBRARY), exe_dir.join("../Resources/tracker-models")));
        // Windows: beside the app
        #[cfg(windows)]
        out.push((exe_dir.join(LIBRARY), exe_dir.join("tracker-models")));
        // development: target/<profile>/<app> (or target/<profile>/deps/<test>)
        for up in ["..", "../.."] {
            let built = exe_dir.join(up).join("modules/lightair-effect-stack");
            out.push((built.join(LIBRARY), built.join("tracker-models")));
        }
    }
    out
}

fn lib() -> Option<&'static Lib> {
    static LIB: OnceLock<Option<Lib>> = OnceLock::new();
    LIB.get_or_init(|| {
        for (path, models) in candidates().into_iter().filter(|(p, _)| p.exists()) {
            // SAFETY: the library runs nothing on load that needs anything from Lightair.
            let Ok(l) = (unsafe { libloading::Library::new(&path) }) else { continue };
            let models = CString::new(models.to_string_lossy().into_owned()).unwrap_or_default();
            // SAFETY: the symbols have the types face-tracker.h gives them.
            let lib = unsafe {
                macro_rules! sym {
                    ($name:literal) => {
                        match l.get($name) {
                            Ok(s) => *s,
                            Err(_) => continue,
                        }
                    };
                }
                Lib {
                    models,
                    create: sym!(b"face_tracker_create\0"),
                    destroy: sym!(b"face_tracker_destroy\0"),
                    submit_bgra: sym!(b"face_tracker_submit_bgra\0"),
                    set_segmentation: sym!(b"face_tracker_set_segmentation\0"),
                    set_segmentation_fast: sym!(b"face_tracker_set_segmentation_fast\0"),
                    set_segmentation_every_frame: sym!(b"face_tracker_set_segmentation_every_frame\0"),
                    set_person: sym!(b"face_tracker_set_person\0"),
                    get_raw: sym!(b"face_tracker_get_raw\0"),
                    copy_mask: sym!(b"face_tracker_copy_mask\0"),
                    get_at: sym!(b"face_tracker_get_at\0"),
                    body_create: sym!(b"body_tracker_create\0"),
                    body_destroy: sym!(b"body_tracker_destroy\0"),
                    body_submit_bgra: sym!(b"body_tracker_submit_bgra\0"),
                    body_get: sym!(b"body_tracker_get\0"),
                    _lib: l,
                }
            };
            #[cfg(windows)]
            windows_setup(&folder_of(&path));
            tracing::info!(path = %path.display(), "trackers loaded");
            return Some(lib);
        }
        None
    })
    .as_ref()
}

/// The folder the library is in.
#[cfg(windows)]
fn folder_of(library: &Path) -> PathBuf {
    library.parent().map(Path::to_path_buf).unwrap_or_default()
}

/// Windows, before any tracker is made: ONNX Runtime from beside the library (never another
/// copy on the system), and DirectML on the adapter Lightair's pictures are on (index 0 is the
/// integrated GPU on a laptop).
#[cfg(windows)]
fn windows_setup(folder: &Path) {
    let Ok(l) = (unsafe { libloading::Library::new(folder.join(LIBRARY)) }) else { return };
    // SAFETY: the symbols have the types face-tracker.h gives them; the library stays loaded
    // (the copy `lib` keeps), so this handle can go.
    unsafe {
        type SetPath = unsafe extern "C" fn(*const c_char);
        type SetDevice = unsafe extern "C" fn(*mut c_void);
        if let Ok(set) = l.get::<SetPath>(b"face_tracker_set_runtime_library\0") {
            let runtime = CString::new(folder.join("onnxruntime.dll").to_string_lossy().into_owned()).unwrap_or_default();
            set(runtime.as_ptr());
        }
        if let Ok(set) = l.get::<SetDevice>(b"face_tracker_set_render_device\0")
            && let Some(device) = crate::buffers::device()
        {
            use windows::core::Interface;
            set(device.as_raw());
        }
    }
}

/// The face tracker and segmentation, for a host that tracks and smooths their results itself
/// (Lightair's compositor drawing the camera looks, the avatars). One per camera; it analyses
/// BGRA frames on threads of its own and keeps the newest results.
pub struct FaceTracker {
    lib: &'static Lib,
    tracker: NonNull<c_void>,
}

// SAFETY: the tracker's calls lock what they share inside the library; any thread may call them.
unsafe impl Send for FaceTracker {}

/// One analysed frame's landmarks, before smoothing.
pub struct RawFace {
    /// The time it was submitted with.
    pub time_ns: i64,
    /// A face was found: `points` holds its [`FACE_POINTS`] points, normalised 0–1, y down.
    pub found: bool,
    pub points: Vec<[f32; 2]>,
    /// The analysed frame's size.
    pub size: (u32, u32),
}

/// A mask the tracker made, before temporal smoothing (the multiclass one) or as it keeps it
/// (the person one).
pub struct RawMask {
    pub time_ns: i64,
    pub bytes: Vec<u8>,
    pub size: (u32, u32),
}

impl FaceTracker {
    /// Whether the trackers' library is there.
    pub fn available() -> bool {
        lib().is_some()
    }

    /// A new tracker (its models load in the background; frames given meanwhile are dropped).
    pub fn new() -> Option<FaceTracker> {
        let lib = lib()?;
        // SAFETY: the models path is a valid C string for the call.
        let t = unsafe { (lib.create)(lib.models.as_ptr()) };
        Some(FaceTracker { lib, tracker: NonNull::new(t)? })
    }

    /// Which analyses run: the segmentation (multiclass, at about 30 a second when `fast`, every
    /// frame given with `every_frame`, about 15 a second otherwise) and with `person` the finer
    /// person mask at its quality (0 fast, 1 balanced, 2 accurate).
    pub fn segmentation(&self, enabled: bool, fast: bool, every_frame: bool, person: Option<u32>) {
        let t = self.tracker.as_ptr();
        // SAFETY: `t` is this tracker's, alive until dropped.
        unsafe {
            (self.lib.set_segmentation)(t, enabled);
            (self.lib.set_segmentation_fast)(t, fast);
            (self.lib.set_segmentation_every_frame)(t, every_frame);
            (self.lib.set_person)(t, person.is_some(), person.unwrap_or(1).min(2) as i32);
        }
    }

    /// Gives a BGRA frame to analyse (copied before this returns); false when the tracker is
    /// still busy with an earlier one (the frame is dropped).
    ///
    /// # Safety
    /// `data` must hold `height` rows of `linesize` bytes, each at least `width` × 4.
    pub unsafe fn submit(&self, data: *const u8, linesize: usize, width: u32, height: u32, id: u64, time_ns: i64) -> bool {
        // SAFETY: as the caller says; the tracker copies the frame before returning.
        unsafe {
            (self.lib.submit_bgra)(self.tracker.as_ptr(), data, linesize as u32, width, height, id, time_ns.max(0) as u64)
        }
    }

    /// The newest analysed frame's landmarks, when one newer than `serial` is there (`serial`
    /// then becomes its count), in the landmark model's order: the mesh's 468 points, then each
    /// iris as its centre.
    pub fn raw_face(&self, serial: &mut u64) -> Option<RawFace> {
        // SAFETY: plain old data, filled in by the tracker
        let mut face: Box<FtFace> = Box::new(unsafe { std::mem::zeroed() });
        let (mut found, mut ts, mut w, mut h) = (false, 0u64, 0u32, 0u32);
        let mut s = *serial;
        // SAFETY: every out pointer is a valid place for its value.
        let ok = unsafe {
            (self.lib.get_raw)(self.tracker.as_ptr(), &mut s, &mut *face, &mut found, &mut ts, &mut w, &mut h)
        };
        if !ok {
            return None;
        }
        *serial = s;
        let points = if found { landmark_order(&face) } else { None };
        Some(RawFace { time_ns: ts as i64, found: points.is_some(), points: points.unwrap_or_default(), size: (w, h) })
    }

    /// The newest multiclass mask (RGBA: R skin, G face skin, B hair, A person) before temporal
    /// smoothing, or with `person` the person mask (one byte a pixel), when one newer than
    /// `serial` is there.
    pub fn raw_mask(&self, person: bool, serial: &mut u64) -> Option<RawMask> {
        let channels = if person { 1 } else { 4 };
        let mut bytes: Vec<u8> = Vec::new();
        for _ in 0..3 {
            let mut s = *serial;
            let (mut w, mut h, mut ts) = (0u32, 0u32, 0u64);
            let (ptr, capacity) = if bytes.is_empty() { (std::ptr::null_mut(), 0) } else { (bytes.as_mut_ptr(), bytes.len()) };
            // SAFETY: `ptr` holds `capacity` bytes (or is null with none); the outs are valid.
            let ok = unsafe {
                (self.lib.copy_mask)(self.tracker.as_ptr(), person, &mut s, ptr, capacity as u64, &mut w, &mut h, &mut ts)
            };
            let needed = w as usize * h as usize * channels;
            if ok {
                *serial = s;
                bytes.truncate(needed);
                return Some(RawMask { time_ns: ts as i64, bytes, size: (w, h) });
            }
            if needed == 0 || needed <= bytes.len() {
                // nothing newer
                return None;
            }
            // a newer mask: asked again with room for it (it may have grown meanwhile)
            bytes.resize(needed, 0);
        }
        None
    }
}

impl Drop for FaceTracker {
    fn drop(&mut self) {
        // SAFETY: made by `new`, destroyed once.
        unsafe { (self.lib.destroy)(self.tracker.as_ptr()) };
    }
}

/// The tracker's face (its region layout from the landmark model: the mesh in order, each pupil
/// one point) as the landmark model's 478 points; `None` without the mesh (a tracker that gives
/// only a constellation of points).
pub fn landmark_order(face: &FtFace) -> Option<Vec<[f32; 2]>> {
    let region = |r: usize| {
        let (s, n) = (face.region_start[r].max(0) as usize, face.region_count[r].max(0) as usize);
        face.pts.get(s..s + n).unwrap_or(&[])
    };
    let mesh = region(FT_MESH);
    let (left, right) = (region(FT_LEFT_PUPIL), region(FT_RIGHT_PUPIL));
    if mesh.len() < 468 || left.is_empty() || right.is_empty() {
        return None;
    }
    let mut points = Vec::with_capacity(FACE_POINTS);
    points.extend_from_slice(&mesh[..468]);
    // 468–472 the subject's right iris (the picture's left), 473–477 the left
    points.extend(std::iter::repeat_n(right[0], 5));
    points.extend(std::iter::repeat_n(left[0], 5));
    Some(points)
}

/// The trackers' C API as it is, for hosts that keep the trackers themselves (an avatar source,
/// Auto-frame): each tracker an opaque pointer, null when the library isn't there.
pub mod raw {
    use super::{FtFace, lib};
    use std::ffi::c_void;

    /// A new face tracker (null without the library).
    pub fn face_new() -> *mut c_void {
        // SAFETY: the models path is a valid C string for the call.
        lib().map_or(std::ptr::null_mut(), |l| unsafe { (l.create)(l.models.as_ptr()) })
    }

    /// # Safety
    /// `t` from [`face_new`], freed once.
    pub unsafe fn face_free(t: *mut c_void) {
        if let Some(l) = lib().filter(|_| !t.is_null()) {
            unsafe { (l.destroy)(t) };
        }
    }

    /// A BGRA frame to analyse (copied before this returns); false when busy (dropped).
    ///
    /// # Safety
    /// `t` a live face tracker; `data` holds `height` rows of `linesize` bytes.
    pub unsafe fn face_submit(t: *mut c_void, data: *const u8, linesize: u32, width: u32, height: u32, frame_id: u64, time_ns: u64) -> bool {
        lib().filter(|_| !t.is_null()).is_some_and(|l| unsafe { (l.submit_bgra)(t, data, linesize, width, height, frame_id, time_ns) })
    }

    /// The landmarks for frame `frame_id` (exact, or interpolated between neighbours).
    ///
    /// # Safety
    /// `t` a live face tracker.
    pub unsafe fn face_get(t: *mut c_void, frame_id: u64, out: &mut FtFace) -> bool {
        lib().filter(|_| !t.is_null()).is_some_and(|l| unsafe { (l.get_at)(t, frame_id, out) })
    }

    /// A new body tracker (null without the library).
    pub fn body_new() -> *mut c_void {
        // SAFETY: the models path is a valid C string for the call.
        lib().map_or(std::ptr::null_mut(), |l| unsafe { (l.body_create)(l.models.as_ptr()) })
    }

    /// # Safety
    /// `t` from [`body_new`], freed once.
    pub unsafe fn body_free(t: *mut c_void) {
        if let Some(l) = lib().filter(|_| !t.is_null()) {
            unsafe { (l.body_destroy)(t) };
        }
    }

    /// # Safety
    /// `t` a live body tracker; `data` holds `height` rows of `linesize` bytes.
    pub unsafe fn body_submit(t: *mut c_void, data: *const u8, linesize: u32, width: u32, height: u32, frame_id: u64) -> bool {
        lib().filter(|_| !t.is_null()).is_some_and(|l| unsafe { (l.body_submit_bgra)(t, data, linesize, width, height, frame_id) })
    }

    /// The latest body (`out` a `bt_body`, body-tracker.h).
    ///
    /// # Safety
    /// `t` a live body tracker; `out` points to a `bt_body`.
    pub unsafe fn body_get(t: *mut c_void, out: *mut c_void) -> bool {
        lib().filter(|_| !t.is_null()).is_some_and(|l| unsafe { (l.body_get)(t, out) })
    }
}
