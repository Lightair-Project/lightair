//! Audio interfaces, captured directly (CoreAudio through cpal): every channel of a device, so
//! an 8-input interface is eight mics (docs/plans/audio-engine.md, "Inputs and outputs").
//! libobs's own device capture mixes a device down to stereo; this doesn't.
//!
//! cpal's streams live on one thread of their own; the engine opens and closes them there.

use std::collections::HashMap;
use std::sync::mpsc;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

/// The input kind of a device input (its settings: `{"device": "<name>"}`, "" for the default).
pub const DEVICE_KIND: &str = "lightair_audio_device";

#[derive(Debug, Clone, PartialEq)]
pub struct DeviceInfo {
    pub name: String,
    pub channels: u16,
    pub sample_rate: u32,
    pub default: bool,
}

/// The audio inputs this Mac has, with their channel counts.
pub fn input_devices() -> Vec<DeviceInfo> {
    let host = cpal::default_host();
    let default = host.default_input_device().and_then(|d| d.name().ok());
    let Ok(devices) = host.input_devices() else { return vec![] };
    devices
        .filter_map(|d| {
            let name = d.name().ok()?;
            let channels = d.supported_input_configs().ok()?.map(|c| c.channels()).max()?;
            let rate = d.default_input_config().ok().map_or(48_000, |c| c.sample_rate().0);
            Some(DeviceInfo { default: default.as_deref() == Some(name.as_str()), name, channels, sample_rate: rate })
        })
        .collect()
}

/// Turns a device's rate into the engine's by linear interpolation (devices that can't run at
/// the engine's rate). Keeps its buffers between callbacks: no allocation once warm.
struct Resampler {
    channels: usize,
    step: f64,
    pos: f64,
    last: Vec<f32>,
    out: Vec<f32>,
}

impl Resampler {
    fn new(channels: usize, from: u32, to: u32) -> Self {
        Self {
            channels,
            step: from as f64 / to as f64,
            pos: 0.0,
            last: vec![0.0; channels],
            out: Vec::with_capacity(16_384 * channels),
        }
    }

    /// Interleaved input in, interleaved output (in `self.out`).
    fn run(&mut self, input: &[f32]) {
        self.out.clear();
        let n = input.len() / self.channels;
        let at = |i: isize, c: usize, last: &[f32]| if i < 0 { last[c] } else { input[i as usize * self.channels + c] };
        while self.pos < n as f64 {
            let i = self.pos.floor() as isize - 1; // `last` is frame -1
            let f = (self.pos - self.pos.floor()) as f32;
            for c in 0..self.channels {
                let a = at(i, c, &self.last);
                let b = at(i + 1, c, &self.last);
                self.out.push(a + (b - a) * f);
            }
            self.pos += self.step;
        }
        self.pos -= n as f64;
        if n > 0 {
            self.last.copy_from_slice(&input[(n - 1) * self.channels..n * self.channels]);
        }
    }
}

enum Cmd {
    Open {
        input: lightair_core::Id,
        device: String,
        sample_rate: u32,
        ring: rtrb::Producer<f32>,
        reply: mpsc::Sender<Result<u16, String>>,
    },
    Close(lightair_core::Id),
}

/// The device thread: owns the open streams.
pub(crate) struct Devices {
    tx: mpsc::Sender<Cmd>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Devices {
    pub fn start() -> Self {
        let (tx, rx) = mpsc::channel::<Cmd>();
        let thread = std::thread::Builder::new()
            .name("lightair-devices".into())
            .spawn(move || {
                let mut streams: HashMap<lightair_core::Id, cpal::Stream> = HashMap::new();
                while let Ok(cmd) = rx.recv() {
                    match cmd {
                        Cmd::Open { input, device, sample_rate, ring, reply } => {
                            streams.remove(&input);
                            match open(&device, sample_rate, ring) {
                                Ok((stream, channels)) => {
                                    streams.insert(input, stream);
                                    let _ = reply.send(Ok(channels));
                                }
                                Err(e) => {
                                    let _ = reply.send(Err(e));
                                }
                            }
                        }
                        Cmd::Close(input) => {
                            streams.remove(&input);
                        }
                    }
                }
            })
            .ok();
        Self { tx, thread }
    }

    /// Opens `device` ("" for the default input) for `input`, feeding `ring` (interleaved, at
    /// `sample_rate`). Returns its channel count.
    pub fn open(
        &self,
        input: lightair_core::Id,
        device: &str,
        sample_rate: u32,
        ring: rtrb::Producer<f32>,
    ) -> Result<u16, String> {
        let (reply, answer) = mpsc::channel();
        self.tx
            .send(Cmd::Open { input, device: device.into(), sample_rate, ring, reply })
            .map_err(|_| "the device thread stopped".to_string())?;
        answer.recv_timeout(std::time::Duration::from_secs(5)).map_err(|_| "the device didn't open".to_string())?
    }

    pub fn close(&self, input: lightair_core::Id) {
        let _ = self.tx.send(Cmd::Close(input));
    }
}

impl Drop for Devices {
    fn drop(&mut self) {
        // closing the channel ends the thread, which drops the streams
        let (tx, _) = mpsc::channel();
        drop(std::mem::replace(&mut self.tx, tx));
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

fn open(name: &str, sample_rate: u32, mut ring: rtrb::Producer<f32>) -> Result<(cpal::Stream, u16), String> {
    let host = cpal::default_host();
    let device = if name.is_empty() {
        host.default_input_device()
    } else {
        host.input_devices().map_err(|e| e.to_string())?.find(|d| d.name().is_ok_and(|n| n == name))
    }
    .ok_or_else(|| format!("no audio input called \"{name}\""))?;
    // all its channels, at the engine's rate if it can, in 32-bit float
    let configs: Vec<_> = device.supported_input_configs().map_err(|e| e.to_string())?.collect();
    let channels = configs.iter().map(|c| c.channels()).max().ok_or("the device has no inputs")?;
    let at_rate = configs.iter().find(|c| {
        c.channels() == channels && c.min_sample_rate().0 <= sample_rate && c.max_sample_rate().0 >= sample_rate
    });
    let rate = if at_rate.is_some() {
        sample_rate
    } else {
        device.default_input_config().map_err(|e| e.to_string())?.sample_rate().0
    };
    let config = cpal::StreamConfig {
        channels,
        sample_rate: cpal::SampleRate(rate),
        buffer_size: cpal::BufferSize::Default,
    };
    let mut resampler = (rate != sample_rate).then(|| Resampler::new(channels as usize, rate, sample_rate));
    let stream = device
        .build_input_stream(
            &config,
            move |data: &[f32], _| {
                let samples: &[f32] = match &mut resampler {
                    Some(r) => {
                        r.run(data);
                        &r.out
                    }
                    None => data,
                };
                // a full ring means the engine stopped reading: drop rather than block
                if ring.slots() >= samples.len()
                    && let Ok(mut chunk) = ring.write_chunk_uninit(samples.len())
                {
                    let (a, b) = chunk.as_mut_slices();
                    for (dst, src) in a.iter_mut().chain(b.iter_mut()).zip(samples) {
                        dst.write(*src);
                    }
                    unsafe { chunk.commit_all() };
                }
            },
            |e| tracing::warn!(%e, "audio input"),
            None,
        )
        .map_err(|e| e.to_string())?;
    stream.play().map_err(|e| e.to_string())?;
    tracing::info!(device = name, channels, rate, "audio input open");
    Ok((stream, channels))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lists_this_macs_inputs() {
        // listing doesn't open anything (no permission prompt)
        for d in input_devices() {
            eprintln!("{} · {} ch · {} Hz{}", d.name, d.channels, d.sample_rate, if d.default { " (default)" } else { "" });
            assert!(d.channels > 0);
        }
    }

    #[test]
    fn resampling_keeps_a_ramp_straight() {
        // 44.1 → 48 kHz on a ramp: the output is the same ramp, denser
        let mut r = Resampler::new(1, 44_100, 48_000);
        let input: Vec<f32> = (0..441).map(|i| i as f32).collect();
        r.run(&input);
        assert!((r.out.len() as i32 - 480).abs() <= 1, "{} frames", r.out.len());
        for w in r.out[2..].windows(2) {
            assert!((w[1] - w[0] - 44_100.0 / 48_000.0).abs() < 1e-3);
        }
    }
}
