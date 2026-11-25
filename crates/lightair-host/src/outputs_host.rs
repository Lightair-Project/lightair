//! Recording, streaming, replay buffers and the virtual camera without libobs: the same calls
//! the app makes on the libobs engine's `Outputs`, taking canvas ids where that took libobs
//! videos (`None`: the primary canvas), all made by Lightair's own encoders and senders from the
//! canvases' compositor frames (`crate::own_outputs`, macOS). Their state goes to
//! [`crate::outputs::runtime`] as before.

use lightair_model::Id;

use crate::outputs::{ClipsConfig, Multitrack, RecordingConfig, StreamConfig};

/// One scene for the virtual camera instead of a canvas's program: the scene, and the canvas it
/// is laid out in (whose compositor draws it for the camera alone).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VirtualCameraScene {
    /// The scene and the canvas it's laid out in (`Host::scene_source`).
    pub source: (Id, Id),
    /// The canvas's size and frame rate (the scene is drawn at the canvas's own).
    pub width: u32,
    pub height: u32,
    pub fps_num: u32,
    pub fps_den: u32,
}

/// Running outputs, owned by the engine thread. They live in `crate::own_outputs`; this is the
/// engine's handle on them.
#[derive(Default)]
pub struct Outputs {
    _private: (),
}

/// The canvas an output takes: `canvas`, or the primary canvas (the document's first) when
/// `None`.
#[cfg(target_os = "macos")]
fn canvas_or_primary(canvas: Option<Id>) -> Result<Id, String> {
    if let Some(c) = canvas {
        return Ok(c);
    }
    crate::show::snapshot()
        .and_then(|s| s.workspace.canvases.first().map(|c| c.id))
        .ok_or_else(|| "There's no canvas to take the picture from yet.".to_string())
}

/// What an output says where Lightair's own outputs don't run yet.
#[cfg(not(target_os = "macos"))]
const NOT_HERE: &str = "Recording and streaming without libobs work on macOS only for now.";

impl Outputs {
    /// Starts recording one canvas (`None`: the primary). Several canvases can record at once,
    /// each its own file.
    pub fn start_recording(&mut self, cfg: &RecordingConfig, canvas: Option<Id>) -> Result<(), String> {
        #[cfg(target_os = "macos")]
        return crate::own_outputs::start_recording(cfg, canvas_or_primary(canvas)?);
        #[cfg(not(target_os = "macos"))]
        {
            let _ = (cfg, canvas);
            Err(NOT_HERE.into())
        }
    }

    /// Stops every recording.
    pub fn stop_recording(&mut self) {
        #[cfg(target_os = "macos")]
        crate::own_outputs::stop_recordings();
    }

    /// Pauses or resumes every recording. Returns whether they are all now in that state.
    pub fn pause_recording(&mut self, pause: bool) -> bool {
        #[cfg(target_os = "macos")]
        return crate::own_outputs::pause_recordings(pause).unwrap_or(false);
        #[cfg(not(target_os = "macos"))]
        {
            let _ = pause;
            false
        }
    }

    /// Ends every recording's current file and continues in a new one, without a gap.
    pub fn split_recording(&mut self) -> bool {
        #[cfg(target_os = "macos")]
        return crate::own_outputs::split_recordings();
        #[cfg(not(target_os = "macos"))]
        false
    }

    /// Adds a marker (a chapter) at the current time to every recording that can hold one.
    /// Returns false when none can or none is running.
    pub fn add_marker(&mut self, name: &str) -> bool {
        #[cfg(target_os = "macos")]
        return crate::own_outputs::add_marker(name);
        #[cfg(not(target_os = "macos"))]
        {
            let _ = name;
            false
        }
    }

    /// Starts sending `canvas`'s program (`None`: the primary) to the destination `cfg.id`,
    /// where Lightair's own senders reach it (RTMP(S), SRT, UDP, WHIP).
    pub fn start_stream(&mut self, cfg: &StreamConfig, canvas: Option<Id>) -> Result<(), String> {
        if crate::outputs::StreamProtocol::of(&cfg.server).is_none() {
            return Err("Use an rtmp://, rtmps://, srt://, rist:// or https:// (WHIP) address.".into());
        }
        #[cfg(target_os = "macos")]
        {
            if !crate::own_outputs::can_stream(cfg) {
                let why =
                    "This destination isn't available without libobs yet (RIST, MPEG-TS over TCP, or AV1).".to_string();
                crate::outputs::mark_failed(crate::outputs::OutputKey::Stream(cfg.id), &why);
                return Err(why);
            }
            crate::own_outputs::start_stream(cfg, canvas_or_primary(canvas)?)
        }
        #[cfg(not(target_os = "macos"))]
        {
            let _ = canvas;
            Err(NOT_HERE.into())
        }
    }

    /// Enhanced Broadcasting: several encodes in one RTMP connection, as the platform's `plan`
    /// says (`canvases[i]`: the canvas video track *i* is made from), made by Lightair's own
    /// encoders. `cfg.server` and `cfg.key` are the plan's ingest and key.
    pub fn start_multitrack_stream(
        &mut self,
        cfg: &StreamConfig,
        plan: &Multitrack,
        canvases: &[Id],
    ) -> Result<(), String> {
        #[cfg(target_os = "macos")]
        return crate::own_outputs::start_multitrack_stream(cfg, plan, canvases);
        #[cfg(not(target_os = "macos"))]
        {
            let _ = (plan, canvases);
            crate::outputs::mark_failed(crate::outputs::OutputKey::Stream(cfg.id), NOT_HERE);
            Err(NOT_HERE.into())
        }
    }

    /// Which running streams share an encoder: one list of destinations per video encoder.
    pub fn stream_share_groups(&self) -> Vec<Vec<Id>> {
        #[cfg(target_os = "macos")]
        return crate::own_outputs::share_groups();
        #[cfg(not(target_os = "macos"))]
        Vec::new()
    }

    /// How many distinct video encoders the streams use.
    pub fn stream_encoder_count(&self) -> usize {
        #[cfg(target_os = "macos")]
        return crate::own_outputs::stream_encoder_count();
        #[cfg(not(target_os = "macos"))]
        0
    }

    /// A stream's audio tracks, in order: the mix each encodes (diagnostics, tests).
    pub fn stream_audio_tracks(&self, id: Id) -> Vec<usize> {
        #[cfg(target_os = "macos")]
        return crate::own_outputs::stream_audio_tracks(id).unwrap_or_default();
        #[cfg(not(target_os = "macos"))]
        {
            let _ = id;
            Vec::new()
        }
    }

    /// The video encoder a stream uses (diagnostics and tests).
    pub fn stream_video_encoder(&self, id: Id) -> Option<usize> {
        #[cfg(target_os = "macos")]
        return crate::own_outputs::stream_video_encoder(id);
        #[cfg(not(target_os = "macos"))]
        {
            let _ = id;
            None
        }
    }

    pub fn stop_stream(&mut self, id: Id) {
        #[cfg(target_os = "macos")]
        crate::own_outputs::stop_stream(id, false);
        #[cfg(not(target_os = "macos"))]
        let _ = id;
    }

    /// Ends a stream at once, without letting its delay drain.
    pub fn force_stop_stream(&mut self, id: Id) {
        #[cfg(target_os = "macos")]
        crate::own_outputs::stop_stream(id, true);
        #[cfg(not(target_os = "macos"))]
        let _ = id;
    }

    pub fn stop_all_streams(&mut self) {
        #[cfg(target_os = "macos")]
        crate::own_outputs::stop_all_streams();
    }

    /// Starts the virtual camera: what a canvas has on air (`canvas`; `None` the primary), or
    /// one scene (`scene`, drawn by its canvas's compositor for the camera alone). Its status is
    /// `OutputKey::VirtualCamera`'s.
    pub fn start_virtual_camera(
        &mut self,
        canvas: Option<Id>,
        scene: Option<VirtualCameraScene>,
    ) -> Result<(), String> {
        #[cfg(target_os = "macos")]
        return match scene {
            Some(sc) => crate::own_outputs::start_virtual_camera_scene(sc.source.0, sc.source.1),
            None => crate::own_outputs::start_virtual_camera(canvas_or_primary(canvas)?),
        };
        #[cfg(not(target_os = "macos"))]
        {
            let _ = (canvas, scene);
            Err(NOT_HERE.into())
        }
    }

    pub fn stop_virtual_camera(&mut self) {
        #[cfg(target_os = "macos")]
        crate::own_outputs::stop_virtual_camera();
    }

    /// Starts the replay buffer of one canvas (`None`: the primary). Several canvases keep a
    /// buffer each.
    pub fn start_clips(&mut self, cfg: &ClipsConfig, canvas: Option<Id>) -> Result<(), String> {
        #[cfg(target_os = "macos")]
        return crate::own_outputs::start_clips(cfg, canvas_or_primary(canvas)?);
        #[cfg(not(target_os = "macos"))]
        {
            let _ = (cfg, canvas);
            Err(NOT_HERE.into())
        }
    }

    /// Stops every replay buffer.
    pub fn stop_clips(&mut self) {
        #[cfg(target_os = "macos")]
        crate::own_outputs::stop_clips(None);
    }

    /// Stops one canvas's replay buffer.
    pub fn stop_clips_of(&mut self, canvas: Id) {
        #[cfg(target_os = "macos")]
        crate::own_outputs::stop_clips(Some(canvas));
        #[cfg(not(target_os = "macos"))]
        let _ = canvas;
    }

    /// The canvases whose replay buffer exists.
    pub fn clips_canvases(&self) -> Vec<Id> {
        #[cfg(target_os = "macos")]
        return crate::own_outputs::clips_canvases();
        #[cfg(not(target_os = "macos"))]
        Vec::new()
    }

    /// Saves the last N seconds of every running buffer as a clip each. How many buffers
    /// accepted the save.
    pub fn save_clip(&mut self) -> usize {
        #[cfg(target_os = "macos")]
        return crate::own_outputs::save_clip();
        #[cfg(not(target_os = "macos"))]
        0
    }

    /// Updates byte and frame counters for running outputs (called by the stats sampler).
    pub fn sample(&self) {
        #[cfg(target_os = "macos")]
        crate::own_outputs::sample();
    }

    /// Stops everything (engine shutdown): recordings finish their files first.
    pub fn shutdown(&mut self) {
        #[cfg(target_os = "macos")]
        crate::own_outputs::shutdown();
    }
}
