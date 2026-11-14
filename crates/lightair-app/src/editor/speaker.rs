//! The Editor player's sound: the sequence's mix (FFmpeg, from the playhead) played on the
//! computer's output, and how far it has played (the picture follows it).

use std::io::Read;
use std::process::Child;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use lightair_render::sequence::Sequence;

/// Sound playing. Dropped: it stops.
pub struct Speaker {
    _stream: cpal::Stream,
    child: Child,
    stop: Arc<AtomicBool>,
    clock: Clock,
}

/// How much of the sound has been heard (shared with the picture's thread).
#[derive(Clone)]
pub struct Clock {
    frames: Arc<AtomicU64>,
    rate: u32,
    /// Silent until the picture is ready.
    go: Arc<AtomicBool>,
}

impl Clock {
    /// The sound starts (with the first frame).
    pub fn start(&self) {
        self.go.store(true, Ordering::Relaxed);
    }

    /// Heard so far, in ms; `None` before the first sound.
    pub fn heard_ms(&self) -> Option<u64> {
        let f = self.frames.load(Ordering::Relaxed);
        (f > 0).then(|| f * 1000 / self.rate as u64)
    }
}

impl Speaker {
    /// The sequence's sound from `from_ms` on the monitor (the output the Live mixer's monitor
    /// uses, in the preferences; else the default one); `None` if it has none, or there's
    /// nowhere to play it.
    pub fn start(seq: &Sequence, from_ms: u64) -> Option<Speaker> {
        let host = cpal::default_host();
        let chosen = crate::prefs::load_from(&crate::prefs::path()).monitor_device.map(|(name, _)| name).filter(|n| !n.is_empty() && n != "default");
        let device = chosen
            .and_then(|name| host.output_devices().ok()?.find(|d| d.name().is_ok_and(|n| n == name)))
            .or_else(|| host.default_output_device())?;
        let config = device.default_output_config().ok()?;
        let (rate, channels) = (config.sample_rate().0, config.channels() as usize);
        let mut child = lightair_render::sound::stream(seq, from_ms, rate).ok()??;
        let mut out = child.stdout.take()?;
        let (mut tx, mut rx) = rtrb::RingBuffer::<f32>::new(rate as usize * 2);
        let stop = Arc::new(AtomicBool::new(false));
        // the sound has all been read: silence after it still counts as time going by
        let ended = Arc::new(AtomicBool::new(false));
        let reader_ended = ended.clone();
        // FFmpeg's output into the ring (waiting while it's full)
        let reader_stop = stop.clone();
        std::thread::spawn(move || {
            let mut buf = vec![0u8; 16_384];
            let mut carry: Vec<u8> = Vec::new();
            while !reader_stop.load(Ordering::Relaxed) {
                let n = match out.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => n,
                };
                carry.extend_from_slice(&buf[..n]);
                let whole = carry.len() / 4 * 4;
                let mut samples = carry[..whole].as_chunks::<4>().0.iter().map(|b| f32::from_le_bytes(*b)).peekable();
                while samples.peek().is_some() {
                    if reader_stop.load(Ordering::Relaxed) {
                        return;
                    }
                    let room = tx.slots();
                    if room == 0 {
                        std::thread::sleep(std::time::Duration::from_millis(5));
                        continue;
                    }
                    for _ in 0..room {
                        match samples.next() {
                            Some(s) => {
                                let _ = tx.push(s);
                            }
                            None => break,
                        }
                    }
                }
                carry.drain(..whole);
            }
            reader_ended.store(true, Ordering::Relaxed);
        });
        let frames = Arc::new(AtomicU64::new(0));
        let heard = frames.clone();
        let go = Arc::new(AtomicBool::new(false));
        let going = go.clone();
        let stream = device
            .build_output_stream(
                &cpal::StreamConfig { channels: channels as u16, sample_rate: cpal::SampleRate(rate), buffer_size: cpal::BufferSize::Default },
                move |data: &mut [f32], _| {
                    data.fill(0.0);
                    if !going.load(Ordering::Relaxed) {
                        return;
                    }
                    let want = data.len() / channels;
                    let have = (rx.slots() / 2).min(want);
                    if let Ok(chunk) = rx.read_chunk(have * 2) {
                        let (a, b) = chunk.as_slices();
                        let at = |i: usize| if i < a.len() { a[i] } else { b[i - a.len()] };
                        for k in 0..have {
                            let (l, r) = (at(k * 2), at(k * 2 + 1));
                            if channels == 1 {
                                data[k] = (l + r) * 0.5;
                            } else {
                                data[k * channels] = l;
                                data[k * channels + 1] = r;
                            }
                        }
                        chunk.commit_all();
                    }
                    let gone = if ended.load(Ordering::Relaxed) { want } else { have };
                    heard.fetch_add(gone as u64, Ordering::Relaxed);
                },
                |e| tracing::warn!(%e, "editor sound"),
                None,
            )
            .ok()?;
        stream.play().ok()?;
        Some(Speaker { _stream: stream, child, stop, clock: Clock { frames, rate, go } })
    }

    pub fn clock(&self) -> Clock {
        self.clock.clone()
    }
}

impl Drop for Speaker {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
