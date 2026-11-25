//! macOS and Windows, with `LIGHTAIR_ENGINE=own`: recording, streaming and the replay buffer made
//! by Lightair itself instead of libobs's encoders and outputs.
//!
//! - **Pictures:** each canvas's compositor hands its finished frames over
//!   (the compositor calls [`frame`]). A thread per canvas in use stamps each frame,
//!   places it on the canvas's frame grid and gives it to the canvas's video encoders
//!   (`lightair-encode`'s VideoToolbox sessions on macOS, the GPU's Media Foundation encoder on
//!   Windows): each copies the frame into a picture of its own (scaled when the encoder's size
//!   differs) and encodes it, so the compositor's buffer goes back to it at once. While the
//!   compositor hands nothing over, the last frame is repeated, so a stream never stalls.
//! - **Sound:** the audio engine's Program and mixes come straight from the mixer
//!   (`audio_host::tap_mix`) and are encoded to AAC, or Opus for WHIP.
//! - **One clock:** pictures are stamped with the frame time when they're handed over, sound
//!   with the mixer's block times; both are `crate::clock`'s, so picture and sound line
//!   up the same way in every file and stream, whatever the compositor's own timing.
//! - **Sharing:** recordings, destinations and replay buffers whose settings match share one
//!   video encoder per canvas and one audio encoder per mix.
//! - **Outputs:** recordings go to `lightair-encode`'s crash-safe writer (MP4, MOV, MKV, several
//!   audio tracks, markers as chapters) on a thread of their own; streams go out through
//!   `lightair-feed`'s RTMP(S) and SRT senders and `lightair-engine`'s WHIP sender; replay buffers
//!   keep the last N seconds of packets and write a clip on request. Their state goes to
//!   [`outputs::runtime`] as libobs's outputs report theirs, so the interface reads it unchanged.
//! - **Pictures out as they are:** the virtual camera and NDI take a canvas's frames unencoded
//!   through a tap ([`add_tap`]): each frame copied into a picture of the tap's own (read back
//!   into memory on Windows), then handed to a thread of its own that sends it (the camera
//!   extension's sink queue or the Windows camera's shared memory, NDI's runtime).
//! - **The RTSP server** is fed by a canvas's H.264 encoder (one already running without
//!   B-frames when there is one) and the Program's AAC encoder.
//! - **Following the primary canvas:** when another canvas becomes primary (another workspace
//!   opened), whatever took the primary canvas's frames moves to it ([`canvas_moved`]), as
//!   libobs's outputs on the primary output canvas carry on.
//!
//! Engine thread for everything that starts, stops or changes an output; the compositor's
//! threads call [`frame`] and [`uses`].

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock, Weak};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, RecvTimeoutError, Sender, TrySendError, bounded, unbounded};
#[cfg(target_os = "macos")]
use lightair_encode::apple::{
    AudioEncoder, AudioSettings, EncodedAudio, Hardware, InputFormat, VideoEncoder, VideoOutput, VideoSettings,
};
#[cfg(windows)]
use lightair_encode::windows::{
    AudioEncoder, AudioSettings, EncodedAudio, Hardware, InputFormat, VideoEncoder, VideoOutput, VideoSettings,
};
use lightair_encode::{
    AudioCodec, AudioFormat, Color, Container as FileContainer, H264Profile, NalLayout, Packet, ProRes, Recorder,
    RecorderOptions, TrackFormat, TrackSpec, VideoCodec, VideoFormat,
};
use lightair_engine::whip::WhipSender;
use lightair_feed::net::Bind;
use lightair_feed::ts::{AudioConfig, Muxer, NalFormat, VideoCodec as TsCodec};
use lightair_feed::{RtmpAudioTrack, RtmpSender, RtmpTarget, RtmpTrack, RtmpTracks, RtmpVideo, SrtSender};
use lightair_model::Id;
use parking_lot::Mutex;

#[cfg(windows)]
#[path = "own_outputs_win.rs"]
mod pictures;

use crate::outputs::{
    self, ClipsConfig, Container, EncoderConfig, OutputKey, OutputState, OutputStatus, ReconnectInfo, RecordingConfig,
    StreamConfig, StreamProtocol, runtime,
};

/// The compositor's frames are this old at most before the last one is repeated.
const STALE: Duration = Duration::from_millis(250);
/// Frames waiting for a canvas's encoders at most; more are dropped (the encoders are behind).
const QUEUED_FRAMES: usize = 3;
/// The sample rate and channels every audio encoder takes (libobs converts its mixes to it).
const SAMPLE_RATE: u32 = 48_000;
const CHANNELS: u32 = 2;
/// Sound kept while a recording waits for its first picture.
const PREROLL_NS: i64 = 5_000_000_000;
/// How often a stream's connection is looked at.
const WATCH: Duration = Duration::from_millis(250);
/// libobs's reconnect tries when a destination says nothing.
const DEFAULT_RETRIES: u32 = 20;

// ---------------------------------------------------------------- the switch and the clock

/// Whether Lightair makes the outputs itself: with its own compositor and mixer on, unless
/// `LIGHTAIR_OUTPUTS=libobs` keeps libobs's (for comparing the two). Read once.
pub fn enabled() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| {
        crate::show::own()
            && crate::audio_host::mixer_on()
            && !std::env::var("LIGHTAIR_OUTPUTS").is_ok_and(|v| v.trim().eq_ignore_ascii_case("libobs"))
    })
}

/// The program clock (`crate::clock`), which the audio times are on.
fn now_ns() -> i64 {
    crate::clock::now_ns() as i64
}

/// When a frame handed over now was made: now (the compositor hands each over as it's drawn).
fn stamp() -> i64 {
    now_ns()
}

fn next_id() -> u64 {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

// ---------------------------------------------------------------- packets and sinks

pub use crate::buffers::Buffer;
pub use pictures::TapFn;
#[cfg(target_os = "macos")]
use pictures::TapFormat;
#[cfg(windows)]
pub use pictures::{Picture, TapFormat};

/// A finished compositor frame on its way to a canvas's encoders.
struct Frame(Buffer);

// SAFETY: Lightair's buffers are reference counted thread-safely, and a finished frame isn't
// written while it's handed over.
unsafe impl Send for Frame {}

/// One encoded picture or sound, shared by every output it goes to.
struct Pkt {
    data: Vec<u8>,
    pts: i64,
    dts: i64,
    key: bool,
}

/// Where an encoder's packets go: a recording, a destination, a replay buffer. Called on the
/// encoders' threads; never blocks.
trait Sink: Send + Sync {
    fn video(&self, packet: &Arc<Pkt>, format: &Arc<VideoFormat>);
    /// `track`: the sound track this encoder is to the output (0 the first).
    fn audio(&self, track: usize, packet: &Arc<Pkt>);
}

// ---------------------------------------------------------------- video encoders

/// What decides whether two outputs of one canvas can share a video encoder.
#[derive(Debug, Clone, PartialEq, Eq)]
struct VideoKey {
    codec: VideoCodec,
    width: u32,
    height: u32,
    /// Every this many canvas frames.
    divisor: u32,
    kbps: u32,
    cbr: bool,
    keyint: u32,
    reorder: bool,
    /// H.264's profile.
    profile: H264Profile,
    /// A ceiling over the average when it isn't constant, kbps (`None`: the encoder's own).
    peak: Option<u32>,
    /// Non-zero: one output's own (its bitrate follows its connection).
    owner: u64,
}

type Subs = Arc<Mutex<Vec<(u64, Arc<dyn Sink>)>>>;

/// A canvas's video encoder and the outputs it feeds.
struct VideoPipe {
    key: VideoKey,
    /// The canvas whose frames it encodes (another when the primary canvas moves).
    canvas: Mutex<Id>,
    encoder: VideoEncoder,
    /// Copies the compositor's frames into the encoder's pictures; keeps the last.
    transfer: Mutex<pictures::Copier>,
    subs: Subs,
    /// Frames not encoded: the copy failed or the encoder dropped them.
    skipped: Arc<AtomicU32>,
    /// Frames given to the encoder.
    fed: AtomicU64,
    /// A track of a multitrack stream: the stream's keyframe beat and this canvas's place in it.
    gop: Option<(Arc<Gop>, usize)>,
}

/// Where a multitrack stream's pictures stand on a canvas frame: not yet sent, a keyframe, or
/// an ordinary picture.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Beat {
    Wait,
    Key,
    Frame,
}

/// One canvas's tracks in a [`Gop`].
struct GopCanvas {
    /// Every this many canvas frames each of its tracks takes one (the divisors' least common
    /// multiple): keyframes only fall there.
    step: i64,
    /// The last canvas frame decided, and what for.
    slot: i64,
    beat: Beat,
    started: bool,
    /// The next keyframe is due at this time.
    next_ns: i64,
    /// Keyframe requests answered.
    answered: u64,
}

/// The keyframes of a multitrack stream's tracks: every track of a canvas makes its keyframes
/// on the same canvas frames (the same times), every keyframe interval on the program clock and
/// when the connection asks for one, so the renditions switch cleanly. Its tracks wait for
/// the first keyframe once the stream is set up.
struct Gop {
    every_ns: i64,
    ready: AtomicBool,
    asked: AtomicU64,
    canvases: Mutex<Vec<GopCanvas>>,
}

impl Gop {
    fn new(keyint_s: u32, steps: &[i64]) -> Gop {
        let canvases = steps
            .iter()
            .map(|&step| GopCanvas {
                step: step.max(1),
                slot: i64::MIN,
                beat: Beat::Wait,
                started: false,
                next_ns: 0,
                answered: 0,
            })
            .collect();
        Gop {
            every_ns: keyint_s.max(1) as i64 * 1_000_000_000,
            ready: AtomicBool::new(false),
            asked: AtomicU64::new(0),
            canvases: Mutex::new(canvases),
        }
    }

    /// What canvas `canvas`'s frame `slot` (shown at `pts`) is to its tracks. Decided once per
    /// frame, by the first track to ask, so every track of the canvas does the same.
    fn beat(&self, canvas: usize, slot: i64, pts: i64) -> Beat {
        let mut canvases = self.canvases.lock();
        let Some(c) = canvases.get_mut(canvas) else { return Beat::Frame };
        if c.slot == slot {
            return c.beat;
        }
        let asked = self.asked.load(Ordering::Acquire);
        let beat = if !self.ready.load(Ordering::Acquire) {
            Beat::Wait
        } else if slot.rem_euclid(c.step) != 0 {
            if c.started { Beat::Frame } else { Beat::Wait }
        } else if !c.started || pts >= c.next_ns || asked != c.answered {
            c.started = true;
            c.answered = asked;
            c.next_ns = (pts.div_euclid(self.every_ns) + 1) * self.every_ns;
            Beat::Key
        } else {
            Beat::Frame
        };
        c.slot = slot;
        c.beat = beat;
        beat
    }
}

impl VideoPipe {
    fn open(
        key: VideoKey,
        canvas: Id,
        settings: VideoSettings,
        gop: Option<(Arc<Gop>, usize)>,
    ) -> Result<Arc<VideoPipe>, String> {
        let subs: Subs = Arc::default();
        let skipped = Arc::new(AtomicU32::new(0));
        let format: Arc<Mutex<Option<Arc<VideoFormat>>>> = Arc::default();
        let (s, k) = (subs.clone(), skipped.clone());
        let sink = Box::new(move |out: VideoOutput| match out {
            VideoOutput::Packet(p) => {
                let fmt = match p.format {
                    Some(new) => {
                        *format.lock() = Some(new.clone());
                        new
                    }
                    None => match format.lock().clone() {
                        Some(f) => f,
                        None => return,
                    },
                };
                let pkt = Arc::new(Pkt { data: p.data, pts: p.pts_ns, dts: p.dts_ns, key: p.keyframe });
                for (_, sink) in s.lock().iter() {
                    sink.video(&pkt, &fmt);
                }
            }
            VideoOutput::Dropped { .. } => {
                k.fetch_add(1, Ordering::Relaxed);
            }
            VideoOutput::Failed(e) => tracing::warn!(%e, "own outputs: a picture wasn't encoded"),
        });
        let name = settings.codec.name();
        let encoder = pictures::open_encoder(settings, key.cbr, sink)
            .map_err(|e| format!("Couldn't open the {name} encoder ({e})."))?;
        tracing::info!(%canvas, codec = name, width = key.width, height = key.height, kbps = key.kbps,
            hardware = encoder.using_hardware(), "own outputs: video encoder open");
        Ok(Arc::new(VideoPipe {
            key,
            canvas: Mutex::new(canvas),
            encoder,
            transfer: Mutex::new(pictures::Copier::new()),
            subs,
            skipped,
            fed: AtomicU64::new(0),
            gop,
        }))
    }

    /// Encodes `frame` (`None`: the last one again) as canvas frame `slot`, shown at `pts`.
    fn push(&self, frame: Option<&Buffer>, slot: i64, pts: i64) {
        if slot.rem_euclid(self.key.divisor as i64) != 0 {
            return;
        }
        // a multitrack stream's track: its keyframes where the stream's other tracks have theirs
        let key = match &self.gop {
            Some((gop, canvas)) => match gop.beat(*canvas, slot, pts) {
                Beat::Wait => return,
                Beat::Key => true,
                Beat::Frame => false,
            },
            None => false,
        };
        match self.transfer.lock().feed(&self.encoder, frame, pts, key) {
            pictures::Fed::NotCopied => {
                self.skipped.fetch_add(1, Ordering::Relaxed);
                if key {
                    self.encoder.request_keyframe();
                }
            }
            pictures::Fed::NoPicture => {
                if key {
                    self.encoder.request_keyframe();
                }
            }
            pictures::Fed::Encoded(result) => {
                self.fed.fetch_add(1, Ordering::Relaxed);
                if let Err(e) = result {
                    self.skipped.fetch_add(1, Ordering::Relaxed);
                    tracing::debug!(%e, "own outputs: frame refused");
                }
            }
        }
    }

    /// Adds an output; the next picture is a keyframe, so it can start at once.
    fn subscribe(&self, sink: Arc<dyn Sink>) -> u64 {
        let id = next_id();
        self.subscribe_as(id, sink);
        id
    }

    /// [`VideoPipe::subscribe`] under the output's id `id` (an output on several encoders).
    fn subscribe_as(&self, id: u64, sink: Arc<dyn Sink>) {
        self.subs.lock().push((id, sink));
        self.encoder.request_keyframe();
    }

    fn unsubscribe(&self, id: u64) {
        self.subs.lock().retain(|(s, _)| *s != id);
    }

    /// A keyframe soon: on every track at once when it's a multitrack stream's.
    fn ask_keyframe(&self) {
        match &self.gop {
            Some((gop, _)) => {
                gop.asked.fetch_add(1, Ordering::AcqRel);
            }
            None => self.encoder.request_keyframe(),
        }
    }

    fn others(&self, id: u64) -> usize {
        self.subs.lock().iter().filter(|(s, _)| *s != id).count()
    }
}

// ---------------------------------------------------------------- the canvases' frames

/// What takes a canvas's frames on the canvas's thread: a video encoder or a tap.
trait Consumer: Send + Sync {
    /// `frame` (`None`: the last one again) as canvas frame `slot`, shown at `pts`. Never blocks.
    fn push(&self, frame: Option<&Buffer>, slot: i64, pts: i64);
}

impl Consumer for VideoPipe {
    fn push(&self, frame: Option<&Buffer>, slot: i64, pts: i64) {
        VideoPipe::push(self, frame, slot, pts);
    }
}

type Consumers = Arc<Mutex<Vec<Arc<dyn Consumer>>>>;

/// A canvas whose compositor feeds encoders or taps: its frames go to a thread of its own.
struct CanvasFeed {
    tx: Sender<(Frame, i64)>,
    pipes: Consumers,
    /// Frames dropped because the encoders were behind.
    dropped: Arc<AtomicU32>,
    thread: Option<JoinHandle<()>>,
}

impl CanvasFeed {
    /// Stops the feed's thread (never call with [`FEEDS`] held: the compositor may be handing a
    /// frame over).
    fn stop(mut self) {
        drop(self.tx);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

static FEEDS: Mutex<Option<HashMap<Id, CanvasFeed>>> = Mutex::new(None);

/// The compositor's thread: `canvas`'s newest finished frame, for its encoders. Cheap: the frame
/// is queued for the canvas's own thread (or dropped when that's behind).
pub fn frame(canvas: Id, buffer: &Buffer) {
    let feeds = FEEDS.lock();
    let Some(feed) = feeds.as_ref().and_then(|f| f.get(&canvas)) else { return };
    if let Err(TrySendError::Full(_)) = feed.tx.try_send((Frame(buffer.clone()), stamp())) {
        feed.dropped.fetch_add(1, Ordering::Relaxed);
    }
}

/// Whether `canvas`'s compositor has to draw for Lightair's own outputs.
pub fn uses(canvas: Id) -> bool {
    FEEDS.lock().as_ref().is_some_and(|f| f.contains_key(&canvas))
}

/// Frames `canvas`'s encoders couldn't take.
fn dropped_of(canvas: Id) -> u32 {
    FEEDS.lock().as_ref().and_then(|f| f.get(&canvas)).map_or(0, |f| f.dropped.load(Ordering::Relaxed))
}

fn add_to_feed(canvas: Id, consumer: Arc<dyn Consumer>, fps: (u32, u32)) {
    let mut feeds = FEEDS.lock();
    let feeds = feeds.get_or_insert_with(HashMap::new);
    if let Some(feed) = feeds.get(&canvas) {
        feed.pipes.lock().push(consumer);
        return;
    }
    let (tx, rx) = bounded(QUEUED_FRAMES);
    let pipes: Consumers = Arc::new(Mutex::new(vec![consumer]));
    let p = pipes.clone();
    let thread = std::thread::Builder::new()
        .name("lightair-encode".into())
        .spawn(move || run_canvas(rx, p, fps))
        .map_err(|e| tracing::error!(%e, "own outputs: no encoding thread"))
        .ok();
    feeds.insert(canvas, CanvasFeed { tx, pipes, dropped: Arc::new(AtomicU32::new(0)), thread });
}

/// Takes `consumer` off whichever canvas it's on; the canvas's thread stops with its last one.
fn remove_from_feed(consumer: &Arc<dyn Consumer>) {
    let gone = {
        let mut feeds = FEEDS.lock();
        let Some(map) = feeds.as_mut() else { return };
        let mut emptied = None;
        for (canvas, feed) in map.iter() {
            let mut pipes = feed.pipes.lock();
            let before = pipes.len();
            pipes.retain(|p| !std::ptr::addr_eq(Arc::as_ptr(p), Arc::as_ptr(consumer)));
            if pipes.len() != before {
                if pipes.is_empty() {
                    emptied = Some(*canvas);
                }
                break;
            }
        }
        emptied.and_then(|c| map.remove(&c))
    };
    // joined outside the lock: the compositor may be handing a frame over
    if let Some(feed) = gone {
        feed.stop();
    }
}

/// Engine thread: what takes canvas `from`'s frames takes canvas `to`'s from now on (the
/// primary canvas's output handed to another canvas: another workspace opened).
pub fn canvas_moved(from: Id, to: Id) {
    if from == to {
        return;
    }
    let merged = with(|st| {
        for p in &st.videos {
            let mut c = p.canvas.lock();
            if *c == from {
                *c = to;
            }
        }
        let mut feeds = FEEDS.lock();
        let map = feeds.as_mut()?;
        let feed = map.remove(&from)?;
        match map.get(&to) {
            Some(there) => {
                there.pipes.lock().append(&mut feed.pipes.lock());
                Some(feed)
            }
            None => {
                map.insert(to, feed);
                None
            }
        }
    });
    if let Some(feed) = merged {
        feed.stop();
    }
    tracing::info!(%from, %to, "own outputs: the primary canvas's outputs follow it");
}

/// A canvas's thread: each frame placed on the canvas's frame grid (from the first frame's
/// time, at the canvas's rate) and encoded; the last frame repeated while none comes.
fn run_canvas(rx: Receiver<(Frame, i64)>, pipes: Consumers, fps: (u32, u32)) {
    let (num, den) = (fps.0.max(1) as i128, fps.1.max(1) as i128);
    let frame_ns = (den * 1_000_000_000 / num) as i64;
    let grid = |origin: i64, slot: i64| origin + (slot as i128 * den * 1_000_000_000 / num) as i64;
    let slot_of = |origin: i64, t: i64| ((t - origin) as f64 / frame_ns as f64).round() as i64;
    let period = Duration::from_nanos(frame_ns.max(1) as u64);
    let mut origin: Option<i64> = None;
    let mut last = -1i64;
    let mut fresh = Instant::now();
    loop {
        match rx.recv_timeout(period) {
            Ok((frame, t)) => {
                let o = *origin.get_or_insert(t);
                let mut slot = slot_of(o, t);
                if slot <= last {
                    // two frames from one libobs frame (a faster canvas): the next slot, unless
                    // that runs ahead of the frame's time
                    if grid(o, last + 1) - t > frame_ns * 3 / 2 {
                        continue;
                    }
                    slot = last + 1;
                }
                last = slot;
                fresh = Instant::now();
                let pts = grid(o, slot);
                let pipes = pipes.lock().clone();
                for p in &pipes {
                    p.push(Some(&frame.0), slot, pts);
                }
            }
            Err(RecvTimeoutError::Timeout) => {
                let Some(o) = origin else { continue };
                if fresh.elapsed() < STALE {
                    continue;
                }
                let slot = slot_of(o, now_ns());
                if slot <= last {
                    continue;
                }
                last = slot;
                let pts = grid(o, slot);
                let pipes = pipes.lock().clone();
                for p in &pipes {
                    p.push(None, slot, pts);
                }
            }
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }
}

// ---------------------------------------------------------------- taps: frames as pictures

/// A tap's copying side: the platform's copier and the last copy (sent again while no new frame
/// comes).
struct TapCopy {
    copier: pictures::TapCopier,
    last: Option<pictures::TapPicture>,
    last_pts: Option<i64>,
}

/// A canvas's frames taken unencoded (the virtual camera, NDI): copied on the canvas's thread,
/// sent on the tap's own, so a slow receiver never holds up the canvas's encoders.
struct Tap {
    copy: Mutex<TapCopy>,
    /// Pictures closer together than this are left out (a receiver slower than the canvas).
    min_gap_ns: i64,
    tx: Mutex<Option<Sender<(pictures::TapPicture, i64)>>>,
    /// Pictures the receiver wasn't ready for.
    skipped: AtomicU32,
}

impl Consumer for Tap {
    fn push(&self, frame: Option<&Buffer>, _slot: i64, pts: i64) {
        let picture = {
            let mut c = self.copy.lock();
            if c.last_pts.is_some_and(|last| pts - last < self.min_gap_ns) {
                return;
            }
            if let Some(src) = frame {
                let Some(copy) = c.copier.copy(src) else {
                    self.skipped.fetch_add(1, Ordering::Relaxed);
                    return;
                };
                c.last = Some(copy);
            }
            let Some(last) = c.last.clone() else { return };
            c.last_pts = Some(pts);
            last
        };
        if let Some(tx) = self.tx.lock().as_ref()
            && let Err(TrySendError::Full(_)) = tx.try_send((picture, pts))
        {
            self.skipped.fetch_add(1, Ordering::Relaxed);
        }
    }
}

struct TapEntry {
    tap: Arc<Tap>,
    thread: Option<JoinHandle<()>>,
}

static TAPS: Mutex<Option<HashMap<u64, TapEntry>>> = Mutex::new(None);

/// Takes `canvas`'s frames as pictures from now on: each copied at `size` (`None`: the canvas's
/// own), at most `max_fps` a second (`None`: every frame), and given to `f` on a thread of the
/// tap's own; the last picture again while the compositor hands nothing over. Returns the tap,
/// for [`remove_tap`]. Engine thread.
pub fn add_tap(canvas: Id, size: Option<(u32, u32)>, max_fps: Option<u32>, f: TapFn) -> Result<u64, String> {
    let shape = canvas_shape(canvas).ok_or_else(|| "The canvas isn't ready yet.".to_string())?;
    add_tap_on(canvas, shape, size, max_fps, TapFormat::Bgra, f)
}

/// [`add_tap`] on the frames handed over under `feed` (a canvas, or a scene drawn for an
/// output), whose pictures are `shape` (width, height, frame rate), in `format`.
fn add_tap_on(
    feed: Id,
    (cw, ch, num, den): (u32, u32, u32, u32),
    size: Option<(u32, u32)>,
    max_fps: Option<u32>,
    format: TapFormat,
    mut f: TapFn,
) -> Result<u64, String> {
    let (w, h) = size.unwrap_or((cw, ch));
    let (w, h) = (w.max(2) & !1, h.max(2) & !1);
    let copier = pictures::TapCopier::new(w, h, format)?;
    let (tx, rx) = bounded::<(pictures::TapPicture, i64)>(1);
    let thread = std::thread::Builder::new()
        .name("lightair-tap".into())
        .spawn(move || {
            while let Ok((picture, pts)) = rx.recv() {
                picture.give(&mut f, pts);
            }
        })
        .map_err(|e| format!("Couldn't start sending pictures: {e}"))?;
    let min_gap_ns = max_fps.filter(|f| *f > 0).map_or(0, |f| 1_000_000_000 / f as i64 * 9 / 10);
    let tap = Arc::new(Tap {
        copy: Mutex::new(TapCopy { copier, last: None, last_pts: None }),
        min_gap_ns,
        tx: Mutex::new(Some(tx)),
        skipped: AtomicU32::new(0),
    });
    let id = next_id();
    add_to_feed(feed, tap.clone(), (num, den));
    TAPS.lock().get_or_insert_with(HashMap::new).insert(id, TapEntry { tap, thread: Some(thread) });
    tracing::info!(%feed, width = w, height = h, "own outputs: pictures taken from the compositor");
    Ok(id)
}

/// Stops tap `id`: once this returns, its function isn't called again (and has been dropped).
/// Engine thread.
pub fn remove_tap(id: u64) {
    let Some(mut entry) = TAPS.lock().as_mut().and_then(|t| t.remove(&id)) else { return };
    remove_from_feed(&(entry.tap.clone() as Arc<dyn Consumer>));
    entry.tap.tx.lock().take();
    if let Some(t) = entry.thread.take() {
        let _ = t.join();
    }
    let skipped = entry.tap.skipped.load(Ordering::Relaxed);
    if skipped > 0 {
        tracing::debug!(skipped, "own outputs: pictures the receiver wasn't ready for");
    }
}

fn remove_all_taps() {
    let ids: Vec<u64> = TAPS.lock().as_ref().map(|t| t.keys().copied().collect()).unwrap_or_default();
    for id in ids {
        remove_tap(id);
    }
}

// ---------------------------------------------------------------- audio encoders

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct AudioKey {
    /// The libobs mix (0: Program).
    mix: usize,
    codec: AudioCodec,
    kbps: u32,
}

struct AudioSub {
    id: u64,
    track: usize,
    sink: Arc<dyn Sink>,
}

/// One libobs mix encoded, and the outputs it feeds.
struct AudioPipe {
    key: AudioKey,
    encoder: Mutex<AudioEncoder>,
    format: AudioFormat,
    subs: Mutex<Vec<AudioSub>>,
    packets: AtomicU64,
    connected: AtomicBool,
    /// The mixer's tap on the mix.
    tap: Mutex<Option<crate::audio_host::MixTap>>,
}

impl AudioPipe {
    fn open(key: AudioKey) -> Result<Arc<AudioPipe>, String> {
        let settings =
            AudioSettings { codec: key.codec, sample_rate: SAMPLE_RATE, channels: CHANNELS, bitrate_kbps: key.kbps };
        let encoder = AudioEncoder::new(settings)
            .map_err(|e| format!("Couldn't open the {} encoder ({e}).", key.codec.name()))?;
        let format = encoder.format().clone();
        let pipe = Arc::new(AudioPipe {
            key,
            encoder: Mutex::new(encoder),
            format,
            subs: Mutex::new(Vec::new()),
            packets: AtomicU64::new(0),
            connected: AtomicBool::new(false),
            tap: Mutex::new(None),
        });
        // the mixer's own blocks, straight from the audio engine
        let weak = Arc::downgrade(&pipe);
        let tap = crate::audio_host::tap_mix(key.mix, move |samples, ts| {
            let Some(pipe) = weak.upgrade() else { return };
            let packets = match pipe.encoder.lock().encode(samples, ts as i64) {
                Ok(p) => p,
                Err(e) => {
                    tracing::debug!(%e, "own outputs: sound not encoded");
                    return;
                }
            };
            pipe.deliver(packets);
        });
        *pipe.tap.lock() = Some(tap);
        pipe.connected.store(true, Ordering::Release);
        Ok(pipe)
    }

    fn deliver(&self, packets: Vec<EncodedAudio>) {
        if packets.is_empty() {
            return;
        }
        self.packets.fetch_add(packets.len() as u64, Ordering::Relaxed);
        let subs = self.subs.lock();
        for p in packets {
            let pkt = Arc::new(Pkt { data: p.data, pts: p.pts_ns, dts: p.pts_ns, key: true });
            for s in subs.iter() {
                s.sink.audio(s.track, &pkt);
            }
        }
    }

    /// The format an output joining now writes: without the encoder's priming once it has
    /// started (the priming is long gone).
    fn format_now(&self) -> AudioFormat {
        if self.packets.load(Ordering::Relaxed) == 0 {
            return self.format.clone();
        }
        let f = &self.format;
        AudioFormat::new(f.codec, f.sample_rate, f.channels, f.bitrate_kbps, 0)
    }

    fn subscribe(&self, track: usize, sink: Arc<dyn Sink>, id: u64) {
        self.subs.lock().push(AudioSub { id, track, sink });
    }

    fn unsubscribe(&self, id: u64) {
        self.subs.lock().retain(|s| s.id != id);
    }

    fn others(&self, id: u64) -> usize {
        self.subs.lock().iter().filter(|s| s.id != id).count()
    }

    /// Stops taking the mix's sound and sends what the encoder still holds.
    fn close(&self) {
        let tap = self.tap.lock().take();
        // after this, the tap's thread calls into the pipe no more
        drop(tap);
        if self.connected.swap(false, Ordering::AcqRel) {
            let tail = self.encoder.lock().flush().unwrap_or_default();
            self.deliver(tail);
        }
    }
}

impl Drop for AudioPipe {
    fn drop(&mut self) {
        self.close();
    }
}

// ---------------------------------------------------------------- settings

/// The codec a libobs encoder id stands for (any H.264 encoder, x264 included, is VideoToolbox's
/// here).
fn video_codec(id: &str) -> VideoCodec {
    let id = id.to_ascii_lowercase();
    if id.contains("prores") {
        VideoCodec::ProRes(ProRes::Standard)
    } else if id.contains("hevc") || id.contains("h265") {
        VideoCodec::Hevc { main10: false }
    } else {
        VideoCodec::H264
    }
}

fn audio_codec(id: &str) -> AudioCodec {
    if id.to_ascii_lowercase().contains("opus") { AudioCodec::Opus } else { AudioCodec::Aac }
}

fn audio_kbps(codec: AudioCodec, kbps: u32) -> u32 {
    match codec {
        AudioCodec::Aac => kbps.clamp(64, 320),
        AudioCodec::Opus => kbps.clamp(32, 256),
    }
}

fn file_container(c: Container) -> FileContainer {
    match c {
        Container::HybridMp4 => FileContainer::Mp4,
        Container::Mov => FileContainer::Mov,
        Container::Mkv => FileContainer::Mkv,
    }
}

/// A canvas's size and frame rate as its outputs take them.
fn canvas_shape(canvas: Id) -> Option<(u32, u32, u32, u32)> {
    if let Some(s) = crate::show::snapshot()
        && let Some(c) = s.workspace.canvas(canvas)
    {
        return Some(crate::program::shape_of(c));
    }
    crate::program::output_shape(canvas)
}

/// The track names: the program, then the mixes.
fn track_name(mix: usize) -> String {
    if mix == 0 { "Program".into() } else { format!("Mix {mix}") }
}

/// `folder/<prefix> <date and time>[ <label>].<ext>`, made unique.
fn stamped_path(folder: &Path, prefix: &str, label: Option<&str>, ext: &str) -> PathBuf {
    let when = chrono::Local::now().format("%Y-%m-%d %H-%M-%S");
    let base = match label {
        Some(l) => format!("{prefix} {when} {l}"),
        None => format!("{prefix} {when}"),
    };
    let mut path = folder.join(format!("{base}.{ext}"));
    let mut n = 2;
    while path.exists() {
        path = folder.join(format!("{base} ({n}).{ext}"));
        n += 1;
    }
    path
}

// ---------------------------------------------------------------- recordings

enum RecMsg {
    Video(Arc<Pkt>, Arc<VideoFormat>),
    Audio(usize, Arc<Pkt>),
    Marker(i64, String),
    Pause(bool, i64),
    Split,
    Stop,
}

struct RecordingSink {
    tx: Sender<RecMsg>,
}

impl Sink for RecordingSink {
    fn video(&self, packet: &Arc<Pkt>, format: &Arc<VideoFormat>) {
        let _ = self.tx.send(RecMsg::Video(packet.clone(), format.clone()));
    }
    fn audio(&self, track: usize, packet: &Arc<Pkt>) {
        let _ = self.tx.send(RecMsg::Audio(track, packet.clone()));
    }
}

/// A recording's file writer, on its own thread, so a slow disk never holds up an encoder.
struct Writer {
    key: OutputKey,
    container: FileContainer,
    folder: PathBuf,
    label: Option<String>,
    /// The file to open next.
    path: PathBuf,
    audio: Vec<(AudioFormat, String)>,
    split_ns: Option<i64>,
    video: Weak<VideoPipe>,
    bytes: Arc<AtomicU64>,
    recorder: Option<Recorder>,
    /// Sound waiting for the first picture.
    held: VecDeque<(usize, Arc<Pkt>)>,
    /// Markers made before the first picture.
    markers: Vec<(i64, String)>,
    split: bool,
    paused_at: Option<i64>,
    /// Time spent paused, taken off every packet's times after it.
    offset: i64,
    resume_at: i64,
    want_key: bool,
    last_dts: Vec<i64>,
    failed: bool,
}

impl Writer {
    fn request_keyframe(&self) {
        if let Some(v) = self.video.upgrade() {
            v.encoder.request_keyframe();
        }
    }

    fn tracks(&self, video: &VideoFormat) -> Vec<TrackSpec> {
        let mut t = vec![TrackSpec::new(TrackFormat::Video(video.clone()))];
        for (f, name) in &self.audio {
            t.push(TrackSpec::new(TrackFormat::Audio(f.clone())).named(name.clone()));
        }
        t
    }

    fn open(&mut self, video: &VideoFormat) -> bool {
        let options = RecorderOptions { title: self.label.clone(), ..RecorderOptions::default() };
        match Recorder::create(&self.path, self.container, self.tracks(video), options) {
            Ok(mut r) => {
                for (at, title) in self.markers.drain(..) {
                    r.add_marker(at, title);
                }
                self.recorder = Some(r);
                self.last_dts = vec![i64::MIN; self.audio.len() + 1];
                let path = self.path.clone();
                runtime().update(self.key, |s| {
                    if !matches!(s.state, OutputState::Active { .. }) {
                        s.state = OutputState::Active { since: Instant::now() };
                    }
                    s.path = Some(path);
                });
                true
            }
            Err(e) => {
                self.fail(format!("The recording couldn't start: {e}"));
                false
            }
        }
    }

    fn fail(&mut self, why: String) {
        tracing::error!(%why, "own outputs: recording failed");
        self.failed = true;
        if let Some(r) = self.recorder.take() {
            let _ = r.finish();
        }
        runtime().update(self.key, |s| s.state = OutputState::Failed(why));
    }

    fn write(&mut self, track: usize, p: &Pkt, layout: NalLayout) {
        let Some(r) = self.recorder.as_mut() else { return };
        let (mut pts, mut dts) = (p.pts - self.offset, p.dts - self.offset);
        // times after a pause may meet the ones before it: kept rising
        let last = self.last_dts[track];
        if dts <= last {
            dts = last + 1;
        }
        pts = pts.max(dts);
        self.last_dts[track] = dts;
        let packet = Packet { data: &p.data, layout, pts_ns: pts, dts_ns: dts, keyframe: p.key };
        match r.write(track, packet) {
            Ok(()) => {
                self.bytes.fetch_add(p.data.len() as u64, Ordering::Relaxed);
            }
            Err(e) => {
                let why = match &e {
                    lightair_encode::Error::Io(io) if io.raw_os_error() == Some(libc::ENOSPC) => {
                        "The disk is full.".to_string()
                    }
                    _ => format!("The recording stopped: {e}"),
                };
                self.fail(why);
            }
        }
    }

    fn video(&mut self, p: Arc<Pkt>, format: Arc<VideoFormat>) {
        if self.failed || self.paused_at.is_some() {
            return;
        }
        if self.want_key {
            if !p.key || p.pts < self.resume_at {
                return;
            }
            self.want_key = false;
        }
        if self.recorder.is_none() {
            if !p.key || !self.open(&format) {
                return;
            }
            let layout = if format.codec.is_nal() { NalLayout::LengthPrefixed } else { NalLayout::Raw };
            self.write(0, &p, layout);
            for (track, a) in std::mem::take(&mut self.held) {
                self.write(track + 1, &a, NalLayout::Raw);
            }
            return;
        }
        if self.split && p.key {
            self.rotate(&format);
            if self.failed {
                return;
            }
        }
        let layout = if format.codec.is_nal() { NalLayout::LengthPrefixed } else { NalLayout::Raw };
        self.write(0, &p, layout);
        if !self.split
            && let (Some(limit), Some(r)) = (self.split_ns, self.recorder.as_ref())
            && r.duration_ns() >= limit
        {
            self.split = true;
            self.request_keyframe();
        }
    }

    fn audio(&mut self, track: usize, p: Arc<Pkt>) {
        if self.failed || self.paused_at.is_some() || p.pts < self.resume_at {
            return;
        }
        if self.recorder.is_none() {
            self.held.push_back((track, p));
            while let (Some(first), Some(last)) = (self.held.front(), self.held.back()) {
                if last.1.pts - first.1.pts <= PREROLL_NS {
                    break;
                }
                self.held.pop_front();
            }
            return;
        }
        self.write(track + 1, &p, NalLayout::Raw);
    }

    /// Ends the file and goes on in a new one, from this keyframe.
    fn rotate(&mut self, format: &VideoFormat) {
        self.split = false;
        if let Some(r) = self.recorder.take()
            && let Err(e) = r.finish()
        {
            tracing::error!(%e, "own outputs: a split file didn't finish");
        }
        self.path = stamped_path(&self.folder, "Lightair", self.label.as_deref(), self.container.extension());
        if self.open(format) {
            runtime().update(self.key, |s| s.files += 1);
        }
    }

    fn marker(&mut self, at: i64, title: String) {
        let at = at - self.offset;
        match self.recorder.as_mut() {
            Some(r) => r.add_marker(at, title),
            None => self.markers.push((at, title)),
        }
    }

    fn pause(&mut self, pause: bool, at: i64) {
        if pause {
            if self.paused_at.is_none() {
                self.paused_at = Some(at);
            }
        } else if let Some(since) = self.paused_at.take() {
            self.offset += (at - since).max(0);
            self.resume_at = at;
            // pictures go on from a keyframe
            self.want_key = true;
            self.request_keyframe();
        }
    }

    fn finish(&mut self) {
        if let Some(r) = self.recorder.take() {
            match r.finish() {
                Ok(done) => {
                    tracing::info!(path = %done.path.display(), bytes = done.bytes, "own outputs: recording finished");
                    runtime().update(self.key, |s| {
                        s.bytes = done.bytes;
                        s.state = OutputState::Idle;
                    });
                    return;
                }
                Err(e) => {
                    runtime().update(self.key, |s| {
                        s.state = OutputState::Failed(format!("The recording didn't finish properly: {e}"))
                    });
                    return;
                }
            }
        }
        if !self.failed {
            runtime().update(self.key, |s| s.state = OutputState::Idle);
        }
    }

    fn run(mut self, rx: Receiver<RecMsg>) {
        while let Ok(msg) = rx.recv() {
            match msg {
                RecMsg::Video(p, f) => self.video(p, f),
                RecMsg::Audio(track, p) => self.audio(track, p),
                RecMsg::Marker(at, title) => self.marker(at, title),
                RecMsg::Pause(pause, at) => self.pause(pause, at),
                RecMsg::Split => {
                    if self.recorder.is_some() {
                        self.split = true;
                        self.request_keyframe();
                    }
                }
                RecMsg::Stop => break,
            }
        }
        self.finish();
    }
}

struct RecordingOut {
    video: Arc<VideoPipe>,
    audio: Vec<Arc<AudioPipe>>,
    sub: u64,
    tx: Sender<RecMsg>,
    thread: Option<JoinHandle<()>>,
    bytes: Arc<AtomicU64>,
    stopping: bool,
    /// The canvas's dropped and the encoder's skipped frames when it began.
    skipped_base: u32,
}

impl RecordingOut {
    fn finished(&self) -> bool {
        self.thread.as_ref().is_none_or(|t| t.is_finished())
    }
}

// ---------------------------------------------------------------- streams

/// A destination's sender.
enum Out {
    Rtmp(RtmpSender),
    /// Several picture tracks in one RTMP stream (Enhanced Broadcasting).
    Multi(RtmpSender),
    Srt(Mutex<SrtOut>),
    Udp(Mutex<UdpOut>),
    Whip(Mutex<WhipSender>),
}

struct SrtOut {
    mux: Muxer,
    sender: SrtSender,
}

/// MPEG-TS over plain UDP (`udp://host:port`, multicast too): seven TS packets a datagram, as
/// players and hardware decoders expect. No connection, so it's live from the start.
struct UdpOut {
    mux: Muxer,
    socket: std::net::UdpSocket,
    to: std::net::SocketAddr,
    sent: u64,
    /// Datagrams the system didn't take.
    dropped: u64,
    warned: bool,
}

impl UdpOut {
    fn send(&mut self, packets: &[u8]) {
        for datagram in packets.chunks(188 * 7) {
            match self.socket.send_to(datagram, self.to) {
                Ok(n) => self.sent += n as u64,
                Err(e) => {
                    self.dropped += 1;
                    if !self.warned {
                        self.warned = true;
                        tracing::warn!(%e, to = %self.to, "own outputs: UDP not sent");
                    }
                }
            }
        }
    }
}

/// Where a `udp://host:port[?options]` address sends to (`@` before the host, as some players
/// write a multicast group, is allowed).
fn udp_target(server: &str) -> Result<std::net::SocketAddr, String> {
    use std::net::ToSocketAddrs;
    let rest = server.trim();
    let rest = rest.get(6..).unwrap_or_default();
    let host = rest.split(['?', '/']).next().unwrap_or_default().trim_start_matches('@');
    host.to_socket_addrs()
        .ok()
        .and_then(|mut a| a.next())
        .ok_or_else(|| format!("{host} isn't an address to send to (udp://host:port)."))
}

struct Stats {
    connected: bool,
    sent_bytes: u64,
    dropped: u64,
    error: Option<String>,
    retry_in: Option<Duration>,
}

impl Out {
    fn parameter_sets(&self, track: u8, format: &VideoFormat) {
        let sets = &format.parameter_sets;
        match self {
            Out::Multi(s) => s.track_config_record(track as usize, &format.config),
            // HEVC over Enhanced RTMP takes the `hvcC` record as the encoder made it
            Out::Rtmp(s) if matches!(format.codec, VideoCodec::Hevc { .. }) => s.config_record(&format.config),
            Out::Rtmp(s) => s.parameter_sets(sets),
            Out::Srt(s) => s.lock().mux.parameter_sets(sets.to_vec()),
            Out::Udp(s) => s.lock().mux.parameter_sets(sets.to_vec()),
            Out::Whip(s) => s.lock().parameter_sets(sets),
        }
    }

    fn video(&self, track: u8, p: &Pkt, pts_us: u64, dts_us: u64) {
        match self {
            Out::Multi(s) => s.track_video(track as usize, &p.data, NalFormat::LengthPrefixed, pts_us, dts_us, p.key),
            Out::Rtmp(s) => s.video(&p.data, NalFormat::LengthPrefixed, pts_us, dts_us, p.key),
            Out::Srt(s) => {
                let mut o = s.lock();
                if o.sender.begin_picture(p.key) {
                    let packets = o.mux.video(&p.data, NalFormat::LengthPrefixed, pts_us, Some(dts_us), p.key);
                    o.sender.write(&packets);
                    o.sender.end_frame();
                }
            }
            Out::Udp(s) => {
                let mut o = s.lock();
                let packets = o.mux.video(&p.data, NalFormat::LengthPrefixed, pts_us, Some(dts_us), p.key);
                o.send(&packets);
            }
            Out::Whip(s) => s.lock().video(&p.data, false, pts_us, p.key),
        }
    }

    /// Sound track `track`'s packet: RTMP carries several (Twitch's VOD track after the live
    /// ones), the others the first only.
    fn audio(&self, track: u8, data: &[u8], pts_us: u64) {
        if track > 0 && !matches!(self, Out::Rtmp(_) | Out::Multi(_)) {
            return;
        }
        match self {
            Out::Rtmp(s) | Out::Multi(s) => s.track_audio(track as usize, data, pts_us),
            Out::Srt(s) => {
                let mut o = s.lock();
                if !o.sender.waiting_for_keyframe() {
                    let packets = o.mux.audio(data, pts_us);
                    o.sender.write(&packets);
                    o.sender.flush();
                }
            }
            Out::Udp(s) => {
                let mut o = s.lock();
                let packets = o.mux.audio(data, pts_us);
                o.send(&packets);
            }
            Out::Whip(s) => s.lock().audio(data, pts_us),
        }
    }

    fn stats(&self) -> Stats {
        match self {
            Out::Rtmp(s) | Out::Multi(s) => {
                let s = s.stats();
                Stats {
                    connected: s.connected,
                    sent_bytes: s.sent_bytes,
                    dropped: s.dropped,
                    error: s.error,
                    retry_in: s.retry_in,
                }
            }
            Out::Srt(s) => {
                let s = s.lock().sender.stats();
                Stats {
                    connected: s.connected,
                    sent_bytes: s.sent_bytes,
                    dropped: s.dropped,
                    error: s.error,
                    retry_in: s.retry_in,
                }
            }
            Out::Udp(s) => {
                let s = s.lock();
                Stats { connected: true, sent_bytes: s.sent, dropped: s.dropped, error: None, retry_in: None }
            }
            Out::Whip(s) => {
                let s = s.lock().stats();
                Stats {
                    connected: s.connected,
                    sent_bytes: s.sent_bytes,
                    dropped: s.dropped,
                    error: s.error,
                    retry_in: s.retry_in,
                }
            }
        }
    }

    fn take_keyframe_request(&self) -> bool {
        match self {
            Out::Rtmp(s) | Out::Multi(s) => s.take_keyframe_request(),
            Out::Srt(s) => s.lock().sender.take_keyframe_request(),
            Out::Udp(_) => false,
            Out::Whip(s) => s.lock().take_keyframe_request(),
        }
    }

    /// Bytes waiting for the network, where the sender counts them.
    fn queued_bytes(&self) -> Option<u64> {
        match self {
            Out::Rtmp(s) | Out::Multi(s) => Some(s.queued_bytes()),
            Out::Srt(s) => Some(s.lock().sender.queued() as u64 * lightair_feed::srt::DATAGRAM as u64),
            Out::Udp(_) => None,
            Out::Whip(s) => Some(s.lock().queued_bytes()),
        }
    }

    fn set_bitrate(&self, kbps: u32) {
        if let Out::Rtmp(s) | Out::Multi(s) = self {
            s.set_bitrate(kbps);
        }
    }
}

/// What a destination's sender is handed; pictures and formats by picture track (0 when it
/// has one).
enum Item {
    /// The picture's format (its parameter sets and decoder configuration record) from now on.
    Format(u8, Arc<VideoFormat>),
    Video(u8, Arc<Pkt>),
    /// A sound track's packet (0 the first).
    Audio(u8, Arc<Pkt>),
    End,
}

struct StreamSink {
    tx: Sender<(Instant, Item)>,
    delay: Duration,
    /// The picture track it is (0 when the destination has one).
    track: u8,
    /// The format whose parameter sets were sent last.
    format: Mutex<Option<Arc<VideoFormat>>>,
}

impl Sink for StreamSink {
    fn video(&self, packet: &Arc<Pkt>, format: &Arc<VideoFormat>) {
        let due = Instant::now() + self.delay;
        {
            let mut f = self.format.lock();
            if f.as_ref().is_none_or(|x| !Arc::ptr_eq(x, format)) {
                *f = Some(format.clone());
                let _ = self.tx.send((due, Item::Format(self.track, format.clone())));
            }
        }
        let _ = self.tx.send((due, Item::Video(self.track, packet.clone())));
    }
    fn audio(&self, track: usize, packet: &Arc<Pkt>) {
        if let Ok(track) = u8::try_from(track) {
            let _ = self.tx.send((Instant::now() + self.delay, Item::Audio(track, packet.clone())));
        }
    }
}

/// Hands a destination's packets to its sender, each when its delay is up.
fn pump(rx: Receiver<(Instant, Item)>, out: Arc<Out>, base_ns: i64, abort: Arc<AtomicBool>, frames: Arc<AtomicU64>) {
    let us = |ns: i64| ((ns - base_ns).max(0) / 1000) as u64;
    'items: while let Ok((due, item)) = rx.recv() {
        loop {
            if abort.load(Ordering::Acquire) {
                break 'items;
            }
            let now = Instant::now();
            if due <= now {
                break;
            }
            std::thread::sleep((due - now).min(Duration::from_millis(50)));
        }
        match item {
            Item::Format(track, format) => out.parameter_sets(track, &format),
            Item::Video(track, p) => {
                out.video(track, &p, us(p.pts), us(p.dts));
                frames.fetch_add(1, Ordering::Relaxed);
            }
            Item::Audio(track, p) => out.audio(track, &p.data, us(p.pts)),
            Item::End => break,
        }
    }
}

/// The bitrate a destination can carry now, when it follows its connection: down a fifth when
/// more than a second waits, up a tenth after ten calm seconds; between a quarter of the set
/// bitrate and the set bitrate. With several picture tracks it's their total, shared out by
/// `levels`.
struct Adapt {
    max: u32,
    target: u32,
    calm_since: Option<Instant>,
    last_step: Instant,
    /// Each picture track's bitrate at each level, lowest first (one track: none and the set
    /// bitrate).
    levels: Vec<Vec<u32>>,
}

impl Adapt {
    fn new(max: u32, levels: Vec<Vec<u32>>) -> Adapt {
        Adapt { max, target: max, calm_since: None, last_step: Instant::now(), levels }
    }

    /// Each picture track's bitrate for `total` kbps together: between the two levels around it,
    /// each track the same part of the way from one to the next; below the lowest level, that
    /// level scaled down.
    fn split(&self, total: u32) -> Vec<u32> {
        let sum = |l: &Vec<u32>| l.iter().map(|&b| b as u64).sum::<u64>();
        let Some(lowest) = self.levels.first() else { return vec![total] };
        let rates: Vec<u64> = match self.levels.iter().rposition(|l| sum(l) <= total as u64) {
            None => {
                let all = sum(lowest).max(1);
                lowest.iter().map(|&b| b as u64 * total as u64 / all).collect()
            }
            Some(at) => {
                let (base, upper) = (&self.levels[at], &self.levels[(at + 1).min(self.levels.len() - 1)]);
                let deltas: Vec<i64> = base.iter().zip(upper).map(|(&b, &u)| u as i64 - b as i64).collect();
                let room = (total as u64 - sum(base)) as f64;
                let part = (room / deltas.iter().sum::<i64>().max(1) as f64).min(1.0);
                base.iter()
                    .zip(&deltas)
                    .map(|(&b, &d)| (b as i64 + (d as f64 * part).round() as i64).max(0) as u64)
                    .collect()
            }
        };
        rates.into_iter().map(|r| r.clamp(50, u32::MAX as u64) as u32).collect()
    }

    fn step(&mut self, queued_bytes: u64, audio_kbps: u32) -> u32 {
        let now = Instant::now();
        let total = (self.target + audio_kbps).max(1) as u64;
        let waiting_ms = queued_bytes * 8 / total;
        let floor = (self.max / 4).max(300).min(self.max);
        if waiting_ms > 1000 && now.duration_since(self.last_step) >= Duration::from_secs(1) {
            self.target = (self.target * 4 / 5).max(floor);
            self.calm_since = None;
            self.last_step = now;
        } else if waiting_ms < 100 {
            let since = *self.calm_since.get_or_insert(now);
            if now.duration_since(since) >= Duration::from_secs(10) && self.target < self.max {
                self.target = (self.target + self.max / 10).min(self.max);
                self.calm_since = Some(now);
                self.last_step = now;
            }
        } else {
            self.calm_since = None;
        }
        self.target
    }
}

/// Watches a destination: its state for the interface, keyframes it asks for, reconnects and
/// their limit, the adaptive bitrate.
struct Watch {
    key: OutputKey,
    out: Arc<Out>,
    /// Its video encoders: one, or a multitrack stream's tracks (the first the main one).
    videos: Vec<Weak<VideoPipe>>,
    /// Its sound tracks' encoders, in track order.
    audio: Vec<Weak<AudioPipe>>,
    sub: u64,
    retries: u32,
    adapt: Option<Adapt>,
    /// Every sound track's bitrate together.
    audio_kbps: u32,
    frames: Arc<AtomicU64>,
    stopping: Arc<AtomicBool>,
    abort: Arc<AtomicBool>,
    pump: JoinHandle<()>,
}

impl Watch {
    fn give_up(&self, why: String) {
        tracing::warn!(%why, "own outputs: stream stopped");
        self.abort.store(true, Ordering::Release);
        for v in self.videos.iter().filter_map(Weak::upgrade) {
            v.unsubscribe(self.sub);
        }
        for a in self.audio.iter().filter_map(Weak::upgrade) {
            a.unsubscribe(self.sub);
        }
        runtime().update(self.key, |s| {
            s.state = OutputState::Failed(why);
            s.reconnect = None;
        });
    }

    fn run(mut self) {
        let mut ever = false;
        let mut adapt_at = Instant::now();
        loop {
            if self.pump.is_finished() {
                break;
            }
            if self.out.take_keyframe_request()
                && let Some(v) = self.videos.first().and_then(Weak::upgrade)
            {
                v.ask_keyframe();
            }
            let s = self.out.stats();
            let frames = self.frames.load(Ordering::Relaxed);
            let stopping = self.stopping.load(Ordering::Acquire);
            let mut give_up = None;
            runtime().update(self.key, |st| {
                st.bytes = s.sent_bytes;
                st.dropped_frames = s.dropped.min(i32::MAX as u64) as i32;
                st.total_frames = frames.min(i32::MAX as u64) as i32;
                if stopping || matches!(st.state, OutputState::Failed(_)) {
                    return;
                }
                if s.connected {
                    let since = match st.state {
                        OutputState::Reconnecting { since } | OutputState::Active { since } => since,
                        _ => Instant::now(),
                    };
                    st.state = OutputState::Active { since };
                    st.reconnect = None;
                    ever = true;
                } else if ever {
                    let since = match st.state {
                        OutputState::Active { since } | OutputState::Reconnecting { since } => since,
                        _ => Instant::now(),
                    };
                    st.state = OutputState::Reconnecting { since };
                    let next_at = Instant::now() + s.retry_in.unwrap_or_default();
                    let info = match st.reconnect {
                        None => ReconnectInfo { attempt: 1, retries: self.retries, next_at },
                        // a new wait: the try before it failed
                        Some(r) if next_at > r.next_at + Duration::from_millis(500) => {
                            ReconnectInfo { attempt: r.attempt + 1, retries: self.retries, next_at }
                        }
                        Some(r) => r,
                    };
                    if info.attempt > self.retries {
                        give_up = Some("The connection to the server was lost.".to_string());
                    }
                    st.reconnect = Some(info);
                } else if let Some(e) = &s.error {
                    give_up = Some(format!(
                        "Couldn't connect to the server. Check the server address and your connection. ({e})"
                    ));
                }
            });
            if let Some(why) = give_up {
                self.give_up(why);
                break;
            }
            if let Some(a) = self.adapt.as_mut()
                && adapt_at.elapsed() >= Duration::from_secs(1)
                && let Some(queued) = self.out.queued_bytes()
            {
                adapt_at = Instant::now();
                let before = a.target;
                let target = a.step(queued, self.audio_kbps);
                if target != before {
                    for (v, kbps) in self.videos.iter().zip(a.split(target)) {
                        if let Some(v) = v.upgrade() {
                            v.encoder.set_bitrate(kbps);
                        }
                    }
                    self.out.set_bitrate(target + self.audio_kbps);
                }
                let max = a.max;
                runtime().update(self.key, |st| {
                    st.current_kbps = target;
                    st.target_kbps = max;
                });
            }
            std::thread::sleep(WATCH);
        }
        let _ = self.pump.join();
        runtime().update(self.key, |st| {
            if !matches!(st.state, OutputState::Failed(_)) {
                st.state = OutputState::Idle;
            }
            st.reconnect = None;
        });
    }
}

struct StreamOut {
    video: Arc<VideoPipe>,
    /// A multitrack stream's other tracks, in order.
    extra: Vec<Arc<VideoPipe>>,
    /// Its sound tracks' encoders, in track order (the first the main one).
    audio: Vec<Arc<AudioPipe>>,
    sub: u64,
    tx: Sender<(Instant, Item)>,
    delay: Duration,
    stopping: Arc<AtomicBool>,
    abort: Arc<AtomicBool>,
    watch: Option<JoinHandle<()>>,
    skipped_base: u32,
}

impl StreamOut {
    /// Its video encoders, the main one first.
    fn videos(&self) -> Vec<Arc<VideoPipe>> {
        std::iter::once(&self.video).chain(&self.extra).cloned().collect()
    }

    fn finished(&self) -> bool {
        self.watch.as_ref().is_none_or(|t| t.is_finished())
    }
}

/// Sends from the local address `ip` (`StreamNet::bind_ip`); a socket of the other family is
/// left to the system.
#[cfg(unix)]
fn bind_to(ip: std::net::IpAddr) -> Bind {
    Bind::new(move |fd, v6| {
        let r = match ip {
            std::net::IpAddr::V4(a) if !v6 => {
                let sa = libc::sockaddr_in {
                    sin_len: std::mem::size_of::<libc::sockaddr_in>() as u8,
                    sin_family: libc::AF_INET as u8,
                    sin_port: 0,
                    sin_addr: libc::in_addr { s_addr: u32::from_ne_bytes(a.octets()) },
                    sin_zero: [0; 8],
                };
                unsafe {
                    libc::bind(
                        fd,
                        &sa as *const _ as *const libc::sockaddr,
                        std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t,
                    )
                }
            }
            std::net::IpAddr::V6(a) if v6 => {
                let sa = libc::sockaddr_in6 {
                    sin6_len: std::mem::size_of::<libc::sockaddr_in6>() as u8,
                    sin6_family: libc::AF_INET6 as u8,
                    sin6_port: 0,
                    sin6_flowinfo: 0,
                    sin6_addr: libc::in6_addr { s6_addr: a.octets() },
                    sin6_scope_id: 0,
                };
                unsafe {
                    libc::bind(
                        fd,
                        &sa as *const _ as *const libc::sockaddr,
                        std::mem::size_of::<libc::sockaddr_in6>() as libc::socklen_t,
                    )
                }
            }
            _ => 0,
        };
        if r == 0 { Ok(()) } else { Err(std::io::Error::last_os_error()) }
    })
}

/// Sends from the local address `ip` (`StreamNet::bind_ip`); a socket of the other family is
/// left to the system.
#[cfg(windows)]
fn bind_to(ip: std::net::IpAddr) -> Bind {
    /// WinSock's `SOCKADDR_IN` and `SOCKADDR_IN6`, port and address in network order.
    #[repr(C)]
    struct V4 {
        family: u16,
        port: u16,
        addr: [u8; 4],
        zero: [u8; 8],
    }
    #[repr(C)]
    struct V6 {
        family: u16,
        port: u16,
        flowinfo: u32,
        addr: [u8; 16],
        scope_id: u32,
    }
    const AF_INET: u16 = 2;
    const AF_INET6: u16 = 23;
    #[link(name = "ws2_32")]
    unsafe extern "system" {
        fn bind(socket: usize, name: *const std::ffi::c_void, length: i32) -> i32;
    }
    Bind::new(move |socket, v6| {
        let socket = socket as usize;
        let r = match ip {
            std::net::IpAddr::V4(a) if !v6 => {
                let sa = V4 { family: AF_INET, port: 0, addr: a.octets(), zero: [0; 8] };
                // SAFETY: `sa` is a whole SOCKADDR_IN, alive for the call.
                unsafe { bind(socket, &sa as *const V4 as *const _, std::mem::size_of::<V4>() as i32) }
            }
            std::net::IpAddr::V6(a) if v6 => {
                let sa = V6 { family: AF_INET6, port: 0, flowinfo: 0, addr: a.octets(), scope_id: 0 };
                // SAFETY: `sa` is a whole SOCKADDR_IN6, alive for the call.
                unsafe { bind(socket, &sa as *const V6 as *const _, std::mem::size_of::<V6>() as i32) }
            }
            _ => 0,
        };
        if r == 0 { Ok(()) } else { Err(std::io::Error::last_os_error()) }
    })
}

// ---------------------------------------------------------------- replay buffers

struct Ring {
    video: VecDeque<Arc<Pkt>>,
    audio: VecDeque<Arc<Pkt>>,
    format: Option<Arc<VideoFormat>>,
}

struct ClipSink {
    ring: Mutex<Ring>,
    keep_ns: i64,
}

impl Sink for ClipSink {
    fn video(&self, packet: &Arc<Pkt>, format: &Arc<VideoFormat>) {
        let mut r = self.ring.lock();
        if r.format.as_ref().is_none_or(|f| !Arc::ptr_eq(f, format)) {
            // another format (the encoder was opened again): what came before can't join it
            if r.format.as_ref().is_some_and(|f| **f != **format) {
                r.video.clear();
            }
            r.format = Some(format.clone());
        }
        if r.video.is_empty() && !packet.key {
            return;
        }
        r.video.push_back(packet.clone());
        // whole groups of pictures, from the newest keyframe at least `keep` old
        let cutoff = packet.pts - self.keep_ns;
        while let Some(i) = r.video.iter().skip(1).position(|p| p.key).map(|i| i + 1) {
            if r.video[i].pts > cutoff {
                break;
            }
            r.video.drain(..i);
        }
        let first = r.video.front().map_or(packet.pts, |p| p.pts);
        while r.audio.front().is_some_and(|a| a.pts < first - 1_000_000_000) {
            r.audio.pop_front();
        }
    }
    fn audio(&self, track: usize, packet: &Arc<Pkt>) {
        if track != 0 {
            return;
        }
        let mut r = self.ring.lock();
        r.audio.push_back(packet.clone());
        // without pictures for a while, sound is kept as long
        while r.audio.front().is_some_and(|a| packet.pts - a.pts > self.keep_ns + 2_000_000_000) {
            r.audio.pop_front();
        }
    }
}

struct ClipsOut {
    video: Arc<VideoPipe>,
    audio: Arc<AudioPipe>,
    sub: u64,
    sink: Arc<ClipSink>,
    folder: PathBuf,
    label: Option<String>,
    audio_format: AudioFormat,
}

/// Writes a replay buffer's packets as a clip (own thread); reports it as libobs's "saved"
/// signal does.
fn write_clip(
    canvas: Id,
    path: PathBuf,
    video: Vec<Arc<Pkt>>,
    audio: Vec<Arc<Pkt>>,
    vf: Arc<VideoFormat>,
    af: AudioFormat,
) {
    let container = if vf.codec.is_nal() { FileContainer::Mp4 } else { FileContainer::Mov };
    let tracks = vec![
        TrackSpec::new(TrackFormat::Video((*vf).clone())),
        TrackSpec::new(TrackFormat::Audio(af)).named(track_name(0)),
    ];
    let options = RecorderOptions { sync_fragments: false, ..RecorderOptions::default() };
    let layout = if vf.codec.is_nal() { NalLayout::LengthPrefixed } else { NalLayout::Raw };
    let result = (|| -> Result<lightair_encode::Finished, lightair_encode::Error> {
        let mut rec = Recorder::create(&path, container, tracks, options)?;
        // in decode order across the two tracks
        let (mut v, mut a) = (video.iter().peekable(), audio.iter().peekable());
        loop {
            let take_video = match (v.peek(), a.peek()) {
                (Some(x), Some(y)) => x.dts <= y.pts,
                (Some(_), None) => true,
                (None, Some(_)) => false,
                (None, None) => break,
            };
            if take_video {
                let p = v.next().unwrap();
                rec.write(0, Packet { data: &p.data, layout, pts_ns: p.pts, dts_ns: p.dts, keyframe: p.key })?;
            } else {
                let p = a.next().unwrap();
                rec.write(1, Packet::audio(&p.data, p.pts))?;
            }
        }
        rec.finish()
    })();
    match result {
        Ok(done) => {
            tracing::info!(path = %done.path.display(), "own outputs: clip saved");
            *crate::outputs::LAST_CLIP.lock() = Some((canvas, std::time::Instant::now()));
            runtime().update(OutputKey::Clips(canvas), |s| {
                s.path = Some(done.path);
                s.saved_clips += 1;
            });
        }
        Err(e) => {
            tracing::error!(%e, "own outputs: the clip couldn't be written");
            let _ = std::fs::remove_file(&path);
        }
    }
}

// ---------------------------------------------------------------- the outputs

#[derive(Default)]
struct State {
    videos: Vec<Arc<VideoPipe>>,
    audios: Vec<Arc<AudioPipe>>,
    recordings: HashMap<Id, RecordingOut>,
    streams: HashMap<Id, StreamOut>,
    clips: HashMap<Id, ClipsOut>,
    /// The virtual camera's tap.
    camera: Option<u64>,
    /// The scene the compositor draws for the virtual camera alone (its "one scene" mode), by
    /// the key its frames are handed over under.
    camera_scene: Option<Id>,
    rtsp: Option<RtspOut>,
}

static STATE: Mutex<Option<State>> = Mutex::new(None);

fn with<R>(f: impl FnOnce(&mut State) -> R) -> R {
    f(STATE.lock().get_or_insert_with(State::default))
}

impl State {
    /// A video encoder for `canvas` with these settings: a running one when they match (and
    /// `exclusive` is false), else a new one.
    fn video_pipe(
        &mut self,
        canvas: Id,
        enc: &EncoderConfig,
        reorder: bool,
        exclusive: bool,
    ) -> Result<Arc<VideoPipe>, String> {
        self.video_pipe_in(canvas, enc, reorder, exclusive, None)
    }

    /// [`State::video_pipe`], a track of a multitrack stream when `track` is given (its own,
    /// its keyframes on the stream's beat, with the plan's profile and ceiling).
    fn video_pipe_in(
        &mut self,
        canvas: Id,
        enc: &EncoderConfig,
        reorder: bool,
        exclusive: bool,
        track: Option<TrackSetup>,
    ) -> Result<Arc<VideoPipe>, String> {
        let exclusive = exclusive || track.is_some();
        let (cw, ch, num, den) = canvas_shape(canvas).ok_or_else(|| "The canvas isn't ready yet.".to_string())?;
        let (w, h) = enc.scaled.unwrap_or((cw, ch));
        let codec = match (video_codec(&enc.video_encoder), &track) {
            (VideoCodec::Hevc { .. }, Some(t)) => VideoCodec::Hevc { main10: t.tune.main10 },
            (codec, _) => codec,
        };
        let prores = matches!(codec, VideoCodec::ProRes(_));
        let key = VideoKey {
            codec,
            width: w.max(2) & !1,
            height: h.max(2) & !1,
            divisor: enc.fps_divisor.max(1),
            kbps: if prores { 0 } else { enc.video_bitrate_kbps.max(100) },
            cbr: enc.constant_bitrate,
            keyint: enc.keyint_sec.max(1),
            reorder: reorder && !prores,
            profile: track.as_ref().map_or(H264Profile::High, |t| t.tune.profile),
            peak: track.as_ref().and_then(|t| t.tune.peak_kbps).filter(|_| !enc.constant_bitrate),
            owner: if exclusive { next_id() } else { 0 },
        };
        if let Some(p) = self.videos.iter().find(|p| p.key == key && *p.canvas.lock() == canvas) {
            return Ok(p.clone());
        }
        // on the stream's beat, the encoder's own keyframes never come first
        let keyframe_interval_s = if track.is_some() { key.keyint as f32 * 2.0 + 1.0 } else { key.keyint as f32 };
        let settings = VideoSettings {
            codec,
            width: key.width,
            height: key.height,
            fps_num: num,
            fps_den: den.saturating_mul(key.divisor),
            bitrate_kbps: key.kbps,
            max_bitrate_kbps: if key.cbr { Some(key.kbps) } else { key.peak.map(|p| p.max(key.kbps)) },
            keyframe_interval_s,
            realtime: true,
            allow_reordering: key.reorder,
            input: InputFormat::Bgra,
            color: Color::BT709,
            hardware: Hardware::Prefer,
            h264_profile: key.profile,
        };
        let pipe = VideoPipe::open(key, canvas, settings, track.map(|t| (t.gop, t.place)))?;
        self.videos.push(pipe.clone());
        add_to_feed(canvas, pipe.clone(), (num, den));
        Ok(pipe)
    }

    fn audio_pipe(&mut self, mix: usize, codec: AudioCodec, kbps: u32) -> Result<Arc<AudioPipe>, String> {
        let key = AudioKey { mix: mix.min(5), codec, kbps: audio_kbps(codec, kbps) };
        if let Some(p) = self.audios.iter().find(|p| p.key == key) {
            return Ok(p.clone());
        }
        let pipe = AudioPipe::open(key)?;
        self.audios.push(pipe.clone());
        Ok(pipe)
    }

    /// Takes output `sub` off `pipe`; the encoder goes with its last output, its last pictures
    /// sent first.
    fn release_video(&mut self, pipe: &Arc<VideoPipe>, sub: u64) {
        if pipe.others(sub) == 0 {
            self.videos.retain(|p| !Arc::ptr_eq(p, pipe));
            remove_from_feed(&(pipe.clone() as Arc<dyn Consumer>));
            pipe.encoder.flush();
        }
        pipe.unsubscribe(sub);
    }

    fn release_audio(&mut self, pipe: &Arc<AudioPipe>, sub: u64) {
        if pipe.others(sub) == 0 {
            self.audios.retain(|p| !Arc::ptr_eq(p, pipe));
            pipe.close();
        }
        pipe.unsubscribe(sub);
    }

    /// Encoders no output uses any more (a destination that gave up) go; outputs that have
    /// finished are let go.
    fn cleanup(&mut self) {
        let finished: Vec<Id> =
            self.recordings.iter().filter(|(_, r)| r.stopping && r.finished()).map(|(id, _)| *id).collect();
        for id in finished {
            if let Some(mut r) = self.recordings.remove(&id)
                && let Some(t) = r.thread.take()
            {
                let _ = t.join();
            }
        }
        let finished: Vec<Id> = self.streams.iter().filter(|(_, s)| s.finished()).map(|(id, _)| *id).collect();
        for id in finished {
            if let Some(mut s) = self.streams.remove(&id) {
                for v in s.videos() {
                    self.release_video(&v, s.sub);
                }
                for a in &s.audio {
                    self.release_audio(a, s.sub);
                }
                if let Some(t) = s.watch.take() {
                    let _ = t.join();
                }
            }
        }
        let unused: Vec<Arc<VideoPipe>> = self.videos.iter().filter(|p| p.subs.lock().is_empty()).cloned().collect();
        for p in unused {
            self.release_video(&p, 0);
        }
        let unused: Vec<Arc<AudioPipe>> = self.audios.iter().filter(|p| p.subs.lock().is_empty()).cloned().collect();
        for p in unused {
            self.release_audio(&p, 0);
        }
    }

    fn stop_recording(&mut self, canvas: Id) {
        let Some(r) = self.recordings.get_mut(&canvas) else { return };
        if r.stopping {
            return;
        }
        r.stopping = true;
        runtime().update(OutputKey::Recording(canvas), |s| {
            if s.state.is_running() {
                s.state = OutputState::Stopping
            }
        });
        let (video, audio, sub, tx) = (r.video.clone(), r.audio.clone(), r.sub, r.tx.clone());
        self.release_video(&video, sub);
        for a in &audio {
            self.release_audio(a, sub);
        }
        let _ = tx.send(RecMsg::Stop);
    }

    fn stop_stream(&mut self, id: Id, force: bool) {
        let Some(s) = self.streams.get_mut(&id) else { return };
        if force {
            s.abort.store(true, Ordering::Release);
        }
        if s.stopping.swap(true, Ordering::AcqRel) {
            return;
        }
        let delay = s.delay;
        runtime().update(OutputKey::Stream(id), |st| {
            if st.state.is_running() && st.state != OutputState::Stopping {
                st.state = OutputState::Stopping;
                st.stopping_since = Some(Instant::now());
            }
        });
        let (videos, audio, sub, tx) = (s.videos(), s.audio.clone(), s.sub, s.tx.clone());
        for v in &videos {
            self.release_video(v, sub);
        }
        for a in &audio {
            self.release_audio(a, sub);
        }
        // what's held back still goes out (unless forced), then the connection closes
        let _ = tx.send((Instant::now() + delay, Item::End));
    }

    fn stop_clips(&mut self, canvas: Id) {
        let Some(c) = self.clips.remove(&canvas) else { return };
        self.release_video(&c.video, c.sub);
        self.release_audio(&c.audio, c.sub);
        runtime().update(OutputKey::Clips(canvas), |s| s.state = OutputState::Idle);
    }
}

// ---------------------------------------------------------------- what `Outputs` calls

/// Starts recording `canvas` (`cfg.canvas` keys it) from its compositor's frames.
pub fn start_recording(cfg: &RecordingConfig, canvas: Id) -> Result<(), String> {
    let key = OutputKey::Recording(cfg.canvas);
    with(|st| {
        if let Some(r) = st.recordings.get(&cfg.canvas) {
            if !r.stopping {
                return Ok(());
            }
            // the last one is still finishing its file: wait for it
            if let Some(mut r) = st.recordings.remove(&cfg.canvas)
                && let Some(t) = r.thread.take()
            {
                let _ = t.join();
            }
        }
        if let Some(dir) = cfg.path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| format!("Can't create the recording folder: {e}"))?;
        }
        let fail = |why: String| {
            runtime().update(key, |s| s.state = OutputState::Failed(why.clone()));
            Err(why)
        };
        let container = file_container(cfg.container);
        let enc = &cfg.encoders;
        let codec = video_codec(&enc.video_encoder);
        if container == FileContainer::Mp4 && matches!(codec, VideoCodec::ProRes(_)) {
            return fail("ProRes is recorded in MOV or MKV, not MP4.".into());
        }
        let video = match st.video_pipe(canvas, enc, enc.bframes, false) {
            Ok(v) => v,
            Err(e) => return fail(e),
        };
        // sound: the encoder's mix, then the extra mixes (up to six tracks); Opus can't go in MOV
        let mut acodec = audio_codec(&enc.audio_encoder);
        if container == FileContainer::Mov {
            acodec = AudioCodec::Aac;
        }
        let mixes: Vec<usize> = std::iter::once(enc.audio_mix).chain(cfg.extra_mixes.iter().copied().take(5)).collect();
        let mut audio = Vec::new();
        for &mix in &mixes {
            match st.audio_pipe(mix, acodec, enc.audio_bitrate_kbps) {
                Ok(a) => audio.push(a),
                Err(e) => {
                    tracing::error!(%e, mix, "own outputs: a sound track left out");
                }
            }
        }
        let (tx, rx) = unbounded();
        let sink: Arc<dyn Sink> = Arc::new(RecordingSink { tx: tx.clone() });
        let bytes = Arc::new(AtomicU64::new(0));
        let folder = cfg.path.parent().map(Path::to_path_buf).unwrap_or_default();
        let writer = Writer {
            key,
            container,
            folder,
            label: cfg.label.clone(),
            path: cfg.path.clone(),
            audio: audio.iter().map(|a| (a.format_now(), track_name(a.key.mix))).collect(),
            split_ns: cfg.split_minutes.filter(|m| *m > 0).map(|m| m as i64 * 60_000_000_000),
            video: Arc::downgrade(&video),
            bytes: bytes.clone(),
            recorder: None,
            held: VecDeque::new(),
            markers: Vec::new(),
            split: false,
            paused_at: None,
            offset: 0,
            resume_at: i64::MIN,
            want_key: false,
            last_dts: Vec::new(),
            failed: false,
        };
        runtime().update(key, |s| {
            *s = OutputStatus {
                state: OutputState::Starting,
                path: Some(cfg.path.clone()),
                files: 1,
                ..Default::default()
            }
        });
        let thread = std::thread::Builder::new()
            .name("lightair-record".into())
            .spawn(move || writer.run(rx))
            .map_err(|e| format!("Couldn't start the recording: {e}"));
        let thread = match thread {
            Ok(t) => t,
            Err(e) => {
                st.release_video(&video, 0);
                for a in &audio {
                    st.release_audio(a, 0);
                }
                return fail(e);
            }
        };
        let sub = video.subscribe(sink.clone());
        for (track, a) in audio.iter().enumerate() {
            a.subscribe(track, sink.clone(), sub);
        }
        let skipped_base = dropped_of(canvas) + video.skipped.load(Ordering::Relaxed);
        st.recordings.insert(
            cfg.canvas,
            RecordingOut { video, audio, sub, tx, thread: Some(thread), bytes, stopping: false, skipped_base },
        );
        tracing::info!(path = %cfg.path.display(), "own outputs: recording");
        Ok(())
    })
}

/// Stops every recording Lightair makes itself.
pub fn stop_recordings() {
    with(|st| {
        let ids: Vec<Id> = st.recordings.keys().copied().collect();
        for id in ids {
            st.stop_recording(id);
        }
    });
}

fn running_recordings(st: &State) -> Vec<(Id, Sender<RecMsg>)> {
    st.recordings.iter().filter(|(_, r)| !r.stopping).map(|(id, r)| (*id, r.tx.clone())).collect()
}

/// Pauses or resumes every recording; `None` when none runs here.
pub fn pause_recordings(pause: bool) -> Option<bool> {
    let running = with(|st| running_recordings(st));
    if running.is_empty() {
        return None;
    }
    let at = now_ns();
    for (canvas, tx) in running {
        let _ = tx.send(RecMsg::Pause(pause, at));
        runtime().update(OutputKey::Recording(canvas), |s| {
            match (s.paused, pause) {
                (false, true) => s.paused_since = Some(Instant::now()),
                (true, false) => s.paused_for += s.paused_since.take().map_or_default(|t| t.elapsed()),
                _ => {}
            }
            s.paused = pause;
        });
    }
    Some(true)
}

/// Every recording goes on in a new file from its next keyframe. Whether any will.
pub fn split_recordings() -> bool {
    let running = with(|st| running_recordings(st));
    for (_, tx) in &running {
        let _ = tx.send(RecMsg::Split);
    }
    !running.is_empty()
}

/// A marker (a chapter) now in every recording. Whether any took it.
pub fn add_marker(name: &str) -> bool {
    let running = with(|st| running_recordings(st));
    let at = now_ns();
    for (canvas, tx) in &running {
        let _ = tx.send(RecMsg::Marker(at, name.to_string()));
        runtime().update(OutputKey::Recording(*canvas), |s| s.markers += 1);
    }
    !running.is_empty()
}

/// Whether Lightair sends to this destination itself: RTMP(S) with H.264, or HEVC over Enhanced
/// RTMP, with Twitch's VOD track too when it has one; SRT and UDP (MPEG-TS) with H.264 or HEVC;
/// WHIP with H.264. Others stay libobs's for now: RTMP with AV1 (no encoder of Lightair's makes
/// it), RIST and MPEG-TS over TCP.
pub fn can_stream(cfg: &StreamConfig) -> bool {
    let server = cfg.server.trim().to_ascii_lowercase();
    let codec = video_codec(&cfg.encoders.video_encoder);
    match StreamProtocol::of(&server) {
        Some(StreamProtocol::Rtmp) => {
            (server.starts_with("rtmp://") || server.starts_with("rtmps://"))
                && matches!(codec, VideoCodec::H264 | VideoCodec::Hevc { .. })
        }
        Some(StreamProtocol::MpegTs) => {
            (server.starts_with("srt://") || server.starts_with("udp://"))
                && matches!(codec, VideoCodec::H264 | VideoCodec::Hevc { .. })
        }
        Some(StreamProtocol::Whip) => codec == VideoCodec::H264,
        None => false,
    }
}

/// Starts sending `canvas`'s program to the destination `cfg.id`; over RTMP with a second sound
/// track on `cfg.vod_mix` when it has one (Twitch's VOD track, in Enhanced RTMP's multitrack
/// audio, as libobs sends it).
pub fn start_stream(cfg: &StreamConfig, canvas: Id) -> Result<(), String> {
    let key = OutputKey::Stream(cfg.id);
    with(|st| {
        if let Some(s) = st.streams.get(&cfg.id) {
            if !s.stopping.load(Ordering::Acquire) && !s.finished() {
                return Ok(());
            }
            // the last connection to it is still closing: it goes now
            s.abort.store(true, Ordering::Release);
            if let Some(mut s) = st.streams.remove(&cfg.id) {
                for v in s.videos() {
                    st.release_video(&v, s.sub);
                }
                for a in &s.audio {
                    st.release_audio(a, s.sub);
                }
                if let Some(t) = s.watch.take() {
                    let _ = t.join();
                }
            }
        }
        let fail = |why: String| {
            runtime().update(key, |s| s.state = OutputState::Failed(why.clone()));
            Err(why)
        };
        let protocol = StreamProtocol::of(&cfg.server);
        let enc = outputs::stream_encoders(&cfg.server, &cfg.encoders);
        let codec = video_codec(&enc.video_encoder);
        let acodec = if protocol == Some(StreamProtocol::Whip) { AudioCodec::Opus } else { AudioCodec::Aac };
        let server = cfg.server.trim().to_string();
        let out = match protocol {
            Some(StreamProtocol::Rtmp) => {
                let bind = match cfg.net.bind_ip.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
                    Some(ip) => match ip.parse() {
                        Ok(ip) => Some(bind_to(ip)),
                        Err(_) => return fail(format!("{ip} isn't an address to send from.")),
                    },
                    None => None,
                };
                let video = if matches!(codec, VideoCodec::Hevc { .. }) { RtmpVideo::Hevc } else { RtmpVideo::Avc };
                match RtmpSender::start_with(RtmpTarget { url: server, key: cfg.key.clone(), bind }, video) {
                    Ok(s) => {
                        s.audio_config(SAMPLE_RATE, CHANNELS as u8);
                        if cfg.vod_mix.is_some() {
                            s.track_audio_config(1, SAMPLE_RATE, CHANNELS as u8);
                        }
                        let sounds = if cfg.vod_mix.is_some() { 2 } else { 1 };
                        s.set_bitrate(enc.video_bitrate_kbps + enc.audio_bitrate_kbps * sounds);
                        Out::Rtmp(s)
                    }
                    Err(e) => return fail(e),
                }
            }
            Some(StreamProtocol::MpegTs) if server.to_ascii_lowercase().starts_with("udp://") => {
                let to = match udp_target(&server) {
                    Ok(a) => a,
                    Err(e) => return fail(e),
                };
                let bind = match cfg.net.bind_ip.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
                    Some(ip) => match ip.parse() {
                        Ok(ip) => Some(bind_to(ip)),
                        Err(_) => return fail(format!("{ip} isn't an address to send from.")),
                    },
                    None => None,
                };
                let socket = match lightair_feed::net::udp_for(&to, bind.as_ref()) {
                    Ok(s) => s,
                    Err(e) => return fail(format!("Couldn't open UDP to {to}: {e}")),
                };
                // blocking: a full send buffer makes the sender thread wait rather than drop
                let _ = socket.set_nonblocking(false);
                if let std::net::SocketAddr::V4(a) = to
                    && a.ip().is_multicast()
                {
                    let _ = socket.set_multicast_ttl_v4(16);
                }
                let ts = if matches!(codec, VideoCodec::Hevc { .. }) { TsCodec::Hevc } else { TsCodec::H264 };
                let mux = Muxer::new(ts, Some(AudioConfig { sample_rate: SAMPLE_RATE, channels: CHANNELS as u8 }));
                Out::Udp(Mutex::new(UdpOut { mux, socket, to, sent: 0, dropped: 0, warned: false }))
            }
            Some(StreamProtocol::MpegTs) => {
                let mut target = match lightair_feed::srt::parse_url(&server) {
                    Ok(t) => t,
                    Err(e) => return fail(e),
                };
                if target.stream_id.as_deref().is_none_or(str::is_empty) && !cfg.key.trim().is_empty() {
                    target.stream_id = Some(cfg.key.trim().to_string());
                }
                let ts = if matches!(codec, VideoCodec::Hevc { .. }) { TsCodec::Hevc } else { TsCodec::H264 };
                let mux = Muxer::new(ts, Some(AudioConfig { sample_rate: SAMPLE_RATE, channels: CHANNELS as u8 }));
                match SrtSender::start(target) {
                    Ok(sender) => Out::Srt(Mutex::new(SrtOut { mux, sender })),
                    Err(e) => return fail(format!("Couldn't start SRT: {e}")),
                }
            }
            Some(StreamProtocol::Whip) => {
                let token = Some(cfg.key.trim().to_string()).filter(|k| !k.is_empty());
                match WhipSender::start(server, token) {
                    Ok(s) => Out::Whip(Mutex::new(s)),
                    Err(e) => return fail(e),
                }
            }
            None => return fail("Use an rtmp://, rtmps://, srt://, udp:// or https:// (WHIP) address.".into()),
        };
        // B-frames only over RTMP (the SRT and WebRTC senders give no decode times)
        let reorder = enc.bframes && protocol == Some(StreamProtocol::Rtmp);
        let video = match st.video_pipe(canvas, &enc, reorder, enc.adaptive) {
            Ok(v) => v,
            Err(e) => return fail(e),
        };
        // the destination's mix, then over RTMP the VOD track's
        let vod = cfg.vod_mix.filter(|_| protocol == Some(StreamProtocol::Rtmp));
        let mut audio: Vec<Arc<AudioPipe>> = Vec::with_capacity(2);
        for mix in std::iter::once(enc.audio_mix).chain(vod) {
            match st.audio_pipe(mix, acodec, enc.audio_bitrate_kbps) {
                Ok(a) => audio.push(a),
                Err(e) => {
                    st.release_video(&video, 0);
                    for a in &audio {
                        st.release_audio(a, 0);
                    }
                    return fail(e);
                }
            }
        }
        let delay = cfg.net.delay.map_or(Duration::ZERO, |(s, _)| Duration::from_secs(s as u64));
        let (tx, rx) = unbounded();
        let sink: Arc<dyn Sink> = Arc::new(StreamSink { tx: tx.clone(), delay, track: 0, format: Mutex::new(None) });
        let out = Arc::new(out);
        let abort = Arc::new(AtomicBool::new(false));
        let stopping = Arc::new(AtomicBool::new(false));
        let frames = Arc::new(AtomicU64::new(0));
        let base_ns = now_ns() - 1_000_000_000;
        let pump = {
            let (out, abort, frames) = (out.clone(), abort.clone(), frames.clone());
            std::thread::Builder::new()
                .name("lightair-send".into())
                .spawn(move || pump(rx, out, base_ns, abort, frames))
        };
        let pump = match pump {
            Ok(p) => p,
            Err(e) => {
                st.release_video(&video, 0);
                for a in &audio {
                    st.release_audio(a, 0);
                }
                return fail(format!("Couldn't start the stream: {e}"));
            }
        };
        let retries = cfg.reconnect.map_or(DEFAULT_RETRIES, |(r, _)| r);
        runtime().update(key, |s| *s = OutputStatus { state: OutputState::Starting, retries, ..Default::default() });
        let sub = video.subscribe(sink.clone());
        for (track, a) in audio.iter().enumerate() {
            a.subscribe(track, sink.clone(), sub);
        }
        let audio_kbps = audio.iter().map(|a| a.key.kbps).sum();
        let watch = Watch {
            key,
            out,
            videos: vec![Arc::downgrade(&video)],
            audio: audio.iter().map(Arc::downgrade).collect(),
            sub,
            retries,
            adapt: enc.adaptive.then(|| Adapt::new(video.key.kbps, vec![vec![0], vec![video.key.kbps]])),
            audio_kbps,
            frames,
            stopping: stopping.clone(),
            abort: abort.clone(),
            pump,
        };
        let watch = std::thread::Builder::new().name("lightair-stream".into()).spawn(move || watch.run()).ok();
        let skipped_base = dropped_of(canvas) + video.skipped.load(Ordering::Relaxed);
        st.streams.insert(
            cfg.id,
            StreamOut { video, extra: Vec::new(), audio, sub, tx, delay, stopping, abort, watch, skipped_base },
        );
        tracing::info!(id = %cfg.id, server = %StreamProtocol::label(&cfg.server), "own outputs: streaming");
        Ok(())
    })
}

/// Stops destination `id` (what its delay holds back still goes, unless `force`).
pub fn stop_stream(id: Id, force: bool) {
    with(|st| st.stop_stream(id, force));
}

pub fn stop_all_streams() {
    with(|st| {
        let ids: Vec<Id> = st.streams.keys().copied().collect();
        for id in ids {
            st.stop_stream(id, false);
        }
    });
}

/// Running destinations grouped by the video encoder they share.
pub fn share_groups() -> Vec<Vec<Id>> {
    with(|st| {
        let mut groups: Vec<(usize, Vec<Id>)> = Vec::new();
        for (id, s) in &st.streams {
            if s.stopping.load(Ordering::Acquire) || s.finished() {
                continue;
            }
            let enc = Arc::as_ptr(&s.video) as usize;
            match groups.iter_mut().find(|(e, _)| *e == enc) {
                Some((_, ids)) => ids.push(*id),
                None => groups.push((enc, vec![*id])),
            }
        }
        groups.into_iter().map(|(_, ids)| ids).collect()
    })
}

/// How many video encoders the destinations use.
pub fn stream_encoder_count() -> usize {
    with(|st| {
        let mut seen: Vec<usize> =
            st.streams.values().flat_map(|s| s.videos()).map(|v| Arc::as_ptr(&v) as usize).collect();
        seen.sort_unstable();
        seen.dedup();
        seen.len()
    })
}

/// Destination `id`'s video encoder (an identity), if Lightair sends it.
pub fn stream_video_encoder(id: Id) -> Option<usize> {
    with(|st| st.streams.get(&id).map(|s| Arc::as_ptr(&s.video) as usize))
}

/// Destination `id`'s sound tracks: the libobs mix each encodes.
pub fn stream_audio_tracks(id: Id) -> Option<Vec<usize>> {
    with(|st| st.streams.get(&id).map(|s| s.audio.iter().map(|a| a.key.mix).collect()))
}

// ---------------------------------------------------------------- multitrack streams

/// What a plan sets for a track beyond [`EncoderConfig`]: its profile and its ceiling.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Tune {
    profile: H264Profile,
    /// HEVC Main 10.
    main10: bool,
    /// A ceiling over the average when the bitrate isn't constant, kbps.
    peak_kbps: Option<u32>,
}

/// A multitrack stream's track: its place on the stream's keyframe beat, and the plan's
/// settings beyond [`EncoderConfig`].
struct TrackSetup {
    gop: Arc<Gop>,
    /// Its canvas's place in the beat.
    place: usize,
    tune: Tune,
}

/// A multitrack stream's track `t`'s encoder settings as the platform planned them (its
/// bitrate, rate control, keyframe interval, B-frames, profile and ceiling, read as libobs's
/// Apple encoders read them, their defaults where the plan says nothing; the rest from the
/// destination's), and whether it may reorder frames (B-frames).
fn track_encoder(cfg: &StreamConfig, t: &outputs::MultitrackVideo) -> (EncoderConfig, bool, Tune) {
    let s = &t.settings;
    let number = |k: &str| {
        s.get(k).and_then(|v| {
            v.as_u64()
                .or_else(|| v.as_f64().map(|f| f as u64))
                .or_else(|| v.as_str().and_then(|x| x.trim().parse().ok()))
        })
    };
    let flag = |k: &str| match s.get(k)? {
        serde_json::Value::Bool(b) => Some(*b),
        serde_json::Value::String(x) => Some(x.trim().eq_ignore_ascii_case("true") || x.trim() == "1"),
        v => v.as_u64().map(|n| n > 0),
    };
    let kbps = number("bitrate").map_or(cfg.encoders.video_bitrate_kbps, |b| b as u32);
    let cbr = s.get("rate_control").and_then(|v| v.as_str()).is_none_or(|r| r.eq_ignore_ascii_case("CBR"));
    // B-frames unless the plan says none, as libobs's Apple encoders make them
    let bframes = match s.get("bframes") {
        Some(_) => flag("bframes").unwrap_or(true),
        None => number("bf").is_none_or(|n| n > 0),
    };
    let keyint = number("keyint_sec").filter(|k| *k > 0).map_or(2, |k| k as u32);
    let profile = s.get("profile").and_then(|v| v.as_str()).map(|p| p.trim().to_ascii_lowercase());
    let tune = Tune {
        profile: match profile.as_deref() {
            Some("baseline") => H264Profile::Baseline,
            Some("main") => H264Profile::Main,
            _ => H264Profile::High,
        },
        main10: profile.as_deref() == Some("main10"),
        // an average with a ceiling: the plan's `max_bitrate` when it asks for one
        peak_kbps: (!cbr && flag("limit_bitrate") == Some(true))
            .then(|| number("max_bitrate"))
            .flatten()
            .filter(|m| *m > 0)
            .map(|m| m as u32),
    };
    let enc = EncoderConfig {
        video_encoder: t.encoder.clone(),
        video_bitrate_kbps: kbps,
        constant_bitrate: cbr,
        bframes,
        keyint_sec: keyint,
        scaled: Some((t.width, t.height)),
        fps_divisor: t.fps_divisor.max(1),
        adaptive: false,
        ..cfg.encoders.clone()
    };
    (enc, bframes, tune)
}

fn gcd(a: i64, b: i64) -> i64 {
    if b == 0 { a.abs() } else { gcd(b, a % b) }
}

/// What a multitrack stream's tracks counted when each one's last IDR went out.
#[derive(Debug, Clone, Copy)]
struct BpmMark {
    rendered: u32,
    lagged: u32,
    dropped: u64,
    /// Every track's encoded frames together.
    output: u64,
    fed: u64,
    skipped: u32,
    encoded: u64,
}

/// A multitrack stream's broadcast performance metrics, put in front of every track's IDRs.
struct Bpm {
    tracks: Vec<Weak<VideoPipe>>,
    hevc: Vec<bool>,
    last: Vec<Option<BpmMark>>,
}

impl Bpm {
    fn mark(&self, track: usize, dropped: u64) -> Option<BpmMark> {
        let (_, _, rendered, lagged) = crate::program::frame_stats();
        let pipes: Vec<Arc<VideoPipe>> = self.tracks.iter().filter_map(Weak::upgrade).collect();
        let p = self.tracks.get(track)?.upgrade()?;
        Some(BpmMark {
            rendered,
            lagged,
            dropped,
            output: pipes.iter().map(|p| p.encoder.packets()).sum(),
            fed: p.fed.load(Ordering::Relaxed),
            skipped: p.skipped.load(Ordering::Relaxed),
            encoded: p.encoder.packets(),
        })
    }

    /// The messages for track `track`'s IDR going out now: what was counted since its last one
    /// (zeros the first time). `dropped`: what the connection has dropped so far.
    fn messages(&mut self, track: usize, dropped: u64) -> Vec<u8> {
        use lightair_feed::bpm::{RenditionCounts, SessionCounts};
        let Some(now) = self.mark(track, dropped) else { return Vec::new() };
        let d = |a: u64, b: u64| a.saturating_sub(b).min(u32::MAX as u64) as u32;
        let (session, rendition) = match self.last.get(track).copied().flatten() {
            Some(was) => (
                SessionCounts {
                    rendered: now.rendered.saturating_sub(was.rendered),
                    lagged: now.lagged.saturating_sub(was.lagged),
                    dropped: d(now.dropped, was.dropped),
                    output: d(now.output, was.output),
                },
                RenditionCounts {
                    input: d(now.fed, was.fed),
                    skipped: now.skipped.saturating_sub(was.skipped),
                    output: d(now.encoded, was.encoded),
                },
            ),
            None => Default::default(),
        };
        if let Some(l) = self.last.get_mut(track) {
            *l = Some(now);
        }
        let hevc = self.hevc.get(track).copied().unwrap_or(false);
        lightair_feed::bpm::sei_nals(hevc, std::time::SystemTime::now(), session, rendition)
    }
}

/// Hands a multitrack destination's packets to its sender, each when its delay is up, in
/// decode time order across its tracks (a packet goes once every picture and sound track has
/// reached its time, or after half a second), every IDR with the broadcast performance metrics
/// in front. `tracks`: its picture tracks; `sounds`: its sound tracks.
#[allow(clippy::too_many_arguments)] // the pump's channel, sender, clock, flags and tracks
fn pump_tracks(
    rx: Receiver<(Instant, Item)>,
    out: Arc<Out>,
    base_ns: i64,
    abort: Arc<AtomicBool>,
    frames: Arc<AtomicU64>,
    tracks: usize,
    sounds: usize,
    mut bpm: Bpm,
) {
    const HOLD: Duration = Duration::from_millis(500);
    let us = |ns: i64| ((ns - base_ns).max(0) / 1000) as u64;
    // per source (each picture track, then each sound track): the latest decode time come
    let mut latest: Vec<Option<i64>> = vec![None; tracks + sounds.max(1)];
    // (decode time, order come, when come, the packet)
    let mut held: Vec<(i64, u64, Instant, Item)> = Vec::new();
    let mut order = 0u64;
    let mut ending = false;
    'run: loop {
        match rx.recv_timeout(Duration::from_millis(20)) {
            Ok((due, item)) => {
                loop {
                    if abort.load(Ordering::Acquire) {
                        break 'run;
                    }
                    let now = Instant::now();
                    if due <= now {
                        break;
                    }
                    std::thread::sleep((due - now).min(Duration::from_millis(50)));
                }
                let (source, dts) = match &item {
                    Item::Format(track, format) => {
                        out.parameter_sets(*track, format);
                        continue;
                    }
                    Item::Video(track, p) => (*track as usize, p.dts),
                    Item::Audio(track, p) => (tracks + *track as usize, p.dts),
                    Item::End => {
                        ending = true;
                        (usize::MAX, 0)
                    }
                };
                if let Some(l) = latest.get_mut(source) {
                    *l = Some(l.map_or(dts, |was| was.max(dts)));
                    held.push((dts, order, Instant::now(), item));
                    order += 1;
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => ending = true,
        }
        if abort.load(Ordering::Acquire) {
            break;
        }
        // what can go: up to the time every source has reached, and whatever waited too long
        let mut floor =
            if ending { i64::MAX } else { latest.iter().map(|l| l.unwrap_or(i64::MIN)).min().unwrap_or(i64::MIN) };
        if let Some(waited) = held.iter().filter(|h| h.2.elapsed() >= HOLD).map(|h| h.0).max() {
            floor = floor.max(waited);
        }
        held.sort_by_key(|h| (h.0, h.1));
        let ready = held.iter().take_while(|h| h.0 <= floor).count();
        for (_, _, _, item) in held.drain(..ready) {
            match item {
                Item::Video(track, p) => {
                    let p = if p.key {
                        let mut data = bpm.messages(track as usize, out.stats().dropped);
                        data.extend_from_slice(&p.data);
                        Arc::new(Pkt { data, pts: p.pts, dts: p.dts, key: true })
                    } else {
                        p
                    };
                    out.video(track, &p, us(p.pts), us(p.dts));
                    if track == 0 {
                        frames.fetch_add(1, Ordering::Relaxed);
                    }
                }
                Item::Audio(track, p) => out.audio(track, &p.data, us(p.pts)),
                Item::Format(..) | Item::End => {}
            }
        }
        if ending {
            break;
        }
    }
}

/// The bitrate table of a multitrack stream for adaptive bitrate: each level's bitrate for every
/// picture track, lowest first, from the plan's JSON (`[track][level]`, every track the same
/// number of levels). `None` when it doesn't read so.
fn bitrate_levels(table: &str, tracks: usize) -> Option<Vec<Vec<u32>>> {
    let per_track: Vec<Vec<u64>> = serde_json::from_str(table).ok()?;
    let levels = per_track.first()?.len();
    if per_track.len() != tracks || levels == 0 || per_track.iter().any(|t| t.len() != levels) {
        return None;
    }
    let level = |i: usize| per_track.iter().map(|t| t[i].min(u32::MAX as u64) as u32).collect();
    Some((0..levels).map(level).collect())
}

/// The kbps of one of a plan's sound tracks (its settings' `bitrate`), else `default`.
fn plan_audio_kbps(settings: &serde_json::Value, default: u32) -> u32 {
    let b = settings.get("bitrate");
    b.and_then(|v| v.as_u64().or_else(|| v.as_str().and_then(|x| x.trim().parse().ok())))
        .filter(|k| *k > 0)
        .map_or(default, |k| k as u32)
}

/// Sound tracks a stream carries at most (libobs's six mixes).
const MAX_SOUND_TRACKS: usize = 6;

/// Starts destination `cfg.id` as several encodes in one RTMP connection (Enhanced
/// Broadcasting), as `plan` says: video track *i* made from canvas `canvases[i]`, scaled to its
/// size, at its rate, bitrate and profile, every track's keyframes together; the plan's live
/// sound tracks on the destination's mix, then its VOD track on `cfg.vod_mix` (when the
/// destination has one), each at its own bitrate. With adaptive bitrate on, the tracks' total
/// follows the connection, shared out by the plan's bitrate table (or in proportion without
/// one). `cfg.server` and `cfg.key` are the plan's ingest and key.
pub fn start_multitrack_stream(cfg: &StreamConfig, plan: &outputs::Multitrack, canvases: &[Id]) -> Result<(), String> {
    let key = OutputKey::Stream(cfg.id);
    with(|st| {
        if let Some(s) = st.streams.get(&cfg.id) {
            if !s.stopping.load(Ordering::Acquire) && !s.finished() {
                return Ok(());
            }
            // the last connection to it is still closing: it goes now
            s.abort.store(true, Ordering::Release);
            if let Some(mut s) = st.streams.remove(&cfg.id) {
                for v in s.videos() {
                    st.release_video(&v, s.sub);
                }
                for a in &s.audio {
                    st.release_audio(a, s.sub);
                }
                if let Some(t) = s.watch.take() {
                    let _ = t.join();
                }
            }
        }
        let fail = |why: String| {
            runtime().update(key, |s| s.state = OutputState::Failed(why.clone()));
            Err(why)
        };
        let server = cfg.server.trim().to_string();
        let lower = server.to_ascii_lowercase();
        if !(lower.starts_with("rtmp://") || lower.starts_with("rtmps://")) {
            return fail("Enhanced Broadcasting goes over RTMP or RTMPS.".into());
        }
        if plan.video.is_empty() || plan.audio.is_empty() || canvases.len() != plan.video.len() {
            return fail("The platform's plan has no video or audio to send.".into());
        }
        if plan.video.len() > 255 {
            return fail("The platform's plan has more video tracks than a stream can carry.".into());
        }
        if let Some(t) =
            plan.video.iter().find(|t| !matches!(video_codec(&t.encoder), VideoCodec::H264 | VideoCodec::Hevc { .. }))
        {
            return fail(format!("The platform's plan asks for an encoder Lightair doesn't have ({}).", t.encoder));
        }
        let encoders: Vec<(EncoderConfig, bool, Tune)> = plan.video.iter().map(|t| track_encoder(cfg, t)).collect();
        // the canvases in the stream, each with the frames its tracks all take (a keyframe can
        // only fall on those), and each track's canvas among them
        let mut beats: Vec<(Id, i64)> = Vec::new();
        let mut places = Vec::with_capacity(canvases.len());
        for (canvas, (enc, ..)) in canvases.iter().zip(&encoders) {
            let divisor = enc.fps_divisor.max(1) as i64;
            match beats.iter().position(|(c, _)| c == canvas) {
                Some(i) => {
                    let step = beats[i].1;
                    beats[i].1 = step / gcd(step, divisor) * divisor;
                    places.push(i);
                }
                None => {
                    places.push(beats.len());
                    beats.push((*canvas, divisor));
                }
            }
        }
        let keyint = encoders.first().map_or(2, |(e, ..)| e.keyint_sec);
        let gop = Arc::new(Gop::new(keyint, &beats.iter().map(|(_, step)| *step).collect::<Vec<_>>()));
        // the encoders: each track's own
        let mut videos: Vec<Arc<VideoPipe>> = Vec::with_capacity(encoders.len());
        for ((canvas, (enc, reorder, tune)), place) in canvases.iter().zip(&encoders).zip(&places) {
            let track = TrackSetup { gop: gop.clone(), place: *place, tune: *tune };
            match st.video_pipe_in(*canvas, enc, *reorder, true, Some(track)) {
                Ok(v) => videos.push(v),
                Err(e) => {
                    for v in &videos {
                        st.release_video(v, 0);
                    }
                    return fail(e);
                }
            }
        }
        // the sound: the plan's live tracks on the destination's mix, then the VOD track's on
        // its own mix (Twitch's archive sound), each at the plan's bitrate
        let default_kbps = cfg.encoders.audio_bitrate_kbps;
        let live = plan.audio.iter().map(|a| (cfg.encoders.audio_mix, plan_audio_kbps(a, default_kbps)));
        let vod = cfg.vod_mix.into_iter().flat_map(|mix| plan.vod_audio.iter().map(move |a| (mix, a)));
        let wanted: Vec<(usize, u32)> =
            live.chain(vod.map(|(mix, a)| (mix, plan_audio_kbps(a, default_kbps)))).collect();
        if wanted.len() > MAX_SOUND_TRACKS {
            tracing::warn!(id = %cfg.id, tracks = wanted.len(), "own outputs: the plan's sound tracks after the sixth are left out");
        }
        let mut audio: Vec<Arc<AudioPipe>> = Vec::with_capacity(wanted.len());
        for (mix, kbps) in wanted.into_iter().take(MAX_SOUND_TRACKS) {
            match st.audio_pipe(mix, AudioCodec::Aac, kbps) {
                Ok(a) => audio.push(a),
                Err(e) => {
                    for v in &videos {
                        st.release_video(v, 0);
                    }
                    for a in &audio {
                        st.release_audio(a, 0);
                    }
                    return fail(e);
                }
            }
        }
        let release = |st: &mut State| {
            for v in &videos {
                st.release_video(v, 0);
            }
            for a in &audio {
                st.release_audio(a, 0);
            }
        };
        // the connection: every track described as it's encoded
        let bind = match cfg.net.bind_ip.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
            Some(ip) => match ip.parse() {
                Ok(ip) => Some(bind_to(ip)),
                Err(_) => {
                    release(st);
                    return fail(format!("{ip} isn't an address to send from."));
                }
            },
            None => None,
        };
        let tracks: Vec<RtmpTrack> = videos
            .iter()
            .map(|v| {
                let canvas = *v.canvas.lock();
                let (num, den) = canvas_shape(canvas).map_or((30, 1), |(_, _, n, d)| (n, d));
                RtmpTrack {
                    codec: if matches!(v.key.codec, VideoCodec::Hevc { .. }) {
                        RtmpVideo::Hevc
                    } else {
                        RtmpVideo::Avc
                    },
                    width: v.key.width,
                    height: v.key.height,
                    fps: num as f64 / (den.max(1) as f64 * v.key.divisor as f64),
                    kbps: v.key.kbps,
                }
            })
            .collect();
        let video_kbps = tracks.iter().map(|t| t.kbps).sum::<u32>();
        let audio_kbps = audio.iter().map(|a| a.key.kbps).sum::<u32>();
        let sounds: Vec<RtmpAudioTrack> = audio
            .iter()
            .map(|a| RtmpAudioTrack { kbps: a.key.kbps, sample_rate: SAMPLE_RATE, channels: CHANNELS as u8 })
            .collect();
        let info = RtmpTracks { video: tracks, audio: sounds };
        let sender = match RtmpSender::start_multitrack(RtmpTarget { url: server, key: cfg.key.clone(), bind }, info) {
            Ok(s) => s,
            Err(e) => {
                release(st);
                return fail(e);
            }
        };
        for track in 0..audio.len() {
            sender.track_audio_config(track, SAMPLE_RATE, CHANNELS as u8);
        }
        sender.set_bitrate(video_kbps + audio_kbps);
        // adaptive bitrate: the plan's table when it has one for every track (else in
        // proportion); a table that doesn't read leaves the bitrate fixed, as libobs does
        let adapt = if cfg.encoders.adaptive {
            let full: Vec<u32> = videos.iter().map(|v| v.key.kbps).collect();
            match plan.bitrate_table.as_deref() {
                None => Some(Adapt::new(video_kbps, vec![vec![0; full.len()], full])),
                Some(table) => match bitrate_levels(table, videos.len()) {
                    Some(levels) => Some(Adapt::new(video_kbps, levels)),
                    None => {
                        tracing::warn!(id = %cfg.id, "own outputs: the plan's bitrate table doesn't read; the bitrate stays fixed");
                        None
                    }
                },
            }
        } else {
            None
        };
        let out = Arc::new(Out::Multi(sender));
        let delay = cfg.net.delay.map_or(Duration::ZERO, |(s, _)| Duration::from_secs(s as u64));
        let (tx, rx) = unbounded();
        let abort = Arc::new(AtomicBool::new(false));
        let stopping = Arc::new(AtomicBool::new(false));
        let frames = Arc::new(AtomicU64::new(0));
        let base_ns = now_ns() - 1_000_000_000;
        let bpm = Bpm {
            tracks: videos.iter().map(Arc::downgrade).collect(),
            hevc: videos.iter().map(|v| matches!(v.key.codec, VideoCodec::Hevc { .. })).collect(),
            last: vec![None; videos.len()],
        };
        let pump = {
            let (out, abort, frames, n, sounds) =
                (out.clone(), abort.clone(), frames.clone(), videos.len(), audio.len());
            std::thread::Builder::new()
                .name("lightair-send".into())
                .spawn(move || pump_tracks(rx, out, base_ns, abort, frames, n, sounds, bpm))
        };
        let pump = match pump {
            Ok(p) => p,
            Err(e) => {
                release(st);
                return fail(format!("Couldn't start the stream: {e}"));
            }
        };
        let retries = cfg.reconnect.map_or(DEFAULT_RETRIES, |(r, _)| r);
        runtime().update(key, |s| *s = OutputStatus { state: OutputState::Starting, retries, ..Default::default() });
        let sub = next_id();
        for (i, v) in videos.iter().enumerate() {
            let sink: Arc<dyn Sink> =
                Arc::new(StreamSink { tx: tx.clone(), delay, track: i as u8, format: Mutex::new(None) });
            if i == 0 {
                for (track, a) in audio.iter().enumerate() {
                    a.subscribe(track, sink.clone(), sub);
                }
            }
            v.subscribe_as(sub, sink);
        }
        // every track set up: their first keyframes come together
        gop.ready.store(true, Ordering::Release);
        let watch = Watch {
            key,
            out,
            videos: videos.iter().map(Arc::downgrade).collect(),
            audio: audio.iter().map(Arc::downgrade).collect(),
            sub,
            retries,
            adapt,
            audio_kbps,
            frames,
            stopping: stopping.clone(),
            abort: abort.clone(),
            pump,
        };
        let watch = std::thread::Builder::new().name("lightair-stream".into()).spawn(move || watch.run()).ok();
        let mut videos = videos.into_iter();
        let Some(video) = videos.next() else { return fail("The platform's plan has no video to send.".into()) };
        let skipped_base = dropped_of(*video.canvas.lock()) + video.skipped.load(Ordering::Relaxed);
        tracing::info!(id = %cfg.id, server = %StreamProtocol::label(&cfg.server), tracks = plan.video.len(),
            sound_tracks = audio.len(), "own outputs: streaming several encodes in one connection");
        st.streams.insert(
            cfg.id,
            StreamOut { video, extra: videos.collect(), audio, sub, tx, delay, stopping, abort, watch, skipped_base },
        );
        Ok(())
    })
}

/// Starts `canvas`'s replay buffer (`cfg.canvas` keys it).
pub fn start_clips(cfg: &ClipsConfig, canvas: Id) -> Result<(), String> {
    let key = OutputKey::Clips(cfg.canvas);
    with(|st| {
        if st.clips.contains_key(&cfg.canvas) {
            return Ok(());
        }
        std::fs::create_dir_all(&cfg.folder).map_err(|e| format!("Can't create the clips folder: {e}"))?;
        let fail = |why: String| {
            runtime().update(key, |s| s.state = OutputState::Failed(why.clone()));
            Err(why)
        };
        let enc = &cfg.encoders;
        let video = match st.video_pipe(canvas, enc, enc.bframes, false) {
            Ok(v) => v,
            Err(e) => return fail(e),
        };
        let audio = match st.audio_pipe(enc.audio_mix, AudioCodec::Aac, enc.audio_bitrate_kbps) {
            Ok(a) => a,
            Err(e) => {
                st.release_video(&video, 0);
                return fail(e);
            }
        };
        let sink = Arc::new(ClipSink {
            ring: Mutex::new(Ring { video: VecDeque::new(), audio: VecDeque::new(), format: None }),
            keep_ns: cfg.seconds.max(1) as i64 * 1_000_000_000,
        });
        let audio_format = audio.format_now();
        let sub = video.subscribe(sink.clone());
        audio.subscribe(0, sink.clone(), sub);
        runtime().update(key, |s| {
            *s = OutputStatus { state: OutputState::Active { since: Instant::now() }, ..Default::default() }
        });
        st.clips.insert(
            cfg.canvas,
            ClipsOut { video, audio, sub, sink, folder: cfg.folder.clone(), label: cfg.label.clone(), audio_format },
        );
        Ok(())
    })
}

pub fn stop_clips(canvas: Option<Id>) {
    with(|st| {
        let ids: Vec<Id> = match canvas {
            Some(c) => vec![c],
            None => st.clips.keys().copied().collect(),
        };
        for id in ids {
            st.stop_clips(id);
        }
    });
}

pub fn clips_canvases() -> Vec<Id> {
    with(|st| st.clips.keys().copied().collect())
}

/// Saves the last N seconds of every replay buffer as a clip each (written on threads of their
/// own). How many buffers had something to save.
pub fn save_clip() -> usize {
    with(|st| {
        let mut saved = 0;
        for (canvas, c) in &st.clips {
            let (video, audio, format) = {
                let r = c.sink.ring.lock();
                (
                    r.video.iter().cloned().collect::<Vec<_>>(),
                    r.audio.iter().cloned().collect::<Vec<_>>(),
                    r.format.clone(),
                )
            };
            let Some(format) = format.filter(|_| !video.is_empty()) else { continue };
            let ext = if format.codec.is_nal() { "mp4" } else { "mov" };
            let path = stamped_path(&c.folder, "Clip", c.label.as_deref(), ext);
            let (canvas, af) = (*canvas, c.audio_format.clone());
            let spawned = std::thread::Builder::new()
                .name("lightair-clip".into())
                .spawn(move || write_clip(canvas, path, video, audio, format, af));
            if spawned.is_ok() {
                saved += 1;
            }
        }
        saved
    })
}

/// The stats sampler: sizes and skipped frames for the interface, and finished outputs and
/// unused encoders let go.
pub fn sample() {
    with(|st| {
        st.cleanup();
        for (canvas, r) in &st.recordings {
            let skipped = (dropped_of(*r.video.canvas.lock()) + r.video.skipped.load(Ordering::Relaxed))
                .saturating_sub(r.skipped_base);
            let bytes = r.bytes.load(Ordering::Relaxed);
            let stopping = r.stopping;
            runtime().update(OutputKey::Recording(*canvas), |s| {
                if !stopping {
                    s.bytes = bytes;
                }
                s.encoder_skipped = skipped;
            });
        }
        for (id, s) in &st.streams {
            let skipped = (dropped_of(*s.video.canvas.lock()) + s.video.skipped.load(Ordering::Relaxed))
                .saturating_sub(s.skipped_base);
            runtime().update(OutputKey::Stream(*id), |st| st.encoder_skipped = skipped);
        }
    });
}

// ---------------------------------------------------------------- the virtual camera

/// The virtual camera's sink and the pictures it takes: the canvas's own size in BGRA for the
/// camera extension (macOS); the camera's 1080p in NV12 for Lightair's camera (Windows).
#[cfg(target_os = "macos")]
mod camera {
    pub use crate::vcam_mac::{FPS, Sink};
    pub const SIZE: Option<(u32, u32)> = None;
    pub const FORMAT: super::TapFormat = super::TapFormat::Bgra;
}

#[cfg(windows)]
mod camera {
    use super::{Picture, TapFormat};
    pub use crate::vcam::win::{FPS, SIZE};
    pub const FORMAT: TapFormat = TapFormat::Nv12Bt601;

    /// Lightair's camera, given the tap's pictures.
    pub struct Sink(crate::vcam::win::Sink);

    impl Sink {
        pub fn open() -> Result<Sink, String> {
            crate::vcam::win::Sink::open().map(Sink)
        }

        pub fn send(&mut self, picture: &Picture<'_>, pts_ns: i64) {
            if picture.format == FORMAT {
                self.0.send(picture.data, picture.stride, (picture.width, picture.height), pts_ns);
            }
        }
    }
}

/// Starts the virtual camera on `canvas`'s compositor frames: copied into buffers of its own
/// and queued on the camera extension's sink stream (the canvas's size, BGRA, at most the
/// camera's rate). Its status is `OutputKey::VirtualCamera`'s.
pub fn start_virtual_camera(canvas: Id) -> Result<(), String> {
    let key = OutputKey::VirtualCamera;
    if with(|st| st.camera.is_some()) {
        return Ok(());
    }
    let fail = |why: String| {
        runtime().update(key, |s| s.state = OutputState::Failed(why.clone()));
        Err(why)
    };
    runtime().update(key, |s| *s = OutputStatus { state: OutputState::Starting, ..Default::default() });
    let Some(shape) = canvas_shape(canvas) else { return fail("The canvas isn't ready yet.".into()) };
    let mut camera = match camera::Sink::open() {
        Ok(c) => c,
        Err(e) => return fail(e),
    };
    let send: TapFn = Box::new(move |picture, pts| camera.send(picture, pts));
    match add_tap_on(canvas, shape, camera::SIZE, Some(camera::FPS), camera::FORMAT, send) {
        Ok(id) => {
            with(|st| st.camera = Some(id));
            runtime().update(key, |s| s.state = OutputState::Active { since: Instant::now() });
            tracing::info!(%canvas, "own outputs: the virtual camera shows the compositor's frames");
            Ok(())
        }
        Err(e) => fail(e),
    }
}

/// Starts the virtual camera on one scene (`scene`, laid out in `canvas`) drawn by `canvas`'s
/// compositor for the camera alone, at the canvas's size, whatever is on air. Its status is
/// `OutputKey::VirtualCamera`'s.
pub fn start_virtual_camera_scene(scene: Id, canvas: Id) -> Result<(), String> {
    let key = OutputKey::VirtualCamera;
    if with(|st| st.camera.is_some()) {
        return Ok(());
    }
    let fail = |why: String| {
        runtime().update(key, |s| s.state = OutputState::Failed(why.clone()));
        Err(why)
    };
    runtime().update(key, |s| *s = OutputStatus { state: OutputState::Starting, ..Default::default() });
    let Some(shape) = canvas_shape(canvas) else { return fail("The canvas isn't ready yet.".into()) };
    let mut camera = match camera::Sink::open() {
        Ok(c) => c,
        Err(e) => return fail(e),
    };
    let feed = Id::new();
    let send: TapFn = Box::new(move |picture, pts| camera.send(picture, pts));
    match add_tap_on(feed, shape, camera::SIZE, Some(camera::FPS), camera::FORMAT, send) {
        Ok(id) => {
            crate::program::add_scene_target(crate::program::SceneTarget {
                key: feed,
                scene,
                canvas,
                fps: (shape.2, shape.3),
            });
            with(|st| {
                st.camera = Some(id);
                st.camera_scene = Some(feed);
            });
            runtime().update(key, |s| s.state = OutputState::Active { since: Instant::now() });
            tracing::info!(%scene, %canvas, "own outputs: the virtual camera shows one scene drawn by the compositor");
            Ok(())
        }
        Err(e) => fail(e),
    }
}

/// Stops the virtual camera Lightair sends itself; false when it wasn't running here.
pub fn stop_virtual_camera() -> bool {
    if let Some(feed) = with(|st| st.camera_scene.take()) {
        crate::program::remove_scene_target(feed);
    }
    let Some(id) = with(|st| st.camera.take()) else { return false };
    runtime().update(OutputKey::VirtualCamera, |s| {
        if s.state.is_running() {
            s.state = OutputState::Stopping
        }
    });
    remove_tap(id);
    runtime().update(OutputKey::VirtualCamera, |s| s.state = OutputState::Idle);
    true
}

// ---------------------------------------------------------------- the RTSP server

/// The RTSP server's encoders and what it hands their packets to.
struct RtspOut {
    video: Arc<VideoPipe>,
    audio: Arc<AudioPipe>,
    sub: u64,
}

struct RtspSink {
    /// The time RTP's clocks start from.
    base_ns: i64,
    format: Mutex<Option<Arc<VideoFormat>>>,
    video: Weak<VideoPipe>,
}

impl Sink for RtspSink {
    fn video(&self, packet: &Arc<Pkt>, format: &Arc<VideoFormat>) {
        {
            let mut f = self.format.lock();
            if f.as_ref().is_none_or(|x| !Arc::ptr_eq(x, format)) {
                *f = Some(format.clone());
                crate::rtsp::own_parameter_sets(&format.parameter_sets);
            }
        }
        // a player that has just pressed play starts at the next keyframe
        if crate::rtsp::take_keyframe_request()
            && let Some(v) = self.video.upgrade()
        {
            v.encoder.request_keyframe();
        }
        crate::rtsp::own_video(&packet.data, packet.pts - self.base_ns);
    }
    fn audio(&self, track: usize, packet: &Arc<Pkt>) {
        if track == 0 {
            crate::rtsp::own_audio(&packet.data, packet.pts - self.base_ns);
        }
    }
}

/// Feeds the RTSP server from `canvas`: an H.264 encoder of the canvas already running without
/// B-frames at the canvas's rate when there is one (a recording's or a destination's, not one
/// whose bitrate follows its connection), else one of its own (6 Mbit/s CBR, a keyframe every
/// second); the Program's AAC encoder likewise.
pub fn start_rtsp(canvas: Id) -> Result<(), String> {
    with(|st| {
        if st.rtsp.is_some() {
            return Ok(());
        }
        let shared = st
            .videos
            .iter()
            .find(|p| {
                *p.canvas.lock() == canvas
                    && p.key.codec == VideoCodec::H264
                    && !p.key.reorder
                    && p.key.divisor == 1
                    && p.key.owner == 0
            })
            .cloned();
        let video = match shared {
            Some(v) => v,
            None => {
                let enc = EncoderConfig {
                    video_encoder: "h264".into(),
                    video_bitrate_kbps: 6000,
                    audio_encoder: "aac".into(),
                    audio_bitrate_kbps: 160,
                    constant_bitrate: true,
                    bframes: false,
                    keyint_sec: 1,
                    scaled: None,
                    audio_mix: 0,
                    adaptive: false,
                    fps_divisor: 1,
                };
                st.video_pipe(canvas, &enc, false, false)?
            }
        };
        let shared = st.audios.iter().find(|a| a.key.mix == 0 && a.key.codec == AudioCodec::Aac).cloned();
        let audio = match shared {
            Some(a) => a,
            None => match st.audio_pipe(0, AudioCodec::Aac, 160) {
                Ok(a) => a,
                Err(e) => {
                    st.release_video(&video, 0);
                    return Err(e);
                }
            },
        };
        crate::rtsp::own_audio_config(&audio.format.config, audio.format.sample_rate, audio.format.channels);
        let sink: Arc<dyn Sink> =
            Arc::new(RtspSink { base_ns: now_ns(), format: Mutex::new(None), video: Arc::downgrade(&video) });
        let sub = video.subscribe(sink.clone());
        audio.subscribe(0, sink, sub);
        tracing::info!(%canvas, kbps = video.key.kbps, "own outputs: RTSP fed by Lightair's encoders");
        st.rtsp = Some(RtspOut { video, audio, sub });
        Ok(())
    })
}

/// Stops feeding the RTSP server.
pub fn stop_rtsp() {
    with(|st| {
        if let Some(r) = st.rtsp.take() {
            st.release_video(&r.video, r.sub);
            st.release_audio(&r.audio, r.sub);
        }
    });
}

/// Engine shutdown: every output stops, recordings finish their files first.
pub fn shutdown() {
    remove_all_taps();
    let Some(mut st) = STATE.lock().take() else { return };
    if st.camera.take().is_some() {
        runtime().update(OutputKey::VirtualCamera, |s| s.state = OutputState::Idle);
    }
    if let Some(r) = st.rtsp.take() {
        st.release_video(&r.video, r.sub);
        st.release_audio(&r.audio, r.sub);
    }
    let ids: Vec<Id> = st.clips.keys().copied().collect();
    for id in ids {
        st.stop_clips(id);
    }
    let ids: Vec<Id> = st.streams.keys().copied().collect();
    for id in ids {
        st.stop_stream(id, true);
    }
    let ids: Vec<Id> = st.recordings.keys().copied().collect();
    for id in ids {
        st.stop_recording(id);
    }
    for (_, mut r) in st.recordings.drain() {
        if let Some(t) = r.thread.take() {
            let _ = t.join();
        }
    }
    for (_, mut s) in st.streams.drain() {
        if let Some(t) = s.watch.take() {
            let _ = t.join();
        }
    }
    for p in std::mem::take(&mut st.videos) {
        remove_from_feed(&(p as Arc<dyn Consumer>));
    }
    for a in std::mem::take(&mut st.audios) {
        a.close();
    }
}

// ---------------------------------------------------------------- macOS: pictures

/// macOS: the compositor's frames copied (and scaled) by VideoToolbox's pixel transfer into the
/// encoders' own buffer pools and the taps' pools.
#[cfg(target_os = "macos")]
mod pictures {
    use std::ptr::NonNull;

    use lightair_encode::apple::{VideoEncoder, VideoSettings, VideoSink};
    use objc2_core_foundation::CFRetained;
    use objc2_core_video::{CVPixelBuffer, CVPixelBufferPool};
    use objc2_video_toolbox::VTPixelTransferSession;

    use super::Buffer;

    /// What a tap does with each picture (its own BGRA copy of the frame, and its time), on the
    /// tap's own thread.
    pub type TapFn = Box<dyn FnMut(&CVPixelBuffer, i64) + Send>;

    /// The pictures a tap takes: BGRA.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum TapFormat {
        Bgra,
    }

    /// A VideoToolbox encoder (its rate control follows the settings' ceiling).
    pub fn open_encoder(
        settings: VideoSettings,
        _cbr: bool,
        sink: VideoSink,
    ) -> Result<VideoEncoder, lightair_encode::Error> {
        VideoEncoder::new(settings, sink)
    }

    /// What became of a frame given to an encoder.
    pub enum Fed {
        /// It couldn't be copied.
        NotCopied,
        /// No picture yet (nothing to repeat).
        NoPicture,
        Encoded(Result<(), lightair_encode::Error>),
    }

    fn transfer_session() -> Option<CFRetained<VTPixelTransferSession>> {
        let mut out: *mut VTPixelTransferSession = std::ptr::null_mut();
        let status = unsafe { VTPixelTransferSession::create(None, NonNull::from(&mut out)) };
        if status != 0 {
            tracing::error!(status, "own outputs: no pixel transfer session");
            return None;
        }
        NonNull::new(out).map(|p| unsafe { CFRetained::from_raw(p) })
    }

    /// The pixel transfer session that copies (and scales) the compositor's frames into the
    /// encoder's buffers, and the last buffer encoded (repeated while no new frame comes).
    pub struct Copier {
        session: Option<CFRetained<VTPixelTransferSession>>,
        last: Option<Buffer>,
    }

    // SAFETY: used under its mutex only; the session and buffers are thread-safe CF objects.
    unsafe impl Send for Copier {}

    impl Drop for Copier {
        fn drop(&mut self) {
            if let Some(s) = &self.session {
                unsafe { s.invalidate() };
            }
        }
    }

    impl Copier {
        pub fn new() -> Copier {
            Copier { session: transfer_session(), last: None }
        }

        /// Copies `frame` into a buffer of `encoder`'s and encodes it (`None`: the last again).
        pub fn feed(&mut self, encoder: &VideoEncoder, frame: Option<&Buffer>, pts: i64, key: bool) -> Fed {
            if let Some(src) = frame {
                match self.copy(encoder, src) {
                    Some(dst) => self.last = Some(dst),
                    None => return Fed::NotCopied,
                }
            }
            let Some(buffer) = self.last.as_ref() else { return Fed::NoPicture };
            Fed::Encoded(encoder.encode(buffer, pts, key))
        }

        /// `src` copied into a buffer from the encoder's pool, at the encoder's size.
        fn copy(&self, encoder: &VideoEncoder, src: &CVPixelBuffer) -> Option<Buffer> {
            let session = self.session.as_ref()?;
            let pool = encoder.pixel_buffer_pool()?;
            let mut out: *mut CVPixelBuffer = std::ptr::null_mut();
            let status = unsafe { CVPixelBufferPool::create_pixel_buffer(None, &pool, NonNull::from(&mut out)) };
            if status != 0 {
                return None;
            }
            let dst = unsafe { CFRetained::from_raw(NonNull::new(out)?) };
            let status = unsafe { session.transfer_image(src, &dst) };
            (status == 0).then_some(dst)
        }
    }

    /// A pool of BGRA, IOSurface-backed buffers of one size.
    fn bgra_pool(width: u32, height: u32) -> Option<CFRetained<CVPixelBufferPool>> {
        use objc2_core_foundation::{CFBoolean, CFDictionary, CFNumber, CFString, CFType};
        use objc2_core_video::{
            kCVPixelBufferHeightKey, kCVPixelBufferIOSurfacePropertiesKey, kCVPixelBufferMetalCompatibilityKey,
            kCVPixelBufferPixelFormatTypeKey, kCVPixelBufferWidthKey, kCVPixelFormatType_32BGRA,
        };
        let empty: CFRetained<CFDictionary<CFString, CFType>> = CFDictionary::from_slices(&[], &[]);
        let (format, w, h) = (
            CFNumber::new_i32(kCVPixelFormatType_32BGRA as i32),
            CFNumber::new_i32(width as i32),
            CFNumber::new_i32(height as i32),
        );
        // SAFETY: CoreVideo's constant keys, read only.
        let keys: [&CFString; 5] = unsafe {
            [
                kCVPixelBufferPixelFormatTypeKey,
                kCVPixelBufferWidthKey,
                kCVPixelBufferHeightKey,
                kCVPixelBufferIOSurfacePropertiesKey,
                kCVPixelBufferMetalCompatibilityKey,
            ]
        };
        let values: [&CFType; 5] =
            [format.as_ref(), w.as_ref(), h.as_ref(), empty.as_ref(), CFBoolean::new(true).as_ref()];
        let attrs = CFDictionary::from_slices(&keys, &values);
        let mut out: *mut CVPixelBufferPool = std::ptr::null_mut();
        let status = unsafe { CVPixelBufferPool::create(None, None, Some(attrs.as_opaque()), NonNull::from(&mut out)) };
        if status != 0 {
            tracing::error!(status, width, height, "own outputs: no buffer pool");
            return None;
        }
        NonNull::new(out).map(|p| unsafe { CFRetained::from_raw(p) })
    }

    /// The session that copies (and scales) the compositor's frames into a tap's pool.
    pub struct TapCopier {
        session: CFRetained<VTPixelTransferSession>,
        pool: CFRetained<CVPixelBufferPool>,
    }

    // SAFETY: used under its tap's mutex only; the session and pool are thread-safe CF objects.
    unsafe impl Send for TapCopier {}

    impl Drop for TapCopier {
        fn drop(&mut self) {
            unsafe { self.session.invalidate() };
        }
    }

    impl TapCopier {
        pub fn new(width: u32, height: u32, _format: TapFormat) -> Result<TapCopier, String> {
            let pool = bgra_pool(width, height).ok_or_else(|| "Couldn't make pictures to send.".to_string())?;
            let session = transfer_session().ok_or_else(|| "Couldn't copy the canvas's pictures.".to_string())?;
            Ok(TapCopier { session, pool })
        }

        /// `src` copied into a buffer from the tap's pool; `None` when it couldn't be.
        pub fn copy(&mut self, src: &Buffer) -> Option<TapPicture> {
            let mut out: *mut CVPixelBuffer = std::ptr::null_mut();
            let made = unsafe { CVPixelBufferPool::create_pixel_buffer(None, &self.pool, NonNull::from(&mut out)) };
            let dst = NonNull::new(out).filter(|_| made == 0).map(|p| unsafe { CFRetained::from_raw(p) })?;
            if unsafe { self.session.transfer_image(src, &dst) } != 0 {
                return None;
            }
            Some(TapPicture(dst))
        }
    }

    /// A tap's copy of a frame, on its way to the tap's thread.
    #[derive(Clone)]
    pub struct TapPicture(Buffer);

    // SAFETY: CoreVideo's buffers are reference counted thread-safely, and a tap's copy isn't
    // written once it's made.
    unsafe impl Send for TapPicture {}

    impl TapPicture {
        /// Gives the picture to the tap's function. The tap's thread.
        pub fn give(&self, f: &mut TapFn, pts: i64) {
            f(&self.0, pts);
        }
    }
}
