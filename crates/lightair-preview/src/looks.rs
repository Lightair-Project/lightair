//! macOS, with `LIGHTAIR_ENGINE=own`: the Effect Stack's camera looks (beauty, makeup, face
//! shape, light, the colour grade, background blur, removal and replacement) drawn by the
//! compositor with `lightair-engine`'s looks, on the bridge's picture of the camera without its
//! libobs filters.
//!
//! The engine's looks draw a camera from a host's feed: its frames, and the faces and masks the
//! host's models found in them (`VideoFeed::push_face`, `push_segmentation`, `push_mask`); the
//! engine tracks, smooths and snaps them itself. Here each camera drawn with looks gets a
//! [`Host`]: a video feed in the compositor's own `Hot`, given the bridge's newest picture each
//! frame, and an analysis thread with the Effect Stack module's face tracker
//! (`bridge::FaceTracker`: the face landmark model, the multiclass segmenter and the person
//! segmenter, on Core ML and Vision), which takes each new picture, scales it to the analysis
//! size the module's own filter uses (720 on its short side), and pushes what the models give,
//! before the tracker smooths it, with the picture's time. A camera no longer drawn with looks
//! lets its host go after a while. The faces each host finds are also shared with the face
//! stickers and the landmarks overlay (`crate::stickers::share`), so a camera isn't analysed twice.
//! A face stickers preset's face distortion is drawn by the looks' warp: the source the engine
//! reads is given it first ([`with_sticker_shape`]).

use std::borrow::Cow;
use std::collections::HashMap;
use std::ptr::NonNull;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender, TrySendError};
use std::sync::{Arc, LazyLock};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use lightair_engine::feeds::{Feed, Picture as Pushed, VideoFeed};
use lightair_engine::gpu::apple::Kept;
use lightair_engine::hot::Hot;
use lightair_engine::looks::render::{EFFECT_SHAPE, Settings};
use lightair_engine::{background, clock, looks};
use lightair_host::bridge::{self, FaceTracker};
use lightair_model::{Id, Source};
use objc2_core_foundation::CFRetained;
use objc2_core_video::{
    CVPixelBuffer, CVPixelBufferGetBaseAddress, CVPixelBufferGetBytesPerRow, CVPixelBufferGetHeight,
    CVPixelBufferGetWidth, CVPixelBufferLockBaseAddress, CVPixelBufferLockFlags, CVPixelBufferUnlockBaseAddress,
    kCVPixelFormatType_32BGRA,
};
use objc2_video_toolbox::VTPixelTransferSession;
use parking_lot::Mutex;
use serde_json::{Map, Value};

/// The analysis picture's short side at most (the Effect Stack filter's: eyelid detail for
/// eyeliner on HD cameras).
const TRACK_SHORT_SIDE: f32 = 720.0;
/// How often the analysis thread looks for new results while no picture comes.
const POLL: Duration = Duration::from_millis(8);
/// A host not drawn for this long is let go (its tracker, thread and feed).
const IDLE: Duration = Duration::from_secs(3);
/// How often a module without the looks' procs is asked again.
const RECHECK: Duration = Duration::from_secs(5);

/// The effect kinds the compositor draws with the engine's looks.
fn drawn_kind(kind: &str) -> bool {
    looks::render::KINDS.contains(&kind) || background::KINDS.contains(&kind)
}

/// Whether `source` has a look switched on (any `studio.*` effect: the camera studio filter), or
/// face stickers whose preset distorts the face.
pub(crate) fn has_looks(source: &Source) -> bool {
    source.effects.iter().any(|e| e.enabled && is_look(&e.kind)) || sticker_shape(source).is_some()
}

/// The face distortion of `source`'s face stickers, with the place of the effect it's from: the
/// last switched-on stickers effect (its preset is the one the looks' merged settings hold), when
/// its preset has one.
fn sticker_shape(source: &Source) -> Option<(usize, &'static Map<String, Value>)> {
    let (i, e) = source.effects.iter().enumerate().rfind(|(_, e)| e.enabled && e.kind == crate::stickers::KIND)?;
    Some((i, crate::stickers::shape(&e.settings)?))
}

/// Gives `source`'s face stickers effect its preset's face distortion (`EFFECT_SHAPE` in its
/// settings), where the engine's looks read it: their warp then draws it. Called on the source
/// the engine draws the looks of.
pub(crate) fn with_sticker_shape(source: &mut Source) {
    let Some((i, shape)) = sticker_shape(source) else { return };
    let settings = &mut source.effects[i].settings;
    if !settings.is_object() {
        *settings = Value::Object(Map::new());
    }
    if let Value::Object(settings) = settings {
        settings.insert(EFFECT_SHAPE.into(), Value::Object(shape.clone()));
    }
}

/// `source` as the engine's looks read it (with its stickers' face distortion).
fn shaped(source: &Source) -> Cow<'_, Source> {
    if sticker_shape(source).is_none() {
        return Cow::Borrowed(source);
    }
    let mut s = source.clone();
    with_sticker_shape(&mut s);
    Cow::Owned(s)
}

/// A `studio.*` effect the engine's looks draw (camera motion, ambience and face stickers are the
/// compositor's own passes: `crate::post`, `crate::stickers`).
fn is_look(kind: &str) -> bool {
    kind.starts_with("studio.") && !matches!(kind, crate::post::MOTION | crate::post::AMBIENCE | crate::stickers::KIND)
}

/// Whether the engine's looks draw everything `source`'s looks do: beauty, light, the grade and
/// the background effects (the makeup look's own `.cube` LUT is the renderer's:
/// `compose::with_makeup_luts`; the landmarks overlay is drawn over the looks by
/// `crate::stickers`).
pub(crate) fn drawable(source: &Source) -> bool {
    source.effects.iter().filter(|e| e.enabled && is_look(&e.kind)).all(|e| drawn_kind(&e.kind))
}

/// Whether `source`'s looks analyse its faces when the compositor draws them (their faces are
/// then shared: `crate::stickers::share`).
pub(crate) fn analyses_faces(source: &Source) -> bool {
    has_looks(source) && Needs::of(source).faces
}

/// Whether the Effect Stack module gives what the looks need (asked again every few seconds
/// while it doesn't: the module may load late, or be an older build).
pub(crate) fn available() -> bool {
    static SEEN: LazyLock<Mutex<(Option<bool>, Instant)>> = LazyLock::new(|| Mutex::new((None, Instant::now())));
    let mut seen = SEEN.lock();
    match seen.0 {
        Some(true) => true,
        Some(false) if seen.1.elapsed() < RECHECK => false,
        _ => {
            let ok = FaceTracker::available();
            if !ok && seen.0.is_none() {
                tracing::info!("compositor: the Effect Stack module has no procs for the looks; libobs draws them");
            }
            *seen = (Some(ok), Instant::now());
            ok
        }
    }
}

/// What a camera's looks need analysed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Needs {
    /// The faces and the multiclass segmentation (a face look draws).
    faces: bool,
    /// The segmentation at about 30 a second (a light follows the body, or a background).
    fast: bool,
    /// Every picture segmented (a background needs steady, current edges).
    every: bool,
    /// The person mask at this quality (a background effect).
    person: Option<u32>,
}

impl Needs {
    /// As the Effect Stack's filter asks its tracker for the same looks.
    fn of(source: &Source) -> Needs {
        let settings = Settings::from_source(&shaped(source));
        let faces = settings.as_ref().is_some_and(|s| s.active());
        let light = settings.as_ref().is_some_and(|s| s.light_active());
        let bg = background::look(source);
        Needs { faces, fast: light || bg.is_some(), every: bg.is_some(), person: bg.map(|b| b.quality) }
    }

    fn any(&self) -> bool {
        self.faces || self.person.is_some()
    }
}

/// A picture for the analysis thread.
struct Job {
    buffer: bridge::Buffer,
    time_ns: i64,
    needs: Needs,
}

// SAFETY: CoreVideo's buffers are reference counted thread-safely; a handed-on picture isn't
// written again while it's held.
unsafe impl Send for Job {}

/// One camera drawn with looks: its feed and its analysis thread.
struct Host {
    feed_id: u32,
    feed: Arc<Feed>,
    /// The newest picture given (its bridge serial) and the host time it was first seen.
    last: Option<(u64, i64)>,
    jobs: SyncSender<Job>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    used: Instant,
}

impl Drop for Host {
    /// The thread is told to stop and not waited for: freeing a tracker whose models are still
    /// loading takes a while, and the compositor's frame can't.
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        drop(self.thread.take());
    }
}

/// The hosts of the cameras one compositor draws with looks.
#[derive(Default)]
pub(crate) struct Hosts {
    hosts: HashMap<Id, Host>,
    next_feed: u32,
}

impl Hosts {
    /// Gives `source`'s feed the bridge's `picture` for this frame (and, when it's new, to the
    /// analysis), registering the feed in `hot` and `feeds` (source → feed) the first time.
    /// False when no host could be made.
    pub(crate) fn push(
        &mut self,
        source: &Source,
        picture: &bridge::Picture,
        hot: &Hot,
        feeds: &mut HashMap<Id, u32>,
    ) -> bool {
        if !self.hosts.contains_key(&source.id) {
            let Some(host) = self.host(source.id) else { return false };
            hot.feeds.write().insert(host.feed_id, host.feed.clone());
            feeds.insert(source.id, host.feed_id);
            self.hosts.insert(source.id, host);
        }
        let Some(host) = self.hosts.get_mut(&source.id) else { return false };
        host.used = Instant::now();
        let Some(v) = host.feed.video() else { return false };
        let Some(pixels) = picture.pixel_buffer() else { return false };
        // SAFETY: the bridge's buffer is a valid CVPixelBuffer
        let Some(kept) = (unsafe { Kept::retain(CFRetained::as_ptr(&picture.buffer).as_ptr().cast()) }) else {
            return false;
        };
        // a picture keeps the time it was first seen at, so its faces match it
        let fresh = host.last.is_none_or(|(serial, _)| serial != picture.serial);
        let time_ns = match host.last {
            Some((serial, t)) if serial == picture.serial => t,
            _ => clock::host_now_ns(),
        };
        host.last = Some((picture.serial, time_ns));
        // only this frame's picture is held (the bridge reuses its buffers once let go)
        v.clear();
        v.push(Pushed::Buffer(pixels, kept), time_ns);
        let needs = if fresh { Needs::of(source) } else { Needs::default() };
        if needs.any() {
            // still analysing the last one (`Full`): this picture is skipped
            if let Err(TrySendError::Disconnected(_)) =
                host.jobs.try_send(Job { buffer: picture.buffer.clone(), time_ns, needs })
            {
                tracing::debug!("compositor: a camera's looks analysis has stopped");
            }
        }
        true
    }

    /// The frame's GPU work is done: the feeds let go of the pictures they were given.
    pub(crate) fn frame_done(&mut self) {
        for host in self.hosts.values() {
            if let Some(v) = host.feed.video() {
                v.clear();
            }
        }
    }

    /// Lets go of the hosts of cameras not drawn with looks lately, or no longer in the
    /// workspace (`keep` false); returns their feeds, to be forgotten by the engine's cache.
    pub(crate) fn sweep(&mut self, keep: impl Fn(Id) -> bool, hot: &Hot, feeds: &mut HashMap<Id, u32>) -> Vec<u32> {
        let gone: Vec<Id> =
            self.hosts.iter().filter(|(id, h)| !keep(**id) || h.used.elapsed() > IDLE).map(|(id, _)| *id).collect();
        let mut out = Vec::new();
        for id in gone {
            if let Some(host) = self.hosts.remove(&id) {
                hot.feeds.write().remove(&host.feed_id);
                feeds.remove(&id);
                out.push(host.feed_id);
            }
        }
        out
    }

    fn host(&mut self, source: Id) -> Option<Host> {
        self.next_feed += 1;
        let feed = Arc::new(Feed::Video(VideoFeed::default()));
        let (jobs, rx) = mpsc::sync_channel(1);
        let stop = Arc::new(AtomicBool::new(false));
        let thread = {
            let (feed, stop) = (feed.clone(), stop.clone());
            std::thread::Builder::new()
                .name("lightair-looks".into())
                .spawn(move || analyse(source, &feed, &rx, &stop))
                .map_err(|e| tracing::error!(%e, "compositor: no thread for the looks' analysis"))
                .ok()?
        };
        Some(Host { feed_id: self.next_feed, feed, last: None, jobs, stop, thread: Some(thread), used: Instant::now() })
    }
}

// ---------------------------------------------------------------- the analysis thread

/// A camera's analysis: each picture given scaled and handed to the tracker, and what the
/// models found pushed to the camera's feed as it comes (its faces also shared under `source`).
fn analyse(source: Id, feed: &Feed, jobs: &Receiver<Job>, stop: &AtomicBool) {
    let Some(v) = feed.video() else { return };
    let Some(tracker) = FaceTracker::new() else {
        tracing::warn!("compositor: no face tracker for the looks");
        return;
    };
    let mut scaler = Scaler::new();
    let mut needs: Option<Needs> = None;
    let (mut faces, mut masks, mut persons) = (0u64, 0u64, 0u64);
    let mut id = 0u64;
    while !stop.load(Ordering::Acquire) {
        match jobs.recv_timeout(POLL) {
            Ok(job) => {
                if needs != Some(job.needs) {
                    let n = job.needs;
                    tracker.segmentation(n.any(), n.fast, n.every, n.person);
                    if n.person.is_none() {
                        // no background any more: its mask isn't left for the next one
                        *v.mask.lock() = None;
                    }
                    needs = Some(n);
                }
                id += 1;
                scaler.submit(&tracker, &job.buffer, id, job.time_ns);
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
        if let Some(f) = tracker.raw_face(&mut faces) {
            crate::stickers::share(source, &f);
            v.push_face(f.time_ns, f.found, if f.found { 1.0 } else { 0.0 }, f.points, f.size);
        }
        if let Some(m) = tracker.raw_mask(false, &mut masks) {
            v.push_segmentation(m.time_ns, m.bytes, m.size);
        }
        if needs.is_some_and(|n| n.person.is_some())
            && let Some(m) = tracker.raw_mask(true, &mut persons)
        {
            v.push_mask(m.bytes, m.size, 0, false);
        }
    }
    crate::stickers::unshare(source);
}

/// Scales pictures to the analysis size on the GPU (VideoToolbox) into a BGRA buffer of its own,
/// and hands them to a face tracker.
pub(crate) struct Scaler {
    session: Option<CFRetained<VTPixelTransferSession>>,
    small: Option<CFRetained<CVPixelBuffer>>,
}

impl Scaler {
    pub(crate) fn new() -> Scaler {
        let mut session: *mut VTPixelTransferSession = std::ptr::null_mut();
        // SAFETY: `session` is a valid place for the new session
        let rc = unsafe { VTPixelTransferSession::create(None, NonNull::from(&mut session)) };
        let session = (rc == 0 && !session.is_null()).then(|| {
            // SAFETY: the session was just made, one reference ours
            unsafe { CFRetained::from_raw(NonNull::new_unchecked(session)) }
        });
        if session.is_none() {
            tracing::warn!(rc, "compositor: no VideoToolbox session to scale pictures for the looks");
        }
        Scaler { session, small: None }
    }

    /// `buffer`, scaled to at most [`TRACK_SHORT_SIDE`] on its short side (even sides), given to
    /// the tracker with `id` and its time.
    pub(crate) fn submit(&mut self, tracker: &FaceTracker, buffer: &CVPixelBuffer, id: u64, time_ns: i64) {
        let (w, h) = (CVPixelBufferGetWidth(buffer) as u32, CVPixelBufferGetHeight(buffer) as u32);
        let s = (TRACK_SHORT_SIDE / w.min(h).max(1) as f32).min(1.0);
        let (sw, sh) = (((w as f32 * s) as u32) & !1, ((h as f32 * s) as u32) & !1);
        if sw < 16 || sh < 16 {
            return;
        }
        let picture: &CVPixelBuffer = if (sw, sh) == (w, h) {
            buffer
        } else {
            let Some(session) = &self.session else { return };
            if self
                .small
                .as_ref()
                .is_none_or(|b| (CVPixelBufferGetWidth(b) as u32, CVPixelBufferGetHeight(b) as u32) != (sw, sh))
            {
                self.small = crate::mac::pixel_buffer(sw, sh, kCVPixelFormatType_32BGRA);
            }
            let Some(small) = &self.small else { return };
            // SAFETY: both are valid BGRA pixel buffers
            if unsafe { session.transfer_image(buffer, small) } != 0 {
                return;
            }
            small
        };
        // SAFETY: the base address is read only while the buffer is locked, within its rows
        unsafe {
            if CVPixelBufferLockBaseAddress(picture, CVPixelBufferLockFlags::ReadOnly) != 0 {
                return;
            }
            let base = CVPixelBufferGetBaseAddress(picture) as *const u8;
            let row = CVPixelBufferGetBytesPerRow(picture);
            if !base.is_null() && row >= sw as usize * 4 {
                tracker.submit(base, row, sw, sh, id, time_ns);
            }
            CVPixelBufferUnlockBaseAddress(picture, CVPixelBufferLockFlags::ReadOnly);
        }
    }
}
