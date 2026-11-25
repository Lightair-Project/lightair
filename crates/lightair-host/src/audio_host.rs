//! Runs the audio engine in real time (docs/plans/audio-engine.md, step 4).
//!
//! - **In**: each track's input is a document source. Audio interfaces are opened by the engine
//!   itself (`devices.rs`); a source that makes its own sound (desktop and app audio, media,
//!   web pages, NDI, links, the phone camera…) registers by its document id with
//!   [`feeds_itself`] or [`feed_slot`], and the engine hands it an [`InputFeed`] to push into
//!   while a track takes it.
//! - **Engine**: a real-time thread takes a block from each input's ring, runs the
//!   `lightair-audio` graph, and
//! - **Out**: hands Program, the mixes and the cue to whoever takes them ([`tap_mix`]: Lightair's
//!   encoders, the loudness meter, NDI out), plays the cue or Program on the monitoring device
//!   and mixes on devices of their own.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use lightair_audio as la;
use lightair_model::Id;
use parking_lot::Mutex;

/// Frames per engine block by default (5.3 ms at 48 kHz); 128 for low latency, 512 for a
/// lighter load ([`AudioHost::start`]).
pub const BLOCK: usize = 256;
/// The block sizes offered.
pub const BLOCKS: [usize; 3] = [128, 256, 512];
/// The mixes the engine makes for the outputs: Program, then up to five mixes. [`tap_mix`]
/// takes them by index (0: Program), and the cue as index `OUT_MIXES`.
pub const OUT_MIXES: usize = 6;
/// Ring size per input (frames).
const RING_FRAMES: usize = 48_000;
/// The engine's output is stamped this far behind real time (frames): about what its inputs
/// buffer.
const PRIME: usize = 2_048;

/// Inputs on the audio thread, and how many had audio in the last block (diagnostics).
pub static RT_INPUTS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
pub static RT_INPUTS_PLAYING: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
/// Blocks the audio thread has processed (diagnostics).
pub static RT_BLOCKS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Times an input ran short (a gap in its audio), for diagnostics and tests.
pub static SHORTFALLS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

// ---------------------------------------------------------------- sources that feed the engine

/// Ring size of an [`InputFeed`] (stereo frames): a second.
const FEED_FRAMES: usize = 48_000;
/// Buffered before a fed input starts playing, and the most kept (frames), at least: a capture
/// that delivers on Core Audio's I/O cycle (512 frames, often) needs a little more than an
/// interface opened at the engine's block size.
const FEED_PRIME: usize = 1_024;
const FEED_MOST: usize = 4_096;
/// Frames a push takes without allocating (larger blocks grow the buffers once).
const FEED_WARM: usize = 8_192;

/// Where a source that captures its own sound pushes it: into one of the engine's inputs,
/// brought to stereo and to the engine's rate on the way. Made by the engine
/// ([`SoundFeeder::attach`]); dropping it ends nothing (the engine removes the input itself).
pub struct InputFeed {
    ring: rtrb::Producer<f32>,
    /// The engine's rate.
    rate: u32,
    /// The rate the resampler is set for (the last block's).
    from: u32,
    resampler: crate::devices::Resampler,
    stereo: Vec<f32>,
}

impl InputFeed {
    fn new(ring: rtrb::Producer<f32>, rate: u32) -> Self {
        Self {
            ring,
            rate,
            from: rate,
            resampler: crate::devices::Resampler::new(2, rate, rate),
            stereo: Vec::with_capacity(FEED_WARM * 2),
        }
    }

    /// The engine's rate.
    pub fn rate(&self) -> u32 {
        self.rate
    }

    /// One block of sound: `samples` interleaved, `channels` to a frame (mono is heard on both
    /// sides; past two, the first two are taken), at `rate` frames a second (resampled when it
    /// isn't the engine's). Real-time safe: no locks, no waiting, and no allocation for blocks
    /// up to 8192 frames; what doesn't fit (the engine stopped reading) is dropped.
    pub fn push(&mut self, samples: &[f32], channels: u32, rate: u32) {
        let ch = channels.max(1) as usize;
        let frames = samples.len() / ch;
        if frames == 0 || rate == 0 {
            return;
        }
        self.stereo.clear();
        for frame in samples.chunks_exact(ch) {
            let l = frame[0];
            let r = if ch > 1 { frame[1] } else { l };
            self.stereo.push(l);
            self.stereo.push(r);
        }
        self.send(rate);
    }

    /// [`Self::push`] for planar sound: one slice a channel, all the same length (a decoder's or
    /// a web page's planes).
    pub fn push_planar(&mut self, planes: &[&[f32]], rate: u32) {
        let Some(l) = planes.first() else { return };
        let r = planes.get(1).unwrap_or(l);
        if l.is_empty() || r.is_empty() || rate == 0 {
            return;
        }
        self.stereo.clear();
        for (&a, &b) in l.iter().zip(r.iter()) {
            self.stereo.push(a);
            self.stereo.push(b);
        }
        self.send(rate);
    }

    /// The stereo block in `self.stereo`, at `rate`, into the engine's ring.
    fn send(&mut self, rate: u32) {
        if rate == self.rate {
            write_ring(&mut self.ring, &self.stereo);
        } else {
            if rate != self.from {
                self.resampler.retune(rate, self.rate);
                self.from = rate;
            }
            self.resampler.run(&self.stereo);
            write_ring(&mut self.ring, self.resampler.output());
        }
    }
}

/// `samples` into `ring` whole, or not at all when it hasn't the room (never waits).
fn write_ring(ring: &mut rtrb::Producer<f32>, samples: &[f32]) {
    if samples.is_empty() || ring.slots() < samples.len() {
        return;
    }
    if let Ok(mut chunk) = ring.write_chunk_uninit(samples.len()) {
        let (a, b) = chunk.as_mut_slices();
        for (dst, src) in a.iter_mut().chain(b.iter_mut()).zip(samples) {
            dst.write(*src);
        }
        unsafe { chunk.commit_all() };
    }
}

/// A source that makes its own sound and pushes it into the [`InputFeed`] it's given while a
/// track takes it.
pub trait SoundFeeder: Send + Sync {
    /// A track takes this source's sound: push it into `feed` from now on (`Some`), or nothing
    /// takes it any more (`None`; the last feed is dropped, and anything only it needed, such as
    /// a capture, can stop). Called on the engine thread: hand over and return, never wait on a
    /// capture starting.
    fn attach(&self, feed: Option<InputFeed>);
}

type Feeders = Vec<(Id, Arc<dyn SoundFeeder>)>;

/// Sources that feed the engine themselves, by document id.
static FEEDERS: Mutex<Feeders> = Mutex::new(Vec::new());

/// Registers document source `source` as feeding the engine itself (see [`SoundFeeder`]): from
/// now on a track on it takes its sound from `feeder`. Call when its runtime is made, before any
/// track can take it, and [`feeds_itself_no_more`] when it goes.
pub fn feeds_itself(source: Id, feeder: Arc<dyn SoundFeeder>) {
    let mut all = FEEDERS.lock();
    all.retain(|(s, _)| *s != source);
    all.push((source, feeder));
}

/// Ends [`feeds_itself`] (or [`feed_slot`]) for `source`. Nothing for a source that never
/// registered.
pub fn feeds_itself_no_more(source: Id) {
    FEEDERS.lock().retain(|(s, _)| *s != source);
    SLOTS.lock().retain(|(s, _)| *s != source);
}

/// Whether `source` feeds the engine itself.
pub fn feeds(source: Id) -> bool {
    FEEDERS.lock().iter().any(|(s, _)| *s == source)
}

/// The feed a track hands a source registered with [`feed_slot`], held for whatever makes the
/// source's sound (a decoder, a page, a receiver) to push into.
#[derive(Default)]
pub struct FeedSlot(Mutex<Option<InputFeed>>);

impl FeedSlot {
    /// Whether a track takes the sound now.
    pub fn attached(&self) -> bool {
        self.0.lock().is_some()
    }

    /// Pushes planar sound into the track's feed ([`InputFeed::push_planar`]). True when the
    /// mixer took it (or it was dropped while the feed was being swapped: never waits); false
    /// when no track takes the sound.
    pub fn push_planar(&self, planes: &[&[f32]], rate: u32) -> bool {
        match self.0.try_lock() {
            Some(mut feed) => feed.as_mut().map(|f| f.push_planar(planes, rate)).is_some(),
            None => true,
        }
    }

    /// [`Self::push_planar`] for interleaved sound ([`InputFeed::push`]).
    pub fn push(&self, samples: &[f32], channels: u32, rate: u32) -> bool {
        match self.0.try_lock() {
            Some(mut feed) => feed.as_mut().map(|f| f.push(samples, channels, rate)).is_some(),
            None => true,
        }
    }
}

impl SoundFeeder for FeedSlot {
    fn attach(&self, feed: Option<InputFeed>) {
        // the old feed (if any) is dropped here, on the engine thread
        drop(std::mem::replace(&mut *self.0.lock(), feed));
    }
}

/// Sources registered with [`feed_slot`], by document id.
static SLOTS: Mutex<Vec<(Id, Arc<FeedSlot>)>> = Mutex::new(Vec::new());

/// Registers document source `source` as feeding the engine itself ([`feeds_itself`]) through a
/// [`FeedSlot`] that whatever makes its sound pushes into, and returns the slot. Call when its
/// runtime is made, and [`feeds_itself_no_more`] when it goes.
pub fn feed_slot(source: Id) -> Arc<FeedSlot> {
    let slot = Arc::new(FeedSlot::default());
    {
        let mut all = SLOTS.lock();
        all.retain(|(s, _)| *s != source);
        all.push((source, slot.clone()));
    }
    feeds_itself(source, slot.clone());
    slot
}

/// The slot `source` was registered with ([`feed_slot`]), if any.
pub fn slot_of(source: Id) -> Option<Arc<FeedSlot>> {
    SLOTS.lock().iter().find(|(s, _)| *s == source).map(|(_, f)| f.clone())
}

fn feeder_of(source: Id) -> Option<Arc<dyn SoundFeeder>> {
    FEEDERS.lock().iter().find(|(s, _)| *s == source).map(|(_, f)| f.clone())
}


// ---------------------------------------------------------------- the real-time side

struct RtInput {
    id: la::NodeId,
    /// Interleaved frames of `bufs.len()` channels.
    ring: rtrb::Consumer<f32>,
    /// This block, one buffer per channel.
    bufs: Vec<Box<[f32]>>,
    /// Frames buffered before it plays, and the most kept.
    prime: usize,
    most: usize,
    primed: bool,
    has: bool,
}

impl RtInput {
    fn new(id: la::NodeId, ring: rtrb::Consumer<f32>, channels: usize, prime: usize, most: usize) -> Self {
        let bufs = (0..channels.max(1)).map(|_| vec![0.0; la::MAX_BLOCK].into_boxed_slice()).collect();
        Self { id, ring, bufs, prime, most, primed: false, has: false }
    }
}

struct RtInputs(Vec<RtInput>);

impl la::Inputs for RtInputs {
    fn channel(&self, input: la::NodeId, ch: u16) -> Option<&[f32]> {
        let i = self.0.iter().find(|i| i.id == input && i.has)?;
        // a mono input is heard on both sides
        Some(&i.bufs[(ch as usize).min(i.bufs.len() - 1)][..])
    }
}

/// A multitrack file being recorded, on the audio thread: what to take and where it goes.
struct MultitrackRt {
    id: la::NodeId,
    tap: la::Tap,
    channels: u16,
    ring: rtrb::Producer<f32>,
}

/// Rate of the sound [`AudioHost::listen`] hands out (mono, 16-bit): what speech recognition
/// and the co-host's live voice take.
pub const LISTEN_RATE: u32 = 16_000;

/// A strip being listened to (the co-host hears it), on the audio thread: its sound folded to
/// mono and brought down to [`LISTEN_RATE`] by averaging (a box filter: enough for speech).
struct ListenRt {
    id: la::NodeId,
    tap: la::Tap,
    /// Silence while set (the track is muted, for a tap before its fader).
    muted: Arc<AtomicBool>,
    ring: rtrb::Producer<i16>,
    step: f64,
    phase: f64,
    sum: f32,
    count: u32,
}

impl ListenRt {
    fn feed(&mut self, src: Option<(&[f32], &[f32])>, block: usize) {
        let muted = self.muted.load(Ordering::Relaxed);
        let mut out = [0i16; la::MAX_BLOCK];
        let mut n = 0usize;
        for k in 0..block {
            let x = match src {
                Some((l, r)) if !muted => (l[k] + r[k]) * 0.5,
                _ => 0.0,
            };
            self.sum += x;
            self.count += 1;
            self.phase += self.step;
            if self.phase >= 1.0 {
                self.phase -= 1.0;
                out[n] = ((self.sum / self.count as f32).clamp(-1.0, 1.0) * 32767.0) as i16;
                n += 1;
                (self.sum, self.count) = (0.0, 0);
            }
        }
        // what doesn't fit (nobody reading) is dropped, never waited for
        let room = self.ring.slots().min(n);
        if room > 0
            && let Ok(chunk) = self.ring.write_chunk_uninit(room)
        {
            chunk.fill_from_iter(out[..room].iter().copied());
        }
    }
}

enum ToRt {
    AddInput(RtInput),
    Listen(ListenRt),
    RemoveInput(la::NodeId),
    /// Start these files on the next block (all on the same one: sample-aligned).
    StartMultitrack(Vec<MultitrackRt>),
    StopMultitrack,
    PauseMultitrack(bool),
    /// What the monitor plays (a mix, or the program), and its ring; `None`: no direct monitor.
    Monitor(Option<(la::NodeId, rtrb::Producer<f32>)>),
    /// Mixes played on devices of their own: each one's node and ring (replacing the last list).
    Extras(Vec<(la::NodeId, rtrb::Producer<f32>)>),
    /// Which mix each out source after Program carries, and the cue mix.
    Mixes([Option<la::NodeId>; OUT_MIXES - 1], Option<la::NodeId>),
}

/// Running state handed back from the audio thread to be freed.
#[allow(dead_code)] // held only so they're freed on the main side, never on the audio thread
enum FromRt {
    Tap(MixTapRt),
    Input(RtInput),
    Multitrack(Vec<MultitrackRt>),
    Monitor(rtrb::Producer<f32>),
    Listen(ListenRt),
    Extras(Vec<(la::NodeId, rtrb::Producer<f32>)>),
}

/// One block of `out` (silence when `None`) into an output's ring (stereo, interleaved), unless
/// the ring is already as far ahead of its device as it should be.
fn feed_output(out: Option<(&[f32], &[f32])>, ring: &mut rtrb::Producer<f32>, block: usize) {
    let ahead = (ring.buffer().capacity() - ring.slots()) / 2;
    if ahead + block <= monitor_most(block)
        && let Ok(mut chunk) = ring.write_chunk_uninit(block * 2)
    {
        let (a, b) = chunk.as_mut_slices();
        for k in 0..block {
            let (l, r) = out.map_or((0.0, 0.0), |(l, r)| (l[k], r[k]));
            for (c, v) in [l, r].into_iter().enumerate() {
                let i = k * 2 + c;
                if i < a.len() {
                    a[i].write(v);
                } else {
                    b[i - a.len()].write(v);
                }
            }
        }
        unsafe { chunk.commit_all() };
    }
}

/// The most the monitor ring holds ahead of the device (frames, at least three blocks): keeps its
/// latency down when the engine's clock runs ahead of the device's.
const MONITOR_MOST: usize = 768;

fn monitor_most(block: usize) -> usize {
    MONITOR_MOST.max(block * 3)
}

/// Whether multitrack files are being recorded (for the UI's record state).
pub static STEMS_RUNNING: AtomicBool = AtomicBool::new(false);
/// When the first sample of the current multitrack files was played (Lightair's clock, ns; 0:
/// not yet).
static STEMS_START_NS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
/// Samples dropped because a file's writer fell behind (never expected).
pub static STEM_OVERRUNS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn rt_loop(
    mut renderer: la::Renderer,
    mut commands: rtrb::Consumer<ToRt>,
    mut garbage: rtrb::Producer<FromRt>,
    sample_rate: u32,
    block: usize,
    stop: Arc<AtomicBool>,
) {
    let mut inputs = RtInputs(Vec::with_capacity(128));
    let mut mixes: [Option<la::NodeId>; OUT_MIXES - 1] = [None; OUT_MIXES - 1];
    let mut cue: Option<la::NodeId> = None;
    let mut track_files: Vec<MultitrackRt> = Vec::new();
    let mut monitor: Option<(la::NodeId, rtrb::Producer<f32>)> = None;
    let mut extras: Vec<(la::NodeId, rtrb::Producer<f32>)> = Vec::new();
    let mut listens: Vec<ListenRt> = Vec::with_capacity(16);
    let mut taps: Vec<MixTapRt> = Vec::with_capacity(MOST_TAPS);
    let mut multitrack_paused = false;
    let mut multitrack_started = false;
    let period = Duration::from_secs_f64(block as f64 / sample_rate as f64);
    // real-time scheduling (Mach time constraint on macOS, MMCSS "Pro Audio" on Windows): without
    // it a busy computer can starve the mixer and the stream drops out
    if !lightair_platform::realtime::promote_audio_thread(period) {
        tracing::warn!("the audio thread runs without real-time scheduling");
    }
    let start = Instant::now();
    let mut blocks = 0u64;
    let anchor = crate::clock::now_ns();
    let ns_per_frame = 1e9 / sample_rate as f64;
    // the engine's output is stamped this far behind real time: what the inputs buffer
    let latency_ns = (PRIME as f64 * ns_per_frame) as u64;
    while !stop.load(Ordering::Relaxed) {
        while let Ok(cmd) = commands.pop() {
            match cmd {
                ToRt::AddInput(i) => {
                    if inputs.0.len() < inputs.0.capacity() {
                        inputs.0.push(i)
                    } else {
                        let _ = garbage.push(FromRt::Input(i));
                    }
                }
                ToRt::RemoveInput(id) => {
                    if let Some(k) = inputs.0.iter().position(|i| i.id == id) {
                        let gone = inputs.0.swap_remove(k);
                        let _ = garbage.push(FromRt::Input(gone));
                    }
                }
                ToRt::Mixes(m, c) => (mixes, cue) = (m, c),
                ToRt::Listen(l) => {
                    if listens.len() < listens.capacity() {
                        listens.push(l)
                    } else {
                        let _ = garbage.push(FromRt::Listen(l));
                    }
                }
                ToRt::StartMultitrack(new) => {
                    let old = std::mem::replace(&mut track_files, new);
                    let _ = garbage.push(FromRt::Multitrack(old));
                    (multitrack_paused, multitrack_started) = (false, false);
                }
                ToRt::StopMultitrack => {
                    let old = std::mem::take(&mut track_files);
                    let _ = garbage.push(FromRt::Multitrack(old));
                }
                ToRt::PauseMultitrack(p) => multitrack_paused = p,
                ToRt::Monitor(m) => {
                    if let Some((_, old)) = std::mem::replace(&mut monitor, m) {
                        let _ = garbage.push(FromRt::Monitor(old));
                    }
                }
                ToRt::Extras(e) => {
                    let old = std::mem::replace(&mut extras, e);
                    let _ = garbage.push(FromRt::Extras(old));
                }
            }
        }
        // mix taps taken or let go of (never waiting: a busy lock is tried again next block)
        if TAPS_CHANGED.swap(false, Ordering::AcqRel) {
            match (TAPS_NEW.try_lock(), TAPS_GONE.try_lock()) {
                (Some(mut new), Some(mut gone)) => {
                    for id in gone.drain(..) {
                        if let Some(k) = taps.iter().position(|t| t.id == id) {
                            let _ = garbage.push(FromRt::Tap(taps.swap_remove(k)));
                        } else if let Some(k) = new.iter().position(|t| t.id == id) {
                            // let go of before it was taken (dropped here: not the audio thread's)
                            new.swap_remove(k);
                        }
                    }
                    while taps.len() < taps.capacity()
                        && let Some(t) = new.pop()
                    {
                        taps.push(t);
                    }
                }
                _ => TAPS_CHANGED.store(true, Ordering::Release),
            }
        }
        // a block from each input
        for input in &mut inputs.0 {
            let nch = input.bufs.len();
            let avail = input.ring.slots() / nch;
            if !input.primed && avail >= input.prime {
                input.primed = true;
            }
            if avail > input.most {
                // drifted ahead (a faster device clock): drop back to the target
                let drop = (avail - input.prime) * nch;
                if let Ok(c) = input.ring.read_chunk(drop) {
                    c.commit_all();
                }
            }
            let avail = input.ring.slots() / nch;
            input.has = input.primed && avail > 0;
            if input.primed && avail == 0 {
                input.primed = false; // empty: the source stopped; buffer up again when it's back
            }
            if input.has {
                // a short ring plays what it has and pads the rest (a tiny gap, not a silence)
                let take = avail.min(block);
                if take < block {
                    SHORTFALLS.fetch_add(1, Ordering::Relaxed);
                }
                if let Ok(chunk) = input.ring.read_chunk(take * nch) {
                    let (a, b) = chunk.as_slices();
                    for k in 0..take {
                        for (c, buf) in input.bufs.iter_mut().enumerate() {
                            let i = k * nch + c;
                            buf[k] = if i < a.len() { a[i] } else { b[i - a.len()] };
                        }
                    }
                    chunk.commit_all();
                }
                for buf in input.bufs.iter_mut() {
                    buf[take..block].fill(0.0);
                }
            }
        }
        RT_INPUTS.store(inputs.0.len(), Ordering::Relaxed);
        RT_INPUTS_PLAYING.store(inputs.0.iter().filter(|i| i.has).count(), Ordering::Relaxed);
        RT_BLOCKS.fetch_add(1, Ordering::Relaxed);
        let graph = renderer.process(&inputs, block);
        let ts = anchor + (blocks as f64 * block as f64 * ns_per_frame) as u64 - latency_ns;
        // the mixes, taken (silence for a mix with nothing routed to it)
        for tap in &mut taps {
            let out = match tap.mix {
                0 => Some(graph.program()),
                OUT_MIXES => cue.and_then(|m| graph.mix(m)),
                k => mixes.get(k - 1).copied().flatten().and_then(|m| graph.mix(m)),
            };
            if tap.ring.is_full() {
                continue;
            }
            let mut p = MixPacket { ts, frames: block, data: [0.0; 2 * la::MAX_BLOCK] };
            if let Some((l, r)) = out {
                for k in 0..block.min(l.len()).min(r.len()) {
                    p.data[2 * k] = l[k];
                    p.data[2 * k + 1] = r[k];
                }
            }
            let _ = tap.ring.push(p);
        }
        // the direct monitor: this block of the cue mix (or the program)
        if let Some((src, ring)) = &mut monitor {
            let out = if *src == la::PROGRAM { Some(graph.program()) } else { graph.mix(*src) };
            feed_output(out, ring, block);
        }
        // mixes played on devices of their own
        for (src, ring) in &mut extras {
            let out = if *src == la::PROGRAM { Some(graph.program()) } else { graph.mix(*src) };
            feed_output(out, ring, block);
        }
        // what's being listened to (the co-host): this block of each; one nobody reads any more goes
        if let Some(k) = listens.iter().position(|l| l.ring.is_abandoned()) {
            let _ = garbage.push(FromRt::Listen(listens.swap_remove(k)));
        }
        for l in &mut listens {
            let src = if l.id == la::PROGRAM { Some(graph.program()) } else { graph.tap(l.id, l.tap) };
            l.feed(src, block);
        }
        // multitrack files: this block of each, interleaved
        if !track_files.is_empty() && !multitrack_paused {
            if !multitrack_started {
                STEMS_START_NS.store(ts, Ordering::Relaxed);
                multitrack_started = true;
            }
            for track_file in &mut track_files {
                let src = if track_file.id == la::PROGRAM { Some(graph.program()) } else { graph.tap(track_file.id, track_file.tap) };
                let n = block * track_file.channels as usize;
                let Ok(mut chunk) = track_file.ring.write_chunk_uninit(n) else {
                    STEM_OVERRUNS.fetch_add(block as u64, Ordering::Relaxed);
                    continue;
                };
                let (a, b) = chunk.as_mut_slices();
                let mut i = 0usize;
                for k in 0..block {
                    let (l, r) = src.map_or((0.0, 0.0), |(l, r)| (l[k], r[k]));
                    let frame = [l, r];
                    for v in &frame[..track_file.channels as usize] {
                        if i < a.len() {
                            a[i].write(*v);
                        } else {
                            b[i - a.len()].write(*v);
                        }
                        i += 1;
                    }
                }
                unsafe { chunk.commit_all() };
            }
        }
        blocks += 1;
        let due = start + period * blocks as u32;
        let now = Instant::now();
        if due > now {
            std::thread::sleep(due - now);
        } else if now - due > period * 20 {
            // far behind (the machine stalled): skip ahead rather than race
            blocks = ((now - start).as_secs_f64() / period.as_secs_f64()) as u64;
        }
    }
    // the taps outlive this thread (the engine restarting with another block size)
    if !taps.is_empty() {
        TAPS_NEW.lock().extend(taps);
        TAPS_CHANGED.store(true, Ordering::Release);
    }
}


// ---------------------------------------------------------------- the mixes, taken

/// Whether the audio engine plays the mixer: always (nothing else mixes the tracks).
pub fn mixer_on() -> bool {
    true
}

/// A block of a mix as the engine made it: its time (Lightair's clock) and stereo frames,
/// interleaved.
struct MixPacket {
    ts: u64,
    frames: usize,
    data: [f32; 2 * la::MAX_BLOCK],
}

/// The rate the audio engine runs at (0 before it first starts): what taps' blocks are at.
static RATE: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

/// The audio engine's rate (what [`tap_mix`]'s blocks are at); 48 kHz before it has started.
pub fn engine_rate() -> u32 {
    match RATE.load(Ordering::Relaxed) {
        0 => 48_000,
        r => r,
    }
}

/// Packets a tap may have waiting (about 0.7 s at 256 frames).
const TAP_PACKETS: usize = 128;

/// A tap on the audio thread: which mix, and where its blocks go.
struct MixTapRt {
    id: u64,
    mix: usize,
    ring: rtrb::Producer<MixPacket>,
}

/// Taps the audio thread hasn't taken yet (new, or kept across an engine restart), and taps
/// let go of; `TAPS_CHANGED` says to look.
static TAPS_NEW: Mutex<Vec<MixTapRt>> = Mutex::new(Vec::new());
static TAPS_GONE: Mutex<Vec<u64>> = Mutex::new(Vec::new());
static TAPS_CHANGED: AtomicBool = AtomicBool::new(false);
/// The most taps at once.
const MOST_TAPS: usize = 32;

/// A mix taken as it's made ([`tap_mix`]); dropping it lets go.
pub struct MixTap {
    id: u64,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Drop for MixTap {
    fn drop(&mut self) {
        TAPS_GONE.lock().push(self.id);
        TAPS_CHANGED.store(true, Ordering::Release);
        self.stop.store(true, Ordering::Release);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// Takes mix `mix` (0: Program, 1…5: the mixes, [`OUT_MIXES`]: the cue) from the audio engine
/// as it's made: `f` gets each
/// block (stereo, interleaved, at the engine's rate) and its time on Lightair's clock, on a
/// thread of the tap's own (never the audio thread). A mix with nothing routed to it is silence.
pub fn tap_mix(mix: usize, mut f: impl FnMut(&[f32], u64) + Send + 'static) -> MixTap {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    let id = NEXT.fetch_add(1, Ordering::Relaxed);
    let (ring, mut packets) = rtrb::RingBuffer::<MixPacket>::new(TAP_PACKETS);
    TAPS_NEW.lock().push(MixTapRt { id, mix, ring });
    TAPS_CHANGED.store(true, Ordering::Release);
    let stop = Arc::new(AtomicBool::new(false));
    let s = stop.clone();
    let thread = std::thread::Builder::new()
        .name("lightair-mix-tap".into())
        .spawn(move || {
            while !s.load(Ordering::Acquire) {
                let mut any = false;
                while let Ok(p) = packets.pop() {
                    f(&p.data[..p.frames * 2], p.ts);
                    any = true;
                }
                if !any {
                    std::thread::sleep(Duration::from_millis(2));
                }
            }
        })
        .ok();
    MixTap { id, stop, thread }
}

// ---------------------------------------------------------------- the main side

/// A multitrack file to record.
#[derive(Debug, Clone)]
pub struct TrackFileSpec {
    /// The strip (track or bus), or [`la::PROGRAM`] for the program mix.
    pub id: la::NodeId,
    pub path: std::path::PathBuf,
    /// 1 (a mono track) or 2.
    pub channels: u16,
    pub tap: la::Tap,
    /// 32-bit float rather than 24-bit integer.
    pub float: bool,
}

/// What a finished multitrack recording wrote.
#[derive(Debug, Clone)]
pub struct MultitrackReport {
    pub files: Vec<std::path::PathBuf>,
    /// Frames in each file.
    pub frames: u64,
    /// When their first sample played (Lightair's clock, ns): lines them up with the video file.
    pub start_ns: u64,
    pub sample_rate: u32,
}

/// A multitrack file's writer, its ring from the audio thread, and whether it's float.
type TrackFileWriter = (hound::WavWriter<std::io::BufWriter<std::fs::File>>, rtrb::Consumer<f32>, bool);

struct Recorder {
    stop: Arc<AtomicBool>,
    thread: std::thread::JoinHandle<Result<MultitrackReport, String>>,
}

/// Engine meters paired with the mixer UI's meter cells they fill.
type MeterCells = Arc<Mutex<Vec<(Arc<la::meter::Meter>, Arc<crate::meters::MeterCell>)>>>;

/// The audio engine, running. Owned by the engine thread's document host.
pub struct AudioHost {
    engine: la::Engine,
    commands: rtrb::Producer<ToRt>,
    garbage: rtrb::Consumer<FromRt>,
    /// The sources whose sound is taken, each through an [`InputFeed`].
    captures: HashMap<Id, Arc<dyn SoundFeeder>>,
    mixes: ([Option<la::NodeId>; OUT_MIXES - 1], Option<la::NodeId>),
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
    /// The mixer UI's meter cells, filled from the engine's meters ~30 times a second.
    cells: MeterCells,
    pump: Option<std::thread::JoinHandle<()>>,
    recorder: Option<Recorder>,
    sample_rate: u32,
    /// Frames per block.
    block: usize,
    /// Open interfaces: their device name and channel count, by input.
    devices: HashMap<Id, (String, u16)>,
    device_thread: Option<crate::devices::Devices>,
    /// The direct monitor: its output device and what it plays.
    monitor: Option<(String, la::NodeId)>,
    /// Mixes played on devices of their own: (the mix's node, the device) as opened.
    extras: Vec<(la::NodeId, String)>,
}

impl AudioHost {
    /// Starts the engine at `sample_rate`, processing `block` frames at a time.
    pub fn start(sample_rate: u32, block: usize) -> Option<Self> {
        let block = block.clamp(64, la::MAX_BLOCK);
        RATE.store(sample_rate, Ordering::Relaxed);
        let (engine, renderer) = la::Engine::new(la::GraphSpec::default(), sample_rate);
        let (commands, rt_commands) = rtrb::RingBuffer::new(256);
        let (rt_garbage, garbage) = rtrb::RingBuffer::new(256);
        let stop = Arc::new(AtomicBool::new(false));
        tracing::info!(sample_rate, block, "audio engine running");
        let s = stop.clone();
        let thread = std::thread::Builder::new()
            .name("lightair-audio".into())
            .spawn(move || rt_loop(renderer, rt_commands, rt_garbage, sample_rate, block, s))
            .ok()?;
        let cells: MeterCells = Default::default();
        let (c, s) = (cells.clone(), stop.clone());
        let pump = std::thread::Builder::new()
            .name("lightair-meters".into())
            .spawn(move || {
                while !s.load(Ordering::Relaxed) {
                    for (meter, cell) in c.lock().iter() {
                        let r = meter.read();
                        cell.store(&r.peak_db, &r.rms_db);
                    }
                    std::thread::sleep(Duration::from_millis(33));
                }
            })
            .ok();
        Some(Self {
            engine,
            commands,
            garbage,
            captures: HashMap::new(),
            mixes: ([None; OUT_MIXES - 1], None),
            stop,
            thread: Some(thread),
            cells,
            pump,
            recorder: None,
            sample_rate,
            block,
            devices: HashMap::new(),
            device_thread: None,
            monitor: None,
            extras: Vec::new(),
        })
    }

    /// Frames per block.
    pub fn block(&self) -> usize {
        self.block
    }

    /// Plays this mixer: compiles it, makes the first five mixes the outputs' mixes 1–5
    /// ([`tap_mix`]) and the cue mix ([`crate::audio::CUE`]) the cue.
    pub fn set(&mut self, spec: la::GraphSpec) {
        let mut mixes = [None; OUT_MIXES - 1];
        let user = spec.mixes.iter().filter(|m| m.id != crate::audio::CUE);
        for (slot, m) in mixes.iter_mut().zip(user) {
            *slot = Some(m.id);
        }
        let cue = spec.mixes.iter().any(|m| m.id == crate::audio::CUE).then_some(crate::audio::CUE);
        if (mixes, cue) != self.mixes {
            self.mixes = (mixes, cue);
            let _ = self.commands.push(ToRt::Mixes(mixes, cue));
        }
        let warnings = self.engine.set(spec);
        for w in warnings {
            tracing::warn!(?w, "audio graph corrected");
        }
        while self.garbage.pop().is_ok() {}
    }

    /// Starts taking `input`'s sound from the source itself, when it feeds the engine
    /// ([`feeds_itself`]): buffered as an interface is (its own clock, resampled to the engine's
    /// rate on the way in, drift trimmed on the audio thread), with room for Core Audio's I/O
    /// cycle. Whether it does (nothing happens otherwise).
    pub fn capture(&mut self, input: Id) -> bool {
        if self.captures.contains_key(&input) {
            return true;
        }
        let Some(feeder) = feeder_of(input) else { return false };
        let (producer, consumer) = rtrb::RingBuffer::new(FEED_FRAMES * 2);
        let prime = (self.block * 2).max(FEED_PRIME);
        let most = (self.block * 8).max(FEED_MOST);
        let rt = RtInput::new(crate::audio::node(input), consumer, 2, prime, most);
        let _ = self.commands.push(ToRt::AddInput(rt));
        feeder.attach(Some(InputFeed::new(producer, self.sample_rate)));
        self.captures.insert(input, feeder);
        true
    }

    /// Stops taking `input`'s sound (the source is told to stop feeding it).
    pub fn release(&mut self, input: Id) {
        let Some(feeder) = self.captures.remove(&input) else { return };
        feeder.attach(None);
        let _ = self.commands.push(ToRt::RemoveInput(crate::audio::node(input)));
    }

    pub fn captured(&self) -> impl Iterator<Item = Id> + '_ {
        self.captures.keys().copied()
    }

    /// Opens an audio interface for `input`, every channel ("" for the default input). A
    /// device delivers small, regular buffers: it needs little buffering.
    pub fn open_device(&mut self, input: Id, device: &str) -> Result<u16, String> {
        if self.devices.get(&input).is_some_and(|(d, _)| d == device) {
            return Ok(self.devices[&input].1);
        }
        self.close_device(input);
        // macOS hasn't asked about the microphone yet: asked without waiting, opened once
        // answered (`Reconciler::access_answered`)
        if crate::access::hold_back(crate::access::Media::Microphone) {
            return Err("waiting for microphone access".into());
        }
        let devices = self.device_thread.get_or_insert_with(crate::devices::Devices::start);
        let (producer, consumer) = rtrb::RingBuffer::new(RING_FRAMES * 32);
        let channels = devices.open(input, device, self.sample_rate, producer)?;
        // two blocks primed: a device's latency is the engine's block
        let rt = RtInput::new(crate::audio::node(input), consumer, channels as usize, self.block * 2, 2_048.max(self.block * 8));
        let _ = self.commands.push(ToRt::AddInput(rt));
        self.devices.insert(input, (device.to_string(), channels));
        Ok(channels)
    }

    pub fn close_device(&mut self, input: Id) {
        if self.devices.remove(&input).is_some() {
            if let Some(d) = &self.device_thread {
                d.close(input);
            }
            let _ = self.commands.push(ToRt::RemoveInput(crate::audio::node(input)));
        }
    }

    pub fn open_devices(&self) -> impl Iterator<Item = Id> + '_ {
        self.devices.keys().copied()
    }

    /// An input fed by the caller (tests): `channels` interleaved, as a device would.
    pub fn test_input(&mut self, input: Id, channels: u16) -> rtrb::Producer<f32> {
        let (producer, consumer) = rtrb::RingBuffer::new(RING_FRAMES * channels as usize);
        let rt = RtInput::new(crate::audio::node(input), consumer, channels as usize, 512, RING_FRAMES / 2);
        let _ = self.commands.push(ToRt::AddInput(rt));
        producer
    }

    /// Starts recording these multitrack files, sample-aligned, until [`Self::stop_multitrack`].
    pub fn start_multitrack(&mut self, specs: Vec<TrackFileSpec>) -> Result<(), String> {
        if self.recorder.is_some() {
            self.stop_multitrack();
        }
        let sample_rate = self.sample_rate;
        let mut writers = Vec::new();
        let mut rts = Vec::new();
        let mut files = Vec::new();
        for spec in &specs {
            let wav = hound::WavSpec {
                channels: spec.channels,
                sample_rate,
                bits_per_sample: if spec.float { 32 } else { 24 },
                sample_format: if spec.float { hound::SampleFormat::Float } else { hound::SampleFormat::Int },
            };
            let writer = hound::WavWriter::create(&spec.path, wav).map_err(|e| format!("{}: {e}", spec.path.display()))?;
            // ten seconds of slack for the writer
            let (producer, consumer) = rtrb::RingBuffer::new(sample_rate as usize * 10 * spec.channels as usize);
            writers.push((writer, consumer, spec.float));
            rts.push(MultitrackRt { id: spec.id, tap: spec.tap, channels: spec.channels, ring: producer });
            files.push(spec.path.clone());
        }
        STEMS_START_NS.store(0, Ordering::Relaxed);
        if self.commands.push(ToRt::StartMultitrack(rts)).is_err() {
            return Err("the audio engine is busy".into());
        }
        STEMS_RUNNING.store(true, Ordering::Relaxed);
        let stop = Arc::new(AtomicBool::new(false));
        let s = stop.clone();
        let thread = std::thread::Builder::new()
            .name("lightair-track_files".into())
            .spawn(move || {
                let mut frames = 0u64;
                let drain = |writers: &mut Vec<TrackFileWriter>, frames: &mut u64|
                 -> Result<(), String> {
                    for (k, (w, ring, float)) in writers.iter_mut().enumerate() {
                        let n = ring.slots();
                        if n == 0 {
                            continue;
                        }
                        let chunk = ring.read_chunk(n).map_err(|e| e.to_string())?;
                        let (a, b) = chunk.as_slices();
                        for &v in a.iter().chain(b) {
                            if *float {
                                w.write_sample(v).map_err(|e| e.to_string())?;
                            } else {
                                w.write_sample((v.clamp(-1.0, 1.0) * 8_388_607.0) as i32).map_err(|e| e.to_string())?;
                            }
                        }
                        if k == 0 {
                            *frames += (n / w.spec().channels as usize) as u64;
                        }
                        chunk.commit_all();
                    }
                    Ok(())
                };
                let mut writers = writers;
                while !s.load(Ordering::Relaxed) {
                    drain(&mut writers, &mut frames)?;
                    std::thread::sleep(Duration::from_millis(40));
                }
                // the audio thread has stopped feeding: take the rest
                drain(&mut writers, &mut frames)?;
                for (w, _, _) in writers {
                    w.finalize().map_err(|e| e.to_string())?;
                }
                Ok(MultitrackReport { files, frames, start_ns: STEMS_START_NS.load(Ordering::Relaxed), sample_rate })
            })
            .map_err(|e| e.to_string())?;
        self.recorder = Some(Recorder { stop, thread });
        Ok(())
    }

    /// Pauses or resumes the multitrack files (no gap: the files just don't get those blocks).
    pub fn pause_multitrack(&mut self, pause: bool) {
        if self.recorder.is_some() {
            let _ = self.commands.push(ToRt::PauseMultitrack(pause));
        }
    }

    /// Stops the multitrack files and finishes writing them.
    pub fn stop_multitrack(&mut self) -> Option<MultitrackReport> {
        let rec = self.recorder.take()?;
        let _ = self.commands.push(ToRt::StopMultitrack);
        // let the audio thread see it (two blocks), then the writer takes the rest
        std::thread::sleep(Duration::from_millis(15));
        rec.stop.store(true, Ordering::Relaxed);
        STEMS_RUNNING.store(false, Ordering::Relaxed);
        while self.garbage.pop().is_ok() {}
        match rec.thread.join() {
            Ok(Ok(report)) => Some(report),
            Ok(Err(e)) => {
                tracing::error!(%e, "multitrack recording failed");
                None
            }
            Err(_) => None,
        }
    }

    /// Plays `source` (the cue mix, or [`la::PROGRAM`]) straight to output `device` ("" for
    /// the default), with a few milliseconds of buffering; `None` stops it.
    pub fn set_headphones(&mut self, monitor: Option<(String, la::NodeId)>) -> Result<(), String> {
        if monitor == self.monitor {
            return Ok(());
        }
        let _ = self.commands.push(ToRt::Monitor(None));
        if let Some(d) = &self.device_thread {
            d.close_output();
        }
        self.monitor = None;
        let Some((device, source)) = monitor else {
            return Ok(());
        };
        let (producer, consumer) = rtrb::RingBuffer::new(monitor_most(self.block) * 4);
        let devices = self.device_thread.get_or_insert_with(crate::devices::Devices::start);
        match devices.open_output(&device, self.sample_rate, consumer) {
            Ok(()) => {
                let _ = self.commands.push(ToRt::Monitor(Some((source, producer))));
                self.monitor = Some((device, source));
                Ok(())
            }
            Err(e) => Err(e),
        }
    }

    /// Plays each of `wanted` (a mix's node, an output device) on that device, besides
    /// everything else; outputs no longer wanted close. Returns what couldn't open.
    pub fn set_extra_outputs(&mut self, wanted: Vec<(la::NodeId, String)>) -> Vec<String> {
        if wanted == self.extras {
            return Vec::new();
        }
        let devices = self.device_thread.get_or_insert_with(crate::devices::Devices::start);
        for (node, _) in &self.extras {
            devices.close_extra(*node);
        }
        let mut rings = Vec::new();
        let mut opened = Vec::new();
        let mut failed = Vec::new();
        for (node, device) in wanted {
            let (producer, consumer) = rtrb::RingBuffer::new(monitor_most(self.block) * 4);
            match devices.open_extra(node, &device, self.sample_rate, consumer) {
                Ok(()) => {
                    rings.push((node, producer));
                    opened.push((node, device));
                }
                Err(e) => failed.push(format!("{device}: {e}")),
            }
        }
        let _ = self.commands.push(ToRt::Extras(rings));
        // (what failed is tried again when the mixer next changes)
        self.extras = opened;
        failed
    }

    /// A monitor whose output the caller reads (tests).
    pub fn test_monitor(&mut self, source: la::NodeId) -> rtrb::Consumer<f32> {
        let (producer, consumer) = rtrb::RingBuffer::new(monitor_most(self.block) * 4);
        let _ = self.commands.push(ToRt::Monitor(Some((source, producer))));
        consumer
    }

    /// A strip's sound to listen to (the co-host hears it): mono at [`LISTEN_RATE`], from `tap`
    /// (silent while `muted`). Stops when the consumer is dropped, or this engine stops (the
    /// consumer is then abandoned).
    pub fn listen(&mut self, id: la::NodeId, tap: la::Tap, muted: Arc<AtomicBool>) -> rtrb::Consumer<i16> {
        let (ring, consumer) = rtrb::RingBuffer::new(LISTEN_RATE as usize * 4);
        let step = LISTEN_RATE as f64 / self.sample_rate as f64;
        let l = ListenRt { id, tap, muted, ring, step, phase: 0.0, sum: 0.0, count: 0 };
        let _ = self.commands.push(ToRt::Listen(l));
        consumer
    }

    /// Which tracks' meter cells (the mixer UI's) show which engine strips.
    pub fn set_meter_cells(&mut self, cells: Vec<(la::NodeId, Arc<crate::meters::MeterCell>)>) {
        let pairs = cells.into_iter().map(|(id, cell)| (self.engine.meter(id), cell)).collect();
        *self.cells.lock() = pairs;
    }

    /// A strip's, mix's or the program's meter.
    pub fn meter(&mut self, id: la::NodeId) -> Arc<la::meter::Meter> {
        self.engine.meter(id)
    }
}

impl Drop for AudioHost {
    fn drop(&mut self) {
        self.stop_multitrack();
        self.device_thread = None; // closes the device streams
        let inputs: Vec<_> = self.captures.keys().copied().collect();
        for i in inputs {
            self.release(i);
        }
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
        if let Some(t) = self.pump.take() {
            let _ = t.join();
        }
    }
}
