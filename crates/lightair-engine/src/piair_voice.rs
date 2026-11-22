//! PiAir's voice as a libobs audio source, `lightair_piair_voice` (piair/docs/design.md §5.2):
//! the speech PiAir streams in (s16 mono at the vendor's rate) is resampled to 48 kHz and paced
//! out in 10 ms blocks, so it's a mixer track like any other (fader, mute, effects, meters, in the
//! stream and the recording). As each block goes out its loudness and brightness (or the vendor's
//! mouth shapes, when it sent them) move the co-host avatar's mouth ([`crate::face_drive`]).

use std::collections::VecDeque;
use std::ffi::{c_char, c_void};
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use crate::sys;

pub const KIND: &str = "lightair_piair_voice";

const RATE: u32 = 48_000;
/// 10 ms.
const BLOCK: usize = 480;

/// A mouth shape the vendor's timings gave.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Shape {
    A,
    I,
    U,
    E,
    O,
    /// Lips closed (m, b, p).
    M,
    Rest,
}

impl Shape {
    pub fn parse(s: &str) -> Option<Shape> {
        Some(match s {
            "A" => Shape::A,
            "I" => Shape::I,
            "U" => Shape::U,
            "E" => Shape::E,
            "O" => Shape::O,
            "M" => Shape::M,
            "rest" => Shape::Rest,
            _ => return None,
        })
    }
}

/// What happened to an utterance, for PiAir (`voice_state`).
#[derive(Clone, Debug, PartialEq)]
pub enum Event {
    Playing(String),
    Done(String),
    Cancelled(String),
}

struct Piece {
    utterance: String,
    samples: VecDeque<f32>,
    /// (sample offset from this piece's start, shape)
    marks: Vec<(usize, Shape)>,
    played: usize,
    first: bool,
}

#[derive(Default)]
struct Queue {
    pieces: VecDeque<Piece>,
    /// Utterances PiAir finished sending.
    ended: Vec<String>,
    started: Vec<String>,
    events: Vec<Event>,
    /// Fading out after a cancel: samples left of the fade.
    fade: usize,
}

static QUEUE: Mutex<Option<Queue>> = Mutex::new(None);
/// Voice sources alive (no source: nothing would play it).
static ALIVE: AtomicUsize = AtomicUsize::new(0);

fn with<R>(f: impl FnOnce(&mut Queue) -> R) -> R {
    let mut g = QUEUE.lock().unwrap_or_else(|e| e.into_inner());
    f(g.get_or_insert_with(Queue::default))
}

/// Whether a voice source exists to play speech.
pub fn available() -> bool {
    ALIVE.load(Ordering::Relaxed) > 0
}

/// Queues a piece of speech: `pcm` s16le mono at `rate`, with mouth shapes at ms offsets.
pub fn push(utterance: &str, rate: u32, pcm: &[u8], marks: &[(f32, Shape)]) {
    if rate == 0 || pcm.len() < 2 {
        return;
    }
    let input: Vec<f32> = pcm.chunks_exact(2).map(|b| i16::from_le_bytes([b[0], b[1]]) as f32 / 32768.0).collect();
    let samples = resample(&input, rate, RATE);
    let k = RATE as f32 / 1000.0;
    let marks = marks.iter().map(|&(ms, s)| ((ms.max(0.0) * k) as usize, s)).collect();
    with(|q| {
        let first = !q.started.iter().any(|u| u == utterance);
        if first {
            q.started.push(utterance.to_owned());
            if q.started.len() > 64 {
                q.started.remove(0);
            }
        }
        q.pieces.push_back(Piece { utterance: utterance.to_owned(), samples: samples.into(), marks, played: 0, first });
    });
}

/// PiAir sent the last piece of `utterance`.
pub fn end(utterance: &str) {
    with(|q| {
        if q.pieces.iter().any(|p| p.utterance == utterance) {
            q.ended.push(utterance.to_owned());
        } else {
            q.events.push(Event::Done(utterance.to_owned()));
        }
    });
}

/// Stops `utterance` (or everything) now, with a short fade.
pub fn cancel(utterance: Option<&str>) {
    with(|q| {
        let before = q.pieces.len();
        let playing = q.pieces.front().map(|p| p.utterance.clone());
        let mut gone: Vec<String> = Vec::new();
        q.pieces.retain(|p| {
            let drop = utterance.is_none_or(|u| u == p.utterance);
            if drop && !gone.contains(&p.utterance) {
                gone.push(p.utterance.clone());
            }
            !drop
        });
        if q.pieces.len() != before && playing.is_some_and(|p| gone.contains(&p)) {
            q.fade = BLOCK;
        }
        q.ended.retain(|u| !gone.contains(u));
        for u in gone {
            q.events.push(Event::Cancelled(u));
        }
    });
}

/// What happened since the last call.
pub fn take_events() -> Vec<Event> {
    with(|q| std::mem::take(&mut q.events))
}

/// Linear interpolation with a light low-pass when going up (speech; 24 → 48 kHz mostly).
fn resample(input: &[f32], from: u32, to: u32) -> Vec<f32> {
    if from == to {
        return input.to_vec();
    }
    let n = (input.len() as u64 * to as u64 / from as u64) as usize;
    let step = from as f64 / to as f64;
    let mut out = Vec::with_capacity(n);
    for i in 0..n {
        let x = i as f64 * step;
        let j = x as usize;
        let f = (x - j as f64) as f32;
        let a = input[j.min(input.len() - 1)];
        let b = input[(j + 1).min(input.len() - 1)];
        out.push(a + (b - a) * f);
    }
    if to > from {
        // one-pole smoothing at ~ the source's Nyquist
        let mut y = 0.0;
        let k = 0.6;
        for s in out.iter_mut() {
            y += (*s - y) * k;
            *s = y;
        }
    }
    out
}

/// Fills one block: the queue's next samples (silence when empty), and the mouth for it.
fn next_block(out: &mut [f32; BLOCK], mouth: &mut Mouth) {
    let mut shape: Option<Shape> = None;
    let mut nod = false;
    with(|q| {
        let mut i = 0;
        while i < BLOCK {
            let Some(p) = q.pieces.front_mut() else { break };
            if p.played == 0 && p.first {
                q.events.push(Event::Playing(p.utterance.clone()));
                p.first = false;
            }
            while i < BLOCK {
                let Some(s) = p.samples.pop_front() else { break };
                out[i] = s;
                i += 1;
                p.played += 1;
            }
            if let Some(&(_, s)) = p.marks.iter().rev().find(|m| m.0 <= p.played) {
                shape = Some(s);
            }
            if p.samples.is_empty() {
                let done = q.pieces.pop_front().map(|p| p.utterance).unwrap_or_default();
                // the utterance's last piece played out
                if !q.pieces.iter().any(|p| p.utterance == done) && q.ended.contains(&done) {
                    q.ended.retain(|u| *u != done);
                    q.events.push(Event::Done(done));
                    nod = true;
                }
            }
        }
        for s in out[i..].iter_mut() {
            *s = 0.0;
        }
        if q.fade > 0 {
            // a cancel cut the sound: fade what's left of the old block out (no click)
            let n = q.fade.min(BLOCK);
            for (k, s) in mouth.last.iter().take(n).enumerate() {
                out[k] += s * (1.0 - k as f32 / n as f32);
            }
            q.fade = 0;
        }
    });
    mouth.last.copy_from_slice(&out[..]);
    mouth.update(out, shape);
    if nod {
        crate::face_drive::nod();
    }
}

/// The mouth from the sound: a loudness envelope opens it, brightness (or a vendor's shape)
/// widens or rounds it.
struct Mouth {
    env: f32,
    bright: f32,
    last: [f32; BLOCK],
    speaking: bool,
}

impl Mouth {
    fn update(&mut self, block: &[f32; BLOCK], shape: Option<Shape>) {
        let rms = (block.iter().map(|s| s * s).sum::<f32>() / BLOCK as f32).sqrt();
        // attack 15 ms, release 60 ms (blocks of 10 ms)
        let k = if rms > self.env { 1.0 - (-10.0f32 / 15.0).exp() } else { 1.0 - (-10.0f32 / 60.0).exp() };
        self.env += (rms - self.env) * k;
        let db = 20.0 * self.env.max(1e-6).log10();
        let mut open = ((db + 48.0) / 30.0).clamp(0.0, 1.0).powf(0.8);
        // zero crossings: bright sounds ("ee", "s") cross often, dark ones ("oo") rarely
        let crossings = block.windows(2).filter(|w| (w[0] >= 0.0) != (w[1] >= 0.0)).count() as f32 / BLOCK as f32;
        self.bright += (crossings - self.bright) * 0.3;
        let mut wide = ((self.bright - 0.06) / 0.08).clamp(-1.0, 1.0) * 0.6;
        match shape {
            Some(Shape::A) => wide = 0.0,
            Some(Shape::I) => {
                wide = 0.8;
                open *= 0.6;
            }
            Some(Shape::E) => {
                wide = 0.6;
                open *= 0.8;
            }
            Some(Shape::U) => {
                wide = -0.8;
                open *= 0.5;
            }
            Some(Shape::O) => {
                wide = -0.6;
                open *= 0.9;
            }
            Some(Shape::M) => open *= 0.1,
            Some(Shape::Rest) => open *= 0.4,
            None => {}
        }
        let speaking = self.env > 0.003;
        if speaking || self.speaking {
            crate::face_drive::push_voice(open, wide);
        }
        self.speaking = speaking;
    }
}

struct Voice {
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

struct SendPtr(*mut sys::obs_source_t);
unsafe impl Send for SendPtr {}

unsafe extern "C" fn get_name(_: *mut c_void) -> *const c_char {
    c"PiAir voice".as_ptr()
}

unsafe extern "C" fn create(_settings: *mut sys::obs_data_t, source: *mut sys::obs_source_t) -> *mut c_void {
    let stop = Arc::new(AtomicBool::new(false));
    let s = stop.clone();
    let src = SendPtr(source);
    ALIVE.fetch_add(1, Ordering::Relaxed);
    let thread = std::thread::Builder::new()
        .name("piair-voice".into())
        .spawn(move || {
            let src = src;
            let mut block = [0f32; BLOCK];
            let mut mouth = Mouth { env: 0.0, bright: 0.0, last: [0.0; BLOCK], speaking: false };
            let start = unsafe { sys::os_gettime_ns() };
            let mut sent = 0u64;
            while !s.load(Ordering::Relaxed) {
                next_block(&mut block, &mut mouth);
                let audio = sys::obs_source_audio {
                    data: [
                        block.as_ptr() as *const u8,
                        block.as_ptr() as *const u8,
                        std::ptr::null(),
                        std::ptr::null(),
                        std::ptr::null(),
                        std::ptr::null(),
                        std::ptr::null(),
                        std::ptr::null(),
                    ],
                    frames: BLOCK as u32,
                    speakers: sys::SPEAKERS_STEREO,
                    format: sys::AUDIO_FORMAT_FLOAT_PLANAR,
                    samples_per_sec: RATE,
                    timestamp: start + sent * 1_000_000_000 / RATE as u64,
                };
                unsafe { sys::obs_source_output_audio(src.0, &audio) };
                sent += BLOCK as u64;
                let due = start + sent * 1_000_000_000 / RATE as u64;
                let now = unsafe { sys::os_gettime_ns() };
                if due > now {
                    std::thread::sleep(Duration::from_nanos(due - now));
                } else if now - due > 200_000_000 {
                    // fell far behind (the Mac slept): start the clock again
                    sent = (now - start) * RATE as u64 / 1_000_000_000;
                }
            }
        })
        .ok();
    Box::into_raw(Box::new(Voice { stop, thread })) as *mut c_void
}

unsafe extern "C" fn destroy(data: *mut c_void) {
    let mut v = unsafe { Box::from_raw(data as *mut Voice) };
    v.stop.store(true, Ordering::Relaxed);
    if let Some(h) = v.thread.take() {
        let _ = h.join();
    }
    ALIVE.fetch_sub(1, Ordering::Relaxed);
}

pub fn register() {
    static DONE: std::sync::Once = std::sync::Once::new();
    DONE.call_once(|| unsafe {
        let mut info: sys::obs_source_info = std::mem::zeroed();
        info.id = c"lightair_piair_voice".as_ptr();
        info.type_ = sys::OBS_SOURCE_TYPE_INPUT;
        info.output_flags = sys::OBS_SOURCE_AUDIO | sys::OBS_SOURCE_DO_NOT_DUPLICATE;
        info.get_name = Some(get_name);
        info.create = Some(create);
        info.destroy = Some(destroy);
        sys::obs_register_source_s(&info, std::mem::size_of::<sys::obs_source_info>());
    });
}
