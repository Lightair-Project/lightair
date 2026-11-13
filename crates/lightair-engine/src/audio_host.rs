//! Runs the audio engine in real time against libobs (docs/plans/audio-engine.md, step 4).
//!
//! - **In**: each track's input is a libobs source; its audio (after any libobs filters) comes
//!   through an audio-capture callback into a lock-free ring, and libobs no longer mixes the
//!   source itself (its audio mixers are cleared).
//! - **Engine**: a real-time thread takes a block from each input's ring, runs the
//!   `lightair-audio` graph, and
//! - **Out**: hands Program and the mixes to libobs through `lightair_audio_out` sources, one per
//!   libobs mix (Program in mix 1, the mixes in 2–6), which the stream and recording encoders take.

use std::collections::HashMap;
use std::ffi::{CStr, c_char, c_void};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use lightair_audio as la;
use libobs_sys as sys;
use parking_lot::Mutex;

use crate::obs::Source;

pub const OUT_SOURCE: &str = "lightair_audio_out";
pub const TEST_TONE: &str = "lightair_test_tone";
/// Frames per engine block (5.3 ms at 48 kHz).
pub const BLOCK: usize = 256;
/// libobs mixes the engine fills: Program, then up to five mixes.
pub const OUT_MIXES: usize = 6;
/// libobs output channels the out sources sit on (kept active there): the cue out, then the
/// mixes. Tracks use the ones below.
pub const OUT_CHANNELS: std::ops::Range<u32> = 57..64;
/// Ring size per input (frames).
const RING_FRAMES: usize = 48_000;
/// Buffered before an input starts playing, and the most kept (frames): libobs delivers audio
/// in packets of 1024 frames, and a busy machine delays them; two packets absorb that.
const PRIME: usize = 2_048;
const MOST: usize = 6_144;

/// Times an input ran short (a gap in its audio), for diagnostics and tests.
pub static SHORTFALLS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

// ---------------------------------------------------------------- libobs source types

unsafe extern "C" fn out_name(_: *mut c_void) -> *const c_char {
    c"Lightair mix".as_ptr()
}

unsafe extern "C" fn out_create(_: *mut sys::obs_data_t, _: *mut sys::obs_source_t) -> *mut c_void {
    Box::into_raw(Box::new(0u8)) as *mut c_void
}

unsafe extern "C" fn out_destroy(data: *mut c_void) {
    drop(unsafe { Box::from_raw(data as *mut u8) });
}

struct Tone {
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

unsafe extern "C" fn tone_name(_: *mut c_void) -> *const c_char {
    c"Test tone".as_ptr()
}

struct SendPtr(*mut sys::obs_source_t);
unsafe impl Send for SendPtr {}

/// A sine at `freq` Hz and `amplitude`, pushed in 1024-frame packets like a device would.
unsafe extern "C" fn tone_create(settings: *mut sys::obs_data_t, source: *mut sys::obs_source_t) -> *mut c_void {
    let freq = unsafe { sys::obs_data_get_double(settings, c"freq".as_ptr()) };
    let amp = unsafe { sys::obs_data_get_double(settings, c"amplitude".as_ptr()) } as f32;
    let stop = Arc::new(AtomicBool::new(false));
    let s = stop.clone();
    let src = SendPtr(source);
    let thread = std::thread::spawn(move || {
        let src = src;
        let sr = 48_000.0;
        let frames = 1024usize;
        let (mut l, mut phase) = (vec![0f32; frames], 0f64);
        let start = unsafe { sys::os_gettime_ns() };
        let mut sent = 0u64;
        while !s.load(Ordering::Relaxed) {
            for x in l.iter_mut() {
                *x = (phase * std::f64::consts::TAU).sin() as f32 * amp;
                phase = (phase + freq / sr).fract();
            }
            let audio = sys::obs_source_audio {
                data: [l.as_ptr() as *const u8, l.as_ptr() as *const u8, std::ptr::null(), std::ptr::null(), std::ptr::null(), std::ptr::null(), std::ptr::null(), std::ptr::null()],
                frames: frames as u32,
                speakers: sys::SPEAKERS_STEREO,
                format: sys::AUDIO_FORMAT_FLOAT_PLANAR,
                samples_per_sec: 48_000,
                timestamp: start + sent * 1_000_000_000 / 48_000,
            };
            unsafe { sys::obs_source_output_audio(src.0, &audio) };
            sent += frames as u64;
            let due = start + sent * 1_000_000_000 / 48_000;
            let now = unsafe { sys::os_gettime_ns() };
            if due > now {
                std::thread::sleep(Duration::from_nanos(due - now));
            }
        }
    });
    Box::into_raw(Box::new(Tone { stop, thread: Some(thread) })) as *mut c_void
}

unsafe extern "C" fn tone_destroy(data: *mut c_void) {
    let mut t = unsafe { Box::from_raw(data as *mut Tone) };
    t.stop.store(true, Ordering::Relaxed);
    if let Some(h) = t.thread.take() {
        let _ = h.join();
    }
}

/// Registers Lightair's own audio source types with libobs (once; after startup).
pub fn register_types() {
    static DONE: std::sync::Once = std::sync::Once::new();
    DONE.call_once(|| unsafe {
        let mut out: sys::obs_source_info = std::mem::zeroed();
        out.id = c"lightair_audio_out".as_ptr();
        out.type_ = sys::OBS_SOURCE_TYPE_INPUT;
        out.output_flags = sys::OBS_SOURCE_AUDIO;
        out.get_name = Some(out_name);
        out.create = Some(out_create);
        out.destroy = Some(out_destroy);
        sys::obs_register_source_s(&out, std::mem::size_of::<sys::obs_source_info>());
        let mut tone: sys::obs_source_info = std::mem::zeroed();
        tone.id = c"lightair_test_tone".as_ptr();
        tone.type_ = sys::OBS_SOURCE_TYPE_INPUT;
        tone.output_flags = sys::OBS_SOURCE_AUDIO;
        tone.get_name = Some(tone_name);
        tone.create = Some(tone_create);
        tone.destroy = Some(tone_destroy);
        sys::obs_register_source_s(&tone, std::mem::size_of::<sys::obs_source_info>());
    });
}

// ---------------------------------------------------------------- capture (libobs → engine)

/// One input's capture: libobs's audio thread pushes, the engine's thread pops.
struct Capture {
    producer: Mutex<rtrb::Producer<f32>>,
}

unsafe extern "C" fn on_capture(param: *mut c_void, _: *mut sys::obs_source_t, data: *const sys::audio_data, muted: bool) {
    let cap = unsafe { &*(param as *const Capture) };
    let data = unsafe { &*data };
    let frames = data.frames as usize;
    let (l, r) = (data.data[0] as *const f32, data.data[1] as *const f32);
    if l.is_null() {
        return;
    }
    let l = unsafe { std::slice::from_raw_parts(l, frames) };
    let r = if r.is_null() { l } else { unsafe { std::slice::from_raw_parts(r, frames) } };
    let mut p = cap.producer.lock();
    // a full ring means the engine stopped reading: drop the packet rather than block
    if p.slots() < frames * 2 {
        return;
    }
    if let Ok(mut chunk) = p.write_chunk_uninit(frames * 2) {
        let (a, b) = chunk.as_mut_slices();
        let mut i = 0usize;
        for k in 0..frames {
            let (x, y) = if muted { (0.0, 0.0) } else { (l[k], r[k]) };
            for v in [x, y] {
                if i < a.len() {
                    a[i].write(v);
                } else {
                    b[i - a.len()].write(v);
                }
                i += 1;
            }
        }
        unsafe { chunk.commit_all() };
    }
}

// ---------------------------------------------------------------- the real-time side

struct RtInput {
    id: la::NodeId,
    ring: rtrb::Consumer<f32>,
    bufs: [Box<[f32]>; 2],
    primed: bool,
    has: bool,
}

struct RtInputs(Vec<RtInput>);

impl la::Inputs for RtInputs {
    fn channel(&self, input: la::NodeId, ch: u16) -> Option<&[f32]> {
        let i = self.0.iter().find(|i| i.id == input && i.has)?;
        Some(&i.bufs[(ch as usize).min(1)][..])
    }
}

/// A multitrack file being recorded, on the audio thread: what to take and where it goes.
struct StemRt {
    id: la::NodeId,
    tap: la::Tap,
    channels: u16,
    ring: rtrb::Producer<f32>,
}

enum ToRt {
    AddInput(RtInput),
    RemoveInput(la::NodeId),
    /// Start these files on the next block (all on the same one: sample-aligned).
    StartStems(Vec<StemRt>),
    StopStems,
    PauseStems(bool),
    /// Which mix each out source after Program carries, and the cue mix.
    Mixes([Option<la::NodeId>; OUT_MIXES - 1], Option<la::NodeId>),
}

/// Running state handed back from the audio thread to be freed.
#[allow(dead_code)] // held only so they're freed on the main side, never on the audio thread
enum FromRt {
    Input(RtInput),
    Stems(Vec<StemRt>),
}

/// Whether multitrack files are being recorded (for the UI's record state).
pub static STEMS_RUNNING: AtomicBool = AtomicBool::new(false);
/// When the first sample of the current multitrack files was played (libobs ns; 0: not yet).
static STEMS_START_NS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
/// Samples dropped because a file's writer fell behind (never expected).
pub static STEM_OVERRUNS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Program, the mixes, then the cue (monitor-only) out.
struct OutPtr([*mut sys::obs_source_t; OUT_MIXES + 1]);
unsafe impl Send for OutPtr {}

fn rt_loop(
    mut renderer: la::Renderer,
    mut commands: rtrb::Consumer<ToRt>,
    mut garbage: rtrb::Producer<FromRt>,
    outs: OutPtr,
    sample_rate: u32,
    stop: Arc<AtomicBool>,
) {
    let mut inputs = RtInputs(Vec::with_capacity(128));
    let mut mixes: [Option<la::NodeId>; OUT_MIXES - 1] = [None; OUT_MIXES - 1];
    let mut cue: Option<la::NodeId> = None;
    let mut stems: Vec<StemRt> = Vec::new();
    let mut stems_paused = false;
    let mut stems_started = false;
    let period = Duration::from_secs_f64(BLOCK as f64 / sample_rate as f64);
    let start = Instant::now();
    let mut blocks = 0u64;
    let anchor = unsafe { sys::os_gettime_ns() };
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
                ToRt::StartStems(new) => {
                    let old = std::mem::replace(&mut stems, new);
                    let _ = garbage.push(FromRt::Stems(old));
                    (stems_paused, stems_started) = (false, false);
                }
                ToRt::StopStems => {
                    let old = std::mem::take(&mut stems);
                    let _ = garbage.push(FromRt::Stems(old));
                }
                ToRt::PauseStems(p) => stems_paused = p,
            }
        }
        // a block from each input
        for input in &mut inputs.0 {
            let avail = input.ring.slots() / 2;
            if !input.primed && avail >= PRIME {
                input.primed = true;
            }
            if avail > MOST {
                // drifted ahead (a faster device clock): drop back to the target
                let drop = (avail - PRIME) * 2;
                if let Ok(c) = input.ring.read_chunk(drop) {
                    c.commit_all();
                }
            }
            let avail = input.ring.slots() / 2;
            input.has = input.primed && avail > 0;
            if input.primed && avail == 0 {
                input.primed = false; // empty: the source stopped; buffer up again when it's back
            }
            if input.has {
                // a short ring plays what it has and pads the rest (a tiny gap, not a silence)
                let take = avail.min(BLOCK);
                if take < BLOCK {
                    SHORTFALLS.fetch_add(1, Ordering::Relaxed);
                }
                if let Ok(chunk) = input.ring.read_chunk(take * 2) {
                    let (a, b) = chunk.as_slices();
                    for k in 0..take {
                        let at = |i: usize| if i < a.len() { a[i] } else { b[i - a.len()] };
                        input.bufs[0][k] = at(k * 2);
                        input.bufs[1][k] = at(k * 2 + 1);
                    }
                    chunk.commit_all();
                }
                input.bufs[0][take..BLOCK].fill(0.0);
                input.bufs[1][take..BLOCK].fill(0.0);
            }
        }
        let graph = renderer.process(&inputs, BLOCK);
        let ts = anchor + (blocks as f64 * BLOCK as f64 * ns_per_frame) as u64 - latency_ns;
        let push = |src: *mut sys::obs_source_t, (l, r): (&[f32], &[f32])| {
            let mut data = [std::ptr::null(); 8];
            data[0] = l.as_ptr() as *const u8;
            data[1] = r.as_ptr() as *const u8;
            let audio = sys::obs_source_audio {
                data,
                frames: l.len() as u32,
                speakers: sys::SPEAKERS_STEREO,
                format: sys::AUDIO_FORMAT_FLOAT_PLANAR,
                samples_per_sec: sample_rate,
                timestamp: ts,
            };
            unsafe { sys::obs_source_output_audio(src, &audio) };
        };
        push(outs.0[0], graph.program());
        for (k, mix) in mixes.iter().enumerate() {
            if let Some(out) = mix.and_then(|m| graph.mix(m)) {
                push(outs.0[k + 1], out);
            }
        }
        if let Some(out) = cue.and_then(|m| graph.mix(m)) {
            push(outs.0[OUT_MIXES], out);
        }
        // multitrack files: this block of each, interleaved
        if !stems.is_empty() && !stems_paused {
            if !stems_started {
                STEMS_START_NS.store(ts, Ordering::Relaxed);
                stems_started = true;
            }
            for stem in &mut stems {
                let src = if stem.id == la::PROGRAM { Some(graph.program()) } else { graph.tap(stem.id, stem.tap) };
                let n = BLOCK * stem.channels as usize;
                let Ok(mut chunk) = stem.ring.write_chunk_uninit(n) else {
                    STEM_OVERRUNS.fetch_add(BLOCK as u64, Ordering::Relaxed);
                    continue;
                };
                let (a, b) = chunk.as_mut_slices();
                let mut i = 0usize;
                for k in 0..BLOCK {
                    let (l, r) = src.map_or((0.0, 0.0), |(l, r)| (l[k], r[k]));
                    let frame = [l, r];
                    for v in &frame[..stem.channels as usize] {
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
}

// ---------------------------------------------------------------- the main side

/// A multitrack file to record.
#[derive(Debug, Clone)]
pub struct StemSpec {
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
pub struct StemReport {
    pub files: Vec<std::path::PathBuf>,
    /// Frames in each file.
    pub frames: u64,
    /// When their first sample played (libobs ns): lines them up with the video file.
    pub start_ns: u64,
    pub sample_rate: u32,
}

/// A multitrack file's writer, its ring from the audio thread, and whether it's float.
type StemWriter = (hound::WavWriter<std::io::BufWriter<std::fs::File>>, rtrb::Consumer<f32>, bool);

struct Recorder {
    stop: Arc<AtomicBool>,
    thread: std::thread::JoinHandle<Result<StemReport, String>>,
}

/// Engine meters paired with the mixer UI's meter cells they fill.
type MeterCells = Arc<Mutex<Vec<(Arc<la::meter::Meter>, Arc<crate::meters::MeterCell>)>>>;

/// The audio engine, running. Owned by the reconciler (engine thread).
pub struct AudioHost {
    engine: la::Engine,
    commands: rtrb::Producer<ToRt>,
    garbage: rtrb::Consumer<FromRt>,
    /// Held for the audio thread, which pushes into them until it stops (in `drop`).
    _outs: Vec<Source>,
    captures: HashMap<lightair_core::Id, (Source, Box<Capture>)>,
    mixes: ([Option<la::NodeId>; OUT_MIXES - 1], Option<la::NodeId>),
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
    /// The mixer UI's meter cells, filled from the engine's meters ~30 times a second.
    cells: MeterCells,
    pump: Option<std::thread::JoinHandle<()>>,
    recorder: Option<Recorder>,
    sample_rate: u32,
}

impl AudioHost {
    pub fn start(sample_rate: u32) -> Option<Self> {
        register_types();
        let outs: Vec<Source> = (0..=OUT_MIXES)
            .map(|k| Source::create_private(OUT_SOURCE, &format!("Lightair mix {k}"), None))
            .collect::<Option<_>>()?;
        for (k, out) in outs.iter().enumerate() {
            unsafe {
                if k < OUT_MIXES {
                    sys::obs_source_set_audio_mixers(out.as_ptr(), 1 << k);
                } else {
                    // the cue: heard on the monitoring device only (in no mix)
                    sys::obs_source_set_audio_mixers(out.as_ptr(), 0);
                    sys::obs_source_set_monitoring_enabled(out.as_ptr(), true);
                }
                sys::obs_set_output_source(OUT_CHANNELS.start + k as u32, out.as_ptr());
            }
        }
        let (engine, renderer) = la::Engine::new(la::GraphSpec::default(), sample_rate);
        let (commands, rt_commands) = rtrb::RingBuffer::new(256);
        let (rt_garbage, garbage) = rtrb::RingBuffer::new(256);
        let stop = Arc::new(AtomicBool::new(false));
        let ptrs = OutPtr(std::array::from_fn(|k| outs[k].as_ptr()));
        tracing::info!(sample_rate, "audio engine running");
        let s = stop.clone();
        let thread = std::thread::Builder::new()
            .name("lightair-audio".into())
            .spawn(move || rt_loop(renderer, rt_commands, rt_garbage, ptrs, sample_rate, s))
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
            _outs: outs,
            captures: HashMap::new(),
            mixes: ([None; OUT_MIXES - 1], None),
            stop,
            thread: Some(thread),
            cells,
            pump,
            recorder: None,
            sample_rate,
        })
    }

    /// Plays this mixer: compiles it, routes the first five mixes to libobs mixes 2–6 and the
    /// cue mix ([`crate::audio::CUE`]) to the monitoring device.
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

    /// Starts taking `input`'s audio from its libobs source (which then stays out of libobs's
    /// own mixes).
    pub fn capture(&mut self, input: lightair_core::Id, source: &Source) {
        if self.captures.contains_key(&input) {
            return;
        }
        let (producer, consumer) = rtrb::RingBuffer::new(RING_FRAMES * 2);
        let cap = Box::new(Capture { producer: Mutex::new(producer) });
        unsafe {
            sys::obs_source_set_audio_mixers(source.as_ptr(), 0);
            sys::obs_source_set_volume(source.as_ptr(), 1.0);
            sys::obs_source_set_muted(source.as_ptr(), false);
            sys::obs_source_set_monitoring_enabled(source.as_ptr(), false);
            sys::obs_source_add_audio_capture_callback(
                source.as_ptr(),
                Some(on_capture),
                &*cap as *const Capture as *mut c_void,
            );
        }
        let rt = RtInput {
            id: crate::audio::node(input),
            ring: consumer,
            bufs: [vec![0.0; la::MAX_BLOCK].into_boxed_slice(), vec![0.0; la::MAX_BLOCK].into_boxed_slice()],
            primed: false,
            has: false,
        };
        let _ = self.commands.push(ToRt::AddInput(rt));
        let Some(held) = (unsafe { Source::from_borrowed(source.as_ptr()) }) else { return };
        self.captures.insert(input, (held, cap));
    }

    /// Stops taking `input`'s audio; libobs mixes it again as it did before.
    pub fn release(&mut self, input: lightair_core::Id) {
        let Some((source, cap)) = self.captures.remove(&input) else { return };
        unsafe {
            sys::obs_source_remove_audio_capture_callback(
                source.as_ptr(),
                Some(on_capture),
                &*cap as *const Capture as *mut c_void,
            );
            sys::obs_source_set_audio_mixers(source.as_ptr(), 0xFF);
        }
        let _ = self.commands.push(ToRt::RemoveInput(crate::audio::node(input)));
        // the callback is removed (libobs holds its lock while calling): the capture can go
        drop(cap);
    }

    pub fn captured(&self) -> impl Iterator<Item = lightair_core::Id> + '_ {
        self.captures.keys().copied()
    }

    /// Starts recording these multitrack files, sample-aligned, until [`Self::stop_stems`].
    pub fn start_stems(&mut self, specs: Vec<StemSpec>) -> Result<(), String> {
        if self.recorder.is_some() {
            self.stop_stems();
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
            rts.push(StemRt { id: spec.id, tap: spec.tap, channels: spec.channels, ring: producer });
            files.push(spec.path.clone());
        }
        STEMS_START_NS.store(0, Ordering::Relaxed);
        if self.commands.push(ToRt::StartStems(rts)).is_err() {
            return Err("the audio engine is busy".into());
        }
        STEMS_RUNNING.store(true, Ordering::Relaxed);
        let stop = Arc::new(AtomicBool::new(false));
        let s = stop.clone();
        let thread = std::thread::Builder::new()
            .name("lightair-stems".into())
            .spawn(move || {
                let mut frames = 0u64;
                let drain = |writers: &mut Vec<StemWriter>, frames: &mut u64|
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
                Ok(StemReport { files, frames, start_ns: STEMS_START_NS.load(Ordering::Relaxed), sample_rate })
            })
            .map_err(|e| e.to_string())?;
        self.recorder = Some(Recorder { stop, thread });
        Ok(())
    }

    /// Pauses or resumes the multitrack files (no gap: the files just don't get those blocks).
    pub fn pause_stems(&mut self, pause: bool) {
        if self.recorder.is_some() {
            let _ = self.commands.push(ToRt::PauseStems(pause));
        }
    }

    /// Stops the multitrack files and finishes writing them.
    pub fn stop_stems(&mut self) -> Option<StemReport> {
        let rec = self.recorder.take()?;
        let _ = self.commands.push(ToRt::StopStems);
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
        self.stop_stems();
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
        for k in OUT_CHANNELS {
            unsafe { sys::obs_set_output_source(k, std::ptr::null_mut()) };
        }
    }
}

/// The name of a libobs source (for logs).
#[allow(dead_code)]
fn name(src: *mut sys::obs_source_t) -> String {
    unsafe { CStr::from_ptr(sys::obs_source_get_name(src)).to_string_lossy().into_owned() }
}
