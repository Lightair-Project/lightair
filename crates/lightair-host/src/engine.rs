//! The engine: a thread of its own owning the document host and the outputs ([`EngineState`]),
//! which the app hands work to ([`Engine::post`], [`Engine::call`]).
//!
//! Nothing here draws: Lightair's own compositor draws each canvas from what the host publishes
//! (`crate::show`), the mixer plays the sound (`crate::audio_host`), and Lightair's own encoders
//! make the recordings and streams (`crate::own_outputs`).

use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crossbeam_channel::{RecvTimeoutError, Sender, bounded, unbounded};

/// How the engine is started (the canvases, their sizes and rates come from the document).
#[derive(Debug, Clone)]
pub struct EngineConfig {
    /// The mixer's sample rate.
    pub sample_rate: u32,
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self { sample_rate: 48_000 }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    #[error("the engine thread didn't start: {0}")]
    Thread(String),
    #[error("the engine thread stopped")]
    Stopped,
}

/// What this engine can make: the encoder ids Lightair's own encoders answer to
/// (`crate::own_outputs`).
#[derive(Debug, Clone, Default)]
pub struct Capabilities {
    pub encoders: Vec<String>,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct Stats {
    pub active_fps: f64,
    pub average_frame_time_ms: f64,
    pub total_frames: u32,
    pub lagged_frames: u32,
}

pub use crate::studio_take::StudioTake;

/// State owned by the engine thread. Jobs receive `&mut EngineState`.
#[derive(Default)]
pub struct EngineState {
    /// The document as it runs: the mixer and each source's runtime.
    pub reconciler: crate::host::Host,
    /// Running recordings, streams, replay buffers and the virtual camera.
    pub outputs: crate::outputs_host::Outputs,
    /// A Studio take's per-source recordings, until they've stopped (never made yet).
    pub studio_take: Option<StudioTake>,
}

/// A piece of work for the engine thread, and where it was asked for (named when it's slow).
type Job = (Box<dyn FnOnce(&mut EngineState) + Send>, &'static std::panic::Location<'static>);

/// A job (or a wait for one) longer than this is logged with where it came from: the UI waits
/// on the engine's jobs, so a slow one is a frozen window.
const SLOW_MS: u128 = 60;

/// How often the host's runtimes are looked after (timers, web pages, switches ending) when no
/// job comes.
const TICK: Duration = Duration::from_millis(100);

/// The engine thread. Dropping it stops the outputs and the host on that thread.
pub struct Engine {
    tx: Option<Sender<Job>>,
    thread: Option<JoinHandle<()>>,
    pub capabilities: Capabilities,
    pub version: String,
}

impl Engine {
    /// Starts the engine thread and waits until it is ready.
    pub fn start(config: EngineConfig) -> Result<Self, EngineError> {
        // Lightair's own compositor, mixer and outputs are all there is
        crate::show::set_own();
        crate::plugins::set_sample_rate(config.sample_rate);
        // the stream mix's loudness, for the meters
        crate::loudness::attach(config.sample_rate, 2);
        let (tx, rx) = unbounded::<Job>();
        let (ready_tx, ready_rx) = bounded::<()>(1);
        let rate = config.sample_rate;
        let thread = std::thread::Builder::new()
            .name("lightair-engine".into())
            .spawn(move || {
                let mut state = EngineState { reconciler: crate::host::Host::new(rate), ..Default::default() };
                let _ = ready_tx.send(());
                let mut ticked = Instant::now();
                loop {
                    match rx.recv_timeout(TICK) {
                        Ok((job, at)) => {
                            let t = Instant::now();
                            job(&mut state);
                            let ms = t.elapsed().as_millis();
                            if ms >= SLOW_MS {
                                tracing::warn!(ms, at = %at, "slow engine job");
                            }
                        }
                        Err(RecvTimeoutError::Timeout) => {}
                        Err(RecvTimeoutError::Disconnected) => break,
                    }
                    if ticked.elapsed() >= TICK {
                        ticked = Instant::now();
                        state.reconciler.tick();
                    }
                }
                shutdown(state);
            })
            .map_err(|e| EngineError::Thread(e.to_string()))?;
        ready_rx.recv().map_err(|_| EngineError::Stopped)?;
        let capabilities = Capabilities { encoders: encoders() };
        let version = env!("CARGO_PKG_VERSION").to_string();
        tracing::info!(
            version,
            encoders = capabilities.encoders.len(),
            sample_rate = config.sample_rate,
            "engine ready"
        );
        Ok(Self { tx: Some(tx), thread: Some(thread), capabilities, version })
    }

    /// Runs `f` on the engine thread without waiting.
    #[track_caller]
    pub fn post(&self, f: impl FnOnce(&mut EngineState) + Send + 'static) {
        if let Some(tx) = &self.tx {
            let _ = tx.send((Box::new(f), std::panic::Location::caller()));
        }
    }

    /// Runs `f` on the engine thread and returns its result (the caller waits: behind whatever
    /// the engine is doing first, too).
    #[track_caller]
    pub fn call<R: Send + 'static>(&self, f: impl FnOnce(&mut EngineState) -> R + Send + 'static) -> R {
        let at = std::panic::Location::caller();
        let t = Instant::now();
        let (tx, rx) = bounded(1);
        if let Some(q) = &self.tx {
            let _ = q.send((
                Box::new(move |s: &mut EngineState| {
                    let _ = tx.send(f(s));
                }),
                at,
            ));
        }
        let r = rx.recv().expect("engine thread stopped");
        let ms = t.elapsed().as_millis();
        if ms >= SLOW_MS {
            tracing::warn!(ms, at = %at, thread = std::thread::current().name().unwrap_or("?"), "waited on the engine");
        }
        r
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        self.tx.take();
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// Stops everything on the engine thread: the outputs first (recordings finish their files),
/// then the host lets every runtime and the mixer go.
fn shutdown(state: EngineState) {
    let EngineState { mut reconciler, mut outputs, studio_take: _ } = state;
    outputs.shutdown();
    reconciler.clear();
    crate::loudness::detach();
    crate::show::clear();
    tracing::info!("engine stopped");
}

/// The encoder ids Lightair's own encoders take (macOS: VideoToolbox's H.264, HEVC and ProRes,
/// Apple's AAC, and Opus for WebRTC), by the ids the app's settings use.
fn encoders() -> Vec<String> {
    #[cfg(target_os = "macos")]
    {
        use crate::outputs::{H264_ENCODERS, HEVC_ENCODERS, PRORES_ENCODERS};
        H264_ENCODERS
            .iter()
            .chain(HEVC_ENCODERS)
            .chain(PRORES_ENCODERS)
            .filter(|id| id.contains("videotoolbox"))
            .copied()
            .chain(["CoreAudio_AAC", "ffmpeg_opus"])
            .map(String::from)
            .collect()
    }
    #[cfg(not(target_os = "macos"))]
    Vec::new()
}

/// Frame statistics: the primary canvas's compositor frames. Any thread.
pub fn read_stats() -> Stats {
    let (active_fps, average_frame_time_ms, total_frames, lagged_frames) = crate::program::frame_stats();
    Stats { active_fps, average_frame_time_ms, total_frames, lagged_frames }
}
