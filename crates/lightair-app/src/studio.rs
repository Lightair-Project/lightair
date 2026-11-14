//! The studio: document + persistence + engine, as one GPUI model.
//!
//! Every change in the app goes through [`Studio::execute`]: the command is validated and
//! applied to the document, appended to the project journal, and the resulting diff is sent to
//! the engine thread for reconciliation (docs/03-architecture.md, "Document flow").

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use gpui_kit::{Context, SharedString};
use lightair_core::{Command, CommandError, Document, Id, Project, diff};
use lightair_engine::outputs::{
    ClipsConfig, Container, OutputKey, OutputState, RecordingConfig, StreamConfig, default_encoders, runtime,
};
use lightair_engine::{Engine, Stats};
use lightair_store::Package;

/// What a panel (the mixer) asks the window to show.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum UiRequest {
    /// A strip's effect, open in the Inspector.
    OpenEffect(lightair_core::MixerStrip, Id),
    /// The effect catalog for a strip.
    AddEffect(lightair_core::MixerStrip),
}

pub struct Studio {
    doc: Document,
    package: Option<Package>,
    engine: Arc<Engine>,
    /// The scene selected for editing (the program scene unless studio mode stages another).
    pub selected_scene: Option<Id>,
    pub selected_layer: Option<Id>,
    /// Further selected layers (⇧-click); `selected_layer` is the one the Inspector edits.
    pub also_selected: Vec<Id>,
    /// The mixer track the Inspector shows (instead of a layer).
    pub selected_track: Option<Id>,
    /// Last user-visible problem, shown in the status bar.
    pub issue: Option<SharedString>,
    /// Something a panel asks the window to show (the window takes it when it sees it).
    pub ui_request: Option<UiRequest>,
    pub stats: Stats,
    /// Clips announced so far (to report each new one once).
    clips_seen: u32,
    /// Frames libobs lagged since the previous stats sample (health is about now, not startup).
    pub recent_lagged: u32,
    /// The newest saved clip, and replays playing on the canvas (scene, layer, input).
    pub last_clip: Option<PathBuf>,
    replays: Vec<(Id, Id, Id)>,
    /// Microphone tracks' loudest peak (dBFS) for each of the last ten seconds.
    mic_history: std::collections::HashMap<Id, std::collections::VecDeque<f32>>,
    /// Imported layers whose size was guessed, waiting for their sources to report a size.
    refit: Option<(Vec<lightair_store::EstimatedLayer>, std::time::Instant)>,
    /// Multitrack audio files being recorded: their base name and paths.
    stems: Option<(String, Vec<std::path::PathBuf>)>,
}

impl Studio {
    pub fn open(engine: Arc<Engine>, dir: PathBuf, initial: impl FnOnce() -> Project) -> Self {
        let (project, package, issue) = match lightair_store::Package::open(&dir, initial) {
            Ok(opened) => {
                let issue = (opened.recovered > 0)
                    .then(|| SharedString::from(format!("Recovered {} unsaved change(s)", opened.recovered)));
                (opened.project, Some(opened.package), issue)
            }
            Err(e) => {
                tracing::error!(%e, "could not open the project; using an unsaved one");
                (Project::new("Untitled"), None, Some(SharedString::from(format!("Project not saved: {e}"))))
            }
        };
        let selected_scene = project.program.or(project.scenes.first().map(|s| s.id));
        let mut studio = Self {
            doc: Document::new(project),
            package,
            engine,
            selected_scene,
            selected_layer: None,
            also_selected: Vec::new(),
            selected_track: None,
            ui_request: None,
            issue,
            stats: Stats::default(),
            clips_seen: 0,
            recent_lagged: 0,
            refit: None,
            stems: None,
            mic_history: Default::default(),
            last_clip: None,
            replays: Vec::new(),
        };
        studio.migrate();
        studio.rebuild_engine();
        studio.sync_clips();
        studio
    }

    /// Brings an older project's effects up to date (saved, but not an undo step).
    fn migrate(&mut self) {
        let mut commands = lightair_core::catalog::migrate(self.doc.project());
        // tracks' libobs audio filters: the audio engine runs its own (LIGHTAIR_LEGACY_AUDIO
        // keeps libobs mixing, so the filters stay)
        if std::env::var("LIGHTAIR_LEGACY_AUDIO").map_or(true, |v| v != "1") {
            commands.extend(lightair_core::catalog::migrate_audio(self.doc.project()));
        }
        if commands.is_empty() {
            return;
        }
        for cmd in &commands {
            if let Err(e) = self.doc.apply(cmd) {
                tracing::warn!(%e, "couldn't update an effect");
                continue;
            }
            if let Some(pkg) = &mut self.package
                && let Err(e) = pkg.record(cmd)
            {
                tracing::warn!(%e, "couldn't save the updated effects");
            }
        }
        self.doc = Document::new(self.doc.project().as_ref().clone());
        tracing::info!(count = commands.len(), "effects updated to the effect stack");
    }

    /// Where the project lives on disk (`None` for an unsaved project).
    pub fn project_dir(&self) -> Option<std::path::PathBuf> {
        self.package.as_ref().map(|p| p.dir().to_path_buf())
    }

    /// The project's `assets` folder (prefabs, images, video, sound), made if missing; `None`
    /// for an unsaved project.
    pub fn assets_dir(&self) -> Option<std::path::PathBuf> {
        let dir = self.package.as_ref()?.dir().join("assets");
        std::fs::create_dir_all(&dir).ok()?;
        Some(dir)
    }

    pub fn engine(&self) -> &Arc<Engine> {
        &self.engine
    }

    pub fn project(&self) -> &Arc<Project> {
        self.doc.project()
    }

    /// Applies, journals and reconciles a command. Errors are shown in the status bar.
    pub fn execute(&mut self, command: Command, cx: &mut Context<Self>) -> Result<(), CommandError> {
        match self.doc.apply(&command) {
            Ok(change) => {
                if let Some(pkg) = &mut self.package
                    && let Err(e) = pkg.record(&command)
                {
                    self.issue = Some(format!("Couldn't save: {e}").into());
                }
                self.push(change.before, change.after);
                self.fix_selection();
                self.sync_clips();
                cx.notify();
                Ok(())
            }
            Err(e) => {
                self.issue = Some(e.to_string().into());
                cx.notify();
                Err(e)
            }
        }
    }

    /// Merges an OBS scene collection into the project as one undo step; the report goes to
    /// the status bar (summary) and the log (details).
    pub fn import_obs(&mut self, path: &std::path::Path, cx: &mut Context<Self>) {
        let result = std::fs::read_to_string(path)
            .map_err(|e| e.to_string())
            .and_then(|json| lightair_store::import_obs_collection(&json, self.project()).map_err(|e| e.to_string()));
        match result {
            Ok((command, report)) => {
                for (group, items) in [
                    ("converted", &report.converted),
                    ("replaced", &report.replaced),
                    ("kept as plugin", &report.kept_plugin),
                    ("needs attention", &report.attention),
                ] {
                    for item in items {
                        tracing::info!(target: "import", "{group}: {item}");
                    }
                }
                if self.execute(command, cx).is_ok() {
                    self.issue = Some(format!("Imported OBS scenes: {}", report.summary()).into());
                    if !report.estimated.is_empty() {
                        let mut inputs: Vec<Id> = report.estimated.iter().map(|e| e.input).collect();
                        inputs.dedup();
                        self.engine.post(move |s| s.reconciler.hold_inputs(&inputs));
                        self.refit = Some((report.estimated, std::time::Instant::now()));
                    }
                }
            }
            Err(e) => self.issue = Some(format!("Couldn't import: {e}").into()),
        }
        cx.notify();
    }

    /// Plays the newest clip full-frame on top of the live scene; it removes itself when done.
    pub fn replay_last_clip(&mut self, cx: &mut Context<Self>) {
        let (Some(path), Some(scene)) = (self.last_clip.clone(), self.project().program) else {
            self.issue = Some("Save a clip first, then replay it.".into());
            cx.notify();
            return;
        };
        let p = self.project().clone();
        let input = lightair_core::Input::new(
            "Replay",
            "ffmpeg_source",
            serde_json::json!({
                "local_file": path.to_string_lossy(),
                "is_local_file": true,
                "looping": false,
                "restart_on_activate": true,
                "clear_on_media_end": true,
            }),
        );
        let layouts: Vec<(Id, lightair_core::Layout)> = p
            .formats
            .iter()
            .map(|f| (f.id, lightair_core::Layout::boxed(0.0, 0.0, f.width as f32, f.height as f32)))
            .collect();
        let layer = lightair_core::Layer::new("Replay", input.id, layouts);
        let (layer_id, input_id) = (layer.id, input.id);
        let command = Command::Batch {
            label: "Replay clip".into(),
            commands: vec![Command::AddInput { input }, Command::AddLayer { scene, layer, index: None }],
        };
        if self.execute(command, cx).is_ok() {
            self.replays.push((scene, layer_id, input_id));
        }
    }

    /// Removes replays whose clip has finished (called on the stats tick).
    pub fn end_replays(&mut self, cx: &mut Context<Self>) {
        let mut finished = Vec::new();
        for (scene, layer, input) in &self.replays {
            let id = *input;
            let state = self.engine.call(move |s| s.reconciler.media_status(id)).map(|s| s.state);
            if matches!(
                state,
                None | Some(lightair_engine::media::MediaState::Ended | lightair_engine::media::MediaState::Error)
            ) {
                finished.push((*scene, *layer, *input));
            }
        }
        if finished.is_empty() {
            return;
        }
        self.replays.retain(|r| !finished.contains(r));
        let p = self.project().clone();
        let commands: Vec<Command> = finished
            .into_iter()
            .filter(|(scene, layer, _)| p.scene(*scene).is_some_and(|s| s.layer(*layer).is_some()))
            .flat_map(|(scene, layer, input)| [Command::RemoveLayer { scene, layer }, Command::RemoveInput { input }])
            .collect();
        if !commands.is_empty() {
            let _ = self.execute(Command::Batch { label: "End replay".into(), commands }, cx);
        }
    }

    /// Plays the mixer again once a plugin insert has loaded (it passes audio through until then).
    pub fn refresh_plugins(&mut self) {
        if lightair_engine::plugins::changed() {
            let project = self.project().clone();
            self.engine.post(move |s| s.reconciler.refresh_audio(&project));
        }
    }

    /// Runs the end action of countdowns that just ended ("Then go to" a scene).
    pub fn run_timer_ends(&mut self, cx: &mut Context<Self>) {
        let ended = self.engine.call(|s| s.reconciler.take_ended_timers());
        for id in ended {
            let target = self
                .project()
                .inputs
                .get(&id)
                .and_then(|i| i.settings["end_scene"].as_str().map(str::to_string))
                .and_then(|s| self.project().scenes.iter().find(|sc| sc.id.to_string() == s).map(|sc| sc.id));
            if let Some(scene) = target
                && self.project().program != Some(scene)
            {
                let _ = self.execute(Command::SetProgram { scene: Some(scene) }, cx);
            }
        }
    }

    /// Corrects imported layers whose source size was guessed, once the sources report their
    /// real size (or after a few seconds, with whatever is known). Layers changed since the
    /// import are left alone. One undo step.
    pub fn refit_imported(&mut self, cx: &mut Context<Self>) {
        let Some((pending, started)) = &self.refit else { return };
        let inputs: Vec<Id> = pending.iter().map(|e| e.input).collect();
        let sizes = self.engine.call(move |s| inputs.iter().map(|id| s.reconciler.input_size(*id)).collect::<Vec<_>>());
        if sizes.iter().any(Option::is_none) && started.elapsed() < std::time::Duration::from_secs(8) {
            return;
        }
        let (pending, _) = self.refit.take().unwrap();
        self.engine.post(|s| s.reconciler.release_inputs());
        let project = self.project().clone();
        let commands: Vec<Command> = pending
            .iter()
            .zip(sizes)
            .filter_map(|(est, size)| {
                let current = project.scene(est.scene)?.layer(est.layer)?.layouts.get(&est.format)?;
                let size = size.filter(|_| *current == est.layout)?;
                Some(Command::SetLayout {
                    scene: est.scene,
                    layer: est.layer,
                    format: est.format,
                    layout: est.refit(size),
                })
            })
            .collect();
        if commands.is_empty() {
            return;
        }
        let n = commands.len();
        if self.execute(Command::Batch { label: "Size imported objects".into(), commands }, cx).is_ok() {
            self.issue =
                Some(format!("Corrected the size of {n} imported object{}", if n == 1 { "" } else { "s" }).into());
        }
    }

    pub fn begin_gesture(&mut self) {
        self.doc.begin_gesture();
    }

    pub fn end_gesture(&mut self) {
        self.doc.end_gesture();
    }

    pub fn undo(&mut self, cx: &mut Context<Self>) {
        if let Some(change) = self.doc.undo() {
            self.issue = Some(format!("Undo: {}", change.label).into());
            self.push(change.before, change.after);
            self.compact();
            self.fix_selection();
            cx.notify();
        }
    }

    pub fn redo(&mut self, cx: &mut Context<Self>) {
        if let Some(change) = self.doc.redo() {
            self.issue = Some(format!("Redo: {}", change.label).into());
            self.push(change.before, change.after);
            self.compact();
            self.fix_selection();
            cx.notify();
        }
    }

    /// Writes the project files and empties the journal (on a timer, on quit, after undo).
    pub fn compact(&mut self) {
        if let Some(pkg) = &mut self.package
            && let Err(e) = pkg.compact(self.doc.project())
        {
            tracing::error!(%e, "compaction failed");
        }
    }

    pub fn flush(&mut self) {
        if let Some(pkg) = &mut self.package {
            if pkg.pending() > 0 {
                let _ = pkg.flush();
            }
            if pkg.pending() > 200 {
                self.compact();
            }
        }
    }

    // ------------------------------------------------------------ outputs
    pub fn recording_folder(&self) -> PathBuf {
        self.project()
            .outputs
            .recording
            .folder
            .clone()
            .unwrap_or_else(lightair_platform::paths::recordings_dir)
    }

    /// Keeps the replay buffer running exactly when clips are on.
    fn sync_clips(&mut self) {
        let settings = self.project().outputs.clips;
        let running = runtime().get(OutputKey::Clips).state.is_running();
        if settings.enabled && !running {
            let cfg = ClipsConfig {
                folder: self.recording_folder().join("Clips"),
                seconds: settings.seconds,
                encoders: default_encoders(&self.engine.capabilities.encoders, 8000, false),
            };
            let format = settings.format;
            self.engine.post(move |s| {
                let video = format.and_then(|f| s.reconciler.format_video(f));
                if let Err(e) = s.outputs.start_clips(&cfg, video) {
                    tracing::error!(%e, "replay buffer failed to start");
                }
            });
        } else if !settings.enabled && running {
            self.engine.post(|s| s.outputs.stop_clips());
        }
    }

    pub fn clips_ready(&self) -> bool {
        matches!(runtime().get(OutputKey::Clips).state, OutputState::Active { .. })
    }

    pub fn save_clip(&mut self, cx: &mut Context<Self>) {
        if !self.clips_ready() {
            self.issue = Some("Turn on Clips in Outputs to save clips.".into());
            cx.notify();
            return;
        }
        self.engine.post(|s| {
            s.outputs.save_clip();
        });
    }

    fn announce_clips(&mut self) {
        let status = runtime().get(OutputKey::Clips);
        if status.saved_clips > self.clips_seen {
            self.clips_seen = status.saved_clips;
            let name = status.path.as_ref().and_then(|p| p.file_name()).map(|n| n.to_string_lossy().into_owned());
            self.issue = Some(format!("Clip saved: {}", name.unwrap_or_default()).into());
            self.last_clip = status.path.clone();
        }
    }

    pub fn is_recording(&self) -> bool {
        runtime().any_recording_running() || lightair_engine::audio_host::STEMS_RUNNING.load(std::sync::atomic::Ordering::Relaxed)
    }

    pub fn toggle_recording(&mut self, cx: &mut Context<Self>) {
        if self.is_recording() {
            self.engine.post(|s| s.outputs.stop_recording());
            self.finish_stems(cx);
            return;
        }
        let project = self.project().clone();
        let settings = project.outputs.recording.clone();
        let container = match settings.container.as_str() {
            "mkv" => Container::Mkv,
            "mov" => Container::Mov,
            _ => Container::HybridMp4,
        };
        // Every chosen canvas, each to its own file; with several, the format's name tells
        // them apart ("Lightair 2026-11-12 10-00-00 Vertical.mp4").
        let targets = settings.targets(&project);
        let several = targets.len() > 1;
        let stamp = chrono::Local::now().format("%Y-%m-%d %H-%M-%S").to_string();
        // Make the journal durable before outputs start (docs/14, "Saving, autosave & recovery").
        self.flush();
        // Multitrack audio files (and, audio only, the program) beside the video.
        if settings.multitrack || settings.audio_only {
            let (p, folder, base) = (project.clone(), self.recording_folder(), format!("Lightair {stamp}"));
            let program = settings.audio_only;
            match self.engine.call(move |s| s.reconciler.start_stems(&p, &folder, &base, program)) {
                Ok(files) => self.stems = Some((format!("Lightair {stamp}"), files)),
                Err(e) => self.issue = Some(format!("Multitrack audio didn't start: {e}").into()),
            }
        }
        if settings.audio_only {
            cx.notify();
            return;
        }
        for canvas in targets {
            let label = several.then(|| project.format(canvas).map(|f| f.name.clone()).unwrap_or_default());
            let name = match &label {
                Some(l) => format!("Lightair {stamp} {l}.{}", container.extension()),
                None => format!("Lightair {stamp}.{}", container.extension()),
            };
            let Some(format) = project.format(canvas) else { continue };
            let cfg = RecordingConfig {
                canvas,
                path: self.recording_folder().join(name),
                label,
                split_minutes: settings.split_minutes,
                container,
                encoders: crate::encoding::for_recording(&self.engine.capabilities.encoders, &settings, format),
                // the engine plays the first five mixes into libobs mixes 1-5, in mixer order
                extra_mixes: settings
                    .file_mixes
                    .iter()
                    .filter_map(|m| project.audio.mixes.iter().position(|x| x.id == *m))
                    .filter(|&k| k < 5)
                    .map(|k| k + 1)
                    .collect(),
            };
            self.engine.post(move |s| {
                let video = s.reconciler.format_video(canvas);
                if let Err(e) = s.outputs.start_recording(&cfg, video) {
                    tracing::error!(%e, "recording failed to start");
                }
            });
        }
        cx.notify();
    }

    pub fn pause_recording(&mut self, cx: &mut Context<Self>) {
        let pause = !runtime().recording().paused;
        self.engine.post(move |s| {
            s.outputs.pause_recording(pause);
            s.reconciler.pause_stems(pause);
        });
        cx.notify();
    }

    /// Stops the multitrack audio files and writes what they hold beside them
    /// ("<name>.multitrack.json": the files, their rate and length, and when they started).
    fn finish_stems(&mut self, cx: &mut Context<Self>) {
        let Some((base, _)) = self.stems.take() else { return };
        let Some(report) = self.engine.call(|s| s.reconciler.stop_stems()) else {
            self.issue = Some("Multitrack audio couldn't be finished.".into());
            cx.notify();
            return;
        };
        let names: Vec<String> =
            report.files.iter().filter_map(|f| f.file_name().map(|n| n.to_string_lossy().into_owned())).collect();
        let sidecar = serde_json::json!({
            "files": names,
            "sample_rate": report.sample_rate,
            "frames": report.frames,
            "seconds": report.frames as f64 / report.sample_rate as f64,
            "start_ns": report.start_ns,
        });
        let path = self.recording_folder().join(format!("{base}.multitrack.json"));
        if let Ok(text) = serde_json::to_string_pretty(&sidecar) {
            let _ = std::fs::write(path, text);
        }
        self.issue = Some(format!("Recorded {} audio files", report.files.len()).into());
        cx.notify();
    }

    pub fn add_marker(&mut self, cx: &mut Context<Self>) {
        let n = runtime().recording().markers + 1;
        let ok = self.engine.call(move |s| s.outputs.add_marker(&format!("Marker {n}")));
        self.issue = Some(if ok {
            format!("Marker {n} added").into()
        } else {
            "Markers need a recording in MP4 or MOV.".into()
        });
        cx.notify();
    }

    pub fn split_recording(&mut self, cx: &mut Context<Self>) {
        if self.engine.call(|s| s.outputs.split_recording()) {
            self.issue = Some("Continuing in a new file".into());
        }
        cx.notify();
    }

    /// Starts every enabled destination.
    pub fn go_live(&mut self, cx: &mut Context<Self>) {
        let project = self.project().clone();
        let enabled: Vec<_> = project.outputs.destinations.iter().filter(|d| d.enabled).cloned().collect();
        if enabled.is_empty() {
            self.issue = Some("Add a destination in Outputs to go live.".into());
            cx.notify();
            return;
        }
        self.flush();
        // Integrated loudness covers this show.
        lightair_engine::loudness::reset_stream_loudness();
        for d in enabled {
            let Some(key) = crate::secrets::stream_key(d.id) else {
                self.issue = Some(format!("No stream key saved for {}.", d.name).into());
                continue;
            };
            let reconnect = project.outputs.reconnect;
            let cfg = StreamConfig {
                id: d.id,
                server: d.server.clone(),
                key,
                encoders: crate::encoding::for_stream(&self.engine.capabilities.encoders, &project, &d),
                reconnect: Some((reconnect.retries, reconnect.delay_s)),
            };
            let format = d.format;
            self.engine.post(move |s| {
                let video = format.and_then(|f| s.reconciler.format_video(f));
                if let Err(e) = s.outputs.start_stream(&cfg, video) {
                    tracing::error!(%e, "stream failed to start");
                }
            });
        }
        cx.notify();
    }

    pub fn end_stream(&mut self, cx: &mut Context<Self>) {
        self.engine.post(|s| s.outputs.stop_all_streams());
        cx.notify();
    }

    /// Surfaces output failures as issues (called on the stats tick).
    fn check_outputs(&mut self) {
        let mut keys = vec![OutputKey::Clips];
        keys.extend(self.project().formats.iter().map(|f| OutputKey::Recording(f.id)));
        keys.extend(self.project().outputs.destinations.iter().map(|d| OutputKey::Stream(d.id)));
        for key in keys {
            if let OutputState::Failed(msg) = runtime().get(key).state {
                let label = match key {
                    OutputKey::Recording(canvas) => match self.project().format(canvas) {
                        Some(f) if self.project().formats.len() > 1 => format!("Recording ({})", f.name),
                        _ => "Recording".to_string(),
                    },
                    OutputKey::Clips => "Clips".to_string(),
                    OutputKey::Stream(id) => self
                        .project()
                        .outputs
                        .destinations
                        .iter()
                        .find(|d| d.id == id)
                        .map(|d| d.name.clone())
                        .unwrap_or_default(),
                };
                let text = SharedString::from(format!("{label}: {msg}"));
                if self.issue.as_ref() != Some(&text) {
                    self.issue = Some(text);
                }
            }
        }
    }

    pub fn refresh_stats(&mut self) {
        self.engine.post(|s| {
            s.outputs.sample();
            s.reconciler.cleanup();
        });
        self.check_outputs();
        self.announce_clips();
        let stats = lightair_engine::read_stats();
        self.recent_lagged = stats.lagged_frames.saturating_sub(self.stats.lagged_frames);
        self.stats = stats;
        self.sample_mics();
    }

    fn mic_tracks(&self) -> Vec<(Id, String)> {
        let p = self.project();
        p.tracks
            .iter()
            .filter(|t| p.inputs.get(&t.input).is_some_and(|i| i.kind == lightair_core::kinds::MIC))
            .map(|t| (t.id, t.name.clone()))
            .collect()
    }

    fn sample_mics(&mut self) {
        let mics = self.mic_tracks();
        self.mic_history.retain(|id, _| mics.iter().any(|(m, _)| m == id));
        for (id, _) in mics {
            let Some(cell) = lightair_engine::meters::meters().get(id) else { continue };
            let h = self.mic_history.entry(id).or_default();
            h.push_back(cell.take_window_peak());
            while h.len() > 10 {
                h.pop_front();
            }
        }
    }

    /// ⇧⌘M: mutes every microphone, or unmutes them all if they're all muted already.
    pub fn toggle_mics(&mut self, cx: &mut Context<Self>) {
        let mics = self.mic_tracks();
        if mics.is_empty() {
            return;
        }
        let p = self.project().clone();
        let all_muted = mics.iter().all(|(id, _)| p.tracks.iter().any(|t| t.id == *id && t.muted));
        let commands = mics.iter().map(|(id, _)| Command::SetTrackMuted { track: *id, muted: !all_muted }).collect();
        let label = if all_muted { "Unmute microphones" } else { "Mute microphones" };
        if self.execute(Command::Batch { label: label.into(), commands }, cx).is_ok() {
            self.issue = Some(if all_muted { "Microphones on" } else { "Microphones muted" }.into());
        }
    }

    /// What the Go Live review checks. `has_key` is looked up once by the caller (Keychain).
    pub fn preflight_facts(&self, has_key: &HashMap<Id, bool>, record_too: bool) -> crate::preflight::Facts {
        let p = self.project();
        let enabled: Vec<_> = p.outputs.destinations.iter().filter(|d| d.enabled).collect();
        let encoders = default_encoders(&self.engine.capabilities.encoders, p.outputs.stream_bitrate_kbps, true);
        let encoder_ok = self.engine.capabilities.encoders.contains(&encoders.video_encoder);
        // Identical settings share an encoder: one per distinct format.
        let mut formats: Vec<_> = enabled.iter().map(|d| d.format).collect();
        formats.sort_by_key(|f| f.map(|id| id.to_string()));
        formats.dedup();
        let target_fps = p.primary_format().fps_num as f64 / p.primary_format().fps_den.max(1) as f64;
        crate::preflight::Facts {
            destinations: p
                .outputs
                .destinations
                .iter()
                .map(|d| crate::preflight::Destination {
                    name: d.name.clone(),
                    enabled: d.enabled,
                    has_key: has_key.get(&d.id).copied().unwrap_or(false),
                })
                .collect(),
            encoder: encoder_ok.then(|| match encoders.video_encoder.as_str() {
                "obs_x264" => "x264 (software)".to_string(),
                id => format!("{} H.264", lightair_engine::outputs::encoder_maker(id)),
            }),
            encoders: formats.len() as u32,
            video_kbps: encoders.video_bitrate_kbps,
            audio_kbps: encoders.audio_bitrate_kbps,
            lagged_recent: self.recent_lagged,
            fps: self.stats.active_fps,
            target_fps,
            mics: self
                .mic_tracks()
                .into_iter()
                .filter_map(|(id, name)| self.mic_history.get(&id).map(|h| (name, h.iter().copied().collect())))
                .collect(),
            record_too,
            free_gb: free_gb(&self.recording_folder()),
        }
    }

    fn push(&self, before: Arc<Project>, after: Arc<Project>) {
        let patches = diff(&before, &after);
        if patches.is_empty() {
            return;
        }
        self.engine.post(move |s| s.reconciler.apply(&after, &patches));
    }

    fn rebuild_engine(&self) {
        let project = self.doc.project().clone();
        let empty = Arc::new(project.empty_like());
        self.push(empty, project);
    }

    /// Every selected layer, the Inspector's first.
    pub fn selection(&self) -> Vec<Id> {
        self.selected_layer.into_iter().chain(self.also_selected.iter().copied()).collect()
    }

    /// Selects a scene for editing: the Inspector shows the scene (no layer or mixer strip).
    pub fn select_scene(&mut self, scene: Id) {
        self.selected_scene = Some(scene);
        self.selected_layer = None;
        self.also_selected.clear();
        self.selected_track = None;
    }

    /// Selects a mixer track for the Inspector (and clears the layer selection).
    pub fn select_track(&mut self, track: Option<Id>) {
        self.selected_track = track;
        if track.is_some() {
            self.selected_layer = None;
            self.also_selected.clear();
        }
    }

    /// ⇧-click: add a layer to the selection, or take it out.
    pub fn toggle_selected(&mut self, layer: Id) {
        if self.selected_layer == Some(layer) {
            self.selected_layer = (!self.also_selected.is_empty()).then(|| self.also_selected.remove(0));
        } else if let Some(i) = self.also_selected.iter().position(|l| *l == layer) {
            self.also_selected.remove(i);
        } else if self.selected_layer.is_none() {
            self.selected_layer = Some(layer);
        } else {
            self.also_selected.push(layer);
        }
    }

    fn fix_selection(&mut self) {
        let p = self.doc.project();
        // a mixer strip: a track, bus, mix or the master
        if self.selected_track.is_some_and(|t| lightair_core::MixerStrip::find(p, t).is_none()) {
            self.selected_track = None;
        }
        if self.selected_scene.is_none_or(|s| p.scene(s).is_none()) {
            self.selected_scene = p.program.or(p.scenes.first().map(|s| s.id));
        }
        if let (Some(scene), Some(layer)) = (self.selected_scene, self.selected_layer)
            && p.scene(scene).is_none_or(|s| s.layer(layer).is_none())
        {
            self.selected_layer = self.also_selected.first().copied();
        }
        let scene = self.selected_scene.and_then(|s| p.scene(s));
        let primary = self.selected_layer;
        self.also_selected.retain(|l| Some(*l) != primary && scene.is_some_and(|s| s.layer(*l).is_some()));
        if self.selected_layer.is_none() {
            self.also_selected.clear();
        }
    }
}

/// Free space on the volume holding `path` (or its nearest existing parent), in GB.
fn free_gb(path: &std::path::Path) -> Option<f64> {
    lightair_platform::files::free_bytes(path).map(|b| b as f64 / 1e9)
}
