//! Outputs: recording and streaming (docs/11-outputs-streaming-recording.md).
//!
//! Each running output owns its libobs output, encoders and (for streams) service. libobs
//! output signals update a process-wide [`OutputsRuntime`] that the UI reads; nothing about
//! outputs is in the undo history (it is runtime state, docs/03-architecture.md).

use std::collections::HashMap;
use std::ffi::{CStr, CString, c_void};
use std::path::PathBuf;
use std::sync::OnceLock;
use std::time::Instant;

use libobs_sys as sys;
use lightair_core::Id;
use parking_lot::Mutex;

use crate::obs::Data;

// ---------------------------------------------------------------- runtime status
#[derive(Debug, Clone, PartialEq)]
pub enum OutputState {
    Idle,
    Starting,
    Active {
        since: Instant,
    },
    Reconnecting {
        since: Instant,
    },
    Stopping,
    /// Stopped with an error the user should see.
    Failed(String),
}

impl OutputState {
    pub fn is_running(&self) -> bool {
        matches!(
            self,
            OutputState::Starting
                | OutputState::Active { .. }
                | OutputState::Reconnecting { .. }
                | OutputState::Stopping
        )
    }
}

#[derive(Debug, Clone)]
pub struct OutputStatus {
    pub state: OutputState,
    /// File being written (recordings).
    pub path: Option<PathBuf>,
    pub bytes: u64,
    pub dropped_frames: i32,
    pub total_frames: i32,
}

impl Default for OutputStatus {
    fn default() -> Self {
        Self { state: OutputState::Idle, path: None, bytes: 0, dropped_frames: 0, total_frames: 0 }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum OutputKey {
    Recording,
    Stream(Id),
}

#[derive(Default)]
pub struct OutputsRuntime {
    status: Mutex<HashMap<OutputKey, OutputStatus>>,
}

impl OutputsRuntime {
    pub fn get(&self, key: OutputKey) -> OutputStatus {
        self.status.lock().get(&key).cloned().unwrap_or_default()
    }

    pub fn any_stream_running(&self) -> bool {
        self.status.lock().iter().any(|(k, s)| matches!(k, OutputKey::Stream(_)) && s.state.is_running())
    }

    /// Earliest start among running streams (for the Go Live timer).
    pub fn stream_since(&self) -> Option<Instant> {
        self.status
            .lock()
            .iter()
            .filter(|(k, _)| matches!(k, OutputKey::Stream(_)))
            .filter_map(|(_, s)| match s.state {
                OutputState::Active { since } | OutputState::Reconnecting { since } => Some(since),
                _ => None,
            })
            .min()
    }

    fn update(&self, key: OutputKey, f: impl FnOnce(&mut OutputStatus)) {
        f(self.status.lock().entry(key).or_default());
    }
}

pub fn runtime() -> &'static OutputsRuntime {
    static RUNTIME: OnceLock<OutputsRuntime> = OnceLock::new();
    RUNTIME.get_or_init(OutputsRuntime::default)
}

// ---------------------------------------------------------------- configuration
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Container {
    /// OBS's hybrid MP4: crash-safe while recording, a normal MP4 when finished.
    HybridMp4,
    Mkv,
    Mov,
}

impl Container {
    pub fn extension(self) -> &'static str {
        match self {
            Container::HybridMp4 => "mp4",
            Container::Mkv => "mkv",
            Container::Mov => "mov",
        }
    }
}

#[derive(Debug, Clone)]
pub struct EncoderConfig {
    /// libobs video encoder id (e.g. `com.apple.videotoolbox.videoencoder.ave.avc`, `obs_x264`).
    pub video_encoder: String,
    pub video_bitrate_kbps: u32,
    pub audio_encoder: String,
    pub audio_bitrate_kbps: u32,
    /// CBR for streaming; quality-oriented VBR for recording.
    pub constant_bitrate: bool,
}

#[derive(Debug, Clone)]
pub struct RecordingConfig {
    pub path: PathBuf,
    pub container: Container,
    pub encoders: EncoderConfig,
}

#[derive(Debug, Clone)]
pub struct StreamConfig {
    pub id: Id,
    pub server: String,
    pub key: String,
    pub encoders: EncoderConfig,
}

// ---------------------------------------------------------------- libobs objects
struct Running {
    output: *mut sys::obs_output_t,
    video: *mut sys::obs_encoder_t,
    audio: *mut sys::obs_encoder_t,
    service: *mut sys::obs_service_t,
    /// Heap-pinned key passed to signal callbacks; freed after disconnecting.
    signal_param: *mut OutputKey,
}

unsafe impl Send for Running {}

impl Running {
    unsafe fn release(self) {
        unsafe {
            let handler = sys::obs_output_get_signal_handler(self.output);
            for (name, cb) in SIGNALS {
                let n = CString::new(*name).unwrap();
                sys::signal_handler_disconnect(handler, n.as_ptr(), Some(*cb), self.signal_param as *mut c_void);
            }
            if sys::obs_output_active(self.output) {
                sys::obs_output_force_stop(self.output);
            }
            sys::obs_output_release(self.output);
            sys::obs_encoder_release(self.video);
            sys::obs_encoder_release(self.audio);
            if !self.service.is_null() {
                sys::obs_service_release(self.service);
            }
            drop(Box::from_raw(self.signal_param));
        }
    }
}

type SignalCb = unsafe extern "C" fn(*mut c_void, *mut sys::calldata_t);
const SIGNALS: &[(&str, SignalCb)] =
    &[("start", on_start), ("stop", on_stop), ("reconnect", on_reconnect), ("reconnect_success", on_start)];

unsafe extern "C" fn on_start(param: *mut c_void, _: *mut sys::calldata_t) {
    let key = unsafe { *(param as *const OutputKey) };
    runtime().update(key, |s| {
        let since = match s.state {
            OutputState::Reconnecting { since } => since,
            _ => Instant::now(),
        };
        s.state = OutputState::Active { since };
    });
}

unsafe extern "C" fn on_reconnect(param: *mut c_void, _: *mut sys::calldata_t) {
    let key = unsafe { *(param as *const OutputKey) };
    runtime().update(key, |s| {
        let since = match s.state {
            OutputState::Active { since } | OutputState::Reconnecting { since } => since,
            _ => Instant::now(),
        };
        s.state = OutputState::Reconnecting { since };
    });
}

unsafe extern "C" fn on_stop(param: *mut c_void, data: *mut sys::calldata_t) {
    let key = unsafe { *(param as *const OutputKey) };
    let mut code: i64 = 0;
    let mut output: *mut sys::obs_output_t = std::ptr::null_mut();
    unsafe {
        let name = c"code";
        sys::calldata_get_data(data, name.as_ptr(), &mut code as *mut i64 as *mut c_void, std::mem::size_of::<i64>());
        let name = c"output";
        sys::calldata_get_data(
            data,
            name.as_ptr(),
            &mut output as *mut _ as *mut c_void,
            std::mem::size_of::<*mut c_void>(),
        );
    }
    let state = if code == 0 {
        OutputState::Idle
    } else {
        let detail = if output.is_null() { None } else { last_error(output) };
        OutputState::Failed(describe_stop(code as i32, detail))
    };
    runtime().update(key, |s| s.state = state);
}

fn last_error(output: *mut sys::obs_output_t) -> Option<String> {
    let p = unsafe { sys::obs_output_get_last_error(output) };
    (!p.is_null()).then(|| unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned()).filter(|s| !s.is_empty())
}

/// Plain-language reason for an output stopping (docs/06-design-system.md, Writing style).
fn describe_stop(code: i32, detail: Option<String>) -> String {
    let base = match code {
        sys::OBS_OUTPUT_BAD_PATH => "The recording folder can't be written to.",
        sys::OBS_OUTPUT_CONNECT_FAILED => {
            "Couldn't connect to the server. Check the server address and your connection."
        }
        sys::OBS_OUTPUT_INVALID_STREAM => "The server rejected the stream key.",
        sys::OBS_OUTPUT_DISCONNECTED => "The connection to the server was lost.",
        sys::OBS_OUTPUT_UNSUPPORTED => "This server doesn't support the chosen format.",
        sys::OBS_OUTPUT_NO_SPACE => "The disk is full.",
        sys::OBS_OUTPUT_ENCODE_ERROR => "The encoder stopped working. Try a different encoder.",
        _ => "The output stopped unexpectedly.",
    };
    match detail {
        Some(d) => format!("{base} ({d})"),
        None => base.to_string(),
    }
}

/// Running outputs, owned by the engine thread.
#[derive(Default)]
pub struct Outputs {
    recording: Option<Running>,
    streams: HashMap<Id, Running>,
}

fn c(s: &str) -> CString {
    CString::new(s).unwrap_or_default()
}

unsafe fn create_encoders(
    name: &str,
    cfg: &EncoderConfig,
) -> Result<(*mut sys::obs_encoder_t, *mut sys::obs_encoder_t), String> {
    let mut v = Data::new();
    v.set_int("bitrate", cfg.video_bitrate_kbps as i64)
        .set_int("keyint_sec", 2)
        .set_str("rate_control", if cfg.constant_bitrate { "CBR" } else { "ABR" })
        .set_str("profile", "high");
    if cfg.video_encoder == "obs_x264" {
        v.set_str("preset", "veryfast");
    }
    let mut a = Data::new();
    a.set_int("bitrate", cfg.audio_bitrate_kbps as i64);
    unsafe {
        let video = sys::obs_video_encoder_create(
            c(&cfg.video_encoder).as_ptr(),
            c(&format!("{name} video")).as_ptr(),
            v.as_ptr(),
            std::ptr::null_mut(),
        );
        if video.is_null() {
            return Err(format!("Couldn't create the video encoder ({}).", cfg.video_encoder));
        }
        let audio = sys::obs_audio_encoder_create(
            c(&cfg.audio_encoder).as_ptr(),
            c(&format!("{name} audio")).as_ptr(),
            a.as_ptr(),
            0,
            std::ptr::null_mut(),
        );
        if audio.is_null() {
            sys::obs_encoder_release(video);
            return Err(format!("Couldn't create the audio encoder ({}).", cfg.audio_encoder));
        }
        sys::obs_encoder_set_video(video, sys::obs_get_video());
        sys::obs_encoder_set_audio(audio, sys::obs_get_audio());
        Ok((video, audio))
    }
}

unsafe fn start(
    key: OutputKey,
    output_type: &str,
    output_settings: &Data,
    encoders: &EncoderConfig,
    service: *mut sys::obs_service_t,
) -> Result<Running, String> {
    let name = match key {
        OutputKey::Recording => "Recording".to_string(),
        OutputKey::Stream(id) => format!("Stream {id}"),
    };
    unsafe {
        let release_service = || {
            if !service.is_null() {
                sys::obs_service_release(service);
            }
        };
        let (video, audio) = match create_encoders(&name, encoders) {
            Ok(v) => v,
            Err(e) => {
                release_service();
                return Err(e);
            }
        };
        let output = sys::obs_output_create(
            c(output_type).as_ptr(),
            c(&name).as_ptr(),
            output_settings.as_ptr(),
            std::ptr::null_mut(),
        );
        if output.is_null() {
            sys::obs_encoder_release(video);
            sys::obs_encoder_release(audio);
            release_service();
            return Err(format!("Couldn't create the {output_type} output."));
        }
        sys::obs_output_set_video_encoder(output, video);
        sys::obs_output_set_audio_encoder(output, audio, 0);
        if !service.is_null() {
            sys::obs_output_set_service(output, service);
        }
        let signal_param = Box::into_raw(Box::new(key));
        let handler = sys::obs_output_get_signal_handler(output);
        for (sig, cb) in SIGNALS {
            sys::signal_handler_connect(handler, c(sig).as_ptr(), Some(*cb), signal_param as *mut c_void);
        }
        let running = Running { output, video, audio, service, signal_param };
        runtime().update(key, |s| *s = OutputStatus { state: OutputState::Starting, ..Default::default() });
        if !sys::obs_output_start(output) {
            let err = last_error(output).unwrap_or_else(|| "The output couldn't start.".into());
            running.release();
            runtime().update(key, |s| s.state = OutputState::Failed(err.clone()));
            return Err(err);
        }
        Ok(running)
    }
}

impl Outputs {
    pub fn start_recording(&mut self, cfg: &RecordingConfig) -> Result<(), String> {
        if self.recording.as_ref().is_some_and(|r| unsafe { sys::obs_output_active(r.output) }) {
            return Ok(());
        }
        if let Some(old) = self.recording.take() {
            unsafe { old.release() };
        }
        if let Some(dir) = cfg.path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| format!("Can't create the recording folder: {e}"))?;
        }
        let (output_type, path_key) = match cfg.container {
            Container::HybridMp4 => ("mp4_output", "path"),
            Container::Mov => ("mov_output", "path"),
            Container::Mkv => ("ffmpeg_muxer", "path"),
        };
        let mut settings = Data::new();
        settings.set_str(path_key, &cfg.path.to_string_lossy());
        let running =
            unsafe { start(OutputKey::Recording, output_type, &settings, &cfg.encoders, std::ptr::null_mut())? };
        runtime().update(OutputKey::Recording, |s| s.path = Some(cfg.path.clone()));
        self.recording = Some(running);
        Ok(())
    }

    pub fn stop_recording(&mut self) {
        if let Some(r) = &self.recording {
            runtime().update(OutputKey::Recording, |s| {
                if s.state.is_running() {
                    s.state = OutputState::Stopping
                }
            });
            unsafe { sys::obs_output_stop(r.output) };
        }
    }

    pub fn start_stream(&mut self, cfg: &StreamConfig) -> Result<(), String> {
        let key = OutputKey::Stream(cfg.id);
        if self.streams.get(&cfg.id).is_some_and(|r| unsafe { sys::obs_output_active(r.output) }) {
            return Ok(());
        }
        if let Some(old) = self.streams.remove(&cfg.id) {
            unsafe { old.release() };
        }
        let mut service_settings = Data::new();
        service_settings.set_str("server", &cfg.server).set_str("key", &cfg.key);
        let service = unsafe {
            sys::obs_service_create(
                c("rtmp_custom").as_ptr(),
                c(&format!("Service {}", cfg.id)).as_ptr(),
                service_settings.as_ptr(),
                std::ptr::null_mut(),
            )
        };
        if service.is_null() {
            return Err("Couldn't set up the streaming service.".into());
        }
        let mut output_settings = Data::new();
        output_settings.set_bool("low_latency_mode_enabled", false);
        // On failure `start` releases everything it was given, the service included.
        let running = unsafe { start(key, "rtmp_output", &output_settings, &cfg.encoders, service)? };
        self.streams.insert(cfg.id, running);
        Ok(())
    }

    pub fn stop_stream(&mut self, id: Id) {
        if let Some(r) = self.streams.get(&id) {
            runtime().update(OutputKey::Stream(id), |s| {
                if s.state.is_running() {
                    s.state = OutputState::Stopping
                }
            });
            unsafe { sys::obs_output_stop(r.output) };
        }
    }

    pub fn stop_all_streams(&mut self) {
        let ids: Vec<Id> = self.streams.keys().copied().collect();
        for id in ids {
            self.stop_stream(id);
        }
    }

    /// Updates byte and frame counters for running outputs (called by the stats sampler).
    pub fn sample(&self) {
        let sample = |key: OutputKey, r: &Running| unsafe {
            let (bytes, dropped, total) = (
                sys::obs_output_get_total_bytes(r.output),
                sys::obs_output_get_frames_dropped(r.output),
                sys::obs_output_get_total_frames(r.output),
            );
            runtime().update(key, |s| {
                s.bytes = bytes;
                s.dropped_frames = dropped;
                s.total_frames = total;
            });
        };
        if let Some(r) = &self.recording {
            sample(OutputKey::Recording, r);
        }
        for (id, r) in &self.streams {
            sample(OutputKey::Stream(*id), r);
        }
    }

    /// Stops and releases everything (engine shutdown).
    pub fn shutdown(&mut self) {
        if let Some(r) = self.recording.take() {
            unsafe {
                sys::obs_output_stop(r.output);
                r.release();
            }
        }
        for (_, r) in self.streams.drain() {
            unsafe {
                sys::obs_output_stop(r.output);
                r.release();
            }
        }
    }
}

/// Picks the best available encoders: Apple's hardware H.264 when present.
pub fn default_encoders(available: &[String], video_bitrate_kbps: u32, constant_bitrate: bool) -> EncoderConfig {
    let has = |id: &str| available.iter().any(|e| e == id);
    let video = ["com.apple.videotoolbox.videoencoder.ave.avc", "com.apple.videotoolbox.videoencoder.h264", "obs_x264"]
        .into_iter()
        .find(|id| has(id))
        .unwrap_or("obs_x264");
    let audio = if has("CoreAudio_AAC") { "CoreAudio_AAC" } else { "ffmpeg_aac" };
    EncoderConfig {
        video_encoder: video.into(),
        video_bitrate_kbps,
        audio_encoder: audio.into(),
        audio_bitrate_kbps: 160,
        constant_bitrate,
    }
}
