//! The Editor (docs/plans/video-editor.md): a multi-track video editor on the workspace's
//! projects. Media on the left (the workspace's Library items and files imported into the
//! project), the player in the middle, the inspector on the right, the timeline below.
//!
//! The work is `lightair_render`'s: frames for the player and playback, and the export, on threads
//! of their own. The project is saved as it changes; undo and redo step through its versions.

use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};

use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::input::{Input as TextInput, InputEvent, InputState};
use gpui_kit::component::menu::{DropdownMenu, PopupMenuItem};
use gpui_kit::component::Sizable;
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;
use lightair_core::Id;
use lightair_render::edit::Edit;
use lightair_render::media::Codec;
use lightair_render::seqrender::SequenceRenderer;
use lightair_render::sequence::{EditProject, Media, MediaKind, Sequence, TrackKind, Transform};
use lightair_render::zoom;
use lightair_store::recordings::{File as ItemFile, Index, Item, Kind};

use crate::studio::Studio;
use crate::ui::{self, tok};

mod audio;
mod looks;
mod speaker;
mod speed;
use lightair_render::sequence::Content;
use looks::{heading, hint, label};

/// What the background work sends back.
enum Done {
    /// A frame for the player (made into an image off the window's thread).
    Frame(u64, Option<Arc<RenderImage>>),
    Progress(f32),
    Exported(Result<PathBuf, String>),
    /// Media made from Library items or files (probing takes a moment): placed at the time, or
    /// (`None`) at the main track's end, or (`Some(u64::MAX)`) only kept in the project.
    Media(Vec<Media>, Option<u64>, Option<usize>),
    /// Caption lines heard in the project's sound.
    Captions(Result<Vec<(u64, u64, String)>, String>),
    Downloaded(Result<(), String>),
    /// Playback reached the end.
    Ended,
    /// A file's waveform.
    Peaks(PathBuf, Option<Arc<Vec<f32>>>),
    /// Voices cleaned up.
    Cleaned(Result<(), String>),
    /// A proxy being made, and made (its original file).
    ProxyProgress(f32),
    /// A video's filmstrip.
    Strip(PathBuf, Option<Arc<speed::Strip>>),
    Proxied(PathBuf, Result<(), String>),
}

/// A medium being dragged from the media panel.
#[derive(Clone)]
pub struct DraggedMedia {
    label: SharedString,
    source: MediaSource,
}

impl Render for DraggedMedia {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
            .px_2()
            .py_1()
            .rounded(px(6.))
            .bg(tok::bg_raised())
            .border_1()
            .border_color(tok::accent_primary())
            .text_size(px(12.))
            .child(self.label.clone())
    }
}

/// Where a medium comes from.
#[derive(Clone)]
enum MediaSource {
    /// A Library item of the workspace.
    Item(Item),
    /// Already in the project (imported).
    Media(Id),
    /// Text or a shape (where it goes in the frame).
    Content(Content, Transform),
}

/// A drag on the timeline.
#[derive(Clone, Copy)]
enum Drag {
    Move { clip: Id, grab_ms: i64 },
    TrimStart(Id),
    TrimEnd(Id),
    Scrub,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum InspectorTab {
    /// A made picture's words or shape.
    Text,
    Video,
    Audio,
    Speed,
    Filter,
    Transition,
    Screen,
}

/// What the left panel shows.
#[derive(Clone, Copy, PartialEq, Eq)]
enum LeftTab {
    Media,
    Audio,
    Text,
    Shapes,
    Captions,
}

/// Pixels per second at zoom 1.
const PPS: f32 = 40.0;
const HEADER_W: f32 = 112.0;
const RULER_H: f32 = 26.0;
const VIDEO_H: f32 = 58.0;
const AUDIO_H: f32 = 44.0;
const EDGE: f32 = 7.0;

pub struct Editor {
    studio: Entity<Studio>,
    workspace: Option<PathBuf>,
    project: Option<EditProject>,
    /// The workspace's Library, as the media panel lists it (read when it changes).
    library: Vec<Item>,
    library_seen: Option<u64>,
    undo: Vec<Sequence>,
    redo: Vec<Sequence>,
    /// The sequence when a drag began (its undo step).
    drag_before: Option<Sequence>,
    renderer: Arc<Mutex<Option<SequenceRenderer>>>,
    /// The sequence as last changed, for the renderer to take before its next frame (it may be
    /// busy, or still being made, when the change happens).
    pending: Arc<Mutex<Pending>>,
    /// The frame being shown, and the one before it (freed from the GPU when the next comes).
    retired: Option<Arc<RenderImage>>,
    /// The player's size in pixels (measured as it's drawn) and the preview size asked for.
    player_px: std::rc::Rc<std::cell::Cell<(f32, f32)>>,
    preview_box: Option<(u32, u32)>,
    preview: Option<Arc<RenderImage>>,
    wanted: Option<u64>,
    busy: bool,
    playing: Option<Arc<std::sync::atomic::AtomicBool>>,
    /// The player's sound, while it plays.
    speaker: Option<speaker::Speaker>,
    exporting: Option<f32>,
    note: Option<SharedString>,
    playhead: u64,
    selected: Option<Id>,
    /// Timeline zoom (× PPS) and scroll (ms at the left edge).
    zoom: f32,
    scroll_ms: f32,
    drag: Option<Drag>,
    timeline_bounds: std::rc::Rc<std::cell::RefCell<Option<Bounds<Pixels>>>>,
    tab: InspectorTab,
    left: LeftTab,
    /// The selected text clip's words, being edited, and the sequence before typing began.
    text_edit: Option<(Id, Entity<InputState>)>,
    text_before: Option<Sequence>,
    captioning: bool,
    downloading: bool,
    /// Waveforms by file (`None`: being read).
    peaks: std::collections::HashMap<PathBuf, Option<Arc<Vec<f32>>>>,
    cleaning: bool,
    /// The proxy being made (its medium's name, how far), and files whose proxy failed.
    proxying: Option<(SharedString, f32)>,
    proxy_failed: Vec<PathBuf>,
    /// Filmstrips by file (`None`: being read).
    strips: std::collections::HashMap<PathBuf, Option<Arc<speed::Strip>>>,
    /// Frames shown while playing, and when the first was (for measuring).
    shown: u64,
    shown_since: Option<std::time::Instant>,
    naming: Option<(Option<Id>, Entity<InputState>)>,
    watching: bool,
    tx: mpsc::Sender<Done>,
    rx: mpsc::Receiver<Done>,
    _subscriptions: Vec<Subscription>,
}

/// What the renderer is to take before its next frame.
#[derive(Default)]
struct Pending {
    /// The sequence's last change, if it hasn't been taken.
    seq: Option<Sequence>,
    /// The most the player shows (always set: the player's frames are BGRA previews).
    preview: Option<(u32, u32)>,
}

/// The renderer brought up to the sequence's last change and the player's size.
fn catch_up(r: &mut SequenceRenderer, pending: &Mutex<Pending>) {
    let mut p = pending.lock().unwrap();
    if let Some(seq) = p.seq.take() {
        let _ = r.set(&seq);
    }
    r.set_preview(p.preview);
}

/// Preview sizes, smallest first: the player gets the first that covers it.
const PREVIEW_BOXES: [(u32, u32); 5] = [(640, 360), (960, 540), (1280, 720), (1920, 1080), (2560, 1440)];

/// A preview frame (BGRA, as the renderer makes them for the player) as an image.
fn image_of(bgra: Vec<u8>, size: (u32, u32)) -> Option<Arc<RenderImage>> {
    if bgra.is_empty() {
        return None;
    }
    let buffer = image::RgbaImage::from_raw(size.0, size.1, bgra)?;
    Some(Arc::new(RenderImage::new([image::Frame::new(buffer)])))
}

fn clock(ms: u64) -> String {
    let s = ms / 1000;
    format!("{:02}:{:02}:{:02}.{}", s / 3600, (s / 60) % 60, s % 60, (ms % 1000) / 100)
}

fn short(ms: u64) -> String {
    let s = ms / 1000;
    if s >= 3600 { format!("{}:{:02}:{:02}", s / 3600, (s / 60) % 60, s % 60) } else { format!("{:02}:{:02}", s / 60, s % 60) }
}

/// The media a Library item gives (probing its files): one per video or sound.
fn media_of(item: &Item) -> Vec<Media> {
    if item.kind == Kind::Take {
        let folder = item.files.iter().find_map(|f| f.path.parent().filter(|p| p.extension().is_some_and(|e| e == "lightair-take")));
        return folder
            .and_then(Media::from_take)
            .map(|mut m| {
                m.name = item.title.clone();
                m.item = Some(item.id);
                m
            })
            .into_iter()
            .collect();
    }
    let playable = |p: &Path| {
        p.exists()
            && p.extension().is_some_and(|e| {
                ["mp4", "mov", "mkv", "m4v", "wav", "m4a", "mp3", "flac", "aac", "png", "jpg", "jpeg", "gif"].iter().any(|x| e.eq_ignore_ascii_case(x))
            })
    };
    let files: Vec<&ItemFile> = item.files.iter().filter(|f| playable(&f.path)).collect();
    // a recording of several formats: the first; a multitrack set: every file
    let pick: Vec<&ItemFile> = if item.kind == Kind::Multitrack { files } else { files.into_iter().take(1).collect() };
    pick.into_iter()
        .filter_map(|f| {
            let mut m = Media::from_file(&f.path).ok()?;
            m.item = Some(item.id);
            if item.kind != Kind::Multitrack {
                m.name = item.title.clone();
            }
            Some(m)
        })
        .collect()
}

fn kind_icon(k: Kind) -> &'static str {
    match k {
        Kind::Recording => "video",
        Kind::Take => "mouse-pointer-2",
        Kind::Clip => "scissors",
        Kind::Multitrack => "audio-lines",
        Kind::Export => "film",
    }
}

impl Editor {
    pub fn new(studio: Entity<Studio>) -> Self {
        let (tx, rx) = mpsc::channel();
        // cleaned voices are kept with the app's data
        lightair_render::sound::set_cache(lightair_platform::paths::data_dir().join("cache").join("sound"));
        Editor {
            studio,
            workspace: None,
            project: None,
            library: Vec::new(),
            library_seen: None,
            undo: Vec::new(),
            redo: Vec::new(),
            drag_before: None,
            renderer: Arc::new(Mutex::new(None)),
            pending: Arc::new(Mutex::new(Pending { seq: None, preview: Some(PREVIEW_BOXES[2]) })),
            retired: None,
            player_px: Default::default(),
            preview_box: Some(PREVIEW_BOXES[2]),
            preview: None,
            wanted: None,
            busy: false,
            playing: None,
            speaker: None,
            exporting: None,
            note: None,
            playhead: 0,
            selected: None,
            zoom: 1.0,
            scroll_ms: 0.0,
            drag: None,
            timeline_bounds: Default::default(),
            tab: InspectorTab::Video,
            left: LeftTab::Media,
            text_edit: None,
            text_before: None,
            captioning: false,
            downloading: false,
            peaks: Default::default(),
            cleaning: false,
            proxying: None,
            proxy_failed: Vec::new(),
            strips: Default::default(),
            shown: 0,
            shown_since: None,
            naming: None,
            watching: false,
            tx,
            rx,
            _subscriptions: Vec::new(),
        }
    }

    /// Shown: the workspace's last project opens (the workspace may have changed since).
    pub fn show(&mut self, cx: &mut Context<Self>) {
        let ws = self.studio.read(cx).project_dir();
        if ws != self.workspace {
            self.stop(cx);
            self.workspace = ws.clone();
            self.project = None;
            self.preview = None;
            self.library_seen = None;
            *self.renderer.lock().unwrap() = None;
            if let Some(ws) = &ws
                && let Some(last) = EditProject::list(ws).first()
            {
                self.open_project(last.id, cx);
            }
        }
        self.watch(cx);
        cx.notify();
    }

    /// A Library item, added at the end of the open project (a new one if none is open).
    pub fn add_item(&mut self, id: Id, cx: &mut Context<Self>) {
        self.show(cx);
        let Some(ws) = &self.workspace else { return };
        if let Some(item) = Index::load(ws).get(id).cloned() {
            self.add(MediaSource::Item(item), None, None, cx);
        }
    }

    /// Takes what the threads send.
    fn watch(&mut self, cx: &mut Context<Self>) {
        if self.watching {
            return;
        }
        self.watching = true;
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(std::time::Duration::from_millis(30)).await;
                let alive = this.update(cx, |this, cx| {
                    let mut changed = false;
                    while let Ok(done) = this.rx.try_recv() {
                        changed = true;
                        match done {
                            Done::Frame(t, image) => {
                                this.busy = false;
                                if this.playing.is_some() {
                                    this.playhead = t;
                                }
                                if this.playing.is_some() {
                                    this.shown += 1;
                                    this.shown_since.get_or_insert_with(std::time::Instant::now);
                                }
                                if let Some(image) = image {
                                    // the frame before last leaves the GPU's atlas (the last one
                                    // may still be on screen until the next draw)
                                    let last = this.preview.replace(image);
                                    if let Some(gone) = std::mem::replace(&mut this.retired, last) {
                                        cx.drop_image(gone, None);
                                    }
                                }
                            }
                            Done::Progress(p) => this.exporting = Some(p),
                            Done::Exported(r) => {
                                this.exporting = None;
                                this.note = Some(match r {
                                    Ok(p) => format!("Exported {} into the Library", p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()).into(),
                                    Err(e) => format!("The export stopped: {e}").into(),
                                });
                                this.studio.update(cx, |s, _| s.library_version += 1);
                            }
                            Done::Media(media, at, track) => this.take_media(media, at, track, cx),
                            Done::Captions(r) => this.take_captions(r, cx),
                            Done::Peaks(f, p) => {
                                this.peaks.insert(f, p);
                            }
                            Done::Cleaned(r) => this.take_cleaned(r, cx),
                            Done::ProxyProgress(f) => {
                                if let Some(p) = &mut this.proxying {
                                    p.1 = f;
                                }
                            }
                            Done::Proxied(file, r) => this.take_proxy(file, r, cx),
                            Done::Strip(f, s) => {
                                this.strips.insert(f, s);
                            }
                            Done::Ended => {
                                this.playing = None;
                                this.speaker = None;
                            }
                            Done::Downloaded(r) => {
                                this.downloading = false;
                                this.note = Some(match r {
                                    Ok(()) => "The speech model is ready: make captions now.".into(),
                                    Err(e) => format!("The speech model couldn't be downloaded: {e}").into(),
                                });
                            }
                        }
                    }
                    this.pump();
                    if changed {
                        cx.notify();
                    }
                });
                if alive.is_err() {
                    break;
                }
            }
        })
        .detach();
    }

    // ------------------------------------------------------------ projects

    fn open_project(&mut self, id: Id, cx: &mut Context<Self>) {
        let Some(ws) = self.workspace.clone() else { return };
        let Ok(p) = EditProject::load(&ws, id) else {
            self.note = Some("That project couldn't be read.".into());
            return;
        };
        self.stop(cx);
        self.undo.clear();
        self.redo.clear();
        self.selected = None;
        self.playhead = 0;
        self.scroll_ms = 0.0;
        self.preview = None;
        let seq = p.sequence.clone();
        self.project = Some(p);
        self.pending.lock().unwrap().seq = None;
        let (r, tx, pending) = (self.renderer.clone(), self.tx.clone(), self.pending.clone());
        self.busy = true;
        std::thread::spawn(move || {
            let mut guard = r.lock().unwrap();
            let preview = pending.lock().unwrap().preview;
            *guard = SequenceRenderer::new_with(&seq, preview).ok();
            let frame = guard.as_mut().and_then(|r| {
                catch_up(r, &pending);
                image_of(r.frame(0), r.out_size())
            });
            drop(guard);
            let _ = tx.send(Done::Frame(0, frame));
        });
        self.make_proxies(cx);
        cx.notify();
    }

    /// A new project (1080p, 30 fps) called `name`, opened.
    pub fn new_project(&mut self, name: &str, cx: &mut Context<Self>) -> Option<Id> {
        let ws = self.workspace.clone().or_else(|| self.studio.read(cx).project_dir())?;
        self.workspace = Some(ws.clone());
        let name = if name.trim().is_empty() { "Untitled".to_string() } else { name.trim().to_string() };
        let mut p = EditProject::new(&name, (1920, 1080), 30);
        p.save(&ws).ok()?;
        let id = p.id;
        self.open_project(id, cx);
        Some(id)
    }

    fn start_naming(&mut self, rename: Option<Id>, window: &mut Window, cx: &mut Context<Self>) {
        let current = rename.and(self.project.as_ref().map(|p| p.name.clone())).unwrap_or_default();
        let input = cx.new(|cx| InputState::new(window, cx).placeholder("Project name"));
        input.update(cx, |s, cx| {
            s.set_value(current, window, cx);
            s.focus(window, cx);
            s.select_all(window, cx);
        });
        let sub = cx.subscribe_in(&input, window, move |this, input, ev: &InputEvent, _, cx| match ev {
            InputEvent::PressEnter { .. } => {
                let name = input.read(cx).value().trim().to_string();
                this.naming = None;
                match rename {
                    Some(_) => {
                        if let (Some(ws), Some(p)) = (this.workspace.clone(), this.project.as_mut())
                            && !name.is_empty()
                        {
                            p.name = name;
                            let _ = p.save(&ws);
                        }
                    }
                    None => {
                        this.new_project(&name, cx);
                    }
                }
                cx.notify();
            }
            InputEvent::Blur => {
                this.naming = None;
                cx.notify();
            }
            _ => {}
        });
        self._subscriptions.push(sub);
        self.naming = Some((rename, input));
        cx.notify();
    }

    fn delete_project(&mut self, cx: &mut Context<Self>) {
        let (Some(ws), Some(p)) = (self.workspace.clone(), self.project.take()) else { return };
        self.stop(cx);
        let _ = EditProject::delete(&ws, p.id);
        *self.renderer.lock().unwrap() = None;
        self.preview = None;
        self.note = Some(format!("Deleted “{}” (its media stay in the Library)", p.name).into());
        if let Some(next) = EditProject::list(&ws).first() {
            self.open_project(next.id, cx);
        }
        cx.notify();
    }

    // ------------------------------------------------------------ changes

    fn seq(&self) -> Option<&Sequence> {
        self.project.as_ref().map(|p| &p.sequence)
    }

    /// A change to the sequence: saved, undoable, drawn again.
    fn change(&mut self, f: impl FnOnce(&mut Sequence), cx: &mut Context<Self>) {
        let Some(p) = &mut self.project else { return };
        let before = p.sequence.clone();
        f(&mut p.sequence);
        if p.sequence == before {
            return;
        }
        self.undo.push(before);
        self.redo.clear();
        self.settle(cx);
    }

    fn settle(&mut self, cx: &mut Context<Self>) {
        let (Some(ws), Some(p)) = (self.workspace.clone(), self.project.as_mut()) else { return };
        let _ = p.save(&ws);
        self.pending.lock().unwrap().seq = Some(p.sequence.clone());
        self.request(self.playhead);
        self.clean_voices(cx);
        self.make_proxies(cx);
        cx.notify();
    }

    pub fn undo(&mut self, back: bool, cx: &mut Context<Self>) {
        let Some(p) = &mut self.project else { return };
        let (from, to) = if back { (&mut self.undo, &mut self.redo) } else { (&mut self.redo, &mut self.undo) };
        let Some(s) = from.pop() else { return };
        to.push(std::mem::replace(&mut p.sequence, s));
        self.settle(cx);
    }

    pub fn delete_selected(&mut self, cx: &mut Context<Self>) {
        if let Some(c) = self.selected.take() {
            self.change(
                |s| {
                    s.remove(c);
                    s.tidy();
                },
                cx,
            );
        }
    }

    fn split(&mut self, cx: &mut Context<Self>) {
        let t = self.playhead;
        let sel = self.selected.filter(|c| self.seq().and_then(|s| s.clip(*c)).is_some_and(|c| c.start_ms < t && t < c.end_ms()));
        self.change(
            |s| match sel {
                Some(c) => {
                    s.split(c, t);
                }
                None => {
                    s.split_all(t);
                }
            },
            cx,
        );
    }

    /// Adds a Library item or imported medium at `at` (on `track` if given), or the main track's end.
    fn add(&mut self, source: MediaSource, at: Option<u64>, track: Option<usize>, cx: &mut Context<Self>) {
        if self.project.is_none() {
            self.new_project("Untitled", cx);
        }
        match source {
            MediaSource::Media(id) => self.change(
                |s| {
                    match at {
                        Some(t) => s.place(id, t, track),
                        None => s.append(id),
                    };
                },
                cx,
            ),
            MediaSource::Content(content, place) => {
                let at = at.unwrap_or(self.playhead);
                let mut made = None;
                self.change(
                    |s| {
                        let id = s.place_content_on(content, at, 3_000, track);
                        if let Some(c) = s.clip_mut(id) {
                            c.transform = place;
                        }
                        made = Some(id);
                    },
                    cx,
                );
                self.selected = made;
                self.tab = InspectorTab::Text;
                cx.notify();
            }
            MediaSource::Item(item) => {
                // reading the files takes a moment: off the window's thread
                let tx = self.tx.clone();
                self.note = Some(format!("Adding “{}”…", item.title).into());
                std::thread::spawn(move || {
                    let _ = tx.send(Done::Media(media_of(&item), at, track));
                });
                cx.notify();
            }
        }
    }

    fn take_media(&mut self, media: Vec<Media>, at: Option<u64>, track: Option<usize>, cx: &mut Context<Self>) {
        if media.is_empty() {
            self.note = Some("Its files couldn't be read (moved? Locate them in the Library).".into());
            return;
        }
        self.note = None;
        let ws = self.workspace.clone();
        self.change(
            |s| {
                for mut m in media {
                    // a take's look lives in the workspace (the screen recording look)
                    if m.take.is_some()
                        && let (Some(ws), Some(item)) = (&ws, m.item)
                    {
                        m.look = Some(ws.join("recordings").join("edits").join(format!("{item}.json")));
                    }
                    let id = s.add_media(m);
                    match at {
                        Some(u64::MAX) => None,
                        Some(t) => s.place(id, t, track),
                        None => s.append(id),
                    };
                }
            },
            cx,
        );
    }

    fn import(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.project.is_none() {
            self.new_project("Untitled", cx);
        }
        let picked = cx.prompt_for_paths(PathPromptOptions { files: true, directories: false, multiple: true, prompt: Some("Import".into()) });
        let tx = self.tx.clone();
        cx.spawn_in(window, async move |_, _| {
            let Ok(Ok(Some(paths))) = picked.await else { return };
            std::thread::spawn(move || {
                let media: Vec<Media> = paths.iter().filter_map(|p| Media::from_file(p).ok()).collect();
                let _ = tx.send(Done::Media(media, Some(u64::MAX), None));
            });
        })
        .detach();
    }

    // ------------------------------------------------------------ the player

    /// Frames at the size the player shows them (the smallest preview size that covers it).
    fn fit_preview(&mut self) {
        let (w, h) = self.player_px.get();
        if w < 2.0 {
            return;
        }
        // larger than them all: the sequence's own size (a box it fits in)
        let fits = PREVIEW_BOXES.iter().copied().find(|b| b.0 as f32 >= w && b.1 as f32 >= h).or(Some((7680, 4320)));
        if fits != self.preview_box {
            self.preview_box = fits;
            self.pending.lock().unwrap().preview = fits;
            if self.playing.is_none() {
                self.request(self.playhead);
            }
        }
    }

    fn request(&mut self, t: u64) {
        self.wanted = Some(t);
        self.pump();
    }

    fn pump(&mut self) {
        if self.busy || self.playing.is_some() {
            return;
        }
        let Some(t) = self.wanted.take() else { return };
        let (r, tx, pending) = (self.renderer.clone(), self.tx.clone(), self.pending.clone());
        self.busy = true;
        std::thread::spawn(move || {
            let mut guard = r.lock().unwrap();
            let frame = guard.as_mut().and_then(|r| {
                catch_up(r, &pending);
                image_of(r.frame(t), r.out_size())
            });
            let _ = tx.send(Done::Frame(t, frame));
        });
    }

    pub fn play(&mut self, cx: &mut Context<Self>) {
        if self.playing.is_some() {
            self.stop(cx);
            return;
        }
        let end = self.seq().map_or(0, |s| s.duration_ms());
        if self.playhead + 100 >= end {
            self.playhead = 0;
        }
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (r, tx, from, flag, pending) = (self.renderer.clone(), self.tx.clone(), self.playhead, stop.clone(), self.pending.clone());
        self.playing = Some(stop);
        self.speaker = self.seq().and_then(|s| speaker::Speaker::start(s, from));
        let clock = self.speaker.as_ref().map(|s| s.clock());
        let step = 1000 / self.seq().map_or(30, |s| s.fps.max(1)) as u64;
        // frames are made ahead on one thread and shown in time on another
        let (made, ready) = std::sync::mpsc::sync_channel::<(u64, Option<Arc<RenderImage>>)>(4);
        let render_flag = flag.clone();
        std::thread::spawn(move || {
            let mut guard = r.lock().unwrap();
            let Some(r) = guard.as_mut() else { return };
            catch_up(r, &pending);
            let size = r.out_size();
            r.play(from, |t, rgba| made.send((t, image_of(rgba, size))).is_ok() && !render_flag.load(std::sync::atomic::Ordering::Relaxed));
        });
        std::thread::spawn(move || {
            let stopped = || flag.load(std::sync::atomic::Ordering::Relaxed);
            // the clock (and the sound) start with the first frame
            let Ok(first) = ready.recv() else { return };
            if let Some(c) = &clock {
                c.start();
            }
            let start = std::time::Instant::now();
            // in time with the sound when there is some, else the clock
            let now = || match clock.as_ref().and_then(|c| c.heard_ms()) {
                Some(ms) => ms,
                // the sound starting: a moment's wait for it
                None if clock.is_some() && start.elapsed() < std::time::Duration::from_secs(1) => 0,
                None => start.elapsed().as_millis() as u64,
            };
            let mut shown = std::time::Instant::now();
            for (t, image) in std::iter::once(first).chain(ready.iter()) {
                let due = t.saturating_sub(from);
                loop {
                    let n = now();
                    if n >= due || stopped() {
                        break;
                    }
                    std::thread::sleep(std::time::Duration::from_millis((due - n).min(10)));
                }
                if stopped() {
                    return;
                }
                // more than two frames late: skipped (unless nothing has been shown lately)
                if now() > due + 2 * step && shown.elapsed() < std::time::Duration::from_millis(100) {
                    continue;
                }
                shown = std::time::Instant::now();
                let _ = tx.send(Done::Frame(t, image));
            }
            if !stopped() {
                let _ = tx.send(Done::Ended);
            }
        });
        cx.notify();
    }

    fn stop(&mut self, cx: &mut Context<Self>) {
        if let Some(s) = self.playing.take() {
            s.store(true, std::sync::atomic::Ordering::Relaxed);
        }
        self.speaker = None;
        self.busy = false;
        cx.notify();
    }

    fn seek(&mut self, t: u64, cx: &mut Context<Self>) {
        self.stop(cx);
        self.playhead = t;
        self.request(t);
        cx.notify();
    }

    fn export(&mut self, codec: Codec, cx: &mut Context<Self>) {
        if self.exporting.is_some() {
            return;
        }
        self.stop(cx);
        let (Some(p), Some(ws)) = (&self.project, self.workspace.clone()) else { return };
        if p.sequence.duration_ms() == 0 {
            self.note = Some("Put something on the timeline first.".into());
            cx.notify();
            return;
        }
        let folder = self.studio.read(cx).recording_folder();
        let ext = match codec {
            Codec::ProRes => "mov",
            Codec::Gif => "gif",
            Codec::H264 => "mp4",
        };
        let base = p.name.replace(['/', ':', '\\'], "-");
        let out = (1..)
            .map(|n| if n == 1 { folder.join(format!("{base}.{ext}")) } else { folder.join(format!("{base} {n}.{ext}")) })
            .find(|p| !p.exists())
            .unwrap_or_else(|| folder.join(format!("{base}.{ext}")));
        let (seq, tx, title, duration) = (p.sequence.clone(), self.tx.clone(), p.name.clone(), p.sequence.duration_ms());
        self.exporting = Some(0.0);
        std::thread::spawn(move || {
            let result = (|| {
                let _ = std::fs::create_dir_all(out.parent().unwrap_or(Path::new(".")));
                let mut r = SequenceRenderer::new(&seq).map_err(|e| e.to_string())?;
                let progress = tx.clone();
                r.export(&out, codec, |p| {
                    let _ = progress.send(Done::Progress(p));
                    true
                })
                .map_err(|e| e.to_string())?;
                let mut item = Item::new(Kind::Export, title, vec![ItemFile { path: out.clone(), format: None }], chrono::Utc::now().timestamp_millis());
                item.duration_ms = Some(duration);
                let _ = Index::add_to(&ws, item);
                Ok(out.clone())
            })();
            let _ = tx.send(Done::Exported(result));
        });
        cx.notify();
    }

    // ------------------------------------------------------------ the timeline's maths

    fn pps(&self) -> f32 {
        PPS * self.zoom
    }

    /// A window x as a time on the timeline.
    fn time_at(&self, x: Pixels) -> Option<u64> {
        let b = (*self.timeline_bounds.borrow())?;
        let px = f32::from(x - b.origin.x) - HEADER_W;
        Some(((px / self.pps() * 1000.0) + self.scroll_ms).max(0.0) as u64)
    }

    fn x_of(&self, ms: u64) -> f32 {
        (ms as f32 - self.scroll_ms) / 1000.0 * self.pps()
    }

    /// The track under a window y.
    fn track_at(&self, y: Pixels) -> Option<usize> {
        let b = (*self.timeline_bounds.borrow())?;
        let seq = self.seq()?;
        let mut top = f32::from(b.origin.y) + RULER_H;
        let y = f32::from(y);
        for (i, t) in seq.tracks.iter().enumerate() {
            let h = if t.kind == TrackKind::Video { VIDEO_H } else { AUDIO_H };
            if y >= top && y < top + h {
                return Some(i);
            }
            top += h;
        }
        None
    }

    /// `t`, pulled to a snap point (clip edges, the playhead) within 8 pixels.
    fn snap(&self, t: i64, except: Option<Id>) -> u64 {
        let Some(seq) = self.seq() else { return t.max(0) as u64 };
        let mut points = seq.snap_points(except);
        points.push(self.playhead);
        let reach = 8.0 / self.pps() * 1000.0;
        points
            .into_iter()
            .map(|p| (p, (p as i64 - t).abs()))
            .filter(|(_, d)| (*d as f32) < reach)
            .min_by_key(|(_, d)| *d)
            .map_or(t.max(0) as u64, |(p, _)| p)
    }

    fn begin_drag(&mut self, d: Drag) {
        self.drag_before = self.project.as_ref().map(|p| p.sequence.clone());
        self.drag = Some(d);
    }

    fn drag_to(&mut self, pos: Point<Pixels>, cx: &mut Context<Self>) {
        let Some(drag) = self.drag else { return };
        let Some(t) = self.time_at(pos.x) else { return };
        match drag {
            Drag::Scrub => {
                self.playhead = t.min(self.seq().map_or(0, |s| s.duration_ms()));
                self.request(self.playhead);
            }
            Drag::Move { clip, grab_ms } => {
                let len = self.seq().and_then(|s| s.clip(clip)).map_or(0, |c| c.len_ms());
                let start = self.snap(t as i64 - grab_ms, Some(clip));
                // or its end snaps
                let by_end = self.snap(start as i64 + len as i64, Some(clip));
                let start = if by_end != start + len { by_end.saturating_sub(len) } else { start };
                let track = self.track_at(pos.y);
                if let Some(p) = &mut self.project {
                    let (cur, _) = p.sequence.find(clip).unwrap_or((0, 0));
                    let kind = p.sequence.tracks[cur].kind;
                    let to = track.filter(|t| p.sequence.tracks.get(*t).is_some_and(|tr| tr.kind == kind)).unwrap_or(cur);
                    if !p.sequence.move_clip(clip, to, start) {
                        p.sequence.move_clip(clip, cur, start);
                    }
                }
            }
            Drag::TrimStart(clip) => {
                let t = self.snap(t as i64, Some(clip));
                if let Some(p) = &mut self.project {
                    p.sequence.trim_start(clip, t);
                }
            }
            Drag::TrimEnd(clip) => {
                let t = self.snap(t as i64, Some(clip));
                if let Some(p) = &mut self.project {
                    p.sequence.trim_end(clip, t);
                }
            }
        }
        cx.notify();
    }

    /// A drag ended: what it changed is one undo step.
    fn drag_end(&mut self, cx: &mut Context<Self>) {
        let Some(drag) = self.drag.take() else { return };
        if matches!(drag, Drag::Scrub) {
            return;
        }
        if let (Some(before), Some(p)) = (self.drag_before.take(), &mut self.project) {
            p.sequence.tidy();
            if before != p.sequence {
                self.undo.push(before);
                self.redo.clear();
            }
        }
        self.settle(cx);
    }

    /// A take's look, changed (saved where the media says, drawn again).
    fn change_look(&mut self, media: Id, f: impl FnOnce(&mut Edit), cx: &mut Context<Self>) {
        let Some(m) = self.seq().and_then(|s| s.media(media)).cloned() else { return };
        let Some(path) = m.look.clone().or_else(|| m.take.as_ref().map(|t| t.join("edit.json"))) else { return };
        let mut e = Edit::load(&path).unwrap_or_else(|_| {
            let mut e = Edit::new(m.duration_ms.max(1));
            if let Some(log) = m.take.as_ref().and_then(|t| lightair_store::input_log::Log::read(&t.join("input.bin")).ok()) {
                e.zooms = zoom::auto(&log, m.duration_ms);
            }
            e
        });
        f(&mut e);
        if let Some(d) = path.parent() {
            let _ = std::fs::create_dir_all(d);
        }
        let _ = e.save(&path);
        // playback holds the renderer: stopped first
        self.stop(cx);
        if let Some(r) = self.renderer.lock().unwrap().as_mut() {
            r.forget_take(media);
        }
        self.request(self.playhead);
        cx.notify();
    }

    fn look_of(&self, media: &Media) -> Edit {
        media
            .look
            .clone()
            .or_else(|| media.take.as_ref().map(|t| t.join("edit.json")))
            .and_then(|p| Edit::load(&p).ok())
            .unwrap_or_else(|| Edit::new(media.duration_ms.max(1)))
    }

    // ------------------------------------------------------------ view

    fn chips<T: Copy + PartialEq + 'static>(
        &self,
        id: &'static str,
        options: &[(&'static str, T)],
        current: T,
        on: impl Fn(&mut Self, T, &mut Context<Self>) + Copy + 'static,
        cx: &mut Context<Self>,
    ) -> Div {
        let mut row = div().flex().flex_wrap().gap_1();
        for (n, (label, v)) in options.iter().enumerate() {
            let v = *v;
            let sel = v == current;
            row = row.child(
                div()
                    .id((id, n))
                    .px_2()
                    .py_0p5()
                    .rounded(px(6.))
                    .text_size(px(12.))
                    .cursor_pointer()
                    .when(sel, |d| d.bg(tok::bg_selected()).text_color(tok::text()))
                    .when(!sel, |d| d.text_color(tok::text_secondary()))
                    .hover(|d| d.bg(tok::bg_selected()))
                    .child(*label)
                    .on_click(cx.listener(move |this, _, _, cx| on(this, v, cx))),
            );
        }
        row
    }

    /// A labelled value with − and + beside it.
    fn stepper(
        &self,
        id: &'static str,
        label: &'static str,
        value: String,
        step: impl Fn(&mut Self, f32, &mut Context<Self>) + Copy + 'static,
        cx: &mut Context<Self>,
    ) -> Div {
        div()
            .flex()
            .items_center()
            .gap_2()
            .child(div().w(px(76.)).text_size(px(12.)).text_color(tok::text_secondary()).child(label))
            .child(
                div()
                    .flex_1()
                    .px_2()
                    .py_0p5()
                    .rounded(px(6.))
                    .bg(tok::bg_app())
                    .text_size(px(12.))
                    .font_family("Menlo")
                    .child(value),
            )
            .child(ui::icon_button((id, 0usize), "minus", tok::text_secondary()).on_click(cx.listener(move |this, _, _, cx| step(this, -1.0, cx))))
            .child(ui::icon_button((id, 1usize), "plus", tok::text_secondary()).on_click(cx.listener(move |this, _, _, cx| step(this, 1.0, cx))))
    }

    fn top_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let me = cx.entity().downgrade();
        let (ws, current) = (self.workspace.clone(), self.project.as_ref().map(|p| p.id));
        let name = self.project.as_ref().map_or("No project".to_string(), |p| p.name.clone());
        let picker = Button::new("ed-project").label(name).small().ghost().dropdown_caret(true).dropdown_menu(move |mut menu, _, _| {
            for p in ws.as_ref().map(|w| EditProject::list(w)).unwrap_or_default() {
                let (me, id) = (me.clone(), p.id);
                menu = menu.item(PopupMenuItem::new(p.name).checked(current == Some(p.id)).on_click(move |_, _, cx| {
                    if let Some(me) = me.upgrade() {
                        me.update(cx, |this, cx| this.open_project(id, cx));
                    }
                }));
            }
            let (a, b, c) = (me.clone(), me.clone(), me.clone());
            menu.separator()
                .item(PopupMenuItem::new("New Project…").icon(ui::icon("plus")).on_click(move |_, w, cx| {
                    if let Some(me) = a.upgrade() {
                        me.update(cx, |this, cx| this.start_naming(None, w, cx));
                    }
                }))
                .item(PopupMenuItem::new("Rename…").icon(ui::icon("pencil")).disabled(current.is_none()).on_click(move |_, w, cx| {
                    if let Some(me) = b.upgrade() {
                        me.update(cx, |this, cx| {
                            let id = this.project.as_ref().map(|p| p.id);
                            this.start_naming(id, w, cx)
                        });
                    }
                }))
                .separator()
                .item(PopupMenuItem::new("Delete Project").icon(ui::icon("trash")).disabled(current.is_none()).on_click(move |_, _, cx| {
                    if let Some(me) = c.upgrade() {
                        me.update(cx, |this, cx| this.delete_project(cx));
                    }
                }))
        });
        let export = match self.exporting {
            Some(p) => Button::new("ed-export").label(format!("Exporting {:.0}%", p * 100.0)).small().outline().loading(true).into_any_element(),
            None => {
                let me = cx.entity().downgrade();
                Button::new("ed-export")
                    .label("Export")
                    .icon(ui::icon("upload"))
                    .small()
                    .primary()
                    .dropdown_menu(move |menu, _, _| {
                        let go = |codec: Codec| {
                            let me = me.clone();
                            move |_: &ClickEvent, _: &mut Window, cx: &mut App| {
                                if let Some(me) = me.upgrade() {
                                    me.update(cx, |this, cx| this.export(codec, cx));
                                }
                            }
                        };
                        menu.item(PopupMenuItem::new("Video (MP4)").on_click(go(Codec::H264)))
                            .item(PopupMenuItem::new("ProRes (MOV)").on_click(go(Codec::ProRes)))
                            .item(PopupMenuItem::new("GIF").on_click(go(Codec::Gif)))
                    })
                    .into_any_element()
            }
        };
        div()
            .flex()
            .items_center()
            .gap_2()
            .px_3()
            .h(px(40.))
            .border_b_1()
            .border_color(tok::border())
            .child(div().p_0p5().rounded(px(8.)).bg(tok::bg_panel()).border_1().border_color(tok::border()).child(picker))
            .when_some(self.naming.as_ref(), |d, (_, input)| d.child(div().w(px(220.)).child(TextInput::new(input).small())))
            .child(div().flex_1())
            .child(ui::icon_button("ed-undo", "undo-2", tok::text_secondary()).on_click(cx.listener(|this, _, _, cx| this.undo(true, cx))))
            .child(ui::icon_button("ed-redo", "redo-2", tok::text_secondary()).on_click(cx.listener(|this, _, _, cx| this.undo(false, cx))))
            .child(export)
    }

    fn media_panel(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let card = |id: SharedString, title: String, detail: String, icon: &'static str, thumb: Option<PathBuf>, source: MediaSource, cx: &mut Context<Self>| {
            let drag = DraggedMedia { label: title.clone().into(), source: source.clone() };
            div()
                .id(id.clone())
                .flex()
                .flex_col()
                .gap_1()
                .w(px(122.))
                .cursor_grab()
                .child(
                    div()
                        .relative()
                        .w_full()
                        .h(px(68.))
                        .rounded(px(6.))
                        .overflow_hidden()
                        .bg(tok::bg_canvas_well())
                        .flex()
                        .items_center()
                        .justify_center()
                        .map(|d| match thumb {
                            Some(p) => d.child(img(p).size_full().object_fit(ObjectFit::Cover)),
                            None => d.child(ui::icon(icon).size(px(20.)).text_color(tok::text_tertiary())),
                        })
                        .child(
                            div()
                                .id((id.clone(), 1usize))
                                .absolute()
                                .right(px(4.))
                                .bottom(px(4.))
                                .size(px(20.))
                                .rounded_full()
                                .bg(rgba(0x000000c0))
                                .border_1()
                                .border_color(rgba(0xffffff60))
                                .flex()
                                .items_center()
                                .justify_center()
                                .cursor_pointer()
                                .hover(|d| d.bg(rgba(0x000000ff)))
                                .child(ui::icon("plus").size(px(12.)).text_color(rgb(0xFFFFFF)))
                                .on_click(cx.listener(move |this, _, _, cx| this.add(source.clone(), None, None, cx))),
                        ),
                )
                .child(div().text_size(px(11.)).overflow_hidden().text_ellipsis().whitespace_nowrap().child(title))
                .child(div().text_size(px(10.)).text_color(tok::text_tertiary()).child(detail))
                .on_drag(drag, |d, _, _, cx| cx.new(|_| d.clone()))
        };
        let mut grid = div().flex().flex_wrap().gap_2();
        for (n, item) in self.library.iter().enumerate() {
            let detail = format!("{} · {}", item.kind.label(), item.duration_ms.map_or("—".into(), short));
            let thumb = item.thumbnail.clone().filter(|t| t.exists());
            grid = grid.child(card(
                SharedString::from(format!("ed-lib-{n}")),
                item.title.clone(),
                detail,
                kind_icon(item.kind),
                thumb,
                MediaSource::Item(item.clone()),
                cx,
            ));
        }
        let imported: Vec<Media> = self.seq().map(|s| s.media.iter().filter(|m| m.item.is_none()).cloned().collect()).unwrap_or_default();
        let mut own = div().flex().flex_wrap().gap_2();
        for (n, m) in imported.iter().enumerate() {
            let icon = match m.kind {
                MediaKind::Video => "film",
                MediaKind::Audio => "music",
                MediaKind::Image => "image",
            };
            let thumb = (m.kind == MediaKind::Image).then(|| m.path.clone());
            own = own.child(card(SharedString::from(format!("ed-imp-{n}")), m.name.clone(), short(m.duration_ms), icon, thumb, MediaSource::Media(m.id), cx));
        }
        let tabs: [(&'static str, &'static str, LeftTab); 5] = [
            ("Media", "film", LeftTab::Media),
            ("Audio", "music", LeftTab::Audio),
            ("Text", "type", LeftTab::Text),
            ("Shapes", "shapes", LeftTab::Shapes),
            ("Captions", "captions", LeftTab::Captions),
        ];
        let mut bar = div().flex().items_center().gap_0p5().px_2().h(px(48.)).border_b_1().border_color(tok::border());
        for (n, (name, icon, tab)) in tabs.into_iter().enumerate() {
            let on = self.left == tab;
            bar = bar.child(
                div()
                    .id(("ed-left", n))
                    .flex()
                    .flex_col()
                    .items_center()
                    .gap_0p5()
                    .w(px(54.))
                    .py_1()
                    .rounded(px(6.))
                    .cursor_pointer()
                    .text_size(px(11.))
                    .text_color(if on { tok::text() } else { tok::text_secondary() })
                    .when(on, |d| d.bg(tok::bg_selected()))
                    .hover(|d| d.bg(tok::bg_selected()))
                    .child(ui::icon(icon).size(px(15.)))
                    .child(name)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.left = tab;
                        cx.notify();
                    })),
            );
        }
        let body = match self.left {
            LeftTab::Media => div()
                .child(
                    div()
                        .flex()
                        .items_center()
                        .child(div().flex_1().child(heading("THIS WORKSPACE")))
                        .child(Button::new("ed-import").label("Import").icon(ui::icon("plus")).xsmall().outline().on_click(cx.listener(|this, _, w, cx| this.import(w, cx)))),
                )
                .child(if self.library.is_empty() { hint("Recordings, takes and clips appear here.") } else { grid })
                .when(!imported.is_empty(), |d| d.child(heading("IMPORTED")).child(own)),
            LeftTab::Audio => self.audio_tab(cx),
            LeftTab::Text => self.text_tab(cx),
            LeftTab::Shapes => self.shapes_tab(cx),
            LeftTab::Captions => self.captions_tab(cx),
        };
        div()
            .flex()
            .flex_col()
            .w(px(290.))
            .flex_none()
            .border_r_1()
            .border_color(tok::border())
            .bg(tok::bg_panel())
            .child(bar)
            .child(div().id("ed-media").flex_1().min_h_0().overflow_y_scroll().px_3().pb_3().pt_1().child(body))
    }

    fn player(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let duration = self.seq().map_or(0, |s| s.duration_ms());
        let size = self.seq().map_or((1920, 1080), |s| s.size);
        div()
            .flex()
            .flex_col()
            .flex_1()
            .min_w_0()
            .child(
                div()
                    .relative()
                    .flex_1()
                    .min_h_0()
                    .m_2()
                    .rounded(px(6.))
                    .overflow_hidden()
                    .bg(rgb(0x000000))
                    .child({
                        let cell = self.player_px.clone();
                        canvas(move |b, window, _| cell.set((f32::from(b.size.width) * window.scale_factor(), f32::from(b.size.height) * window.scale_factor())), |_, _, _, _| {})
                            .absolute()
                            .size_full()
                    })
                    .when_some(self.preview.clone(), |d, img_| d.child(img(ImageSource::Render(img_)).absolute().size_full().object_fit(ObjectFit::Contain)))
                    .when(self.project.is_none(), |d| {
                        d.flex().items_center().justify_center().child(
                            div()
                                .flex()
                                .flex_col()
                                .items_center()
                                .gap_2()
                                .child(div().text_color(tok::text_secondary()).child("No project open"))
                                .child(Button::new("ed-first").label("New project").small().primary().on_click(cx.listener(|this, _, w, cx| this.start_naming(None, w, cx)))),
                        )
                    }),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .px_3()
                    .pb_2()
                    .child(
                        div()
                            .flex_1()
                            .text_size(px(12.))
                            .font_family("Menlo")
                            .child(div().flex().gap_1().child(div().text_color(tok::accent_primary()).child(clock(self.playhead))).child(div().text_color(tok::text_tertiary()).child(format!("/ {}", clock(duration))))),
                    )
                    .child(ui::icon_button("ed-play", if self.playing.is_some() { "pause" } else { "play" }, tok::text()).on_click(cx.listener(|this, _, _, cx| this.play(cx))))
                    .child(div().flex_1().flex().justify_end().text_size(px(11.)).text_color(tok::text_tertiary()).child(format!("{}×{} · {} fps", size.0, size.1, self.seq().map_or(30, |s| s.fps)))),
            )
    }

    fn inspector(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let clip = self.selected.and_then(|c| self.seq().and_then(|s| s.clip(c)).cloned());
        let media = clip.as_ref().and_then(|c| self.seq().and_then(|s| s.media(c.media)).cloned());
        let mut body = div().flex().flex_col().gap_2().p_3();
        match (clip, media) {
            (Some(c), None) if c.content.is_some() => {
                let name = match &c.content {
                    Some(Content::Shape(_)) => "Shape",
                    _ => "Text",
                };
                let tabs = [(name, InspectorTab::Text), ("Transform", InspectorTab::Video), ("Transition", InspectorTab::Transition)];
                let tab = if tabs.iter().any(|t| t.1 == self.tab) { self.tab } else { InspectorTab::Text };
                body = body.child(div().text_size(px(13.)).font_weight(FontWeight::SEMIBOLD).overflow_hidden().text_ellipsis().child(c.content.as_ref().map(looks::content_name).unwrap_or_default()));
                body = body.child(self.chips("ed-tabs", &tabs, tab, |this, t, cx| {
                    this.tab = t;
                    cx.notify();
                }, cx));
                body = match tab {
                    InspectorTab::Video => self.transform_section(&c, body, cx),
                    InspectorTab::Transition => self.transition_section(&c, body, cx),
                    _ => self.content_section(&c, body, cx),
                };
            }
            (Some(c), Some(m)) => {
                let take = m.take.is_some();
                let audio = m.kind == MediaKind::Audio;
                let mut tabs: Vec<(&'static str, InspectorTab)> = Vec::new();
                if !audio {
                    tabs.push(("Video", InspectorTab::Video));
                }
                tabs.extend([("Audio", InspectorTab::Audio), ("Speed", InspectorTab::Speed)]);
                if !audio {
                    tabs.extend([("Filter", InspectorTab::Filter), ("Transition", InspectorTab::Transition)]);
                }
                if take {
                    tabs.push(("Screen", InspectorTab::Screen));
                }
                let tab = if tabs.iter().any(|t| t.1 == self.tab) { self.tab } else { tabs[0].1 };
                body = body.child(div().text_size(px(13.)).font_weight(FontWeight::SEMIBOLD).overflow_hidden().text_ellipsis().child(m.name.clone()));
                body = body.child(self.chips("ed-tabs", &tabs, tab, |this, t, cx| {
                    this.tab = t;
                    cx.notify();
                }, cx));
                let cid = c.id;
                match tab {
                    InspectorTab::Text | InspectorTab::Video => body = self.transform_section(&c, body, cx),
                    InspectorTab::Filter => body = self.filter_section(&c, body, cx),
                    InspectorTab::Transition => body = self.transition_section(&c, body, cx),
                    InspectorTab::Audio => body = self.audio_section(&c, &m, body, cx),
                    InspectorTab::Speed => {
                        body = body.child(label("SPEED")).child(self.chips(
                            "ed-speed",
                            &[("0.25×", 25), ("0.5×", 50), ("1×", 100), ("1.5×", 150), ("2×", 200), ("4×", 400), ("8×", 800)],
                            (c.speed * 100.0).round() as i32,
                            move |this, v, cx| {
                                let mut ok = true;
                                this.change(|s| ok = s.set_speed(cid, v as f32 / 100.0), cx);
                                if !ok {
                                    this.note = Some("Slower would run into the next clip: make room first.".into());
                                    cx.notify();
                                }
                            },
                            cx,
                        ));
                    }
                    InspectorTab::Screen => {
                        let look = self.look_of(&m);
                        let mid = m.id;
                        let mut sw = div().flex().flex_wrap().gap_1p5();
                        for (n, (colours, _)) in lightair_render::compose::BACKDROPS.iter().enumerate() {
                            let on = look.look.backdrop == n as i64;
                            sw = sw.child(
                                div()
                                    .id(("ed-bd", n))
                                    .size(px(26.))
                                    .rounded(px(6.))
                                    .border_2()
                                    .border_color(if on { tok::accent_primary() } else { tok::bg_panel() })
                                    .bg(rgb(colours[1]))
                                    .cursor_pointer()
                                    .on_click(cx.listener(move |this, _, _, cx| this.change_look(mid, |e| e.look.backdrop = n as i64, cx))),
                            );
                        }
                        body = body
                            .child(label("BACKDROP"))
                            .child(sw)
                            .child(label("PADDING"))
                            .child(self.chips("ed-pad", &[("None", 0), ("Small", 1), ("Medium", 2), ("Large", 3)], ((look.look.padding / 0.035).round() as i32).clamp(0, 3), move |this, v, cx| this.change_look(mid, |e| e.look.padding = v as f32 * 0.035, cx), cx))
                            .child(label("POINTER"))
                            .child(self.chips("ed-ps", &[("Pointer", 0i64), ("Dot", 1), ("Ring", 2), ("Hidden", -1)], if look.pointer.hidden { -1 } else { look.pointer.style }, move |this, v, cx| {
                                this.change_look(mid, |e| {
                                    e.pointer.hidden = v < 0;
                                    if v >= 0 {
                                        e.pointer.style = v;
                                    }
                                }, cx)
                            }, cx))
                            .child(self.chips("ed-psize", &[("Small", 10), ("Medium", 15), ("Large", 25)], (look.pointer.size * 10.0).round() as i32, move |this, v, cx| this.change_look(mid, |e| e.pointer.size = v as f32 / 10.0, cx), cx))
                            .child(label("CLICKS"))
                            .child(self.chips("ed-clicks", &[("None", 0i64), ("Circle", 1), ("Ripple", 2)], look.pointer.clicks, move |this, v, cx| this.change_look(mid, |e| e.pointer.clicks = v, cx), cx))
                            .child(label("ZOOMS"))
                            .child(div().text_size(px(12.)).text_color(tok::text_secondary()).child(format!("{} zooms", look.zooms.len())))
                            .child(Button::new("ed-autozoom").label("Automatic zooms").icon(ui::icon("sparkles")).xsmall().outline().on_click(cx.listener(move |this, _, _, cx| {
                                let Some(m) = this.seq().and_then(|s| s.media(mid)).cloned() else { return };
                                let Some(log) = m.take.as_ref().and_then(|t| lightair_store::input_log::Log::read(&t.join("input.bin")).ok()) else { return };
                                let auto = zoom::auto(&log, m.duration_ms);
                                this.change_look(mid, |e| {
                                    e.zooms.retain(|z| !z.auto);
                                    e.zooms.extend(auto);
                                }, cx)
                            })));
                    }
                }
            }
            _ => {
                let seq_size = self.seq().map_or((1920, 1080), |s| s.size);
                let fps = self.seq().map_or(30, |s| s.fps);
                body = body
                    .child(div().text_size(px(13.)).font_weight(FontWeight::SEMIBOLD).child("Project"))
                    .child(label("SIZE"))
                    .child(self.chips("ed-size", &[("1080p", (1920u32, 1080u32)), ("4K", (3840, 2160)), ("Vertical", (1080, 1920)), ("Square", (1080, 1080))], seq_size, |this, v, cx| this.change(|s| s.size = v, cx), cx))
                    .child(label("FRAME RATE"))
                    .child(self.chips("ed-fps", &[("24", 24u32), ("30", 30), ("60", 60)], fps, |this, v, cx| this.change(|s| s.fps = v, cx), cx));
                body = self.proxies_section(body, cx).child(div().pt_2().text_size(px(12.)).text_color(tok::text_tertiary()).child("Select a clip to change it."));
            }
        }
        div().id("ed-inspector").w(px(290.)).flex_none().border_l_1().border_color(tok::border()).bg(tok::bg_panel()).overflow_y_scroll().child(body)
    }

    fn tools(&self, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .flex()
            .items_center()
            .gap_1()
            .px_2()
            .h(px(34.))
            .border_t_1()
            .border_b_1()
            .border_color(tok::border())
            .child(ui::icon_button("ed-split", "scissors", tok::text_secondary()).tooltip(|w, cx| gpui_kit::component::tooltip::Tooltip::new("Split at the playhead").build(w, cx)).on_click(cx.listener(|this, _, _, cx| this.split(cx))))
            .child(ui::icon_button("ed-del", "trash", tok::text_secondary()).tooltip(|w, cx| gpui_kit::component::tooltip::Tooltip::new("Delete the clip").build(w, cx)).on_click(cx.listener(|this, _, _, cx| this.delete_selected(cx))))
            .child(div().flex_1())
            .when_some(self.note.clone(), |d, n| d.child(div().text_size(px(12.)).text_color(tok::text_secondary()).child(n)))
            .when_some(self.proxying.clone(), |d, (name, f)| {
                d.child(div().pl_3().text_size(px(12.)).text_color(tok::text_tertiary()).child(format!("Proxy for “{name}” {:.0}%", f * 100.0)))
            })
            .child(div().flex_1())
            .child(ui::icon_button("ed-zout", "zoom-out", tok::text_secondary()).on_click(cx.listener(|this, _, _, cx| {
                this.zoom = (this.zoom / 1.5).max(0.05);
                cx.notify();
            })))
            .child(div().w(px(46.)).text_center().text_size(px(11.)).text_color(tok::text_tertiary()).child(format!("{:.0}%", self.zoom * 100.0)))
            .child(ui::icon_button("ed-zin", "zoom-in", tok::text_secondary()).on_click(cx.listener(|this, _, _, cx| {
                this.zoom = (this.zoom * 1.5).min(40.0);
                cx.notify();
            })))
    }

    fn timeline(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let pps = self.pps();
        let bounds = self.timeline_bounds.clone();
        let Some(seq) = self.seq().cloned() else {
            return div().h(px(200.)).flex().items_center().justify_center().text_color(tok::text_tertiary()).child("Make a project to start editing.").into_any_element();
        };
        // the ruler: a label every ~90 px
        let step_s = [0.5f32, 1.0, 2.0, 5.0, 10.0, 15.0, 30.0, 60.0, 120.0, 300.0, 600.0].into_iter().find(|s| s * pps >= 90.0).unwrap_or(600.0);
        let mut ruler = div().relative().h(px(RULER_H)).flex_1().overflow_hidden();
        let first = (self.scroll_ms / 1000.0 / step_s).floor() as i64;
        for k in first..first + 60 {
            let s = k as f32 * step_s;
            let x = self.x_of((s * 1000.0) as u64);
            if x > 4000.0 {
                break;
            }
            ruler = ruler.child(
                div()
                    .absolute()
                    .left(px(x))
                    .top_0()
                    .h_full()
                    .border_l_1()
                    .border_color(tok::border())
                    .pl_1()
                    .text_size(px(10.))
                    .text_color(tok::text_tertiary())
                    .child(short((s * 1000.0) as u64)),
            );
        }
        let mut rows = div().flex().flex_col();
        let mut video_n = seq.tracks.iter().filter(|t| t.kind == TrackKind::Video).count();
        let mut audio_n = 0;
        for (ti, track) in seq.tracks.iter().enumerate() {
            let h = if track.kind == TrackKind::Video { VIDEO_H } else { AUDIO_H };
            let name = if track.kind == TrackKind::Video {
                let n = video_n;
                video_n -= 1;
                format!("V{n}")
            } else {
                audio_n += 1;
                format!("A{audio_n}")
            };
            let toggle = |icon: &'static str, on: bool, id: usize, f: fn(&mut lightair_render::sequence::Track), cx: &mut Context<Self>| {
                ui::icon_button(("ed-tr", ti * 4 + id), icon, if on { tok::text_tertiary() } else { tok::text_secondary() })
                    .on_click(cx.listener(move |this, _, _, cx| this.change(|s| if let Some(t) = s.tracks.get_mut(ti) { f(t) }, cx)))
            };
            let header = div()
                .flex()
                .items_center()
                .gap_0p5()
                .w(px(HEADER_W))
                .flex_none()
                .h_full()
                .px_1()
                .border_r_1()
                .border_color(tok::border())
                .child(div().w(px(24.)).text_size(px(11.)).text_color(tok::text_tertiary()).child(if track.captions { "CC".to_string() } else { name }))
                .when(track.kind == TrackKind::Video, |d| d.child(toggle(if track.hidden { "eye-off" } else { "eye" }, track.hidden, 0, |t| t.hidden = !t.hidden, cx)))
                .child(toggle(if track.muted { "volume-x" } else { "volume-2" }, track.muted, 1, |t| t.muted = !t.muted, cx))
                .child(toggle(if track.locked { "lock" } else { "lock-open" }, track.locked, 2, |t| t.locked = !t.locked, cx));
            let mut lane = div().relative().flex_1().h_full().overflow_hidden();
            let mut badges = Vec::new();
            for c in &track.clips {
                let m = seq.media(c.media);
                let left = self.x_of(c.start_ms);
                let width = (c.len_ms() as f32 / 1000.0 * pps).max(4.0);
                if left > 4000.0 || left + width < 0.0 {
                    continue;
                }
                let selected = self.selected == Some(c.id);
                let colour = match (track.kind, &c.content, m.and_then(|m| m.take.as_ref())) {
                    (TrackKind::Audio, _, _) => rgb(0x1E4E8C),
                    (_, Some(Content::Text(_)), _) => rgb(0xB45309),
                    (_, Some(Content::Shape(_)), _) => rgb(0xBE185D),
                    (_, _, Some(_)) => rgb(0x6D28D9),
                    _ => rgb(0x0E7C86),
                };
                let title = match (&c.content, m) {
                    (Some(content), _) => looks::content_name(content),
                    (None, Some(m)) => m.name.clone(),
                    (None, None) => "Missing".into(),
                };
                let pps_ = pps;
                let diamonds: Vec<f32> = c.keyframes.iter().map(|k| k.at_ms as f32 / 1000.0 * pps_).collect();
                let volume_marks: Vec<f32> = c.volume_keys.iter().map(|k| k.at_ms as f32 / 1000.0 * pps_).collect();
                let wave = if track.kind == TrackKind::Audio { self.wave(c, m, left, width, h) } else { None };
                let strip = if track.kind == TrackKind::Video && c.content.is_none() { self.filmstrip(c, m, left, width, h - 6.0) } else { None };
                let has_strip = strip.is_some();
                // a transition straddles the cut before the clip
                if let Some(tn) = c.transition {
                    let half = tn.duration_ms as f32 / 2000.0 * pps;
                    badges.push(
                        div()
                            .absolute()
                            .top(px(VIDEO_H / 2.0 - 9.0))
                            .left(px(left - half.max(7.0)))
                            .w(px((half * 2.0).max(14.0)))
                            .h(px(18.))
                            .rounded(px(4.))
                            .bg(rgba(0xFFFFFFD0))
                            .flex()
                            .items_center()
                            .justify_center()
                            .child(ui::icon("blend").size(px(11.)).text_color(rgb(0x111111))),
                    );
                }
                let (cid, start, locked) = (c.id, c.start_ms, track.locked);
                lane = lane.child(
                    div()
                        .id(SharedString::from(format!("ed-clip-{}", c.id)))
                        .absolute()
                        .top(px(3.))
                        .bottom(px(3.))
                        .left(px(left))
                        .w(px(width))
                        .rounded(px(5.))
                        .bg(colour)
                        .border_2()
                        .border_color(if selected { rgb(0xFFFFFF).into() } else { tok::bg_app() })
                        .overflow_hidden()
                        .px_1p5()
                        .py_0p5()
                        .text_size(px(11.))
                        .text_color(rgb(0xFFFFFF))
                        .cursor_pointer()
                        .children(wave)
                        .children(strip)
                        .child(
                            div()
                                .relative()
                                .flex()
                                .gap_2()
                                .whitespace_nowrap()
                                .when(has_strip, |d| d.self_start().px_1().rounded(px(3.)).bg(rgba(0x000000a0)))
                                .child(title)
                                .child(div().opacity(0.7).child(short(c.len_ms())))
                                .when((c.speed - 1.0).abs() > 0.01, |d| d.child(div().opacity(0.8).child(format!("{}×", c.speed))))
                                .when(c.muted, |d| d.child(ui::icon("volume-x").size(px(11.)))),
                        )
                        .children(diamonds.into_iter().map(|x| {
                            div().absolute().bottom(px(2.)).left(px(x - 5.0)).child(ui::icon("diamond").size(px(10.)).text_color(rgb(0xFDE68A)))
                        }))
                        .children(volume_marks.into_iter().map(|x| {
                            div().absolute().top(px(2.)).left(px(x - 5.0)).child(ui::icon("diamond").size(px(10.)).text_color(rgb(0x93C5FD)))
                        }))
                        .when(c.fade_in_ms > 0 || c.fade_out_ms > 0, |d| d.child(div().absolute().right(px(4.)).top(px(2.)).child(ui::icon("audio-waveform").size(px(10.)).text_color(rgba(0xFFFFFFA0)))))
                        .when(c.voice.is_some(), |d| d.child(div().absolute().right(px(16.)).top(px(2.)).child(ui::icon("mic-vocal").size(px(10.)).text_color(rgba(0xFFFFFFA0)))))
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |this, ev: &MouseDownEvent, _, cx| {
                                cx.stop_propagation();
                                this.selected = Some(cid);
                                if locked {
                                    cx.notify();
                                    return;
                                }
                                let Some(t) = this.time_at(ev.position.x) else { return };
                                let Some(b) = *this.timeline_bounds.borrow() else { return };
                                let x = f32::from(ev.position.x - b.origin.x) - HEADER_W;
                                let left = this.x_of(start);
                                let width = this.seq().and_then(|s| s.clip(cid)).map_or(0.0, |c| c.len_ms() as f32 / 1000.0 * this.pps());
                                let d = if x - left < EDGE {
                                    Drag::TrimStart(cid)
                                } else if left + width - x < EDGE {
                                    Drag::TrimEnd(cid)
                                } else {
                                    Drag::Move { clip: cid, grab_ms: t as i64 - start as i64 }
                                };
                                this.begin_drag(d);
                                cx.notify();
                            }),
                        ),
                );
            }
            let lane = lane.children(badges);
            rows = rows.child(
                div()
                    .flex()
                    .h(px(h))
                    .border_b_1()
                    .border_color(tok::border())
                    .when(track.kind == TrackKind::Audio, |d| d.bg(tok::bg_panel()))
                    .child(header)
                    .child(lane),
            );
        }
        let head = HEADER_W + self.x_of(self.playhead);
        div()
            .id("ed-timeline")
            .relative()
            .flex()
            .flex_col()
            .flex_1()
            .min_h(px(180.))
            .overflow_hidden()
            .bg(tok::bg_app())
            .child(canvas(move |b, _, _| *bounds.borrow_mut() = Some(b), |_, _, _, _| {}).absolute().size_full())
            .child(div().flex().h(px(RULER_H)).border_b_1().border_color(tok::border()).child(div().w(px(HEADER_W)).flex_none()).child(ruler))
            .child(rows)
            .when(head >= HEADER_W, |d| d.child(div().absolute().top_0().bottom_0().left(px(head)).w(px(2.)).bg(rgb(0xFFFFFF))))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, ev: &MouseDownEvent, _, cx| {
                    let Some(t) = this.time_at(ev.position.x) else { return };
                    this.selected = None;
                    this.seek(t.min(this.seq().map_or(0, |s| s.duration_ms())), cx);
                    this.begin_drag(Drag::Scrub);
                }),
            )
            .on_mouse_move(cx.listener(|this, ev: &MouseMoveEvent, _, cx| {
                if this.drag.is_some() {
                    this.drag_to(ev.position, cx);
                }
            }))
            .on_mouse_up(MouseButton::Left, cx.listener(|this, _, _, cx| this.drag_end(cx)))
            .on_mouse_up_out(MouseButton::Left, cx.listener(|this, _, _, cx| this.drag_end(cx)))
            .on_scroll(cx.listener(|this, ev: &ScrollEvent, w, cx| {
                let d = ev.delta.pixel_delta(px(20.));
                if ev.modifiers.platform || ev.modifiers.control {
                    // zoom around the pointer
                    let k = (1.0 - f32::from(d.y) * 0.01).clamp(0.5, 2.0);
                    this.zoom = (this.zoom * k).clamp(0.05, 40.0);
                } else {
                    let dx = if f32::from(d.x).abs() > f32::from(d.y).abs() { f32::from(d.x) } else { f32::from(d.y) };
                    this.scroll_ms = (this.scroll_ms - dx / this.pps() * 1000.0).max(0.0);
                }
                let _ = w;
                cx.notify();
            }))
            .on_drop(cx.listener(|this, m: &DraggedMedia, w, cx| {
                let pos = w.mouse_position();
                let t = this.time_at(pos.x).map(|t| this.snap(t as i64, None));
                let track = this.track_at(pos.y);
                this.add(m.source.clone(), t, track, cx);
            }))
            .into_any_element()
    }
}

impl Render for Editor {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.sync_text_input(window, cx);
        self.want_peaks();
        self.want_strips();
        self.fit_preview();
        // the media panel lists the workspace's Library as it is now
        let v = self.studio.read(cx).library_version;
        if self.library_seen != Some(v) {
            self.library_seen = Some(v);
            self.library = self.workspace.as_ref().map(|ws| Index::load(ws).newest_first().into_iter().cloned().collect()).unwrap_or_default();
        }
        div()
            .flex()
            .flex_col()
            .size_full()
            .child(self.top_bar(cx))
            .child(
                div()
                    .flex()
                    .h(relative(0.58))
                    .min_h_0()
                    .child(self.media_panel(cx))
                    .child(self.player(cx))
                    .child(self.inspector(cx)),
            )
            .child(self.tools(cx))
            .child(div().id("ed-timeline-wrap").flex().flex_col().flex_1().min_h_0().overflow_y_scroll().child(self.timeline(cx)))
    }
}
