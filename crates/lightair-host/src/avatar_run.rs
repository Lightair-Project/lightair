//! An avatar source's tracking: the compositor draws and poses the model (`lightair-preview`'s
//! avatars); this runs what poses it and publishes each frame's posing by the source's id
//! (`crate::bridge::set_avatar_posing`).
//!
//! - **The camera**: the source's own capture (lightair-capture), running while something shows
//!   the avatar and a moment after (`LINGER`). Each frame goes to the trackers as it comes.
//! - **The trackers**: Lightair's tracker library (`crate::tracker::raw`): the face, and a VRM's
//!   body when its body tracking is on.
//! - **Each step** (a thread of its own, at the display's pace): the face measured, calibrated
//!   and mirrored as the settings say, the co-host's face merged in when this is its avatar, the
//!   body read, a PNGtuber's mic level and push to talk; published for the compositor.
//! - **Status** for the Inspector: the model read once in the background (its name,
//!   expressions, costumes, the anchors things can be pinned to), whether a face is followed.
//!
//! What's pinned to the avatar follows its anchors as the compositor's renderer poses it; an
//! anchor's place at rest comes from there too ([`AvatarRuntime::anchor_rest`]).
//!
//! Windows has no tracker library yet: the camera isn't opened there (the log says so once), so
//! a VRM or a flat avatar settles at rest; a PNGtuber's mic and push to talk, the model's status
//! and its anchors work as on macOS.

use std::ffi::c_void;
#[cfg(target_os = "macos")]
use std::ptr::NonNull;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use lightair_avatar::body::{BodyFrame, BodyMode, HAND_POINTS, JOINTS};
use lightair_avatar::source::{self, Settings};
use lightair_avatar::{Calibration, Landmarks, Measures};
use lightair_model::Id;
#[cfg(target_os = "macos")]
use objc2_core_foundation::CFRetained;
#[cfg(target_os = "macos")]
use objc2_core_video::{
    CVPixelBuffer, CVPixelBufferGetBaseAddress, CVPixelBufferGetBytesPerRow, CVPixelBufferGetHeight,
    CVPixelBufferGetWidth, CVPixelBufferLockBaseAddress, CVPixelBufferLockFlags, CVPixelBufferUnlockBaseAddress,
};
use parking_lot::Mutex;

use crate::avatar::Status;
use crate::tracker::{FT_LEFT_PUPIL, FT_MESH, FT_RIGHT_PUPIL, FtFace, raw};

/// How long the camera keeps running once nothing shows the avatar.
const LINGER: Duration = Duration::from_secs(2);
/// How often the posing is worked out and published.
const STEP: Duration = Duration::from_millis(16);
/// The tracking view's picture: at most this wide.
#[cfg(target_os = "macos")]
const VIEW_WIDTH: u32 = 640;

/// The body tracker's frame (body-tracker.h's `bt_body`).
#[repr(C)]
struct BtBody {
    present: bool,
    has3d: bool,
    frame_id: u64,
    j3: [[f32; 4]; JOINTS],
    j2: [[f32; 3]; JOINTS],
    hand_present: [bool; 2],
    hand: [[[f32; 3]; HAND_POINTS]; 2],
    hand3d: [[[f32; 3]; HAND_POINTS]; 2],
}

/// The trackers (null until made, or without the library).
struct Trackers {
    face: usize,
    body: usize,
}

impl Drop for Trackers {
    fn drop(&mut self) {
        // SAFETY: made by `raw::face_new`/`body_new`, freed once here
        unsafe {
            raw::face_free(self.face as *mut c_void);
            raw::body_free(self.body as *mut c_void);
        }
    }
}

/// A small BGRA picture and its size.
type View = (Arc<Vec<u8>>, (u32, u32));

/// What the capture's queue and the stepping thread share.
struct Shared {
    id: Id,
    settings: Mutex<Settings>,
    /// Something shows the avatar (on air, or held by a view).
    shown: AtomicBool,
    quit: AtomicBool,
    trackers: Mutex<Trackers>,
    /// The newest frame given to the trackers, and its size.
    frame: AtomicU64,
    size: Mutex<(u32, u32)>,
    /// With `show_tracking`: the newest picture as the trackers saw it (BGRA, small).
    view: Mutex<Option<View>>,
    /// The face measured last (for calibrating).
    measures: Mutex<Option<Measures>>,
    push_to_talk: AtomicBool,
    status: Mutex<Status>,
}

/// One avatar source's tracking.
pub struct AvatarRuntime {
    shared: Arc<Shared>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl AvatarRuntime {
    pub fn new(id: Id, settings: &serde_json::Value) -> AvatarRuntime {
        let shared = Arc::new(Shared {
            id,
            settings: Mutex::new(Settings::from_json(settings)),
            shown: AtomicBool::new(false),
            quit: AtomicBool::new(false),
            trackers: Mutex::new(Trackers { face: 0, body: 0 }),
            frame: AtomicU64::new(0),
            size: Mutex::new((0, 0)),
            view: Mutex::new(None),
            measures: Mutex::new(None),
            push_to_talk: AtomicBool::new(false),
            status: Mutex::new(Status::default()),
        });
        let s = shared.clone();
        let thread = std::thread::Builder::new().name("lightair-avatar".into()).spawn(move || run(s)).ok();
        AvatarRuntime { shared, thread }
    }

    /// New settings (a new file is read again; a new camera opened).
    pub fn set_settings(&self, settings: &serde_json::Value) {
        *self.shared.settings.lock() = Settings::from_json(settings);
    }

    /// Whether something shows the avatar (its camera runs only then, and a moment after).
    pub fn set_shown(&self, shown: bool) {
        self.shared.shown.store(shown, Ordering::Relaxed);
    }

    /// A PNGtuber's push to talk held, or let go.
    pub fn set_push_to_talk(&self, held: bool) {
        self.shared.push_to_talk.store(held, Ordering::Relaxed);
    }

    /// The face seen now as the neutral one: the calibration's JSON (`None`: no face seen).
    pub fn calibrate(&self) -> Option<String> {
        let m = (*self.shared.measures.lock())?;
        Some(source::calibration_json(&Calibration::from(&m)))
    }

    pub fn status(&self) -> Status {
        self.shared.status.lock().clone()
    }

    /// Where anchor `key` is in the avatar's picture at rest (pixels), once the compositor draws
    /// its model. `size`: as it would be at that size of the picture instead.
    pub fn anchor_rest(&self, key: &str, size: Option<(u32, u32)>) -> Option<crate::avatar_pins::Spot> {
        let settings = self.shared.settings.lock().clone();
        crate::avatar::anchor_rest(&settings, key, size)
    }
}

impl Drop for AvatarRuntime {
    fn drop(&mut self) {
        self.shared.quit.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
        crate::avatar::set_status(self.shared.id, None);
        crate::bridge::forget_avatar(self.shared.id);
    }
}

/// The source's camera while it runs (macOS).
#[cfg(target_os = "macos")]
type Camera = lightair_capture::camera::CameraCapture;
/// Windows: none is opened.
#[cfg(windows)]
type Camera = ();

/// The stepping thread: the camera kept, the model read, the posing published, until dropped.
fn run(s: Arc<Shared>) {
    let mut camera: Option<(String, Camera)> = None;
    let mut hidden_since: Option<Instant> = None;
    let mut read: Option<(String, i64)> = None;
    let mut loading: Option<std::thread::JoinHandle<Result<source::Loaded, String>>> = None;
    while !s.quit.load(Ordering::Relaxed) {
        let started = Instant::now();
        let settings = s.settings.lock().clone();
        // the model, read in the background when its file changes (for the Inspector)
        if read.as_ref() != Some(&(settings.file.clone(), settings.revision)) {
            read = Some((settings.file.clone(), settings.revision));
            let file = settings.file.clone();
            loading = (!file.is_empty())
                .then(|| {
                    std::thread::Builder::new()
                        .name("lightair-avatar-load".into())
                        .spawn(move || source::load(&file))
                        .ok()
                })
                .flatten();
            if settings.file.is_empty() {
                *s.status.lock() = Status::default();
                crate::avatar::set_status(s.id, None);
            }
        }
        if loading.as_ref().is_some_and(|l| l.is_finished())
            && let Some(l) = loading.take()
        {
            let mut status = s.status.lock();
            match l.join() {
                Ok(Ok(loaded)) => {
                    let anchors = match &loaded.model {
                        source::Posable::Vrm(a) => {
                            crate::avatar_pins::vrm_anchors(&a.model).into_iter().map(|(a, _)| a).collect()
                        }
                        source::Posable::Flat(f) => {
                            crate::avatar_pins::flat_anchors(f).into_iter().map(|(a, _)| a).collect()
                        }
                    };
                    *status = Status {
                        loaded: true,
                        name: loaded.name,
                        expressions: loaded.expressions,
                        costumes: loaded.costumes,
                        anchors,
                        ..Status::default()
                    };
                }
                Ok(Err(e)) => {
                    tracing::warn!(file = settings.file, error = %e, "avatar didn't load");
                    *status = Status { error: e, ..Status::default() };
                }
                Err(_) => *status = Status { error: "reading it failed".into(), ..Status::default() },
            }
            crate::avatar::set_status(s.id, Some(status.clone()));
        }
        // the camera: while shown (and a moment after), on the device the settings name
        let shown = s.shown.load(Ordering::Relaxed);
        if camera.as_ref().is_some_and(|(d, _)| *d != settings.device) {
            camera = None;
        }
        if shown {
            hidden_since = None;
            if camera.is_none()
                && !settings.device.is_empty()
                && !crate::access::hold_back(crate::access::Media::Camera)
            {
                camera = open_camera(&s, &settings.device).map(|c| (settings.device.clone(), c));
            }
        } else if camera.is_some() && hidden_since.get_or_insert_with(Instant::now).elapsed() >= LINGER {
            camera = None;
            hidden_since = None;
        }
        if shown {
            publish(&s, &settings, camera.is_some());
        }
        std::thread::sleep(STEP.saturating_sub(started.elapsed()));
    }
}

/// Windows: no tracker library to give the camera's frames to yet.
#[cfg(windows)]
fn open_camera(_s: &Arc<Shared>, _device: &str) -> Option<Camera> {
    static SAID: std::sync::Once = std::sync::Once::new();
    SAID.call_once(|| {
        tracing::warn!("not yet on this platform: an avatar's face and body tracking (no tracker library)")
    });
    None
}

/// The source's camera, its frames going to the trackers as they come.
#[cfg(target_os = "macos")]
fn open_camera(s: &Arc<Shared>, device: &str) -> Option<Camera> {
    use lightair_capture::{PixelFormat, camera};
    let config = camera::Config { format: None, height: Some(720), fps: Some(30.0), pixel_format: PixelFormat::Bgra };
    let to = s.clone();
    let sink = move |frame: lightair_capture::Frame| {
        if frame.format != PixelFormat::Bgra {
            return;
        }
        let Some(raw) = NonNull::new(frame.into_raw().cast::<CVPixelBuffer>()) else { return };
        // SAFETY: `into_raw` gave up the frame's reference, now this one's
        let buffer: CFRetained<CVPixelBuffer> = unsafe { CFRetained::from_raw(raw) };
        to.track(&buffer);
    };
    match camera::CameraCapture::start(device, &config, sink) {
        Ok(c) => Some(c),
        Err(e) => {
            tracing::warn!(%e, device, "avatar: the camera didn't open");
            None
        }
    }
}

impl Shared {
    /// A camera frame to the trackers (the capture's queue).
    #[cfg(target_os = "macos")]
    fn track(&self, buffer: &CVPixelBuffer) {
        let settings = self.settings.lock().clone();
        let body = !settings.is_flat() && settings.body_mode() != BodyMode::Off;
        let (face_t, body_t) = {
            let mut t = self.trackers.lock();
            if t.face == 0 {
                t.face = raw::face_new() as usize;
            }
            if body && t.body == 0 {
                t.body = raw::body_new() as usize;
            }
            (t.face as *mut c_void, if body { t.body as *mut c_void } else { std::ptr::null_mut() })
        };
        if face_t.is_null() {
            return;
        }
        let read_only = CVPixelBufferLockFlags::ReadOnly;
        // SAFETY: the buffer is locked for reading while its bytes are read
        unsafe { CVPixelBufferLockBaseAddress(buffer, read_only) };
        let (w, h) = (CVPixelBufferGetWidth(buffer) as u32, CVPixelBufferGetHeight(buffer) as u32);
        let stride = CVPixelBufferGetBytesPerRow(buffer);
        let base = CVPixelBufferGetBaseAddress(buffer) as *const u8;
        if !base.is_null() {
            let id = self.frame.load(Ordering::Relaxed) + 1;
            let now = crate::clock::now_ns();
            // SAFETY: `base` holds `h` rows of `stride` bytes while locked; the trackers copy it
            unsafe {
                raw::face_submit(face_t, base, stride as u32, w, h, id, now);
                if !body_t.is_null() {
                    raw::body_submit(body_t, base, stride as u32, w, h, id);
                }
            }
            self.frame.store(id, Ordering::Release);
            *self.size.lock() = (w, h);
            if settings.show_tracking {
                // SAFETY: as above
                let bytes = unsafe { std::slice::from_raw_parts(base, stride * h as usize) };
                *self.view.lock() = Some(small(bytes, stride, (w, h)));
            }
        }
        unsafe { CVPixelBufferUnlockBaseAddress(buffer, read_only) };
    }

    /// The face the tracker sees in the newest frame, if any, and its points.
    fn landmarks(&self) -> Option<(Measures, Vec<[f32; 2]>)> {
        let tracker = self.trackers.lock().face as *mut c_void;
        let frame_id = self.frame.load(Ordering::Acquire);
        if tracker.is_null() || frame_id == 0 {
            return None;
        }
        let mut face: Box<FtFace> = Box::new(unsafe { std::mem::zeroed() });
        // SAFETY: a live tracker, and a whole `ft_face` to fill
        if !unsafe { raw::face_get(tracker, frame_id, &mut face) } || !face.valid || !face.present {
            return None;
        }
        let region = |r: usize| -> &[[f32; 2]] {
            let (s, n) = (face.region_start[r].max(0) as usize, face.region_count[r].max(0) as usize);
            face.pts.get(s..s + n).unwrap_or(&[])
        };
        let mesh = region(FT_MESH);
        let irises: Vec<[f32; 2]> =
            [FT_LEFT_PUPIL, FT_RIGHT_PUPIL].iter().flat_map(|&r| region(r).first().copied()).collect();
        let (w, h) = *self.size.lock();
        let m = lightair_avatar::rig::measure(&Landmarks {
            mesh,
            irises: &irises,
            width: w.max(1) as f32,
            height: h.max(1) as f32,
        })?;
        Some((m, mesh.iter().chain(&irises).copied().collect()))
    }

    /// The body the body tracker sees, if any.
    fn body(&self) -> Option<BodyFrame> {
        let tracker = self.trackers.lock().body as *mut c_void;
        if tracker.is_null() {
            return None;
        }
        let mut b: Box<BtBody> = Box::new(unsafe { std::mem::zeroed() });
        // SAFETY: a live tracker, and a whole `bt_body` to fill
        if !unsafe { raw::body_get(tracker, &mut *b as *mut BtBody as *mut c_void) } || !b.present {
            return None;
        }
        let (w, h) = *self.size.lock();
        let joints3d = b.has3d.then(|| {
            std::array::from_fn(|j| (b.j3[j][3] > 0.5).then(|| glam::Vec3::new(b.j3[j][0], b.j3[j][1], b.j3[j][2])))
        });
        let joints2d =
            std::array::from_fn(|j| (b.j2[j][2] > 0.0).then(|| (glam::Vec2::new(b.j2[j][0], b.j2[j][1]), b.j2[j][2])));
        let hands = std::array::from_fn(|side| {
            b.hand_present[side].then(|| {
                std::array::from_fn(|k| (glam::Vec2::new(b.hand[side][k][0], b.hand[side][k][1]), b.hand[side][k][2]))
            })
        });
        let hands3d = std::array::from_fn(|side| {
            b.hand_present[side].then(|| std::array::from_fn(|k| glam::Vec3::from(b.hand3d[side][k])))
        });
        Some(BodyFrame {
            joints3d,
            joints2d,
            hands,
            hands3d,
            aspect: w.max(1) as f32 / h.max(1) as f32,
            frame_id: b.frame_id,
        })
    }
}

/// One step's posing, published for the compositor.
fn publish(s: &Shared, settings: &Settings, tracking: bool) {
    let found = if tracking { s.landmarks() } else { None };
    let (measures, points) = match found {
        Some((m, p)) => (Some(m), p),
        None => (None, Vec::new()),
    };
    *s.measures.lock() = measures;
    let seen = measures.map(|m| settings.face(&m));
    // the co-host's face, when this is its avatar
    let now = Instant::now();
    let (seen, mood) = crate::face_drive::merge_at(&s.id.to_string(), now, seen);
    let body = (tracking && !settings.is_flat() && settings.body_mode() != BodyMode::Off).then(|| s.body()).flatten();
    let mic_db = mic_level(&settings.mic);
    {
        let mut status = s.status.lock();
        let following = measures.is_some();
        if status.tracking != following {
            status.tracking = following;
            crate::avatar::set_status(s.id, Some(status.clone()));
        }
    }
    if settings.show_tracking
        && let Some((bgra, size)) = s.view.lock().clone()
    {
        crate::bridge::set_avatar_view(s.id, bgra, size);
    }
    crate::bridge::set_avatar_posing(
        s.id,
        crate::bridge::AvatarPosing {
            face: seen,
            body,
            mood,
            mic_db,
            push_to_talk_held: s.push_to_talk.load(Ordering::Relaxed),
            view_face: if settings.show_tracking { points } else { Vec::new() },
            pins: Vec::new(),
            size: (settings.width, settings.height),
            at: now,
        },
    );
}

/// A mixer track's level now (dBFS, its louder channel's), for a PNGtuber's mic.
fn mic_level(track: &str) -> f32 {
    let Ok(id) = serde_json::from_value::<Id>(serde_json::Value::String(track.to_string())) else {
        return f32::NEG_INFINITY;
    };
    crate::meters::meters()
        .get(id)
        .map_or(f32::NEG_INFINITY, |m| m.levels().magnitude.iter().copied().fold(f32::NEG_INFINITY, f32::max))
}

/// A BGRA picture at most [`VIEW_WIDTH`] wide (every n-th pixel), rows packed.
#[cfg(target_os = "macos")]
fn small(bytes: &[u8], stride: usize, (w, h): (u32, u32)) -> View {
    let step = w.div_ceil(VIEW_WIDTH).max(1);
    let (sw, sh) = (w / step, h / step);
    let mut out = Vec::with_capacity((sw * sh * 4) as usize);
    for y in 0..sh {
        let row = &bytes[(y * step) as usize * stride..];
        for x in 0..sw {
            let at = (x * step) as usize * 4;
            out.extend_from_slice(&row[at..at + 4]);
        }
    }
    (Arc::new(out), (sw, sh))
}
