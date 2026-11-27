//! Speech as a source: the voices Lightair makes (s16 mono at the vendor's rate) are resampled to
//! 48 kHz and paced out in 10 ms blocks, so each is a mixer track like any other (fader, mute,
//! effects, meters, in the stream and the recording). Two channels, each with its own queue:
//!
//! - [`Channel::Cohost`], `lightair_piair_voice`: the AI co-host's voice (piair/docs/design.md
//!   §5.2). As each block goes out its loudness and brightness (or the vendor's mouth shapes)
//!   move the co-host avatar's mouth ([`crate::face_drive`]).
//! - [`Channel::Voice`], `lightair_voice`: the Voice track, for what alerts, automations and the
//!   chat reader say (docs/plans/voices-and-hearing.md). Things said play one after another.

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use lightair_model::Id;


pub const KIND: &str = "lightair_piair_voice";
/// The Voice track's source.
pub const VOICE_KIND: &str = "lightair_voice";

/// Which queue a voice source plays.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Channel {
    /// The AI co-host's speech.
    Cohost,
    /// The Voice track: alerts, automations, the chat reader.
    Voice,
}

impl Channel {
    /// The channel a source kind plays, if it's a voice.
    pub fn of_kind(kind: &str) -> Option<Channel> {
        match kind {
            KIND => Some(Channel::Cohost),
            VOICE_KIND => Some(Channel::Voice),
            _ => None,
        }
    }
}

pub const RATE: u32 = 48_000;
/// 10 ms.
pub const BLOCK: usize = 480;

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
    /// The utterance playing now (until its last piece has played).
    current: Option<String>,
    /// It ran dry before PiAir finished it: wait for a lead again before going on.
    dry: bool,
}

/// How much of an utterance is queued before it starts (or goes on after running dry), unless
/// PiAir has finished it: speech that streams in (a live voice) arrives a little unevenly, and
/// without this lead every late packet is a gap.
const LEAD: usize = RATE as usize * 3 / 20;

static QUEUES: Mutex<[Option<Queue>; 2]> = Mutex::new([None, None]);
/// Voice sources alive per channel (none: nothing would play it).
static ALIVE: [AtomicUsize; 2] = [AtomicUsize::new(0), AtomicUsize::new(0)];
/// The Voice track's source that plays (the first one made; any others stay silent).
static VOICE_OWNER: Mutex<Option<String>> = Mutex::new(None);

fn index(c: Channel) -> usize {
    match c {
        Channel::Cohost => 0,
        Channel::Voice => 1,
    }
}
/// The voice source that plays PiAir's speech (the active AI co-host's, by its source Id as
/// text); the others stay silent. `None`: none plays.
static ACTIVE: Mutex<Option<String>> = Mutex::new(None);

/// Which voice source plays the speech: the active co-host's (its source Id).
pub fn set_active(source: Option<String>) {
    *ACTIVE.lock().unwrap_or_else(|e| e.into_inner()) = source;
}

fn is_active(source: &str) -> bool {
    ACTIVE.lock().unwrap_or_else(|e| e.into_inner()).as_deref() == Some(source)
}

fn with<R>(c: Channel, f: impl FnOnce(&mut Queue) -> R) -> R {
    let mut g = QUEUES.lock().unwrap_or_else(|e| e.into_inner());
    f(g[index(c)].get_or_insert_with(Queue::default))
}

/// Whether a voice source exists to play a channel's speech.
pub fn available(c: Channel) -> bool {
    ALIVE[index(c)].load(Ordering::Relaxed) > 0
}

/// Queues a piece of speech: `pcm` s16le mono at `rate`, with mouth shapes at ms offsets.
pub fn push(c: Channel, utterance: &str, rate: u32, pcm: &[u8], marks: &[(f32, Shape)]) {
    if rate == 0 || pcm.len() < 2 {
        return;
    }
    let input: Vec<f32> = pcm.as_chunks::<2>().0.iter().map(|b| i16::from_le_bytes(*b) as f32 / 32768.0).collect();
    push_f32(c, utterance, rate, &input, marks);
}

/// Queues a piece of speech given as samples.
pub fn push_samples(c: Channel, utterance: &str, rate: u32, pcm: &[i16], marks: &[(f32, Shape)]) {
    if rate == 0 || pcm.is_empty() {
        return;
    }
    let input: Vec<f32> = pcm.iter().map(|&s| s as f32 / 32768.0).collect();
    push_f32(c, utterance, rate, &input, marks);
}

fn push_f32(c: Channel, utterance: &str, rate: u32, input: &[f32], marks: &[(f32, Shape)]) {
    let samples = resample(input, rate, RATE);
    let k = RATE as f32 / 1000.0;
    let marks = marks.iter().map(|&(ms, s)| ((ms.max(0.0) * k) as usize, s)).collect();
    with(c, |q| {
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

/// The last piece of `utterance` came.
pub fn end(c: Channel, utterance: &str) {
    with(c, |q| {
        if q.pieces.iter().any(|p| p.utterance == utterance) {
            q.ended.push(utterance.to_owned());
        } else {
            q.events.push(Event::Done(utterance.to_owned()));
        }
    });
}

/// Stops `utterance` (or everything) now, with a short fade.
pub fn cancel(c: Channel, utterance: Option<&str>) {
    with(c, |q| {
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
        if q.current.as_ref().is_some_and(|c| gone.contains(c)) {
            (q.current, q.dry) = (None, false);
        }
        for u in gone {
            q.events.push(Event::Cancelled(u));
        }
    });
}

/// What happened since the last call.
pub fn take_events(c: Channel) -> Vec<Event> {
    with(c, |q| std::mem::take(&mut q.events))
}

/// The utterance playing now, if any.
pub fn playing(c: Channel) -> Option<String> {
    with(c, |q| q.current.clone().or_else(|| q.pieces.front().map(|p| p.utterance.clone())))
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
fn next_block(c: Channel, out: &mut [f32; BLOCK], mouth: &mut Mouth) {
    let mut shape: Option<Shape> = None;
    let mut nod = false;
    with(c, |q| {
        let mut i = 0;
        // a new utterance, or one that ran dry: wait for its lead
        let wait = q.pieces.front().filter(|p| p.first || q.dry).is_some_and(|p| {
            let u = &p.utterance;
            let queued: usize = q.pieces.iter().filter(|p| &p.utterance == u).map(|p| p.samples.len()).sum();
            queued < LEAD && !q.ended.contains(u)
        });
        if !wait {
            q.dry = false;
        }
        while i < BLOCK && !wait {
            let Some(p) = q.pieces.front_mut() else { break };
            q.current = Some(p.utterance.clone());
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
                    q.current = None;
                    nod = true;
                }
            }
        }
        // ran out mid-utterance (more of it is still coming)
        if i < BLOCK && !wait && q.pieces.is_empty() && q.current.as_ref().is_some_and(|u| !q.ended.contains(u)) {
            if !q.dry {
                tracing::debug!("piair voice: ran dry mid-utterance (waiting for a lead)");
            }
            q.dry = true;
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
    // only the co-host's voice has a face
    if c == Channel::Cohost {
        mouth.update(out, shape);
        if nod {
            crate::face_drive::nod();
        }
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
        let source = crate::face_drive::mouth_source();
        // the sound alone: the vendor's shapes left out
        let shape = if source == crate::face_drive::MouthSource::Audio { None } else { shape };
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
        // the vendor's data: each shape opens the mouth its own way while there's sound
        if source == crate::face_drive::MouthSource::Data
            && let Some(s) = shape
        {
            let shaped = match s {
                Shape::A => 0.9,
                Shape::O => 0.7,
                Shape::E => 0.6,
                Shape::I => 0.45,
                Shape::U => 0.4,
                Shape::Rest => 0.1,
                Shape::M => 0.0,
            };
            open = if speaking { shaped } else { 0.0 };
        }
        if speaking || self.speaking {
            crate::face_drive::push_voice(open, wide);
        }
        self.speaking = speaking;
    }
}

/// Where a voice's blocks go when no track of the mixer takes them.
pub trait Elsewhere: Send + Sync {
    /// A 10 ms block of mono speech at 48 kHz, shown at `timestamp_ns` (Lightair's clock).
    fn block(&self, block: &[f32], timestamp_ns: u64);
}

/// A voice source's speech, paced out in 10 ms blocks on a thread of its own from when it's
/// made until it's dropped: into its track in the mixer, else `elsewhere`. Only the active
/// co-host's voice ([`set_active`]) speaks, and only the first Voice track's source; the others
/// are silent.
pub struct Voice {
    source: Id,
    channel: Channel,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Voice {
    pub fn start(source: Id, channel: Channel, elsewhere: Option<Arc<dyn Elsewhere>>) -> Voice {
        let stop = Arc::new(AtomicBool::new(false));
        let s = stop.clone();
        let slot = crate::audio_host::mixer_on().then(|| crate::audio_host::feed_slot(source));
        let me = source.to_string();
        ALIVE[index(channel)].fetch_add(1, Ordering::Relaxed);
        if channel == Channel::Voice {
            VOICE_OWNER.lock().unwrap_or_else(|e| e.into_inner()).get_or_insert_with(|| me.clone());
        }
        let thread = std::thread::Builder::new()
            .name(if channel == Channel::Cohost { "cohost-voice" } else { "voice-track" }.into())
            .spawn(move || {
                let mut block = [0f32; BLOCK];
                let mut mouth = Mouth { env: 0.0, bright: 0.0, last: [0.0; BLOCK], speaking: false };
                let start = crate::clock::now_ns();
                let mut sent = 0u64;
                while !s.load(Ordering::Relaxed) {
                    // only the active co-host's voice (or the Voice track's first source) takes the
                    // speech; the others are silent
                    let plays = match channel {
                        Channel::Cohost => is_active(&me),
                        Channel::Voice => VOICE_OWNER.lock().unwrap_or_else(|e| e.into_inner()).as_deref() == Some(me.as_str()),
                    };
                    if plays {
                        next_block(channel, &mut block, &mut mouth);
                    } else {
                        block.fill(0.0);
                    }
                    let at = start + sent * 1_000_000_000 / RATE as u64;
                    let fed = slot.as_ref().is_some_and(|s| s.push_planar(&[&block[..]], RATE));
                    if !fed && let Some(e) = &elsewhere {
                        e.block(&block, at);
                    }
                    sent += BLOCK as u64;
                    let due = start + sent * 1_000_000_000 / RATE as u64;
                    let now = crate::clock::now_ns();
                    if due > now {
                        std::thread::sleep(Duration::from_nanos(due - now));
                    } else if now - due > 200_000_000 {
                        // fell far behind (the Mac slept): start the clock again
                        sent = (now - start) * RATE as u64 / 1_000_000_000;
                    }
                }
            })
            .ok();
        Voice { source, channel, stop, thread }
    }
}

impl Drop for Voice {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.thread.take() {
            let _ = h.join();
        }
        if crate::audio_host::mixer_on() {
            crate::audio_host::feeds_itself_no_more(self.source);
        }
        ALIVE[index(self.channel)].fetch_sub(1, Ordering::Relaxed);
        if self.channel == Channel::Voice {
            let mut owner = VOICE_OWNER.lock().unwrap_or_else(|e| e.into_inner());
            if owner.as_deref() == Some(self.source.to_string().as_str()) {
                *owner = None;
            }
        }
    }
}
