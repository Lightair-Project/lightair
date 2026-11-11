//! The studio: document + persistence + engine, as one GPUI model.
//!
//! Every change in the app goes through [`Studio::execute`]: the command is validated and
//! applied to the document, appended to the project journal, and the resulting diff is sent to
//! the engine thread for reconciliation (docs/03-architecture.md, "Document flow").

use std::path::PathBuf;
use std::sync::Arc;

use gpui_kit::{Context, SharedString};
use lightair_core::{Command, CommandError, Document, Id, Project, diff};
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
        let studio = Self {
            doc: Document::new(project),
            package,
            engine,
            selected_scene,
            selected_layer: None,
            issue,
            stats: Stats::default(),
            recent_lagged: 0,
        };
        studio.rebuild_engine();
        studio
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

    pub fn refresh_stats(&mut self) {
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
