//! Audio interfaces, captured directly (CoreAudio on macOS, WASAPI on Windows, through cpal): every channel of a device, so
//! an 8-input interface is eight mics (docs/plans/audio-engine.md, "Inputs and outputs").
//! Every channel of a device is its own input: nothing mixes it down to stereo.
//!
//! cpal's streams live on one thread of their own; the engine opens and closes them there.

use std::collections::HashMap;
use std::sync::mpsc;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

/// The input kind of a device input (its settings: `{"device": "<id>"}`, "" for the system's).
pub const DEVICE_KIND: &str = lightair_model::kinds::AUDIO_DEVICE;

#[derive(Debug, Clone, PartialEq)]
pub struct DeviceInfo {
    /// What settings store: the device's own id (CoreAudio's UID, WASAPI's endpoint id), the
    /// same one OBS stores. Two identical interfaces have different ids; renaming one in the
    /// system's settings doesn't change it.
    pub id: String,
    /// What it's called (shown, never looked up by).
    pub name: String,
    pub channels: u16,
    pub sample_rate: u32,
    pub default: bool,
}

/// A device's id, as settings store it (its host's own id: CoreAudio's UID, WASAPI's endpoint id).
pub fn id_of(d: &cpal::Device) -> Option<String> {
    d.id().ok().map(|i| i.id().to_string())
}

/// What a device is called.
pub fn name_of(d: &cpal::Device) -> String {
    d.description().map(|x| x.name().to_string()).unwrap_or_default()
}

/// Whether a device setting means the system's default: "" (and "default", as some settings
/// saved it).
pub fn is_system_default(device: &str) -> bool {
    device.is_empty() || device == "default"
}

/// The device a setting names: the system's default, the device with that id, or (settings
/// saved before devices were stored by id) the one with that name.
fn find(device: &str, default: Option<cpal::Device>, all: impl Iterator<Item = cpal::Device>) -> Option<cpal::Device> {
    if is_system_default(device) {
        return default;
    }
    let all: Vec<cpal::Device> = all.collect();
    all.iter().find(|d| id_of(d).as_deref() == Some(device)).or_else(|| all.iter().find(|d| name_of(d) == device)).cloned()
}

/// The input a setting names (see [`find`]).
pub fn find_input(device: &str) -> Option<cpal::Device> {
    let host = cpal::default_host();
    find(device, host.default_input_device(), host.input_devices().ok()?)
}

/// The output a setting names (see [`find`]).
pub fn find_output(device: &str) -> Option<cpal::Device> {
    let host = cpal::default_host();
    find(device, host.default_output_device(), host.output_devices().ok()?)
}

/// The audio inputs this computer has, with their channel counts.
pub fn input_devices() -> Vec<DeviceInfo> {
    let host = cpal::default_host();
    let default = host.default_input_device().and_then(|d| id_of(&d));
    let Ok(devices) = host.input_devices() else { return vec![] };
    devices
        .filter_map(|d| {
            let id = id_of(&d)?;
            let channels = d.supported_input_configs().ok()?.map(|c| c.channels()).max()?;
            let rate = d.default_input_config().ok().map_or(48_000, |c| c.sample_rate());
            Some(DeviceInfo { default: default.as_deref() == Some(id.as_str()), name: name_of(&d), id, channels, sample_rate: rate })
        })
        .collect()
}

/// The audio outputs this computer has, with their channel counts (Windows: each can be
/// captured as desktop audio, through WASAPI loopback).
pub fn output_devices() -> Vec<DeviceInfo> {
    let host = cpal::default_host();
    let default = host.default_output_device().and_then(|d| id_of(&d));
    let Ok(devices) = host.output_devices() else { return vec![] };
    devices
        .filter_map(|d| {
            let id = id_of(&d)?;
            let config = d.default_output_config().ok()?;
            Some(DeviceInfo {
                default: default.as_deref() == Some(id.as_str()),
                name: name_of(&d),
                id,
                channels: config.channels(),
                sample_rate: config.sample_rate(),
            })
        })
        .collect()
}

/// What a device setting is called: "" for the system's default, else the device's name (the
/// setting itself when that device isn't here).
pub fn device_name(device: &str) -> Option<String> {
    if is_system_default(device) {
        return None;
    }
    Some(find_input(device).or_else(|| find_output(device)).map_or_else(|| missing_name(device), |d| name_of(&d)))
}

/// What a device that isn't here is called: the setting itself, except a Windows endpoint id
/// (`{0.0.1.00000000}.{…}`, a GUID nobody can read), which says what kind of device is missing.
fn missing_name(device: &str) -> String {
    match device.get(..6) {
        Some("{0.0.1") => "Disconnected microphone".into(),
        Some("{0.0.0") => "Disconnected output".into(),
        _ => device.to_string(),
    }
}

/// Turns a device's rate into the engine's by linear interpolation (devices that can't run at
/// the engine's rate, and sound the engine is fed: `audio_host::InputFeed`). Keeps its buffers
/// between callbacks: no allocation once warm.
pub struct Resampler {
    channels: usize,
    step: f64,
    pos: f64,
    last: Vec<f32>,
    out: Vec<f32>,
}

impl Resampler {
    pub fn new(channels: usize, from: u32, to: u32) -> Self {
        Self {
            channels,
            step: from as f64 / to as f64,
            pos: 0.0,
            last: vec![0.0; channels],
            out: Vec::with_capacity(16_384 * channels),
        }
    }

    /// Converts from `from` to `to` from now on (the source's rate changed); the last frame is
    /// kept, so the join is smooth. No allocation.
    pub fn retune(&mut self, from: u32, to: u32) {
        self.step = from as f64 / to.max(1) as f64;
        self.pos = self.pos.fract();
    }

    /// What the last [`Self::run`] made (interleaved).
    pub fn output(&self) -> &[f32] {
        &self.out
    }

    /// Interleaved input in, interleaved output (in `self.out`).
    pub fn run(&mut self, input: &[f32]) {
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
        input: lightair_model::Id,
        device: String,
        sample_rate: u32,
        ring: rtrb::Producer<f32>,
        reply: mpsc::Sender<Result<u16, String>>,
    },
    Close(lightair_model::Id),
    /// The monitor: plays `ring` (stereo, interleaved) on an output device.
    OpenOutput {
        device: String,
        sample_rate: u32,
        ring: rtrb::Consumer<f32>,
        reply: mpsc::Sender<Result<(), String>>,
    },
    CloseOutput,
    /// Another output (a mix played on a device of its own), by `key`.
    OpenExtra {
        key: u128,
        device: String,
        sample_rate: u32,
        ring: rtrb::Consumer<f32>,
        reply: mpsc::Sender<Result<(), String>>,
    },
    CloseExtra(u128),
}

/// Times the monitor ran short (a gap in what's heard), for diagnostics.
pub static MONITOR_SHORTFALLS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// The device thread: owns the open streams.
pub struct Devices {
    tx: mpsc::Sender<Cmd>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Devices {
    pub fn start() -> Self {
        let (tx, rx) = mpsc::channel::<Cmd>();
        let thread = std::thread::Builder::new()
            .name("lightair-devices".into())
            .spawn(move || {
                let mut streams: HashMap<lightair_model::Id, cpal::Stream> = HashMap::new();
                // the monitor's stream, held open (at most one)
                let mut output: Vec<cpal::Stream> = Vec::new();
                // mixes played on devices of their own
                let mut extras: HashMap<u128, cpal::Stream> = HashMap::new();
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
                        Cmd::OpenOutput { device, sample_rate, ring, reply } => {
                            output.clear();
                                match open_output(&device, sample_rate, ring) {
                                Ok(stream) => {
                                    output.push(stream);
                                    let _ = reply.send(Ok(()));
                                }
                                Err(e) => {
                                    let _ = reply.send(Err(e));
                                }
                            }
                        }
                        Cmd::CloseOutput => output.clear(),
                        Cmd::OpenExtra { key, device, sample_rate, ring, reply } => {
                            extras.remove(&key);
                            match open_output(&device, sample_rate, ring) {
                                Ok(stream) => {
                                    extras.insert(key, stream);
                                    let _ = reply.send(Ok(()));
                                }
                                Err(e) => {
                                    let _ = reply.send(Err(e));
                                }
                            }
                        }
                        Cmd::CloseExtra(key) => {
                            extras.remove(&key);
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
        input: lightair_model::Id,
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

    pub fn close(&self, input: lightair_model::Id) {
        let _ = self.tx.send(Cmd::Close(input));
    }

    /// Plays `ring` (stereo, interleaved, at `sample_rate`) on output `device` ("" for the
    /// default output).
    pub fn open_output(&self, device: &str, sample_rate: u32, ring: rtrb::Consumer<f32>) -> Result<(), String> {
        let (reply, answer) = mpsc::channel();
        self.tx
            .send(Cmd::OpenOutput { device: device.into(), sample_rate, ring, reply })
            .map_err(|_| "the device thread stopped".to_string())?;
        answer.recv_timeout(std::time::Duration::from_secs(5)).map_err(|_| "the output didn't open".to_string())?
    }

    pub fn close_output(&self) {
        let _ = self.tx.send(Cmd::CloseOutput);
    }

    /// Plays `ring` on output `device` as another output, `key` (closing any it had).
    pub fn open_extra(&self, key: u128, device: &str, sample_rate: u32, ring: rtrb::Consumer<f32>) -> Result<(), String> {
        let (reply, answer) = mpsc::channel();
        self.tx
            .send(Cmd::OpenExtra { key, device: device.into(), sample_rate, ring, reply })
            .map_err(|_| "the device thread stopped".to_string())?;
        answer.recv_timeout(std::time::Duration::from_secs(5)).map_err(|_| "the output didn't open".to_string())?
    }

    pub fn close_extra(&self, key: u128) {
        let _ = self.tx.send(Cmd::CloseExtra(key));
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

/// The device a setting `name` is captured from, and whether it's an output captured as it plays
/// (loopback): the input it names (`input`), else, where outputs can be (`loopback`: Windows'
/// WASAPI), the output it names (`output`). The system's default is always an input.
fn capture_from<D>(
    name: &str,
    input: Option<D>,
    output: impl FnOnce() -> Option<D>,
    loopback: bool,
) -> Result<(D, bool), String> {
    match input {
        Some(d) => Ok((d, false)),
        None if loopback && !is_system_default(name) => {
            Ok((output().ok_or_else(|| format!("no audio input or output \"{name}\""))?, true))
        }
        None => Err(format!("no audio input \"{name}\"")),
    }
}

/// The format a device is opened in, from what it supports (`configs`): all its channels (the
/// most any config has), and whether a config with that many takes `sample_rate` (else the
/// device's default rate, resampled). `None` when it supports nothing.
fn stream_format(configs: &[cpal::SupportedStreamConfigRange], sample_rate: u32) -> Option<(u16, bool)> {
    let channels = configs.iter().map(|c| c.channels()).max()?;
    let at_rate = configs.iter().any(|c| {
        c.channels() == channels && c.min_sample_rate() <= sample_rate && c.max_sample_rate() >= sample_rate
    });
    Some((channels, at_rate))
}

fn open(name: &str, sample_rate: u32, mut ring: rtrb::Producer<f32>) -> Result<(cpal::Stream, u16), String> {
    // Windows: an output is captured as it plays (WASAPI loopback: an input stream on it, in
    // the output's own formats)
    let (device, loopback) = capture_from(name, find_input(name), || find_output(name), cfg!(windows))?;
    // all its channels, at the engine's rate if it can, in 32-bit float
    let configs: Vec<_> = if loopback {
        device.supported_output_configs().map_err(|e| e.to_string())?.collect()
    } else {
        device.supported_input_configs().map_err(|e| e.to_string())?.collect()
    };
    let (channels, at_rate) = stream_format(&configs, sample_rate).ok_or("the device has no inputs")?;
    let rate = if at_rate {
        sample_rate
    } else {
        let default = if loopback { device.default_output_config() } else { device.default_input_config() };
        default.map_err(|e| e.to_string())?.sample_rate()
    };
    let config = cpal::StreamConfig {
        channels,
        sample_rate: rate,
        buffer_size: cpal::BufferSize::Default,
    };
    let mut resampler = (rate != sample_rate).then(|| Resampler::new(channels as usize, rate, sample_rate));
    let stream = device
        .build_input_stream(
            config,
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

fn open_output(name: &str, sample_rate: u32, mut ring: rtrb::Consumer<f32>) -> Result<cpal::Stream, String> {
    let device = find_output(name).ok_or_else(|| format!("no audio output \"{name}\""))?;
    // the engine's rate, as few channels as it takes (at least two), small buffers
    let config = device
        .supported_output_configs()
        .map_err(|e| e.to_string())?
        .filter(|c| c.min_sample_rate() <= sample_rate && c.max_sample_rate() >= sample_rate && c.channels() >= 2)
        .min_by_key(|c| c.channels())
        .ok_or("the output can't play at the engine's rate")?;
    let channels = config.channels() as usize;
    let buffer = match config.buffer_size() {
        cpal::SupportedBufferSize::Range { min, max } => cpal::BufferSize::Fixed(128.clamp(*min, *max)),
        cpal::SupportedBufferSize::Unknown => cpal::BufferSize::Default,
    };
    let stream_config = cpal::StreamConfig { channels: channels as u16, sample_rate, buffer_size: buffer };
    let stream = device
        .build_output_stream(
            stream_config,
            move |data: &mut [f32], _| {
                let frames = data.len() / channels;
                let have = (ring.slots() / 2).min(frames);
                if have < frames {
                    MONITOR_SHORTFALLS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                }
                data.fill(0.0);
                if let Ok(chunk) = ring.read_chunk(have * 2) {
                    let (a, b) = chunk.as_slices();
                    for k in 0..have {
                        for c in 0..2 {
                            let i = k * 2 + c;
                            data[k * channels + c] = if i < a.len() { a[i] } else { b[i - a.len()] };
                        }
                    }
                    chunk.commit_all();
                }
            },
            |e| tracing::warn!(%e, "monitor output"),
            None,
        )
        .map_err(|e| e.to_string())?;
    stream.play().map_err(|e| e.to_string())?;
    tracing::info!(device = name, "monitor output open");
    Ok(stream)
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn an_input_is_captured_before_an_output_of_that_name() {
        let never = || -> Option<&str> { panic!("an output isn't looked for when the input is there") };
        assert_eq!(capture_from("usb", Some("mic"), never, true), Ok(("mic", false)));
        assert_eq!(capture_from("", Some("default mic"), never, true), Ok(("default mic", false)));
    }

    /// Windows: a setting naming an output (desktop audio from one output) is captured from it as
    /// it plays (loopback); elsewhere, or with no such output, it's missing.
    #[test]
    fn an_output_is_captured_by_loopback_only_where_it_can_be() {
        let output = "{0.0.0.00000000}.{headphones}";
        assert_eq!(capture_from(output, None, || Some("headphones"), true), Ok(("headphones", true)));
        assert_eq!(
            capture_from(output, None, || None::<&str>, true),
            Err(format!("no audio input or output \"{output}\""))
        );
        assert_eq!(capture_from(output, None, || Some("headphones"), false), Err(format!("no audio input \"{output}\"")));
    }

    /// The system's default ("" or "default") is never an output's loopback: plain desktop audio is
    /// the process loopback's, not the default output's.
    #[test]
    fn the_system_default_is_never_an_outputs_loopback() {
        for name in ["", "default"] {
            let never = || -> Option<&str> { panic!("the default isn't looked for among outputs") };
            assert_eq!(capture_from(name, None, never, true), Err(format!("no audio input \"{name}\"")));
        }
    }

    fn range(channels: u16, min: u32, max: u32) -> cpal::SupportedStreamConfigRange {
        cpal::SupportedStreamConfigRange::new(channels, min, max, cpal::SupportedBufferSize::Unknown, cpal::SampleFormat::F32)
    }

    /// A device (an output's loopback as much as an input) opens with all its channels, at the
    /// engine's rate when a config with that many channels takes it.
    #[test]
    fn a_device_opens_with_all_its_channels_at_the_engines_rate_when_it_can() {
        // a shared-mode WASAPI output: one format, its mix format
        assert_eq!(stream_format(&[range(2, 48_000, 48_000)], 48_000), Some((2, true)));
        assert_eq!(stream_format(&[range(2, 44_100, 44_100)], 48_000), Some((2, false)), "resampled from its own rate");
        // a 7.1 output: eight channels, even though stereo is offered at the engine's rate
        let surround = [range(2, 48_000, 48_000), range(8, 44_100, 44_100)];
        assert_eq!(stream_format(&surround, 48_000), Some((8, false)));
        // a range taking the rate
        assert_eq!(stream_format(&[range(1, 8_000, 96_000), range(2, 8_000, 96_000)], 48_000), Some((2, true)));
        assert_eq!(stream_format(&[], 48_000), None, "a device with no formats");
    }

    /// A Windows endpoint that isn't here is called by its kind (an output: what desktop audio from
    /// an unplugged output is called), not its GUID.
    #[test]
    fn a_missing_output_is_called_by_its_kind() {
        assert_eq!(missing_name("{0.0.0.00000000}.{aaaaaaaa-0000-0000-0000-000000000000}"), "Disconnected output");
        assert_eq!(missing_name("{0.0.1.00000000}.{aaaaaaaa-0000-0000-0000-000000000000}"), "Disconnected microphone");
        assert_eq!(missing_name("BlackHole 2ch"), "BlackHole 2ch");
        assert_eq!(device_name(""), None);
        assert_eq!(device_name("default"), None);
        assert_eq!(
            device_name("{0.0.0.00000000}.{aaaaaaaa-0000-0000-0000-00000000dead}").as_deref(),
            Some("Disconnected output")
        );
    }

    /// The outputs listed (desktop audio's choices on Windows) each have an id and channels, and
    /// at most one is the default; each is found again by its id. Passes with no outputs.
    #[test]
    fn listed_outputs_are_found_by_their_ids() {
        let outputs = output_devices();
        assert!(outputs.iter().filter(|d| d.default).count() <= 1, "{outputs:?}");
        for d in &outputs {
            assert!(!d.id.is_empty() && d.channels > 0 && d.sample_rate > 0, "{d:?}");
            let found = find_output(&d.id).and_then(|o| id_of(&o));
            assert_eq!(found.as_deref(), Some(d.id.as_str()), "{} is found by its id", d.name);
        }
    }
}
