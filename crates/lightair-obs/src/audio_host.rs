//! The mixer's libobs side, while libobs is still there (the engine itself is
//! `lightair_host::audio_host`, re-exported here):
//!
//! - **Out sources** (`lightair_audio_out`): Program, the mixes and the cue as libobs sources,
//!   fed from the mixer's taps, for libobs's outputs and its cue monitoring ([`LibobsOuts`]).
//! - **Capture**: a track on a source that doesn't feed the engine itself takes its sound
//!   through libobs's audio-capture callback into a ring the engine hands out ([`capture`]).
//! - **Test tone** and the audio interface placeholder source types.

use std::collections::HashMap;
use std::ffi::{c_char, c_void};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use lightair_model::Id;
use parking_lot::Mutex;

pub use lightair_host::audio_host::*;

use crate::obs::ObsSource;
use crate::sys;

pub const OUT_SOURCE: &str = "lightair_audio_out";
pub const TEST_TONE: &str = "lightair_test_tone";
/// libobs output channels the out sources sit on (kept active there): Program, the mixes, then
/// the cue. Tracks use the ones below.
pub const OUT_CHANNELS: std::ops::Range<u32> = 57..64;

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

unsafe extern "C" fn device_name(_: *mut c_void) -> *const c_char {
    c"Audio interface".as_ptr()
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
        let start = crate::clock::now_ns();
        let mut sent = 0u64;
        while !s.load(Ordering::Relaxed) {
            for x in l.iter_mut() {
                *x = (phase * std::f64::consts::TAU).sin() as f32 * amp;
                phase = (phase + freq / sr).fract();
            }
            let audio = sys::obs_source_audio {
                data: [
                    l.as_ptr() as *const u8,
                    l.as_ptr() as *const u8,
                    std::ptr::null(),
                    std::ptr::null(),
                    std::ptr::null(),
                    std::ptr::null(),
                    std::ptr::null(),
                    std::ptr::null(),
                ],
                frames: frames as u32,
                speakers: sys::SPEAKERS_STEREO,
                format: sys::AUDIO_FORMAT_FLOAT_PLANAR,
                samples_per_sec: 48_000,
                timestamp: start + sent * 1_000_000_000 / 48_000,
            };
            unsafe { sys::obs_source_output_audio(src.0, &audio) };
            sent += frames as u64;
            let due = start + sent * 1_000_000_000 / 48_000;
            let now = crate::clock::now_ns();
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
        // device inputs are captured by the engine itself; libobs just holds a placeholder
        let mut dev: sys::obs_source_info = std::mem::zeroed();
        dev.id = c"lightair_audio_device".as_ptr();
        dev.type_ = sys::OBS_SOURCE_TYPE_INPUT;
        dev.output_flags = sys::OBS_SOURCE_AUDIO;
        dev.get_name = Some(device_name);
        dev.create = Some(out_create);
        dev.destroy = Some(out_destroy);
        sys::obs_register_source_s(&dev, std::mem::size_of::<sys::obs_source_info>());
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

unsafe extern "C" fn on_capture(
    param: *mut c_void,
    _: *mut sys::obs_source_t,
    data: *const sys::audio_data,
    muted: bool,
) {
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

/// What a track's capture holds on the libobs side: the source (its own mixing turned off while
/// taken) and, for a source that doesn't feed the engine, its callback's capture.
struct Held {
    source: ObsSource,
    callback: Option<Box<Capture>>,
}

// SAFETY: used on the engine thread only (the map is static for convenience); libobs sources are
// reference counted thread-safely.
unsafe impl Send for Held {}

static HELD: Mutex<Option<HashMap<Id, Held>>> = Mutex::new(None);

/// Starts taking `input`'s sound from its libobs source `source` (which then stays out of
/// libobs's own mixes): from the source itself when it feeds the engine, else through libobs's
/// audio-capture callback. Engine thread.
pub fn capture(host: &mut AudioHost, input: Id, source: &ObsSource) {
    if HELD.lock().as_ref().is_some_and(|h| h.contains_key(&input)) {
        return;
    }
    let Some(held) = (unsafe { ObsSource::from_borrowed(source.as_ptr()) }) else { return };
    unsafe {
        sys::obs_source_set_audio_mixers(source.as_ptr(), 0);
        sys::obs_source_set_volume(source.as_ptr(), 1.0);
        sys::obs_source_set_muted(source.as_ptr(), false);
        sys::obs_source_set_monitoring_enabled(source.as_ptr(), false);
    }
    let callback = if host.capture(input) {
        None
    } else {
        let Some(producer) = host.capture_ring(input, Box::new(())) else { return };
        let cap = Box::new(Capture { producer: Mutex::new(producer) });
        unsafe {
            sys::obs_source_add_audio_capture_callback(
                source.as_ptr(),
                Some(on_capture),
                &*cap as *const Capture as *mut c_void,
            );
        }
        Some(cap)
    };
    HELD.lock().get_or_insert_with(HashMap::new).insert(input, Held { source: held, callback });
}

/// Stops taking `input`'s sound; libobs mixes its source again as it did before. Engine thread.
pub fn release(host: &mut AudioHost, input: Id) {
    let held = HELD.lock().as_mut().and_then(|h| h.remove(&input));
    if let Some(Held { source, callback }) = held {
        if let Some(cap) = callback {
            unsafe {
                sys::obs_source_remove_audio_capture_callback(
                    source.as_ptr(),
                    Some(on_capture),
                    &*cap as *const Capture as *mut c_void,
                );
            }
            // the callback is removed (libobs holds its lock while calling): the capture can go
            drop(cap);
        }
        unsafe { sys::obs_source_set_audio_mixers(source.as_ptr(), 0xFF) };
    }
    host.release(input);
}

/// Lets go of every track's capture (before the engine stops). Engine thread.
pub fn release_all(host: &mut AudioHost) {
    let inputs: Vec<Id> = HELD.lock().as_ref().map(|h| h.keys().copied().collect()).unwrap_or_default();
    for input in inputs {
        release(host, input);
    }
}

// ---------------------------------------------------------------- the out sources

/// libobs's out sources (Program, the mixes, the cue), fed from the mixer's taps, for libobs's
/// own outputs and its monitoring of the cue. Dropping them takes them off their channels.
pub struct LibobsOuts {
    outs: Vec<ObsSource>,
    taps: Vec<MixTap>,
}

/// A libobs source the taps push into.
struct OutPtr(*mut sys::obs_source_t);
// SAFETY: libobs takes audio from any thread.
unsafe impl Send for OutPtr {}

impl LibobsOuts {
    /// Makes the out sources and feeds each from its mix. Engine thread.
    pub fn start(sample_rate: u32) -> Option<LibobsOuts> {
        register_types();
        let outs: Vec<ObsSource> = (0..=OUT_MIXES)
            .map(|k| ObsSource::create_private(OUT_SOURCE, &format!("Lightair mix {k}"), None))
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
        let taps = outs
            .iter()
            .enumerate()
            .map(|(k, out)| {
                let to = OutPtr(out.as_ptr());
                let mut left = Vec::new();
                let mut right = Vec::new();
                tap_mix(k, move |interleaved, ts| {
                    let to = &to;
                    left.clear();
                    right.clear();
                    for frame in interleaved.chunks_exact(2) {
                        left.push(frame[0]);
                        right.push(frame[1]);
                    }
                    let mut data = [std::ptr::null(); 8];
                    data[0] = left.as_ptr() as *const u8;
                    data[1] = right.as_ptr() as *const u8;
                    let audio = sys::obs_source_audio {
                        data,
                        frames: left.len() as u32,
                        speakers: sys::SPEAKERS_STEREO,
                        format: sys::AUDIO_FORMAT_FLOAT_PLANAR,
                        samples_per_sec: sample_rate,
                        timestamp: ts,
                    };
                    unsafe { sys::obs_source_output_audio(to.0, &audio) };
                })
            })
            .collect();
        Some(LibobsOuts { outs, taps })
    }

    /// libobs's own monitoring of the cue: on while the mixer's direct monitoring isn't playing.
    pub fn set_cue_monitoring(&self, on: bool) {
        if let Some(cue) = self.outs.get(OUT_MIXES) {
            unsafe { sys::obs_source_set_monitoring_enabled(cue.as_ptr(), on) };
        }
    }
}

impl Drop for LibobsOuts {
    fn drop(&mut self) {
        // the taps stop pushing first
        self.taps.clear();
        for k in OUT_CHANNELS {
            unsafe { sys::obs_set_output_source(k, std::ptr::null_mut()) };
        }
    }
}
