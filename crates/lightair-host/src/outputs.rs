//! Outputs' state and settings (docs/11-outputs-streaming-recording.md): what each output is
//! doing (a process-wide [`OutputsRuntime`] the UI reads; nothing about outputs is in the undo
//! history, it's runtime state, docs/03-architecture.md), what recordings, streams and clips are
//! asked for, and the encoders there are. The outputs themselves run elsewhere.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::OnceLock;
use std::time::Instant;

use lightair_model::Id;
use parking_lot::Mutex;

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
    /// The virtual camera: a canvas as a camera other apps can pick (macOS: through Lightair's
    /// camera extension, by lightair-effect-stack's `lightair_virtualcam` output, or straight
    /// from the compositor with `LIGHTAIR_ENGINE=own`: `own_outputs::start_virtual_camera`).
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

    pub fn update(&self, key: OutputKey, f: impl FnOnce(&mut OutputStatus)) {
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

/// The canvas whose buffer saved a clip last.
pub static LAST_CLIP: Mutex<Option<(Id, std::time::Instant)>> = Mutex::new(None);
