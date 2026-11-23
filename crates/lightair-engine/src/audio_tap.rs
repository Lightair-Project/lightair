//! Listening to a source's sound (the AI co-host hearing a mixer track, piair/docs/design.md §4):
//! its audio after its libobs filters, folded to mono and resampled to 16 kHz, 16-bit, the way
//! speech recognition wants it, through a lock-free ring. A muted source is heard as silence.

use std::ffi::c_void;

use parking_lot::Mutex;

use crate::sys;

/// Samples a tap hands out per second.
pub const RATE: u32 = 16_000;
/// Ten seconds of room in the ring.
const RING: usize = RATE as usize * 10;

struct TapState {
    producer: Mutex<rtrb::Producer<i16>>,
    /// The resampler: how far along to the next output sample, and the sum and count so far.
    phase: Mutex<(f64, f32, u32)>,
    /// Output samples per input sample (16 kHz over the engine's rate).
    step: f64,
}

/// A tap on a source: its sound comes out of the consumer [`Tap::open`] returns, until dropped.
pub struct Tap {
    source: *mut sys::obs_source_t,
    state: Box<TapState>,
}

// SAFETY: the source is reference counted and its capture callbacks take libobs's own lock.
unsafe impl Send for Tap {}

impl Tap {
    /// Taps `source`, taking over the strong reference the caller holds (from
    /// `Reconciler::source_ref`).
    ///
    /// # Safety
    /// `source` must be a live libobs source with a reference the caller owns.
    pub unsafe fn open(source: *mut sys::obs_source_t) -> (Tap, rtrb::Consumer<i16>) {
        let mut info: sys::obs_audio_info = unsafe { std::mem::zeroed() };
        let rate = if unsafe { sys::obs_get_audio_info(&mut info) } && info.samples_per_sec > 0 {
            info.samples_per_sec
        } else {
            48_000
        };
        let (producer, consumer) = rtrb::RingBuffer::new(RING);
        let state = Box::new(TapState {
            producer: Mutex::new(producer),
            phase: Mutex::new((0.0, 0.0, 0)),
            step: RATE as f64 / rate as f64,
        });
        unsafe {
            sys::obs_source_add_audio_capture_callback(
                source,
                Some(on_audio),
                &*state as *const TapState as *mut c_void,
            )
        };
        (Tap { source, state }, consumer)
    }
}

impl Drop for Tap {
    fn drop(&mut self) {
        unsafe {
            sys::obs_source_remove_audio_capture_callback(
                self.source,
                Some(on_audio),
                &*self.state as *const TapState as *mut c_void,
            );
            sys::obs_source_release(self.source);
        }
    }
}

unsafe extern "C" fn on_audio(
    param: *mut c_void,
    _: *mut sys::obs_source_t,
    data: *const sys::audio_data,
    muted: bool,
) {
    let state = unsafe { &*(param as *const TapState) };
    let data = unsafe { &*data };
    let frames = data.frames as usize;
    let (l, r) = (data.data[0] as *const f32, data.data[1] as *const f32);
    if l.is_null() || frames == 0 {
        return;
    }
    let l = unsafe { std::slice::from_raw_parts(l, frames) };
    let r = if r.is_null() { l } else { unsafe { std::slice::from_raw_parts(r, frames) } };
    let mut phase = state.phase.lock();
    let mut out = [0i16; 2048];
    let mut n = 0usize;
    let mut producer = state.producer.lock();
    for k in 0..frames {
        let x = if muted { 0.0 } else { (l[k] + r[k]) * 0.5 };
        phase.1 += x;
        phase.2 += 1;
        phase.0 += state.step;
        if phase.0 >= 1.0 {
            phase.0 -= 1.0;
            let v = (phase.1 / phase.2 as f32).clamp(-1.0, 1.0);
            out[n] = (v * 32767.0) as i16;
            n += 1;
            phase.1 = 0.0;
            phase.2 = 0;
            if n == out.len() {
                push(&mut producer, &out[..n]);
                n = 0;
            }
        }
    }
    push(&mut producer, &out[..n]);
}

/// Into the ring; what doesn't fit (nobody reading) is dropped, never waited for.
fn push(p: &mut rtrb::Producer<i16>, samples: &[i16]) {
    let room = p.slots().min(samples.len());
    if room == 0 {
        return;
    }
    if let Ok(chunk) = p.write_chunk_uninit(room) {
        chunk.fill_from_iter(samples[..room].iter().copied());
    }
}
