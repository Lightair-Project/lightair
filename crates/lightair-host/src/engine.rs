//! The engine without libobs: a thread of its own owning the document host and the outputs
//! ([`EngineState`]), which the app hands work to ([`Engine::post`], [`Engine::call`]), with the
//! same shape as the libobs engine's (`lightair-obs`'s `Engine`), so the app can run on either.
//!
//! Nothing here draws: Lightair's own compositor draws each canvas from what the host publishes
//! (`crate::show`), the mixer plays the sound (`crate::audio_host`), and Lightair's own encoders
//! make the recordings and streams (`crate::own_outputs`).

use std::path::PathBuf;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crossbeam_channel::{RecvTimeoutError, Sender, bounded, unbounded};

/// How the engine is started. The fields the libobs engine also has are kept so the app sets
/// them the same way; only the sample rate matters here (the canvases come from the document).
#[derive(Debug, Clone)]
pub struct EngineConfig {
    /// The primary canvas's size before the document says otherwise (unused: the compositor
    /// takes each canvas's size from the document).
    pub canvas: (u32, u32),
    /// The size the primary canvas is output at (unused, as `canvas`).
    pub output: (u32, u32),
    /// Frames a second, as a fraction (unused: each canvas has its own rate).
    pub fps: (u32, u32),
    /// The mixer's sample rate.
    pub sample_rate: u32,
    /// Directories of libobs modules (unused: nothing loads modules without libobs).
    pub extra_plugin_dirs: Vec<PathBuf>,
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self { canvas: (1920, 1080), output: (1920, 1080), fps: (60, 1), sample_rate: 48_000, extra_plugin_dirs: Vec::new() }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    #[error("the engine thread didn't start: {0}")]
    Thread(String),
    #[error("the engine thread stopped")]
    Stopped,
}

/// What this engine can make, in the libobs engine's terms. Only `encoders` has content: the
/// encoder ids Lightair's own encoders answer to (`crate::own_outputs`). The other lists are
/// libobs's types, which don't exist here.
#[derive(Debug, Clone, Default)]
pub struct Capabilities {
    pub sources: Vec<String>,
    pub filters: Vec<String>,
    pub transitions: Vec<String>,
    pub encoders: Vec<String>,
    pub outputs: Vec<String>,
    pub services: Vec<String>,
    pub modules: Vec<String>,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct Stats {
    pub active_fps: f64,
    pub average_frame_time_ms: f64,
    pub total_frames: u32,
    pub lagged_frames: u32,
}

/// A Studio take's per-source recordings. Not made without libobs yet: a placeholder so
/// [`EngineState`] has the libobs engine's shape.
#[derive(Debug, Default)]
pub struct StudioTake {
    _private: (),
}

impl StudioTake {
    /// Whether every recording of the take has stopped (there are none).
    pub fn finished(&self) -> bool {
        true
    }
}

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
        let capabilities = Capabilities { encoders: encoders(), ..Default::default() };
        let version = env!("CARGO_PKG_VERSION").to_string();
        tracing::info!(version, encoders = capabilities.encoders.len(), sample_rate = config.sample_rate, "engine ready (no libobs)");
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
    crate::show::clear();
    tracing::info!("engine stopped");
}

/// The encoder ids Lightair's own encoders take (macOS: VideoToolbox's H.264, HEVC and ProRes,
/// Apple's AAC, and Opus for WebRTC), in the libobs ids the app's settings use.
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

/// Frame statistics. Lightair's compositor keeps no engine-wide frame counters yet, so these are
/// zeros (no frames counted, none lagged). Any thread.
pub fn read_stats() -> Stats {
    Stats::default()
}
