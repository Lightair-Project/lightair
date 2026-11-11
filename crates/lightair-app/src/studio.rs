//! The studio: document + persistence + engine, as one GPUI model.
//!
//! Every change in the app goes through [`Studio::execute`]: the command is validated and
//! applied to the document, appended to the project journal, and the resulting diff is sent to
//! the engine thread for reconciliation (docs/03-architecture.md, "Document flow").

use std::path::PathBuf;
use std::sync::Arc;

use gpui_kit::{Context, SharedString};
use lightair_core::{Command, CommandError, Document, Id, Project, diff};
use lightair_engine::outputs::{
    ClipsConfig, Container, OutputKey, OutputState, RecordingConfig, StreamConfig, default_encoders, runtime,
};
use lightair_engine::{Engine, Stats};
use lightair_store::Package;

pub struct Studio {
    doc: Document,
    package: Option<Package>,
    engine: Arc<Engine>,
    /// The scene selected for editing (the program scene unless studio mode stages another).
    pub selected_scene: Option<Id>,
    pub selected_layer: Option<Id>,
    /// Last user-visible problem, shown in the status bar.
    pub issue: Option<SharedString>,
    pub stats: Stats,
    /// Clips announced so far (to report each new one once).
    clips_seen: u32,
    /// Frames libobs lagged since the previous stats sample (health is about now, not startup).
    pub recent_lagged: u32,
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
            issue,
            stats: Stats::default(),
            clips_seen: 0,
            recent_lagged: 0,
        };
        studio.rebuild_engine();
        studio.sync_clips();
        studio
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
            .unwrap_or_else(|| std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default().join("Movies/Lightair"))
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
        }
    }

    pub fn is_recording(&self) -> bool {
        runtime().get(OutputKey::Recording).state.is_running()
    }

    pub fn toggle_recording(&mut self, cx: &mut Context<Self>) {
        if self.is_recording() {
            self.engine.post(|s| s.outputs.stop_recording());
            return;
        }
        let settings = self.project().outputs.recording.clone();
        let container = match settings.container.as_str() {
            "mkv" => Container::Mkv,
            "mov" => Container::Mov,
            _ => Container::HybridMp4,
        };
        let name = format!("Lightair {}.{}", chrono::Local::now().format("%Y-%m-%d %H-%M-%S"), container.extension());
        let cfg = RecordingConfig {
            path: self.recording_folder().join(name),
            container,
            encoders: default_encoders(&self.engine.capabilities.encoders, settings.bitrate_kbps, false),
        };
        // Make the journal durable before outputs start (docs/14, "Saving, autosave & recovery").
        self.flush();
        let format = settings.format;
        self.engine.post(move |s| {
            let video = format.and_then(|f| s.reconciler.format_video(f));
            if let Err(e) = s.outputs.start_recording(&cfg, video) {
                tracing::error!(%e, "recording failed to start");
            }
        });
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
        for d in enabled {
            let Some(key) = crate::secrets::stream_key(d.id) else {
                self.issue = Some(format!("No stream key saved for {}.", d.name).into());
                continue;
            };
            let cfg = StreamConfig {
                id: d.id,
                server: d.server.clone(),
                key,
                encoders: default_encoders(
                    &self.engine.capabilities.encoders,
                    project.outputs.stream_bitrate_kbps,
                    true,
                ),
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
        let mut keys = vec![OutputKey::Recording, OutputKey::Clips];
        keys.extend(self.project().outputs.destinations.iter().map(|d| OutputKey::Stream(d.id)));
        for key in keys {
            if let OutputState::Failed(msg) = runtime().get(key).state {
                let label = match key {
                    OutputKey::Recording => "Recording".to_string(),
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

    fn fix_selection(&mut self) {
        let p = self.doc.project();
        if self.selected_scene.is_none_or(|s| p.scene(s).is_none()) {
            self.selected_scene = p.program.or(p.scenes.first().map(|s| s.id));
        }
        if let (Some(scene), Some(layer)) = (self.selected_scene, self.selected_layer)
            && p.scene(scene).is_none_or(|s| s.layer(layer).is_none())
        {
            self.selected_layer = None;
        }
    }
}
