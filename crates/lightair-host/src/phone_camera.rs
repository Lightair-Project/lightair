//! The Phone camera layer's picture and sound (docs/07-scenes-layers-canvas.md, LY-16;
//! lightair-live's docs/05 §6). A phone in Lightair Live's Camera mode sends MPEG-TS over SRT;
//! the app receives it (`lightair-phone-camera`) and hands each datagram to a UDP port on this
//! machine, where a private `ffmpeg_source` decodes it (libobs's FFmpeg has no SRT of its own).
//!
//! `lightair_phone_camera` wraps that decoder:
//!
//! - a new feed (the phone connected again, or was asked for another codec or size) makes a new
//!   decoder, so nothing of the last stream is kept;
//! - a feed with nothing new for [`GONE_AFTER`] shows a clean placeholder (a dark slate and a
//!   line saying what it waits for) until pictures come again;
//! - the phone encodes as its sensor sees and says which way is up: the wrapper turns the
//!   picture (90° steps) and reports the turned size;
//! - the phone's microphone is passed through as the wrapper's sound (a mixer track); with
//!   `LIGHTAIR_ENGINE=own` and the audio engine (macOS), straight into the track's feed while a
//!   track takes it (`audio_host::feed_slot`), not through libobs.
//!
//! The app talks to a source's [`Feed`] by the source's id. The feeds, their timing and the
//! decoder live here; libobs's wrapper source (the placeholder, drawing the pictures where libobs
//! shows the source) is lightair-obs's `phone_camera`.
//!
//! With `LIGHTAIR_ENGINE=own` (macOS), Lightair's compositor takes a feed's pictures while it
//! shows the source ([`Feed::take`]): the decoder makes BGRA IOSurfaces and hands each to the
//! compositor instead of copying it into libobs, and the wrapper draws those same surfaces
//! (turned upright) where libobs still shows the source. The sound goes the same way either way.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use parking_lot::Mutex;


/// The decoder phone feeds use (VideoToolbox and AudioToolbox), for other streams of the same
/// kind (H.264 or HEVC and AAC, as access units).
#[cfg(target_os = "macos")]
pub use crate::phone_decode::{Decoder, Pictures};

/// The workspace kind (and libobs id) of a Phone camera source.
pub const KIND: &str = "lightair_phone_camera";

/// How long a feed may bring nothing before the placeholder shows (the phone dropped).
pub const GONE_AFTER: Duration = Duration::from_millis(700);

/// The size shown before any picture came (a 1080p landscape camera).
pub const DEFAULT_SIZE: (u32, u32) = (1920, 1080);

/// The desktop's clock for phone cameras, in microseconds: what the phones' clock sync answers
/// on, and what every stage of a frame's way is timed on.
pub fn clock_us() -> u64 {
    now_us()
}

/// How far ahead of the capture time Lightair Live's muxer puts a frame's PTS (100 ms, so a
/// decoder that paces by the PCR has it in time; `lightair-feed`'s `DELAY_90K`).
pub const PTS_LEAD_90K: u64 = 9_000;

/// What an access unit holds (from the app's demuxer).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnitKind {
    H264,
    Hevc,
    /// AAC in ADTS.
    Aac,
}

/// One access unit of a phone's stream: a video frame (Annex B) or AAC frames (ADTS), with its
/// PTS on the 90 kHz clock.
#[derive(Debug, Clone)]
pub struct Unit {
    pub kind: UnitKind,
    pub pts: u64,
    pub data: Vec<u8>,
}

/// Whether this platform decodes phone feeds itself (macOS: VideoToolbox, `Feed::push`), not
/// through libobs's media source over UDP (`Feed::addr`).
pub const DECODES_DIRECTLY: bool = cfg!(any(target_os = "macos", windows));

/// How long a frame took to get somewhere, over the last few seconds (milliseconds).
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Stage {
    pub median_ms: f32,
    pub min_ms: f32,
    pub max_ms: f32,
    pub frames: usize,
}

/// A frame's way, each stage from its capture on the phone (milliseconds): arrived here, decoded,
/// drawn in the output. `None` for a stage not timed (yet).
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Latency {
    pub arrived: Option<Stage>,
    pub decoded: Option<Stage>,
    pub drawn: Option<Stage>,
    /// The phone's clock less this one's, as worked out from the sync (µs), and the samples used.
    pub offset_us: Option<i64>,
}

/// Clock sync and the stages' recent times.
#[derive(Default)]
pub struct Timing {
    /// Recent sync samples: this clock at arrival less the phone's when sent (µs).
    samples: std::collections::VecDeque<i64>,
    /// The phone's clock in the last sample (to unwrap 33-bit PTS).
    phone_us: u64,
    stages: [std::collections::VecDeque<(u64, f32)>; 3],
}

const SAMPLES: usize = 16;
/// Stage times are kept this long (µs).
const KEEP_US: u64 = 5_000_000;

impl Timing {
    /// The clocks' offset: the smallest recent difference (the trip one way is never negative,
    /// and the shortest is closest to none).
    fn offset(&self) -> Option<i64> {
        self.samples.iter().copied().min()
    }

    /// A PTS (90 kHz, the phone's capture clock plus its lead) on this clock (µs).
    fn captured(&self, pts_90k: u64) -> Option<i64> {
        let offset = self.offset()?;
        const WRAP: i64 = 1 << 33;
        let reference = (self.phone_us as i64 * 9 / 100) & (WRAP - 1);
        let pts = (pts_90k as i64 - PTS_LEAD_90K as i64).rem_euclid(WRAP);
        let diff = (pts - reference + WRAP / 2).rem_euclid(WRAP) - WRAP / 2;
        let phone = self.phone_us as i64 + diff * 100 / 9;
        Some(phone + offset)
    }

    fn record(&mut self, stage: usize, pts_90k: u64) {
        let now = now_us();
        let Some(captured) = self.captured(pts_90k) else { return };
        let ms = (now as i64 - captured) as f32 / 1000.0;
        let q = &mut self.stages[stage];
        q.push_back((now, ms));
        while q.front().is_some_and(|(t, _)| now.saturating_sub(*t) > KEEP_US) {
            q.pop_front();
        }
    }

    fn stage(&self, stage: usize) -> Option<Stage> {
        let mut v: Vec<f32> = self.stages[stage].iter().map(|(_, ms)| *ms).collect();
        if v.is_empty() {
            return None;
        }
        v.sort_by(|a, b| a.total_cmp(b));
        Some(Stage { median_ms: v[v.len() / 2], min_ms: v[0], max_ms: v[v.len() - 1], frames: v.len() })
    }
}

fn now_us() -> u64 {
    static START: OnceLock<Instant> = OnceLock::new();
    // (+1: zero means "never")
    START.get_or_init(Instant::now).elapsed().as_micros() as u64 + 1
}

/// What takes a feed's pictures instead of libobs (Lightair's compositor): each decoded picture
/// (BGRA, IOSurface-backed, borrowed for the call) and the clockwise degrees that turn it
/// upright. It answers whether it took the picture; one it didn't take goes to libobs.
#[cfg(target_os = "macos")]
pub type Taker = Arc<dyn Fn(&objc2_core_video::CVPixelBuffer, u32) -> bool + Send + Sync>;

/// A feed's pictures while the compositor takes them: who takes them, and the newest it took
/// (as decoded, not yet turned), which the wrapper draws.
#[cfg(target_os = "macos")]
#[derive(Default)]
pub struct Taken {
    taker: Option<Taker>,
    newest: Option<crate::own_outputs::Buffer>,
}

// SAFETY: CoreVideo's buffers are reference counted thread-safely, and a decoded picture isn't
// written again while it's held.
#[cfg(target_os = "macos")]
unsafe impl Send for Taken {}

/// One Phone camera source's feed, as the app sees it: where its stream goes, and what the
/// wrapper should know about it.
pub struct Feed {
    /// The UDP port on 127.0.0.1 the decoder listens on.
    pub port: u16,
    /// Bumped for each new stream: the wrapper makes a new decoder.
    pub generation: AtomicU64,
    /// When data last came (`now_us`), 0 for never.
    pub last_data: AtomicU64,
    /// Clockwise degrees that turn the picture upright (0, 90, 180, 270).
    pub rotation: AtomicU32,
    /// What the placeholder says.
    pub message: Mutex<String>,
    /// Bytes received, for the Inspector's rate.
    pub bytes: AtomicU64,
    /// Clock sync and each stage's times.
    pub timing: Mutex<Timing>,
    /// Where units go to be decoded (macOS, Windows), while a decoder runs.
    pub units: Mutex<Option<crossbeam_channel::Sender<Unit>>>,
    /// The PTS (+1) of the last frame decoded and not yet drawn; 0 for none.
    pub undrawn: AtomicU64,
    /// Data came since the decoder was last made.
    pub streamed: AtomicBool,
    /// The compositor taking the pictures, if it does.
    #[cfg(target_os = "macos")]
    pub taken: Mutex<Taken>,
}

impl Default for Feed {
    fn default() -> Feed {
        Feed::new()
    }
}

impl Feed {
    pub fn new() -> Feed {
        // a free port, found by taking one and letting it go (the decoder binds it next)
        let port = std::net::UdpSocket::bind("127.0.0.1:0").and_then(|s| s.local_addr()).map_or(0, |a| a.port());
        Feed {
            port,
            generation: AtomicU64::new(0),
            last_data: AtomicU64::new(0),
            rotation: AtomicU32::new(0),
            message: Mutex::new("Waiting for the phone".into()),
            bytes: AtomicU64::new(0),
            streamed: AtomicBool::new(false),
            timing: Mutex::new(Timing::default()),
            units: Mutex::new(None),
            undrawn: AtomicU64::new(0),
            #[cfg(target_os = "macos")]
            taken: Mutex::new(Taken::default()),
        }
    }

    /// Where the stream goes: datagrams of MPEG-TS packets, as they came.
    pub fn addr(&self) -> std::net::SocketAddr {
        std::net::SocketAddr::from(([127, 0, 0, 1], self.port))
    }

    /// A new stream starts (a phone connected, or changed codec or size): a new decoder.
    pub fn restart(&self) {
        self.streamed.store(false, Ordering::Release);
        self.generation.fetch_add(1, Ordering::AcqRel);
    }

    /// A stream started: a new decoder, unless the one there has had nothing yet (it's ready).
    pub fn new_stream(&self) {
        if self.streamed.swap(false, Ordering::AcqRel) {
            self.restart();
        }
    }

    /// Data arrived (`n` bytes).
    pub fn data(&self, n: usize) {
        self.streamed.store(true, Ordering::Release);
        self.last_data.store(now_us(), Ordering::Release);
        self.bytes.fetch_add(n as u64, Ordering::Relaxed);
    }

    /// Bytes received so far.
    /// A clock sync sample: the phone's capture clock when it sent it, this clock (`clock_us`)
    /// when it came.
    pub fn clock_sample(&self, phone_us: u64, host_us: u64) {
        let mut t = self.timing.lock();
        t.samples.push_back(host_us as i64 - phone_us as i64);
        while t.samples.len() > SAMPLES {
            t.samples.pop_front();
        }
        t.phone_us = phone_us;
    }

    /// A video frame (its PTS) arrived whole.
    pub fn arrived(&self, pts_90k: u64) {
        self.timing.lock().record(0, pts_90k);
    }

    /// A video frame was decoded.
    pub fn decoded(&self, pts_90k: u64) {
        self.timing.lock().record(1, pts_90k);
    }

    /// A video frame was drawn in the output.
    pub fn drawn(&self, pts_90k: u64) {
        self.timing.lock().record(2, pts_90k);
    }

    /// A unit to decode (macOS; dropped while no decoder runs, or when it's behind).
    pub fn push(&self, unit: Unit) {
        if let Some(tx) = self.units.lock().as_ref() {
            let _ = tx.try_send(unit);
        }
    }

    /// Lightair's compositor takes this feed's pictures (`Some`), or gives them back to libobs
    /// (`None`). Pictures are BGRA IOSurfaces only with `LIGHTAIR_ENGINE=own`.
    #[cfg(target_os = "macos")]
    pub fn take(&self, taker: Option<Taker>) {
        let mut t = self.taken.lock();
        if taker.is_none() {
            t.newest = None;
        }
        t.taker = taker;
    }

    /// A decoded picture, offered to the compositor: whether it took it (else it goes to libobs).
    #[cfg(target_os = "macos")]
    pub fn offer(&self, image: &objc2_core_video::CVPixelBuffer) -> bool {
        use objc2_core_foundation::Type;
        let taker = {
            let mut t = self.taken.lock();
            match t.taker.clone() {
                Some(taker) => taker,
                None => {
                    t.newest = None;
                    return false;
                }
            }
        };
        // (called outside the lock: the taker may turn the picture, or give the feed back)
        let took = taker(image, self.rotation());
        let mut t = self.taken.lock();
        // (kept only while still taken: given back meanwhile, the wrapper draws libobs's again)
        t.newest = (took && t.taker.is_some()).then(|| image.retain());
        took
    }

    /// The newest picture the compositor took, as decoded (not turned).
    #[cfg(target_os = "macos")]
    pub fn newest_taken(&self) -> Option<crate::own_outputs::Buffer> {
        self.taken.try_lock()?.newest.clone()
    }

    /// The decoder handed a frame to libobs: it's drawn next time the layer draws.
    pub fn shown_next(&self, pts_90k: u64) {
        self.undrawn.store(pts_90k + 1, Ordering::Release);
    }

    /// The layer drew: the frame decoded last, if not yet timed, is on screen now.
    pub fn on_draw(&self) {
        let p = self.undrawn.swap(0, Ordering::AcqRel);
        if p != 0 {
            self.drawn(p - 1);
        }
    }

    /// Each stage's recent times, from capture.
    pub fn latency(&self) -> Latency {
        let t = self.timing.lock();
        Latency { arrived: t.stage(0), decoded: t.stage(1), drawn: t.stage(2), offset_us: t.offset() }
    }

    pub fn bytes(&self) -> u64 {
        self.bytes.load(Ordering::Relaxed)
    }

    /// Whether pictures are coming (data within [`GONE_AFTER`]).
    pub fn live(&self) -> bool {
        let at = self.last_data.load(Ordering::Acquire);
        at != 0 && now_us().saturating_sub(at) < GONE_AFTER.as_micros() as u64
    }

    /// The phone's `rotation`: clockwise degrees that turn the picture upright.
    pub fn set_rotation(&self, degrees: i64) {
        let r = (degrees.rem_euclid(360) as u32 + 45) / 90 * 90 % 360;
        self.rotation.store(r, Ordering::Release);
    }

    pub fn rotation(&self) -> u32 {
        self.rotation.load(Ordering::Acquire)
    }

    /// What the placeholder says ("Waiting for Sam's iPhone").
    pub fn set_message(&self, text: &str) {
        let mut m = self.message.lock();
        if *m != text {
            *m = text.to_owned();
        }
    }
}

static FEEDS: Mutex<Option<HashMap<String, Arc<Feed>>>> = Mutex::new(None);

/// The feed of the Phone camera source with this id (made the first time it's asked for).
pub fn feed(source: &str) -> Arc<Feed> {
    FEEDS
        .lock()
        .get_or_insert_with(HashMap::new)
        .entry(source.to_owned())
        .or_insert_with(|| Arc::new(Feed::new()))
        .clone()
}

/// Lets a source's feed go (the source was removed).
pub fn forget(source: &str) {
    if let Some(f) = FEEDS.lock().as_mut() {
        f.remove(source);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_are_timed_from_their_capture() {
        let f = Feed::new();
        // the phone's clock is 10 s ahead of this one; its samples take 1–3 ms to come
        let now = clock_us();
        let phone_now = now + 10_000_000;
        for trip in [3_000u64, 1_000, 2_000] {
            f.clock_sample(phone_now - trip, now);
        }
        // the shortest trip (1 ms) is in the offset
        assert_eq!(f.latency().offset_us, Some(-10_000_000 + 1_000));
        // a frame captured 50 ms ago on the phone (PTS = capture + the 100 ms lead)
        let pts = ((phone_now - 50_000) * 9 / 100 + PTS_LEAD_90K) & ((1 << 33) - 1);
        f.arrived(pts);
        let a = f.latency().arrived.expect("timed");
        // 50 ms less the 1 ms the offset carries (and a little rounding)
        assert!((a.median_ms - 49.0).abs() < 1.5, "{a:?}");
    }

    #[test]
    fn rotation_snaps_to_quarter_turns() {
        let f = Feed::new();
        for (given, want) in [(0, 0), (90, 90), (-90, 270), (270, 270), (360, 0), (100, 90), (179, 180), (450, 90)] {
            f.set_rotation(given);
            assert_eq!(f.rotation(), want, "{given}");
        }
    }

    #[test]
    fn a_feed_is_live_only_while_data_comes() {
        let f = Feed::new();
        assert!(!f.live());
        f.data(1316);
        assert!(f.live());
        assert_eq!(f.bytes(), 1316);
        std::thread::sleep(GONE_AFTER + Duration::from_millis(50));
        assert!(!f.live());
    }

    #[test]
    fn feeds_are_kept_by_source_and_have_their_own_ports() {
        let (a, b) = (feed("phone-camera-test-a"), feed("phone-camera-test-b"));
        assert_eq!(feed("phone-camera-test-a").addr(), a.addr(), "the same source: the same port");
        assert_ne!(a.addr().port(), b.addr().port(), "each source its own");
        forget("phone-camera-test-a");
        assert_ne!(feed("phone-camera-test-a").addr(), a.addr(), "forgotten: a new one");
    }
}
