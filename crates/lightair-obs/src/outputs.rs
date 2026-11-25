//! Outputs: recording and streaming (docs/11-outputs-streaming-recording.md).
//!
//! Each running output owns its libobs output, encoders and (for streams) service. libobs
//! output signals update a process-wide [`OutputsRuntime`] that the UI reads; nothing about
//! outputs is in the undo history (it is runtime state, docs/03-architecture.md).

use std::collections::HashMap;
use std::ffi::{CStr, CString, c_void};
use std::path::PathBuf;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use libobs_sys as sys;
use lightair_model::Id;
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

/// A stream trying to get back: which try is next, how many there are, and when it comes.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ReconnectInfo {
    /// The try about to run (1 for the first).
    pub attempt: u32,
    /// Tries allowed before it gives up.
    pub retries: u32,
    /// When that try runs.
    pub next_at: Instant,
}

impl ReconnectInfo {
    /// Whole seconds until the next try (0 once it's due).
    pub fn seconds_left(&self) -> u64 {
        self.next_at.saturating_duration_since(Instant::now()).as_secs_f64().ceil() as u64
    }
}

#[derive(Debug, Clone)]
pub struct OutputStatus {
    pub state: OutputState,
    /// File being written (recordings).
    pub path: Option<PathBuf>,
    pub bytes: u64,
    /// Frames the network dropped (the output's own count).
    pub dropped_frames: i32,
    pub total_frames: i32,
    /// Frames the video output skipped since this output began because its encoder was too slow
    /// (per canvas: outputs on one canvas see the same ones).
    pub encoder_skipped: u32,
    /// Streams: tries allowed when the connection drops (set when it starts).
    pub retries: u32,
    /// Streams: while reconnecting, the try coming and when.
    pub reconnect: Option<ReconnectInfo>,
    /// Streams with a delay: the delay in force, seconds (while live or draining).
    pub delay_s: u32,
    /// Streams: when it was asked to stop (a delayed stream goes on sending for a while).
    pub stopping_since: Option<Instant>,
    /// Streams with adaptive bitrate: the encoder's bitrate now and what it is set to (kbps).
    pub current_kbps: u32,
    pub target_kbps: u32,
    /// Clips saved since the replay buffer started.
    pub saved_clips: u32,
    /// Recording: paused (the file holds no time while paused).
    pub paused: bool,
    /// Recording: time spent paused before the current pause, and when the current one began.
    pub paused_for: std::time::Duration,
    pub paused_since: Option<Instant>,
    /// Recording: markers added (chapters in MP4/MOV).
    pub markers: u32,
    /// Recording: files written so far (1 + splits); `path` is the current one.
    pub files: u32,
}

impl OutputStatus {
    /// Time actually recorded since `since` (excludes pauses).
    pub fn recorded(&self, since: Instant) -> std::time::Duration {
        let paused = self.paused_for + self.paused_since.map_or_default(|t| t.elapsed());
        since.elapsed().saturating_sub(paused)
    }
}

impl Default for OutputStatus {
    fn default() -> Self {
        Self {
            state: OutputState::Idle,
            path: None,
            bytes: 0,
            dropped_frames: 0,
            total_frames: 0,
            encoder_skipped: 0,
            retries: 0,
            reconnect: None,
            delay_s: 0,
            stopping_since: None,
            current_kbps: 0,
            target_kbps: 0,
            saved_clips: 0,
            paused: false,
            paused_for: std::time::Duration::ZERO,
            paused_since: None,
            markers: 0,
            files: 0,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum OutputKey {
    /// A recording of one canvas (by its canvas id); several can run at once.
    Recording(Id),
    Stream(Id),
    /// The replay buffer of one canvas (by its canvas id), behind the Clip button; several can
    /// run at once.
    Clips(Id),
    /// The virtual camera: a canvas as a camera other apps can pick (macOS: through the OBS
    /// Virtual Camera extension, lightair-effect-stack's `lightair_virtualcam` output).
    VirtualCamera,
}

#[derive(Default)]
pub struct OutputsRuntime {
    status: Mutex<HashMap<OutputKey, OutputStatus>>,
}

impl OutputsRuntime {
    pub fn get(&self, key: OutputKey) -> OutputStatus {
        self.status.lock().get(&key).cloned().unwrap_or_default()
    }

    pub fn any_recording_running(&self) -> bool {
        self.status.lock().iter().any(|(k, s)| matches!(k, OutputKey::Recording(_)) && s.state.is_running())
    }

    /// The recordings as one: the longest-running one while any run (its time, pause and
    /// markers stand for all), else one starting, stopping or failed, else idle.
    pub fn recording(&self) -> OutputStatus {
        let status = self.status.lock();
        let rank = |s: &OutputStatus| match s.state {
            OutputState::Active { .. } | OutputState::Reconnecting { .. } => 0,
            OutputState::Starting | OutputState::Stopping => 1,
            OutputState::Failed(_) => 2,
            OutputState::Idle => 3,
        };
        let since = |s: &OutputStatus| match s.state {
            OutputState::Active { since } | OutputState::Reconnecting { since } => Some(since),
            _ => None,
        };
        status
            .iter()
            .filter(|(k, _)| matches!(k, OutputKey::Recording(_)))
            .map(|(_, s)| s)
            .min_by_key(|s| (rank(s), since(s)))
            .cloned()
            .unwrap_or_default()
    }

    /// The replay buffers as one: running ones first (their saved clips added up; the path is
    /// the clip saved last), else one starting, stopping or failed, else idle.
    pub fn clips(&self) -> OutputStatus {
        let status = self.status.lock();
        let all: Vec<&OutputStatus> =
            status.iter().filter(|(k, _)| matches!(k, OutputKey::Clips(_))).map(|(_, s)| s).collect();
        let rank = |s: &OutputStatus| match s.state {
            OutputState::Active { .. } | OutputState::Reconnecting { .. } => 0,
            OutputState::Starting | OutputState::Stopping => 1,
            OutputState::Failed(_) => 2,
            OutputState::Idle => 3,
        };
        let mut best = all.iter().min_by_key(|s| rank(s)).map(|s| (*s).clone()).unwrap_or_default();
        best.saved_clips = all.iter().map(|s| s.saved_clips).sum();
        if let Some(last) = LAST_CLIP.lock().as_ref().and_then(|(canvas, _)| status.get(&OutputKey::Clips(*canvas))) {
            best.path = last.path.clone();
        }
        best
    }

    /// Whether any replay buffer is running.
    pub fn any_clips_running(&self) -> bool {
        self.status.lock().iter().any(|(k, s)| matches!(k, OutputKey::Clips(_)) && s.state.is_running())
    }

    /// Whether any replay buffer is recording (ready for a clip).
    pub fn clips_ready(&self) -> bool {
        self.status
            .lock()
            .iter()
            .any(|(k, s)| matches!(k, OutputKey::Clips(_)) && matches!(s.state, OutputState::Active { .. }))
    }

    pub fn any_stream_running(&self) -> bool {
        self.status.lock().iter().any(|(k, s)| matches!(k, OutputKey::Stream(_)) && s.state.is_running())
    }

    /// Earliest start among running streams (for the Go Live timer).
    /// Each stream destination's status, by its id.
    pub fn streams(&self) -> Vec<(Id, OutputStatus)> {
        self.status
            .lock()
            .iter()
            .filter_map(|(k, s)| match k {
                OutputKey::Stream(id) => Some((*id, s.clone())),
                _ => None,
            })
            .collect()
    }

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

    pub(crate) fn update(&self, key: OutputKey, f: impl FnOnce(&mut OutputStatus)) {
        f(self.status.lock().entry(key).or_default());
    }
}

/// Marks an output as starting before the engine thread has got to it (the UI waits on the
/// state: Starting, then Active or Failed).
pub fn mark_starting(key: OutputKey) {
    runtime().update(key, |s| *s = OutputStatus { state: OutputState::Starting, ..Default::default() });
}

/// Marks an output as failed with a reason the user should see (a start that never reached
/// libobs, such as a bad address, would otherwise leave no trace).
pub fn mark_failed(key: OutputKey, reason: &str) {
    runtime().update(key, |s| s.state = OutputState::Failed(reason.to_string()));
}

/// Marks an output as off again: a start called off before it reached libobs (the person said
/// no to a platform's plan).
pub fn mark_idle(key: OutputKey) {
    runtime().update(key, |s| *s = OutputStatus::default());
}

pub fn runtime() -> &'static OutputsRuntime {
    static RUNTIME: OnceLock<OutputsRuntime> = OnceLock::new();
    RUNTIME.get_or_init(OutputsRuntime::default)
}

/// A canvas's video output, as returned by `Reconciler::canvas_video`. Only the engine creates
/// these, so the pointer is always a live libobs video output.
#[derive(Clone, Copy)]
pub struct VideoOutput(pub(crate) *mut sys::video_t);

/// The primary canvas's video, for outputs given no canvas: its output canvas, showing Lightair's
/// own compositor's picture, with `LIGHTAIR_ENGINE=own` (macOS); libobs's main canvas otherwise.
pub(crate) fn primary_video() -> *mut sys::video_t {
    #[cfg(target_os = "macos")]
    if let Some(video) = crate::program_source::video(None) {
        return video;
    }
    unsafe { sys::obs_get_video() }
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

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct EncoderConfig {
    /// libobs video encoder id (e.g. `com.apple.videotoolbox.videoencoder.ave.avc`, `obs_x264`).
    pub video_encoder: String,
    pub video_bitrate_kbps: u32,
    pub audio_encoder: String,
    pub audio_bitrate_kbps: u32,
    /// CBR for streaming; quality-oriented VBR for recording.
    pub constant_bitrate: bool,
    /// B-frames (WebRTC/WHIP can't carry them).
    pub bframes: bool,
    /// Seconds between keyframes.
    pub keyint_sec: u32,
    /// Encode at this size instead of the canvas's (scaled on the GPU).
    pub scaled: Option<(u32, u32)>,
    /// The libobs mix its audio is (0: Program, the master; 1–5: the audio engine's mixes).
    pub audio_mix: usize,
    /// The output lowers and raises this encoder's bitrate with the connection (RTMP's dynamic
    /// bitrate). Such an encoder is its own: its output changes the bitrate, so others on it
    /// would change too.
    pub adaptive: bool,
    /// Encode every this many canvas frames (1: all of them; 2: half the canvas's rate). Part
    /// of what decides whether destinations share an encoder.
    pub fps_divisor: u32,
}

impl EncoderConfig {
    /// The first setting that keeps `other` from sharing this encoder, in words ("bitrate",
    /// "size"…); `None` when every setting matches.
    pub fn difference(&self, other: &EncoderConfig) -> Option<&'static str> {
        if self.video_encoder != other.video_encoder {
            Some("codec")
        } else if self.video_bitrate_kbps != other.video_bitrate_kbps {
            Some("bitrate")
        } else if self.scaled != other.scaled {
            Some("size")
        } else if self.keyint_sec != other.keyint_sec {
            Some("keyframe interval")
        } else if self.audio_bitrate_kbps != other.audio_bitrate_kbps || self.audio_encoder != other.audio_encoder {
            Some("audio bitrate")
        } else if self.audio_mix != other.audio_mix {
            Some("audio mix")
        } else if self.bframes != other.bframes {
            Some("B-frames")
        } else if self.constant_bitrate != other.constant_bitrate {
            Some("rate control")
        } else if self.fps_divisor != other.fps_divisor {
            Some("frame rate")
        } else if self.adaptive != other.adaptive {
            Some("adaptive bitrate")
        } else {
            None
        }
    }
}

/// The encoders a stream to `server` really uses: its settings, with what the protocol needs
/// (WebRTC carries Opus and no B-frames). Destinations share an encoder when these are equal.
pub fn stream_encoders(server: &str, cfg: &EncoderConfig) -> EncoderConfig {
    let mut encoders = cfg.clone();
    if StreamProtocol::of(server) == Some(StreamProtocol::Whip) {
        encoders.audio_encoder = "ffmpeg_opus".into();
        encoders.bframes = false;
    }
    encoders
}

#[derive(Debug, Clone)]
pub struct RecordingConfig {
    /// The canvas (canvas id) recorded; it keys the output.
    pub canvas: Id,
    pub path: PathBuf,
    /// Added to split files' names so canvases recorded together don't collide ("Vertical").
    pub label: Option<String>,
    /// Start a new file every this many minutes.
    pub split_minutes: Option<u32>,
    pub container: Container,
    pub encoders: EncoderConfig,
    /// libobs mixes recorded as extra audio tracks after the program (the audio engine's mixes,
    /// 1-5).
    pub extra_mixes: Vec<usize>,
}

#[derive(Debug, Clone)]
pub struct ClipsConfig {
    /// The canvas (canvas id) kept in this buffer; it keys the output.
    pub canvas: Id,
    /// Added to the clip's file name when several canvases are kept ("Vertical").
    pub label: Option<String>,
    pub folder: PathBuf,
    pub seconds: u32,
    pub encoders: EncoderConfig,
}

/// How a destination is reached, from its URL.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamProtocol {
    /// rtmp:// and rtmps://
    Rtmp,
    /// srt://, rist://, udp://, tcp:// (MPEG-TS). The key is the SRT stream ID.
    MpegTs,
    /// http(s):// WHIP (WebRTC). The key is the bearer token; audio is Opus.
    Whip,
}

impl StreamProtocol {
    pub fn of(server: &str) -> Option<Self> {
        let s = server.trim().to_ascii_lowercase();
        let has = |p: &str| s.starts_with(p);
        // "auto:twitch" is an RTMP server picked when going live (the nearest Twitch ingest)
        if has("rtmp://") || has("rtmps://") || has("auto:twitch") {
            Some(Self::Rtmp)
        } else if has("srt://") || has("rist://") || has("udp://") || has("tcp://") {
            Some(Self::MpegTs)
        } else if has("http://") || has("https://") {
            Some(Self::Whip)
        } else {
            None
        }
    }

    pub fn label(server: &str) -> &'static str {
        let s = server.trim().to_ascii_lowercase();
        match Self::of(&s) {
            Some(Self::Rtmp) if s.starts_with("rtmps") => "RTMPS",
            Some(Self::Rtmp) => "RTMP",
            Some(Self::MpegTs) if s.starts_with("srt") => "SRT",
            Some(Self::MpegTs) if s.starts_with("rist") => "RIST",
            Some(Self::MpegTs) => "MPEG-TS",
            Some(Self::Whip) => "WHIP",
            None => "?",
        }
    }
}

#[derive(Debug, Clone)]
pub struct StreamConfig {
    pub id: Id,
    pub server: String,
    pub key: String,
    pub encoders: EncoderConfig,
    /// (retries, seconds apart) when the connection drops; `None`: libobs's defaults.
    pub reconnect: Option<(u32, u32)>,
    /// A second audio track (Twitch's VOD track): the libobs mix it carries (RTMP only).
    pub vod_mix: Option<usize>,
    /// Which way out it goes, and any delay.
    pub net: StreamNet,
}

/// How a stream is sent: over which address, and held back how long.
#[derive(Debug, Clone, Default)]
pub struct StreamNet {
    /// The local address to send from (an interface's); `None`: the system's choice. RTMP only.
    pub bind_ip: Option<String>,
    pub ip_family: lightair_model::IpFamily,
    /// Seconds to hold the stream back, and whether a dropped connection carries on from where
    /// it stopped (libobs's "preserve cutoff point").
    pub delay: Option<(u32, bool)>,
}

/// A stream sent as several encodes in one RTMP connection (Enhanced Broadcasting: Twitch's
/// Dual Format), the encodes as the platform planned them. Video track *i* is `video[i]`.
#[derive(Debug, Clone, Default)]
pub struct Multitrack {
    pub video: Vec<MultitrackVideo>,
    /// The live audio tracks' encoder settings (JSON objects), on the stream's mix.
    pub audio: Vec<serde_json::Value>,
    /// The VOD track's audio settings, on `StreamConfig::vod_mix` (none without one).
    pub vod_audio: Vec<serde_json::Value>,
    /// The connection's bitrate table for adaptive bitrate: a JSON array with one table per video
    /// track, when the plan has one for every track (else libobs's own).
    pub bitrate_table: Option<String>,
}

/// One video encode of a [`Multitrack`] stream.
#[derive(Debug, Clone)]
pub struct MultitrackVideo {
    /// The libobs encoder id.
    pub encoder: String,
    /// Scaled to this size from its canvas, on the GPU.
    pub width: u32,
    pub height: u32,
    /// Every this many canvas frames (1: all).
    pub fps_divisor: u32,
    /// The encoder's own settings (bitrate, keyint_sec, profile, rate_control…), as planned.
    pub settings: serde_json::Value,
    /// The scaler ("OBS_SCALE_BICUBIC", "…_LANCZOS"…); `None`: bicubic.
    pub scale: Option<String>,
}

/// libobs's scaler for a plan's name of it.
fn scale_type(name: Option<&str>) -> sys::obs_scale_type {
    match name {
        Some("OBS_SCALE_DISABLE") => sys::OBS_SCALE_DISABLE,
        Some("OBS_SCALE_POINT") => sys::OBS_SCALE_POINT,
        Some("OBS_SCALE_BILINEAR") => sys::OBS_SCALE_BILINEAR,
        Some("OBS_SCALE_LANCZOS") => sys::OBS_SCALE_LANCZOS,
        Some("OBS_SCALE_AREA") => sys::OBS_SCALE_AREA,
        _ => sys::OBS_SCALE_BICUBIC,
    }
}

// ---------------------------------------------------------------- libobs objects
struct Running {
    output: *mut sys::obs_output_t,
    video: *mut sys::obs_encoder_t,
    audio: *mut sys::obs_encoder_t,
    service: *mut sys::obs_service_t,
    /// Heap-pinned key passed to signal callbacks; freed after disconnecting.
    signal_param: *mut OutputKey,
    signals: &'static [(&'static str, SignalCb)],
    /// What its encoders were made for; outputs with the same key share them.
    share: (usize, EncoderConfig),
    /// Audio encoders of the extra tracks (recordings carrying mixes).
    extra: Vec<*mut sys::obs_encoder_t>,
    /// The canvas video's skipped-frame count when this output began (it counts from the
    /// canvas's creation).
    skipped_base: u32,
    /// Its bitrate is adaptive (it reports the current one).
    adaptive: bool,
    /// A multitrack stream's encoders, kept aligned (null otherwise). Its other video encoders
    /// are in `extra`.
    group: *mut sys::obs_encoder_group_t,
}

unsafe impl Send for Running {}

impl Running {
    unsafe fn release(self) {
        unsafe {
            let handler = sys::obs_output_get_signal_handler(self.output);
            for (name, cb) in self.signals {
                let n = CString::new(*name).unwrap();
                sys::signal_handler_disconnect(handler, n.as_ptr(), Some(*cb), self.signal_param as *mut c_void);
            }
            if sys::obs_output_active(self.output) {
                sys::obs_output_force_stop(self.output);
            }
            sys::obs_output_release(self.output);
            sys::obs_encoder_release(self.video);
            sys::obs_encoder_release(self.audio);
            for e in self.extra {
                sys::obs_encoder_release(e);
            }
            if !self.service.is_null() {
                sys::obs_service_release(self.service);
            }
            // (the group holds its own references; it goes once its encoders have stopped)
            if !self.group.is_null() {
                sys::obs_encoder_group_destroy(self.group);
            }
            drop(Box::from_raw(self.signal_param));
        }
    }
}

type SignalCb = unsafe extern "C" fn(*mut c_void, *mut sys::calldata_t);
/// Signals each kind of output declares (libobs warns about connecting to undeclared ones).
fn signals_for(key: OutputKey) -> &'static [(&'static str, SignalCb)] {
    match key {
        OutputKey::Recording(_) => &[("start", on_start), ("stop", on_stop), ("file_changed", on_file_changed)],
        OutputKey::Stream(_) => {
            &[("start", on_start), ("stop", on_stop), ("reconnect", on_reconnect), ("reconnect_success", on_start)]
        }
        OutputKey::Clips(_) => &[("start", on_start), ("stop", on_stop), ("saved", on_saved)],
        OutputKey::VirtualCamera => &[("start", on_start), ("stop", on_stop)],
    }
}

/// The recording moved on to a new file (a split).
unsafe extern "C" fn on_file_changed(param: *mut c_void, data: *mut sys::calldata_t) {
    let key = unsafe { *(param as *const OutputKey) };
    let mut p: *const std::ffi::c_char = std::ptr::null();
    let path = unsafe { sys::calldata_get_string(data, c"next_file".as_ptr(), &mut p) && !p.is_null() }
        .then(|| PathBuf::from(unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned()));
    runtime().update(key, |s| {
        if path.is_some() {
            s.path = path;
        }
        s.files += 1;
    });
}

/// Replay buffer saved a clip: record its path (the UI shows it and counts clips).
unsafe extern "C" fn on_saved(param: *mut c_void, _: *mut sys::calldata_t) {
    let key = unsafe { *(param as *const OutputKey) };
    let OutputKey::Clips(canvas) = key else { return };
    let path = CLIPS_OUTPUT.lock().get(&canvas).and_then(|p| unsafe { last_replay(p.0) });
    *LAST_CLIP.lock() = Some((canvas, std::time::Instant::now()));
    runtime().update(key, |s| {
        s.path = path;
        s.saved_clips += 1;
    });
}

struct OutputPtr(*mut sys::obs_output_t);
unsafe impl Send for OutputPtr {}
static CLIPS_OUTPUT: Mutex<std::collections::BTreeMap<Id, OutputPtr>> = Mutex::new(std::collections::BTreeMap::new());
/// The canvas whose buffer saved a clip last.
static LAST_CLIP: Mutex<Option<(Id, std::time::Instant)>> = Mutex::new(None);

/// `canvas`'s replay buffer saved a clip (one Lightair wrote itself).
#[cfg(target_os = "macos")]
pub(crate) fn note_clip_saved(canvas: Id) {
    *LAST_CLIP.lock() = Some((canvas, std::time::Instant::now()));
}

unsafe fn proc_call(output: *mut sys::obs_output_t, name: &str, cd: &mut sys::calldata_t) -> bool {
    unsafe {
        let ph = sys::obs_output_get_proc_handler(output);
        sys::proc_handler_call(ph, c(name).as_ptr(), cd)
    }
}

unsafe fn last_replay(output: *mut sys::obs_output_t) -> Option<PathBuf> {
    let mut cd = sys::calldata_t::default();
    let mut path = None;
    unsafe {
        if proc_call(output, "get_last_replay", &mut cd) {
            let mut p: *const std::ffi::c_char = std::ptr::null();
            if sys::calldata_get_string(&cd, c"path".as_ptr(), &mut p) && !p.is_null() {
                path = Some(PathBuf::from(CStr::from_ptr(p).to_string_lossy().into_owned()));
            }
        }
        if !cd.fixed && !cd.stack.is_null() {
            sys::bfree(cd.stack as *mut c_void);
        }
    }
    path
}

unsafe extern "C" fn on_start(param: *mut c_void, _: *mut sys::calldata_t) {
    let key = unsafe { *(param as *const OutputKey) };
    runtime().update(key, |s| {
        let since = match s.state {
            OutputState::Reconnecting { since } => since,
            _ => Instant::now(),
        };
        s.state = OutputState::Active { since };
        s.reconnect = None;
    });
}

unsafe extern "C" fn on_reconnect(param: *mut c_void, data: *mut sys::calldata_t) {
    let key = unsafe { *(param as *const OutputKey) };
    // libobs says how long it waits before the next try
    let mut timeout: i64 = 0;
    unsafe {
        sys::calldata_get_data(
            data,
            c"timeout_sec".as_ptr(),
            &mut timeout as *mut i64 as *mut c_void,
            std::mem::size_of::<i64>(),
        );
    }
    runtime().update(key, |s| {
        let since = match s.state {
            OutputState::Active { since } | OutputState::Reconnecting { since } => since,
            _ => Instant::now(),
        };
        s.state = OutputState::Reconnecting { since };
        let attempt = s.reconnect.map_or(1, |r| r.attempt + 1);
        s.reconnect = Some(ReconnectInfo {
            attempt,
            retries: s.retries,
            next_at: Instant::now() + Duration::from_secs(timeout.max(0) as u64),
        });
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
    runtime().update(key, |s| {
        s.state = state;
        s.reconnect = None;
    });
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
    recordings: HashMap<Id, Running>,
    streams: HashMap<Id, Running>,
    clips: HashMap<Id, Running>,
    virtual_camera: Option<Running>,
    /// The virtual camera's own canvas, while it shows one scene rather than what's on air.
    virtual_camera_canvas: Option<crate::obs::ObsCanvas>,
}

/// One scene for the virtual camera (instead of a canvas's program): its scene source and the
/// size and rate of the canvas it belongs to.
#[derive(Debug, Clone, Copy)]
pub struct VirtualCameraScene {
    pub source: *mut sys::obs_source_t,
    pub width: u32,
    pub height: u32,
    pub fps_num: u32,
    pub fps_den: u32,
}

unsafe impl Send for VirtualCameraScene {}

/// Frames skipped so far by the canvas video an encoder reads from, because encoders were too
/// slow (0 without an encoder).
fn skipped_of(encoder: *mut sys::obs_encoder_t) -> u32 {
    if encoder.is_null() {
        return 0;
    }
    unsafe {
        let video = sys::obs_encoder_video(encoder);
        if video.is_null() { 0 } else { sys::video_output_get_skipped_frames(video) }
    }
}

/// The delay a stream is holding its data back by, seconds (0: none).
fn obs_delay(output: *mut sys::obs_output_t) -> u32 {
    unsafe { sys::obs_output_get_active_delay(output) }
}

/// The bitrate an encoder is set to now, kbps (dynamic bitrate changes it while live).
fn encoder_kbps(encoder: *mut sys::obs_encoder_t) -> u32 {
    if encoder.is_null() {
        return 0;
    }
    unsafe {
        let settings = sys::obs_encoder_get_settings(encoder);
        if settings.is_null() {
            return 0;
        }
        let kbps = sys::obs_data_get_int(settings, c"bitrate".as_ptr()).max(0) as u32;
        sys::obs_data_release(settings);
        kbps
    }
}

/// Frames the primary canvas skipped because an encoder was too slow, since the app began
/// (the other canvases' are in each output's status).
pub fn primary_skipped_frames() -> u32 {
    unsafe {
        if !sys::obs_initialized() {
            return 0;
        }
        let video = primary_video();
        if video.is_null() { 0 } else { sys::video_output_get_skipped_frames(video) }
    }
}

fn c(s: &str) -> CString {
    CString::new(s).unwrap_or_default()
}

unsafe fn create_encoders(
    name: &str,
    cfg: &EncoderConfig,
    video_output: *mut sys::video_t,
) -> Result<(*mut sys::obs_encoder_t, *mut sys::obs_encoder_t), String> {
    let mut v = Data::new();
    let prores = cfg.video_encoder.contains("prores");
    let hevc = cfg.video_encoder.contains("hevc");
    v.set_int("keyint_sec", cfg.keyint_sec.max(1) as i64);
    let apple = cfg.video_encoder.contains("videotoolbox");
    if !prores {
        // VideoToolbox and x264 average over the file; the Windows encoders call it VBR
        let variable = if apple || cfg.video_encoder == "obs_x264" { "ABR" } else { "VBR" };
        v.set_int("bitrate", cfg.video_bitrate_kbps as i64)
            .set_str("rate_control", if cfg.constant_bitrate { "CBR" } else { variable })
            .set_str("profile", if hevc { "main" } else { "high" });
    }
    if cfg.video_encoder == "obs_x264" {
        v.set_str("preset", "veryfast");
        if !cfg.bframes {
            v.set_str("x264opts", "bframes=0");
        }
    } else if apple {
        v.set_bool("bframes", cfg.bframes);
    } else if cfg.video_encoder.contains("qsv") {
        v.set_int("bframes", if cfg.bframes { 3 } else { 0 });
    } else {
        // NVENC and AMF: B-frames between references
        v.set_int("bf", if cfg.bframes { 2 } else { 0 });
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
            cfg.audio_mix,
            std::ptr::null_mut(),
        );
        if audio.is_null() {
            sys::obs_encoder_release(video);
            return Err(format!("Couldn't create the audio encoder ({}).", cfg.audio_encoder));
        }
        // The canvas's canvas (main canvas when null), scaled when asked.
        sys::obs_encoder_set_video(video, if video_output.is_null() { primary_video() } else { video_output });
        if let Some((w, h)) = cfg.scaled {
            sys::obs_encoder_set_scaled_size(video, w & !1, h & !1);
        }
        // a destination's own frame rate: every Nth canvas frame
        if cfg.fps_divisor > 1 {
            sys::obs_encoder_set_frame_rate_divisor(video, cfg.fps_divisor);
        }
        sys::obs_encoder_set_audio(audio, sys::obs_get_audio());
        Ok((video, audio))
    }
}

#[allow(clippy::too_many_arguments)] // each output kind sets a different few
unsafe fn start(
    key: OutputKey,
    output_type: &str,
    output_settings: &Data,
    encoders: &EncoderConfig,
    service: *mut sys::obs_service_t,
    video_output: *mut sys::video_t,
    shared: Option<(*mut sys::obs_encoder_t, *mut sys::obs_encoder_t)>,
    extra_mixes: &[usize],
    delay: Option<(u32, bool)>,
) -> Result<Running, String> {
    let name = match key {
        OutputKey::Recording(canvas) => format!("Recording {canvas}"),
        OutputKey::Stream(id) => format!("Stream {id}"),
        OutputKey::Clips(canvas) => format!("Clips {canvas}"),
        OutputKey::VirtualCamera => "Virtual camera".to_string(),
    };
    unsafe {
        let release_service = || {
            if !service.is_null() {
                sys::obs_service_release(service);
            }
        };
        // `shared` carries strong references to another output's encoders.
        let made = match shared {
            Some(pair) => Ok(pair),
            None => create_encoders(&name, encoders, video_output),
        };
        let (video, audio) = match made {
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
        // more audio tracks: one encoder per libobs mix, in order after the program
        let mut extra = Vec::new();
        for (track, &mix) in extra_mixes.iter().enumerate().take(5) {
            let mut a = Data::new();
            a.set_int("bitrate", encoders.audio_bitrate_kbps as i64);
            let enc = sys::obs_audio_encoder_create(
                c(&encoders.audio_encoder).as_ptr(),
                c(&format!("{name} audio {}", track + 2)).as_ptr(),
                a.as_ptr(),
                mix,
                std::ptr::null_mut(),
            );
            if enc.is_null() {
                continue;
            }
            sys::obs_encoder_set_audio(enc, sys::obs_get_audio());
            sys::obs_output_set_audio_encoder(output, enc, track + 1);
            extra.push(enc);
        }
        if !service.is_null() {
            sys::obs_output_set_service(output, service);
        }
        // held back before it goes out (must be set before the output starts)
        if let Some((seconds, preserve)) = delay.filter(|(s, _)| *s > 0) {
            sys::obs_output_set_delay(output, seconds, if preserve { sys::OBS_OUTPUT_DELAY_PRESERVE } else { 0 });
        }
        let signal_param = Box::into_raw(Box::new(key));
        let handler = sys::obs_output_get_signal_handler(output);
        let signals = signals_for(key);
        for (sig, cb) in signals {
            sys::signal_handler_connect(handler, c(sig).as_ptr(), Some(*cb), signal_param as *mut c_void);
        }
        let share = (video_output as usize, encoders.clone());
        let skipped_base = skipped_of(video);
        let adaptive = encoders.adaptive;
        let running =
            Running { output, video, audio, service, signal_param, signals, share, extra, skipped_base, adaptive, group: std::ptr::null_mut() };
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
    /// Starts recording one canvas. `video` is that canvas's canvas video
    /// (`Reconciler::canvas_video`); `None` means the primary canvas. Several canvases can
    /// record at once, each its own output and file.
    pub fn start_recording(&mut self, cfg: &RecordingConfig, video: Option<VideoOutput>) -> Result<(), String> {
        // Lightair's own encoders and file writer, from the canvas's compositor
        #[cfg(target_os = "macos")]
        if let Some(canvas) = crate::own_outputs::takes(video) {
            return crate::own_outputs::start_recording(cfg, canvas);
        }
        let video_output = video.map_or(std::ptr::null_mut(), |v| v.0);
        let key = OutputKey::Recording(cfg.canvas);
        if self.recordings.get(&cfg.canvas).is_some_and(|r| unsafe { sys::obs_output_active(r.output) }) {
            return Ok(());
        }
        if let Some(old) = self.recordings.remove(&cfg.canvas) {
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
        // Splits on request, or every N minutes; later files go next to the first one.
        settings.set_bool("split_file", true);
        settings.set_int("max_time_sec", cfg.split_minutes.map_or(0, |m| m as i64 * 60));
        settings.set_int("max_size_mb", 0);
        if let Some(dir) = cfg.path.parent() {
            settings.set_str("directory", &dir.to_string_lossy());
        }
        let pattern = match &cfg.label {
            Some(label) => format!("Lightair %CCYY-%MM-%DD %hh-%mm-%ss {label}"),
            None => "Lightair %CCYY-%MM-%DD %hh-%mm-%ss".to_string(),
        };
        settings.set_str("format", &pattern);
        settings.set_str("extension", cfg.container.extension());
        settings.set_bool("allow_spaces", true);
        let running = unsafe {
            start(
                key,
                output_type,
                &settings,
                &cfg.encoders,
                std::ptr::null_mut(),
                video_output,
                None,
                &cfg.extra_mixes,
                None,
            )?
        };
        runtime().update(key, |s| {
            s.path = Some(cfg.path.clone());
            s.files = 1;
        });
        self.recordings.insert(cfg.canvas, running);
        Ok(())
    }

    /// Stops every recording.
    pub fn stop_recording(&mut self) {
        #[cfg(target_os = "macos")]
        crate::own_outputs::stop_recordings();
        for (canvas, r) in &self.recordings {
            runtime().update(OutputKey::Recording(*canvas), |s| {
                if s.state.is_running() {
                    s.state = OutputState::Stopping
                }
            });
            unsafe { sys::obs_output_stop(r.output) };
        }
    }

    /// The running recordings: canvas and output.
    fn active_recordings(&self) -> Vec<(Id, *mut sys::obs_output_t)> {
        self.recordings
            .iter()
            .map(|(c, r)| (*c, r.output))
            .filter(|(_, o)| unsafe { sys::obs_output_active(*o) })
            .collect()
    }

    /// Pauses or resumes every recording. Returns whether they are all now in that state.
    pub fn pause_recording(&mut self, pause: bool) -> bool {
        let active = self.active_recordings();
        #[cfg(target_os = "macos")]
        let own = crate::own_outputs::pause_recordings(pause);
        #[cfg(not(target_os = "macos"))]
        let own: Option<bool> = None;
        if active.is_empty() {
            return own.unwrap_or(false);
        }
        let mut all = own.unwrap_or(true);
        // libobs applies a pause at the next video frame; resuming before it has leaves the
        // encoder waiting forever (and the recording can't stop). Give a fresh pause time.
        if !pause {
            let newest = active
                .iter()
                .filter_map(|(c, _)| runtime().get(OutputKey::Recording(*c)).paused_since)
                .map(|t| t.elapsed())
                .min();
            if let Some(age) = newest.filter(|a| *a < Duration::from_millis(150)) {
                std::thread::sleep(Duration::from_millis(150) - age);
            }
        }
        for (canvas, output) in active {
            let ok = unsafe { sys::obs_output_pause(output, pause) };
            let paused = unsafe { sys::obs_output_paused(output) };
            runtime().update(OutputKey::Recording(canvas), |s| {
                match (s.paused, paused) {
                    (false, true) => s.paused_since = Some(Instant::now()),
                    (true, false) => s.paused_for += s.paused_since.take().map_or_default(|t| t.elapsed()),
                    _ => {}
                }
                s.paused = paused;
            });
            all &= ok && paused == pause;
        }
        all
    }

    /// Ends every recording's current file and continues in a new one, without a gap.
    pub fn split_recording(&mut self) -> bool {
        let mut any = false;
        #[cfg(target_os = "macos")]
        {
            any |= crate::own_outputs::split_recordings();
        }
        for (_, output) in self.active_recordings() {
            any |= unsafe { split_file(output) };
        }
        any
    }

    /// Adds a marker (an MP4/MOV chapter) at the current time to every recording that can hold
    /// one. Returns false when none can (MKV) or none is running.
    pub fn add_marker(&mut self, name: &str) -> bool {
        let mut any = false;
        #[cfg(target_os = "macos")]
        {
            any |= crate::own_outputs::add_marker(name);
        }
        for (canvas, output) in self.active_recordings() {
            if unsafe { add_chapter(output, name) } {
                runtime().update(OutputKey::Recording(canvas), |s| s.markers += 1);
                any = true;
            }
        }
        any
    }

    pub fn start_stream(&mut self, cfg: &StreamConfig, video: Option<VideoOutput>) -> Result<(), String> {
        let video_output = video.map_or(std::ptr::null_mut(), |v| v.0);
        let key = OutputKey::Stream(cfg.id);
        if self.streams.get(&cfg.id).is_some_and(|r| unsafe { sys::obs_output_active(r.output) }) {
            return Ok(());
        }
        if let Some(old) = self.streams.remove(&cfg.id) {
            unsafe { old.release() };
        }
        let protocol = StreamProtocol::of(&cfg.server)
            .ok_or_else(|| "Use an rtmp://, rtmps://, srt://, rist:// or https:// (WHIP) address.".to_string())?;
        // Lightair's own encoders and senders, from the canvas's compositor, where they can
        // reach the destination (others stay libobs's)
        #[cfg(target_os = "macos")]
        if crate::own_outputs::can_stream(cfg)
            && let Some(canvas) = crate::own_outputs::takes(video)
        {
            return crate::own_outputs::start_stream(cfg, canvas);
        }
        let (service_type, output_type) = match protocol {
            StreamProtocol::Rtmp => ("rtmp_custom", "rtmp_output"),
            StreamProtocol::MpegTs => ("rtmp_custom", "ffmpeg_mpegts_muxer"),
            StreamProtocol::Whip => ("whip_custom", "whip_output"),
        };
        // (WebRTC carries Opus audio, and B-frames aren't allowed.)
        let encoders = stream_encoders(&cfg.server, &cfg.encoders);
        let mut service_settings = Data::new();
        service_settings.set_str("server", &cfg.server);
        match protocol {
            StreamProtocol::Whip => service_settings.set_str("bearer_token", &cfg.key),
            _ => service_settings.set_str("key", &cfg.key),
        };
        let service = unsafe {
            sys::obs_service_create(
                c(service_type).as_ptr(),
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
        // RTMP's dynamic bitrate: the output lowers its encoder's bitrate when the connection
        // backs up and raises it again. The other protocols manage congestion themselves.
        output_settings.set_bool("dyn_bitrate", encoders.adaptive && protocol == StreamProtocol::Rtmp);
        // which way out (RTMP's own options; the other protocols don't have them yet)
        if protocol == StreamProtocol::Rtmp {
            if let Some(ip) = &cfg.net.bind_ip {
                output_settings.set_str("bind_ip", ip);
            }
            output_settings.set_str(
                "ip_family",
                match cfg.net.ip_family {
                    lightair_model::IpFamily::Auto => "IPv4+IPv6",
                    lightair_model::IpFamily::V4 => "IPv4",
                    lightair_model::IpFamily::V6 => "IPv6",
                },
            );
        }
        // On failure `start` releases everything it was given, the service included.
        // Destinations with identical encoder settings on the same canvas share one encoder pair.
        // (An adaptive destination's encoder is its own: its output changes the bitrate.)
        let share_key = (video_output as usize, encoders.clone());
        let shared = (!encoders.adaptive)
            .then(|| self.streams.values().find(|r| r.share == share_key))
            .flatten()
            .map(|r| unsafe { (sys::obs_encoder_get_ref(r.video), sys::obs_encoder_get_ref(r.audio)) });
        let shared = shared.filter(|(v, a)| !v.is_null() && !a.is_null());
        // Twitch's VOD track: a second audio track, as OBS sends it (RTMP's multitrack audio)
        let vod: Vec<usize> = cfg.vod_mix.filter(|_| protocol == StreamProtocol::Rtmp).into_iter().collect();
        let running = unsafe {
            start(key, output_type, &output_settings, &encoders, service, video_output, shared, &vod, cfg.net.delay)?
        };
        if let Some((retries, delay)) = cfg.reconnect {
            unsafe { sys::obs_output_set_reconnect_settings(running.output, retries as i32, delay as i32) };
            runtime().update(key, |s| s.retries = retries);
        }
        self.streams.insert(cfg.id, running);
        Ok(())
    }

    /// Starts a stream as several encodes in one RTMP connection (Enhanced Broadcasting), as
    /// `plan` says: `videos[i]` is the canvas video track *i* is made from. Its encoders are its
    /// own (shared with no other destination) and start together, so their keyframes line up.
    /// `cfg.server` and `cfg.key` are the plan's ingest address and key.
    pub fn start_multitrack_stream(&mut self, cfg: &StreamConfig, plan: &Multitrack, videos: &[VideoOutput]) -> Result<(), String> {
        let key = OutputKey::Stream(cfg.id);
        if self.streams.get(&cfg.id).is_some_and(|r| unsafe { sys::obs_output_active(r.output) }) {
            return Ok(());
        }
        if let Some(old) = self.streams.remove(&cfg.id) {
            unsafe { old.release() };
        }
        if StreamProtocol::of(&cfg.server) != Some(StreamProtocol::Rtmp) {
            return Err("Enhanced Broadcasting goes over RTMP or RTMPS.".into());
        }
        if plan.video.is_empty() || plan.audio.is_empty() || videos.len() != plan.video.len() {
            return Err("The platform's plan has no video or audio to send.".into());
        }
        let name = format!("Stream {}", cfg.id);
        unsafe {
            let mut service_settings = Data::new();
            service_settings.set_str("server", &cfg.server).set_str("key", &cfg.key);
            let service = sys::obs_service_create(
                c("rtmp_custom").as_ptr(),
                c(&format!("Service {}", cfg.id)).as_ptr(),
                service_settings.as_ptr(),
                std::ptr::null_mut(),
            );
            if service.is_null() {
                return Err("Couldn't set up the streaming service.".into());
            }
            let mut output_settings = Data::new();
            output_settings.set_bool("low_latency_mode_enabled", false);
            output_settings.set_bool("dyn_bitrate", cfg.encoders.adaptive);
            if let Some(table) = plan.bitrate_table.as_deref().filter(|_| cfg.encoders.adaptive) {
                output_settings.set_str("interpolation_table_data", table);
            }
            if let Some(ip) = &cfg.net.bind_ip {
                output_settings.set_str("bind_ip", ip);
            }
            output_settings.set_str(
                "ip_family",
                match cfg.net.ip_family {
                    lightair_model::IpFamily::Auto => "IPv4+IPv6",
                    lightair_model::IpFamily::V4 => "IPv4",
                    lightair_model::IpFamily::V6 => "IPv6",
                },
            );
            let output = sys::obs_output_create(
                c("rtmp_output").as_ptr(),
                c(&name).as_ptr(),
                output_settings.as_ptr(),
                std::ptr::null_mut(),
            );
            if output.is_null() {
                sys::obs_service_release(service);
                return Err("Couldn't create the RTMP output.".into());
            }
            let group = sys::obs_encoder_group_create();
            // what's made so far, released together if a step fails
            let mut made: Vec<*mut sys::obs_encoder_t> = Vec::new();
            let fail = |made: &[*mut sys::obs_encoder_t], why: String| -> Result<(), String> {
                sys::obs_output_release(output);
                for e in made {
                    sys::obs_encoder_release(*e);
                }
                if !group.is_null() {
                    sys::obs_encoder_group_destroy(group);
                }
                sys::obs_service_release(service);
                Err(why)
            };
            for (i, (t, video)) in plan.video.iter().zip(videos).enumerate() {
                let mut settings = t.settings.clone();
                if let Some(o) = settings.as_object_mut() {
                    // keyframes only on the interval, so every track's line up
                    o.entry("disable_scenecut").or_insert(serde_json::Value::Bool(true));
                }
                let settings = Data::from_json(&settings);
                let enc = sys::obs_video_encoder_create(
                    c(&t.encoder).as_ptr(),
                    c(&format!("{name} video {}", i + 1)).as_ptr(),
                    settings.as_ptr(),
                    std::ptr::null_mut(),
                );
                if enc.is_null() {
                    return fail(&made, format!("Couldn't create video track {} ({}).", i + 1, t.encoder)).map(|_| ());
                }
                made.push(enc);
                sys::obs_encoder_set_video(enc, video.0);
                sys::obs_encoder_set_scaled_size(enc, t.width & !1, t.height & !1);
                sys::obs_encoder_set_gpu_scale_type(enc, scale_type(t.scale.as_deref()));
                if t.fps_divisor > 1 {
                    sys::obs_encoder_set_frame_rate_divisor(enc, t.fps_divisor);
                }
                if !group.is_null() && !sys::obs_encoder_set_group(enc, group) {
                    return fail(&made, format!("Couldn't keep video track {} with the others.", i + 1));
                }
                sys::obs_output_set_video_encoder2(output, enc, i);
            }
            let videos_made = made.len();
            // audio: the live tracks on the stream's mix, then the VOD track's on its own
            let tracks = plan.audio.iter().map(|a| (a, cfg.encoders.audio_mix)).chain(
                cfg.vod_mix.into_iter().flat_map(|mix| plan.vod_audio.iter().map(move |a| (a, mix))),
            );
            for (j, (a, mix)) in tracks.enumerate().take(6) {
                let settings = Data::from_json(a);
                let enc = sys::obs_audio_encoder_create(
                    c(&cfg.encoders.audio_encoder).as_ptr(),
                    c(&format!("{name} audio {}", j + 1)).as_ptr(),
                    settings.as_ptr(),
                    mix,
                    std::ptr::null_mut(),
                );
                if enc.is_null() {
                    return fail(&made, format!("Couldn't create audio track {} ({}).", j + 1, cfg.encoders.audio_encoder));
                }
                sys::obs_encoder_set_audio(enc, sys::obs_get_audio());
                sys::obs_output_set_audio_encoder(output, enc, j);
                made.push(enc);
            }
            sys::obs_output_set_service(output, service);
            if let Some((seconds, preserve)) = cfg.net.delay.filter(|(s, _)| *s > 0) {
                sys::obs_output_set_delay(output, seconds, if preserve { sys::OBS_OUTPUT_DELAY_PRESERVE } else { 0 });
            }
            let signal_param = Box::into_raw(Box::new(key));
            let handler = sys::obs_output_get_signal_handler(output);
            let signals = signals_for(key);
            for (sig, cb) in signals {
                sys::signal_handler_connect(handler, c(sig).as_ptr(), Some(*cb), signal_param as *mut c_void);
            }
            let (video, audio) = (made[0], made[videos_made]);
            let extra: Vec<_> = made.iter().enumerate().filter(|(k, _)| *k != 0 && *k != videos_made).map(|(_, e)| *e).collect();
            let running = Running {
                output,
                video,
                audio,
                service,
                signal_param,
                signals,
                // shares with nothing: no canvas video is at this address
                share: (output as usize, cfg.encoders.clone()),
                extra,
                skipped_base: skipped_of(video),
                adaptive: cfg.encoders.adaptive,
                group,
            };
            if let Some((retries, delay)) = cfg.reconnect {
                sys::obs_output_set_reconnect_settings(output, retries as i32, delay as i32);
                runtime().update(key, |s| s.retries = retries);
            }
            runtime().update(key, |s| *s = OutputStatus { state: OutputState::Starting, ..Default::default() });
            if !sys::obs_output_start(output) {
                let err = last_error(output).unwrap_or_else(|| "The stream couldn't start.".into());
                running.release();
                runtime().update(key, |s| s.state = OutputState::Failed(err.clone()));
                return Err(err);
            }
            self.streams.insert(cfg.id, running);
        }
        Ok(())
    }

    /// Which running streams share an encoder: one list of destinations per video encoder.
    pub fn stream_share_groups(&self) -> Vec<Vec<Id>> {
        #[cfg(target_os = "macos")]
        let own = crate::own_outputs::share_groups();
        #[cfg(not(target_os = "macos"))]
        let own: Vec<Vec<Id>> = Vec::new();
        let mut groups: Vec<(usize, Vec<Id>)> = Vec::new();
        for (id, r) in &self.streams {
            if !runtime().get(OutputKey::Stream(*id)).state.is_running() {
                continue;
            }
            match groups.iter_mut().find(|(enc, _)| *enc == r.video as usize) {
                Some((_, ids)) => ids.push(*id),
                None => groups.push((r.video as usize, vec![*id])),
            }
        }
        groups.into_iter().map(|(_, ids)| ids).chain(own).collect()
    }

    /// How many distinct video encoders the streams use (for the UI's "N encoders" note).
    pub fn stream_encoder_count(&self) -> usize {
        let mut seen: Vec<usize> = self.streams.values().map(|r| r.video as usize).collect();
        seen.sort_unstable();
        seen.dedup();
        #[cfg(target_os = "macos")]
        return seen.len() + crate::own_outputs::stream_encoder_count();
        #[cfg(not(target_os = "macos"))]
        seen.len()
    }

    /// The video encoder a stream uses (diagnostics and tests).
    /// A stream's audio tracks, in order: the libobs mix each encodes (diagnostics, tests).
    pub fn stream_audio_tracks(&self, id: Id) -> Vec<usize> {
        #[cfg(target_os = "macos")]
        if let Some(tracks) = crate::own_outputs::stream_audio_tracks(id) {
            return tracks;
        }
        let Some(r) = self.streams.get(&id) else { return Vec::new() };
        (0..6)
            .map_while(|k| {
                let enc = unsafe { sys::obs_output_get_audio_encoder(r.output, k) };
                (!enc.is_null()).then(|| unsafe { sys::obs_encoder_get_mixer_index(enc) })
            })
            .collect()
    }

    pub fn stream_video_encoder(&self, id: Id) -> Option<usize> {
        #[cfg(target_os = "macos")]
        if let Some(enc) = crate::own_outputs::stream_video_encoder(id) {
            return Some(enc);
        }
        self.streams.get(&id).map(|r| r.video as usize)
    }

    pub fn stop_stream(&mut self, id: Id) {
        #[cfg(target_os = "macos")]
        crate::own_outputs::stop_stream(id, false);
        if let Some(r) = self.streams.get(&id) {
            runtime().update(OutputKey::Stream(id), |s| {
                if s.state.is_running() && s.state != OutputState::Stopping {
                    s.state = OutputState::Stopping;
                    s.stopping_since = Some(Instant::now());
                }
            });
            unsafe { sys::obs_output_stop(r.output) };
        }
    }

    /// Ends a stream at once, without letting its delay drain (the viewers lose what was still
    /// held back).
    pub fn force_stop_stream(&mut self, id: Id) {
        #[cfg(target_os = "macos")]
        crate::own_outputs::stop_stream(id, true);
        if let Some(r) = self.streams.get(&id) {
            unsafe { sys::obs_output_force_stop(r.output) };
        }
    }

    pub fn stop_all_streams(&mut self) {
        #[cfg(target_os = "macos")]
        crate::own_outputs::stop_all_streams();
        let ids: Vec<Id> = self.streams.keys().copied().collect();
        for id in ids {
            self.stop_stream(id);
        }
    }

    /// Starts the virtual camera: what a canvas has on air (`video`: its canvas video; `None` the
    /// primary), or one scene (`scene`: on a canvas of the camera's own). Raw frames, no
    /// encoders. Its status is `OutputKey::VirtualCamera`'s.
    pub fn start_virtual_camera(
        &mut self,
        video: Option<VideoOutput>,
        scene: Option<VirtualCameraScene>,
    ) -> Result<(), String> {
        if self.virtual_camera.as_ref().is_some_and(|r| unsafe { sys::obs_output_active(r.output) }) {
            return Ok(());
        }
        self.drop_virtual_camera();
        let video = match scene {
            Some(sc) => {
                let canvas =
                    crate::obs::ObsCanvas::create("Virtual camera", sc.width, sc.height, sc.fps_num, sc.fps_den)
                        .ok_or_else(|| "Couldn't make the virtual camera's canvas.".to_string())?;
                unsafe { sys::obs_canvas_set_channel(canvas.as_ptr(), 0, sc.source) };
                let v = VideoOutput(unsafe { sys::obs_canvas_get_video(canvas.as_ptr()) });
                self.virtual_camera_canvas = Some(canvas);
                Some(v)
            }
            None => video,
        };
        let key = OutputKey::VirtualCamera;
        unsafe {
            // Lightair's camera (macOS: the camera extension, lightair-effect-stack's output;
            // Windows 11: lightair_camera.dll, crate::vcam's output), else OBS's DirectShow camera
            // ("OBS Virtual Camera"), registered only where it's installed
            let id = match crate::vcam::camera() {
                Some(crate::vcam::Camera::Obs) => c"virtualcam_output",
                _ => c"lightair_virtualcam",
            };
            let output = sys::obs_output_create(
                id.as_ptr(),
                c"Virtual camera".as_ptr(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            );
            if output.is_null() {
                let err = if cfg!(windows) {
                    crate::vcam::NOT_INSTALLED.to_string()
                } else {
                    "The virtual camera isn't available on this system.".to_string()
                };
                runtime().update(key, |s| s.state = OutputState::Failed(err.clone()));
                return Err(err);
            }
            let video_output = video.map_or_else(primary_video, |v| v.0);
            sys::obs_output_set_media(output, video_output, sys::obs_get_audio());
            let signal_param = Box::into_raw(Box::new(key));
            let handler = sys::obs_output_get_signal_handler(output);
            let signals = signals_for(key);
            for (sig, cb) in signals {
                sys::signal_handler_connect(handler, c(sig).as_ptr(), Some(*cb), signal_param as *mut c_void);
            }
            let running = Running {
                output,
                video: std::ptr::null_mut(),
                audio: std::ptr::null_mut(),
                service: std::ptr::null_mut(),
                signal_param,
                signals,
                share: (
                    video_output as usize,
                    EncoderConfig {
                        video_encoder: String::new(),
                        video_bitrate_kbps: 0,
                        audio_encoder: String::new(),
                        audio_bitrate_kbps: 0,
                        constant_bitrate: false,
                        bframes: false,
                        keyint_sec: 0,
                        scaled: None,
                        audio_mix: 0,
                        adaptive: false,
                        fps_divisor: 1,
                    },
                ),
                extra: Vec::new(),
                skipped_base: 0,
                adaptive: false,
                group: std::ptr::null_mut(),
            };
            runtime().update(key, |s| *s = OutputStatus { state: OutputState::Starting, ..Default::default() });
            if !sys::obs_output_start(output) {
                let err = last_error(output).unwrap_or_else(|| "The virtual camera didn't start.".into());
                running.release();
                self.virtual_camera_canvas = None;
                runtime().update(key, |s| s.state = OutputState::Failed(err.clone()));
                return Err(err);
            }
            self.virtual_camera = Some(running);
        }
        Ok(())
    }

    pub fn stop_virtual_camera(&mut self) {
        if let Some(r) = &self.virtual_camera {
            runtime().update(OutputKey::VirtualCamera, |s| {
                if s.state.is_running() {
                    s.state = OutputState::Stopping
                }
            });
            unsafe { sys::obs_output_stop(r.output) };
        }
        // (a raw output stops at once: its canvas can go)
        self.virtual_camera_canvas = None;
    }

    fn drop_virtual_camera(&mut self) {
        if let Some(old) = self.virtual_camera.take() {
            unsafe { old.release() };
        }
        self.virtual_camera_canvas = None;
    }

    /// Starts the replay buffer of one canvas (docs/11-outputs-streaming-recording.md, "Clips"):
    /// `video` is that canvas's video (`None`: the primary). Several canvases keep a buffer each.
    pub fn start_clips(&mut self, cfg: &ClipsConfig, video: Option<VideoOutput>) -> Result<(), String> {
        // Lightair's own encoders and packet ring, from the canvas's compositor
        #[cfg(target_os = "macos")]
        if let Some(canvas) = crate::own_outputs::takes(video) {
            return crate::own_outputs::start_clips(cfg, canvas);
        }
        let video_output = video.map_or(std::ptr::null_mut(), |v| v.0);
        let key = OutputKey::Clips(cfg.canvas);
        if self.clips.get(&cfg.canvas).is_some_and(|r| unsafe { sys::obs_output_active(r.output) }) {
            return Ok(());
        }
        self.drop_clips(cfg.canvas);
        std::fs::create_dir_all(&cfg.folder).map_err(|e| format!("Can't create the clips folder: {e}"))?;
        let pattern = match &cfg.label {
            Some(label) => format!("Clip %CCYY-%MM-%DD %hh-%mm-%ss {label}"),
            None => "Clip %CCYY-%MM-%DD %hh-%mm-%ss".to_string(),
        };
        let mut settings = Data::new();
        settings
            .set_str("directory", &cfg.folder.to_string_lossy())
            .set_str("format", &pattern)
            .set_str("extension", "mp4")
            .set_int("max_time_sec", cfg.seconds as i64)
            .set_int("max_size_mb", 0);
        let running = unsafe {
            start(key, "replay_buffer", &settings, &cfg.encoders, std::ptr::null_mut(), video_output, None, &[], None)?
        };
        CLIPS_OUTPUT.lock().insert(cfg.canvas, OutputPtr(running.output));
        self.clips.insert(cfg.canvas, running);
        Ok(())
    }

    /// Stops every replay buffer.
    pub fn stop_clips(&mut self) {
        #[cfg(target_os = "macos")]
        crate::own_outputs::stop_clips(None);
        let canvases: Vec<Id> = self.clips.keys().copied().collect();
        for canvas in canvases {
            self.stop_clips_of(canvas);
        }
    }

    /// Stops one canvas's replay buffer.
    pub fn stop_clips_of(&mut self, canvas: Id) {
        #[cfg(target_os = "macos")]
        crate::own_outputs::stop_clips(Some(canvas));
        if let Some(r) = self.clips.get(&canvas) {
            runtime().update(OutputKey::Clips(canvas), |s| {
                if s.state.is_running() {
                    s.state = OutputState::Stopping
                }
            });
            unsafe { sys::obs_output_stop(r.output) };
        }
    }

    /// The canvases whose replay buffer exists (running or stopped, not yet dropped).
    pub fn clips_canvases(&self) -> Vec<Id> {
        #[allow(unused_mut)]
        let mut canvases: Vec<Id> = self.clips.keys().copied().collect();
        #[cfg(target_os = "macos")]
        canvases.extend(crate::own_outputs::clips_canvases());
        canvases
    }

    fn drop_clips(&mut self, canvas: Id) {
        CLIPS_OUTPUT.lock().remove(&canvas);
        if let Some(old) = self.clips.remove(&canvas) {
            unsafe { old.release() };
        }
    }

    /// Saves the last N seconds of every running buffer as a clip each; the "saved" signals
    /// report the files. How many buffers accepted the save.
    pub fn save_clip(&mut self) -> usize {
        let mut saved = 0;
        #[cfg(target_os = "macos")]
        {
            saved += crate::own_outputs::save_clip();
        }
        for r in self.clips.values() {
            if !unsafe { sys::obs_output_active(r.output) } {
                continue;
            }
            let mut cd = sys::calldata_t::default();
            if unsafe { proc_call(r.output, "save", &mut cd) } {
                saved += 1;
            }
            if !cd.fixed && !cd.stack.is_null() {
                unsafe { sys::bfree(cd.stack as *mut c_void) };
            }
        }
        saved
    }

    /// Updates byte and frame counters for running outputs (called by the stats sampler).
    pub fn sample(&self) {
        #[cfg(target_os = "macos")]
        crate::own_outputs::sample();
        let sample = |key: OutputKey, r: &Running| unsafe {
            let (bytes, dropped, total) = (
                sys::obs_output_get_total_bytes(r.output),
                sys::obs_output_get_frames_dropped(r.output),
                sys::obs_output_get_total_frames(r.output),
            );
            let skipped = skipped_of(r.video).saturating_sub(r.skipped_base);
            let delay = obs_delay(r.output);
            let (current, target) = if r.adaptive { (encoder_kbps(r.video), r.share.1.video_bitrate_kbps) } else { (0, 0) };
            runtime().update(key, |s| {
                s.current_kbps = current;
                s.target_kbps = target;
                s.delay_s = delay;
                s.bytes = bytes;
                s.dropped_frames = dropped;
                s.total_frames = total;
                s.encoder_skipped = skipped;
            });
        };
        for (canvas, r) in &self.recordings {
            sample(OutputKey::Recording(*canvas), r);
        }
        for (id, r) in &self.streams {
            sample(OutputKey::Stream(*id), r);
        }
    }

    /// Stops and releases everything (engine shutdown).
    pub fn shutdown(&mut self) {
        #[cfg(target_os = "macos")]
        crate::own_outputs::shutdown();
        for r in self.clips.values() {
            unsafe { sys::obs_output_stop(r.output) };
        }
        let canvases: Vec<Id> = self.clips.keys().copied().collect();
        for canvas in canvases {
            self.drop_clips(canvas);
        }
        if let Some(r) = &self.virtual_camera {
            unsafe { sys::obs_output_stop(r.output) };
        }
        self.drop_virtual_camera();
        for (_, r) in self.recordings.drain() {
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

/// H.264 encoders, best first: Apple's (macOS); NVIDIA, AMD and Intel hardware (Windows); x264.
pub const H264_ENCODERS: &[&str] = &[
    "com.apple.videotoolbox.videoencoder.ave.avc",
    "com.apple.videotoolbox.videoencoder.h264",
    "obs_nvenc_h264_tex",
    "jim_nvenc",
    "h264_texture_amf",
    "obs_qsv11_v2",
    "obs_x264",
];

/// HEVC encoders, best first.
pub const HEVC_ENCODERS: &[&str] = &[
    "com.apple.videotoolbox.videoencoder.ave.hevc",
    "com.apple.videotoolbox.videoencoder.hevc.vcp",
    "obs_nvenc_hevc_tex",
    "jim_hevc_nvenc",
    "h265_texture_amf",
    "obs_qsv11_hevc",
];

/// ProRes encoders (Apple's; Windows has none in libobs).
pub const PRORES_ENCODERS: &[&str] =
    &["com.apple.videotoolbox.videoencoder.appleproreshw.422", "com.apple.videotoolbox.videoencoder.prores-422"];

/// The first of `ids` that's available.
pub fn first_available(available: &[String], ids: &[&str]) -> Option<String> {
    ids.iter().find(|id| available.iter().any(|e| e == *id)).map(|id| id.to_string())
}

/// The AAC encoder: Apple's on macOS, FFmpeg's elsewhere.
pub fn aac_encoder(available: &[String]) -> String {
    first_available(available, &["CoreAudio_AAC"]).unwrap_or_else(|| "ffmpeg_aac".into())
}

/// Who makes an encoder, for people: "Apple hardware", "NVIDIA NVENC", "x264 (software)".
pub fn encoder_maker(id: &str) -> &'static str {
    if id.contains("videotoolbox") {
        "Apple hardware"
    } else if id.contains("nvenc") {
        "NVIDIA NVENC"
    } else if id.contains("amf") {
        "AMD AMF"
    } else if id.contains("qsv") {
        "Intel Quick Sync"
    } else if id == "obs_x264" {
        "x264 (software)"
    } else {
        "Encoder"
    }
}

/// Picks the best available encoders: hardware H.264 when present.
pub fn default_encoders(available: &[String], video_bitrate_kbps: u32, constant_bitrate: bool) -> EncoderConfig {
    let video = first_available(available, H264_ENCODERS).unwrap_or_else(|| "obs_x264".into());
    let audio = aac_encoder(available);
    EncoderConfig {
        video_encoder: video,
        video_bitrate_kbps,
        audio_encoder: audio,
        audio_bitrate_kbps: 160,
        constant_bitrate,
        bframes: true,
        keyint_sec: 2,
        scaled: None,
        audio_mix: 0,
        adaptive: false,
        fps_divisor: 1,
    }
}

/// Asks a recording to continue in a new file; whether it will.
unsafe fn split_file(output: *mut sys::obs_output_t) -> bool {
    let mut cd = sys::calldata_t::default();
    let mut enabled = false;
    unsafe {
        if proc_call(output, "split_file", &mut cd) {
            // calldata_get_bool is a header inline over calldata_get_data.
            sys::calldata_get_data(
                &cd,
                c"split_file_enabled".as_ptr(),
                &mut enabled as *mut bool as *mut c_void,
                std::mem::size_of::<bool>(),
            );
        }
        if !cd.fixed && !cd.stack.is_null() {
            sys::bfree(cd.stack as *mut c_void);
        }
    }
    enabled
}

/// Adds a chapter to an MP4/MOV recording; false for other containers.
unsafe fn add_chapter(output: *mut sys::obs_output_t, name: &str) -> bool {
    let id = unsafe { CStr::from_ptr(sys::obs_output_get_id(output)) };
    if !matches!(id.to_bytes(), b"mp4_output" | b"mov_output") {
        return false;
    }
    let mut cd = sys::calldata_t::default();
    let name = c(name);
    unsafe {
        // calldata_set_string is a header inline: the string with its terminator.
        let bytes = name.as_bytes_with_nul();
        sys::calldata_set_data(&mut cd, c"chapter_name".as_ptr(), bytes.as_ptr() as *const c_void, bytes.len());
        let ok = proc_call(output, "add_chapter", &mut cd);
        if !cd.fixed && !cd.stack.is_null() {
            sys::bfree(cd.stack as *mut c_void);
        }
        ok
    }
}
