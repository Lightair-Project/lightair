//! The Editor (docs/plans/screen-studio.md, "Editor"): one Library item at a time. A preview, a
//! timeline with its cuts and zooms, the look, and Export into the Library.
//!
//! The work is `lightair_render`'s: frames for the preview and playback, and the export, on
//! threads of their own. The edit is saved in the project (`recordings/edits/<item>.json`) as it
//! changes; undo and redo step through its versions.

use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};

use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::Sizable;
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;
use lightair_core::Id;
use lightair_render::edit::{Edit, Focus, Zoom};
use lightair_render::export::{Renderer, Source};
use lightair_render::media::Codec;
use lightair_render::{timeline, zoom};
use lightair_store::recordings::{File as ItemFile, Index, Item, Kind};

use crate::ui::{self, tok};

/// What the background work sends back.
enum Done {
    Opened(Result<(), String>),
    Frame(u64, Vec<u8>, (u32, u32)),
    Progress(f32),
    Exported(Result<PathBuf, String>),
    Transcribed(Result<lightair_render::speech::Transcript, String>),
    Downloaded(Result<(), String>),
}

/// Where the speech model goes (Lightair's data folder).
fn model() -> PathBuf {
    lightair_render::speech::model_path(&lightair_platform::paths::data_dir())
}

/// The item being edited.
struct Open {
    project: PathBuf,
    item: Item,
    source: Source,
    edit_path: PathBuf,
    edit: Edit,
    undo: Vec<Edit>,
    redo: Vec<Edit>,
    renderer: Arc<Mutex<Option<Renderer>>>,
    /// The result's length.
    duration: u64,
    /// The transcript (kept beside the edit), once made.
    transcript: Option<lightair_render::speech::Transcript>,
}

impl Open {
    fn transcript_path(&self) -> PathBuf {
        self.edit_path.with_extension("transcript.json")
    }

    /// The transcript as subtitles for the result, written beside the edit.
    fn write_srt(&self) -> Option<PathBuf> {
        let t = self.transcript.as_ref()?;
        let path = self.edit_path.with_extension("srt");
        std::fs::write(&path, lightair_render::speech::srt(&self.edit, t, 42)).ok()?;
        Some(path)
    }
}

pub struct Editor {
    open: Option<Open>,
    note: Option<SharedString>,
    playhead: u64,
    preview: Option<Arc<RenderImage>>,
    /// The frame asked for last, and whether one is being made.
    wanted: Option<u64>,
    busy: bool,
    playing: Option<Arc<std::sync::atomic::AtomicBool>>,
    exporting: Option<f32>,
    selected_clip: Option<usize>,
    selected_zoom: Option<usize>,
    tx: mpsc::Sender<Done>,
    rx: mpsc::Receiver<Done>,
    /// The timeline's place on screen, for clicks.
    track: std::rc::Rc<std::cell::RefCell<Option<Bounds<Pixels>>>>,
    transcribing: bool,
    downloading: bool,
}

/// The take folder or the video an item is made of.
fn source_of(item: &Item) -> Option<Source> {
    if item.kind == Kind::Take {
        let folder = item.files.iter().find_map(|f| f.path.parent().filter(|p| p.extension().is_some_and(|e| e == "lightair-take")))?;
        return Source::of_take(folder);
    }
    let video = item.files.iter().map(|f| f.path.clone()).find(|p| {
        p.exists() && p.extension().is_some_and(|e| ["mp4", "mov", "mkv", "m4v"].iter().any(|v| e.eq_ignore_ascii_case(v)))
    })?;
    Some(Source::of_file(&video))
}

/// RGBA → the BGRA GPUI draws.
fn image_of(rgba: Vec<u8>, size: (u32, u32)) -> Option<Arc<RenderImage>> {
    let mut bgra = rgba;
    for p in bgra.as_chunks_mut::<4>().0 {
        p.swap(0, 2);
    }
    let buffer = image::RgbaImage::from_raw(size.0, size.1, bgra)?;
    Some(Arc::new(RenderImage::new([image::Frame::new(buffer)])))
}

fn clock(ms: u64) -> String {
    let s = ms / 1000;
    format!("{}:{:02}.{}", s / 60, s % 60, (ms % 1000) / 100)
}

impl Editor {
    pub fn new() -> Self {
        let (tx, rx) = mpsc::channel();
        Editor {
            open: None,
            note: None,
            playhead: 0,
            preview: None,
            wanted: None,
            busy: false,
            playing: None,
            exporting: None,
            selected_clip: None,
            selected_zoom: None,
            tx,
            rx,
            track: Default::default(),
            transcribing: false,
            downloading: false,
        }
    }

    /// Opens a Library item: its saved edit, or a new one with automatic zooms.
    pub fn open(&mut self, project: PathBuf, id: Id, cx: &mut Context<Self>) {
        self.stop(cx);
        let Some(item) = Index::load(&project).get(id).cloned() else { return };
        let Some(source) = source_of(&item) else {
            self.open = None;
            self.note = Some("Its files aren't where they were. Locate them in the Library first.".into());
            cx.notify();
            return;
        };
        let edit_path = project.join("recordings").join("edits").join(format!("{id}.json"));
        let renderer = Arc::new(Mutex::new(None));
        let (r, tx, src, path) = (renderer.clone(), self.tx.clone(), source.clone(), edit_path.clone());
        // the probe, the GPU and (new edits) the automatic zooms, away from the window
        // the project's edit; for a take package from elsewhere, the one it came with
        let saved = Edit::load(&edit_path).ok().or_else(|| source.video.parent().and_then(|f| Edit::load(&f.join("edit.json")).ok()));
        let duration = item.duration_ms.unwrap_or(0);
        let mut edit = saved.clone().unwrap_or_else(|| Edit::new(duration.max(1)));
        std::thread::spawn(move || {
            let result = (|| {
                let probe = lightair_render::media::probe(&src.video).map_err(|e| e.to_string())?;
                let mut e = saved.unwrap_or_else(|| {
                    let mut e = Edit::new(probe.duration_ms);
                    if let Some(log) = src.log.as_ref().and_then(|p| lightair_store::input_log::Log::read(p).ok()) {
                        e.zooms = zoom::auto(&log, probe.duration_ms);
                    }
                    e
                });
                if e.clips.is_empty() {
                    e = Edit { clips: Edit::new(probe.duration_ms).clips, ..e };
                }
                let renderer = Renderer::new(src, &e).map_err(|e| e.to_string())?;
                *r.lock().unwrap() = Some(renderer);
                if let Some(dir) = path.parent() {
                    let _ = std::fs::create_dir_all(dir);
                }
                let _ = e.save(&path);
                Ok(())
            })();
            let _ = tx.send(Done::Opened(result));
        });
        edit.version = edit.version.max(1);
        let duration = timeline::duration_ms(&edit);
        let transcript = std::fs::read(edit_path.with_extension("transcript.json")).ok().and_then(|b| serde_json::from_slice(&b).ok());
        self.open = Some(Open { project, item, source, edit_path, edit, undo: Vec::new(), redo: Vec::new(), renderer, duration, transcript });
        self.playhead = 0;
        self.preview = None;
        self.note = Some("Opening…".into());
        self.watch(cx);
        cx.notify();
    }

    /// Takes what the threads send, while there's something to wait for.
    fn watch(&mut self, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(std::time::Duration::from_millis(30)).await;
                let alive = this.update(cx, |this, cx| {
                    let mut changed = false;
                    while let Ok(done) = this.rx.try_recv() {
                        changed = true;
                        match done {
                            Done::Opened(Ok(())) => {
                                // the saved edit, as the thread settled it
                                if let Some(o) = &mut this.open {
                                    if let Ok(e) = Edit::load(&o.edit_path) {
                                        o.edit = e;
                                    }
                                    o.duration = timeline::duration_ms(&o.edit);
                                }
                                this.note = None;
                                this.request(0);
                            }
                            Done::Opened(Err(e)) => this.note = Some(format!("Couldn't open it: {e}").into()),
                            Done::Frame(t, rgba, size) => {
                                this.busy = false;
                                if this.playing.is_some() {
                                    this.playhead = t;
                                }
                                this.preview = image_of(rgba, size).or(this.preview.take());
                            }
                            Done::Progress(p) => this.exporting = Some(p),
                            Done::Transcribed(r) => {
                                this.transcribing = false;
                                match r {
                                    Ok(t) => {
                                        if let Some(o) = &mut this.open {
                                            let _ = std::fs::write(o.transcript_path(), serde_json::to_vec(&t).unwrap_or_default());
                                            this.note = Some(format!("{} words", t.words.len()).into());
                                            o.transcript = Some(t);
                                        }
                                    }
                                    Err(e) => this.note = Some(e.into()),
                                }
                            }
                            Done::Downloaded(r) => {
                                this.downloading = false;
                                this.note = Some(match r {
                                    Ok(()) => "The speech model is ready.".into(),
                                    Err(e) => format!("The download didn't finish: {e}").into(),
                                });
                            }
                            Done::Exported(r) => {
                                this.exporting = None;
                                this.note = Some(match r {
                                    Ok(p) => format!("Exported {} into the Library", p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()).into(),
                                    Err(e) => format!("The export stopped: {e}").into(),
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

    /// Asks for the frame at `t` (the newest ask wins).
    fn request(&mut self, t: u64) {
        self.wanted = Some(t);
        self.pump();
    }

    fn pump(&mut self) {
        if self.busy || self.playing.is_some() {
            return;
        }
        let (Some(t), Some(o)) = (self.wanted.take(), &self.open) else { return };
        let (r, tx) = (o.renderer.clone(), self.tx.clone());
        let size = o.edit.size;
        self.busy = true;
        std::thread::spawn(move || {
            let frame = r.lock().unwrap().as_mut().and_then(|r| r.frame_at(t).ok());
            match frame {
                Some(f) => {
                    let _ = tx.send(Done::Frame(t, f, size));
                }
                None => {
                    let _ = tx.send(Done::Frame(t, Vec::new(), (0, 0)));
                }
            }
        });
    }

    /// A change to the edit: saved, undoable, and drawn again.
    fn change(&mut self, f: impl FnOnce(&mut Edit), cx: &mut Context<Self>) {
        let Some(o) = &mut self.open else { return };
        let before = o.edit.clone();
        f(&mut o.edit);
        if o.edit == before {
            return;
        }
        o.undo.push(before);
        o.redo.clear();
        self.settle(cx);
    }

    fn settle(&mut self, cx: &mut Context<Self>) {
        let Some(o) = &mut self.open else { return };
        o.duration = timeline::duration_ms(&o.edit);
        let _ = o.edit.save(&o.edit_path);
        if let Some(r) = o.renderer.lock().unwrap().as_mut() {
            let _ = r.set_edit(&o.edit);
        }
        self.playhead = self.playhead.min(o.duration.saturating_sub(1));
        self.request(self.playhead);
        cx.notify();
    }

    fn undo(&mut self, back: bool, cx: &mut Context<Self>) {
        let Some(o) = &mut self.open else { return };
        let (from, to) = if back { (&mut o.undo, &mut o.redo) } else { (&mut o.redo, &mut o.undo) };
        let Some(e) = from.pop() else { return };
        to.push(std::mem::replace(&mut o.edit, e));
        self.settle(cx);
    }

    fn play(&mut self, cx: &mut Context<Self>) {
        if self.playing.is_some() {
            self.stop(cx);
            return;
        }
        let Some(o) = &self.open else { return };
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (r, tx, from, flag) = (o.renderer.clone(), self.tx.clone(), self.playhead, stop.clone());
        self.playing = Some(stop);
        std::thread::spawn(move || {
            let mut guard = r.lock().unwrap();
            let Some(r) = guard.as_mut() else { return };
            let start = std::time::Instant::now();
            let size = r.size();
            let _ = r.frames(from, 30.0, |t, rgba| {
                // in time with the clock (frames that come late are shown late, not skipped)
                let due = std::time::Duration::from_millis(t - from);
                if let Some(wait) = due.checked_sub(start.elapsed()) {
                    std::thread::sleep(wait);
                }
                let _ = tx.send(Done::Frame(t, rgba, size));
                !flag.load(std::sync::atomic::Ordering::Relaxed)
            });
        });
        cx.notify();
    }

    fn stop(&mut self, cx: &mut Context<Self>) {
        if let Some(s) = self.playing.take() {
            s.store(true, std::sync::atomic::Ordering::Relaxed);
        }
        self.busy = false;
        cx.notify();
    }

    /// Transcribes the item's audio on the computer.
    fn transcribe(&mut self, cx: &mut Context<Self>) {
        let Some(o) = &self.open else { return };
        if self.transcribing {
            return;
        }
        let (audio, tx) = (o.source.audio.clone(), self.tx.clone());
        self.transcribing = true;
        self.note = Some("Listening…".into());
        std::thread::spawn(move || {
            let r = lightair_render::speech::transcribe(&audio, &model()).map_err(|e| e.to_string());
            let _ = tx.send(Done::Transcribed(r));
        });
        cx.notify();
    }

    /// Downloads the speech model (asked for by the user's click), with the system's curl.
    fn download_model(&mut self, cx: &mut Context<Self>) {
        if self.downloading {
            return;
        }
        let (path, tx) = (model(), self.tx.clone());
        self.downloading = true;
        self.note = Some("Downloading the speech model (142 MB)…".into());
        std::thread::spawn(move || {
            let r = (|| {
                std::fs::create_dir_all(path.parent().ok_or("no folder")?).map_err(|e| e.to_string())?;
                let part = path.with_extension("part");
                let curl = if cfg!(windows) { "curl.exe" } else { "curl" };
                let ok = std::process::Command::new(curl)
                    .args(["-fsSL", "-o"])
                    .arg(&part)
                    .arg(lightair_render::speech::MODEL_URL)
                    .status()
                    .map_err(|e| e.to_string())?
                    .success();
                if !ok {
                    let _ = std::fs::remove_file(&part);
                    return Err("couldn't reach the download".to_string());
                }
                std::fs::rename(&part, &path).map_err(|e| e.to_string())
            })();
            let _ = tx.send(Done::Downloaded(r));
        });
        cx.notify();
    }

    fn export(&mut self, codec: Codec, cx: &mut Context<Self>) {
        if self.open.is_none() || self.exporting.is_some() {
            return;
        }
        self.stop(cx);
        let Some(o) = &self.open else { return };
        let folder = o.source.video.parent().map(Path::to_path_buf).unwrap_or_default();
        let folder = if folder.extension().is_some_and(|e| e == "lightair-take") { folder.parent().map(Path::to_path_buf).unwrap_or(folder) } else { folder };
        let ext = match codec {
            Codec::ProRes => "mov",
            Codec::Gif => "gif",
            Codec::H264 => "mp4",
        };
        let subtitles = o.write_srt();
        let fps = if codec == Codec::Gif { 15.0 } else { 60.0 };
        let name = format!("{} (edited).{ext}", o.item.title.replace(['/', ':', '\\'], "-"));
        let out = (1..).map(|n| if n == 1 { folder.join(&name) } else { folder.join(name.replace(" (edited)", &format!(" (edited {n})"))) }).find(|p| !p.exists()).unwrap_or_else(|| folder.join(&name));
        let (r, tx, project, from, title, duration) =
            (o.renderer.clone(), self.tx.clone(), o.project.clone(), o.item.id, o.item.title.clone(), o.duration);
        self.exporting = Some(0.0);
        std::thread::spawn(move || {
            let mut guard = r.lock().unwrap();
            let Some(r) = guard.as_mut() else { return };
            r.subtitles = subtitles.clone().filter(|_| codec != Codec::Gif);
            let progress = tx.clone();
            let result = r
                .export(&out, fps, codec, |p| {
                    let _ = progress.send(Done::Progress(p));
                    true
                })
                .map_err(|e| e.to_string());
            if result.is_ok() {
                // the captions beside it too, for players and sites that take them separately
                if let Some(srt) = subtitles.filter(|_| codec != Codec::Gif) {
                    let _ = std::fs::copy(srt, out.with_extension("srt"));
                }
                let mut item = Item::new(Kind::Export, format!("{title} (edited)"), vec![ItemFile { path: out.clone(), format: None }], chrono::Utc::now().timestamp_millis());
                item.duration_ms = Some(duration);
                item.from = Some(from);
                let _ = Index::add_to(&project, item);
            }
            let _ = tx.send(Done::Exported(result.map(|_| out)));
        });
        cx.notify();
    }

    // ------------------------------------------------------------ view

    fn chips<T: Copy + PartialEq + 'static>(
        &self,
        id: &'static str,
        options: &[(&'static str, T)],
        current: T,
        set: impl Fn(&mut Edit, T) + Copy + 'static,
        cx: &mut Context<Self>,
    ) -> Div {
        let mut row = div().flex().flex_wrap().gap_1();
        for (n, (label, v)) in options.iter().enumerate() {
            let v = *v;
            let on = v == current;
            row = row.child(
                div()
                    .id((id, n))
                    .px_2()
                    .py_0p5()
                    .rounded(px(6.))
                    .text_size(px(12.))
                    .cursor_pointer()
                    .when(on, |d| d.bg(tok::bg_selected()).text_color(tok::text()))
                    .when(!on, |d| d.text_color(tok::text_secondary()))
                    .hover(|d| d.bg(tok::bg_selected()))
                    .child(*label)
                    .on_click(cx.listener(move |this, _, _, cx| this.change(|e| set(e, v), cx))),
            );
        }
        row
    }

    fn side(&self, o: &Open, cx: &mut Context<Self>) -> impl IntoElement {
        let e = &o.edit;
        let label = |t: &'static str| div().pt_2().text_size(px(11.)).font_weight(FontWeight::SEMIBOLD).text_color(tok::text_tertiary()).child(t);
        let swatches = {
            let mut row = div().flex().flex_wrap().gap_1p5();
            for (n, (colours, _)) in lightair_render::compose::BACKDROPS.iter().enumerate() {
                let on = e.look.backdrop == n as i64;
                row = row.child(
                    div()
                        .id(("ed-backdrop", n))
                        .size(px(28.))
                        .rounded(px(6.))
                        .border_2()
                        .border_color(if on { tok::accent_primary() } else { tok::bg_panel() })
                        .bg(rgb(colours[1]))
                        .cursor_pointer()
                        .on_click(cx.listener(move |this, _, _, cx| this.change(|e| e.look.backdrop = n as i64, cx))),
                );
            }
            row
        };
        div()
            .flex()
            .flex_col()
            .gap_1()
            .w(px(260.))
            .flex_none()
            .p_3()
            .border_l_1()
            .border_color(tok::border())
            .bg(tok::bg_panel())
            .child(label("BACKDROP"))
            .child(swatches)
            .child(label("PADDING"))
            .child(self.chips("ed-pad", &[("None", 0), ("Small", 1), ("Medium", 2), ("Large", 3)], ((e.look.padding / 0.035).round() as i32).clamp(0, 3), |e, v| e.look.padding = v as f32 * 0.035, cx))
            .child(label("CORNERS"))
            .child(self.chips("ed-radius", &[("Square", 0), ("Soft", 1), ("Round", 2)], ((e.look.radius / 0.015).round() as i32).clamp(0, 2), |e, v| e.look.radius = v as f32 * 0.015, cx))
            .child(label("POINTER"))
            .child(self.chips("ed-pstyle", &[("Pointer", 0i64), ("Dot", 1), ("Ring", 2), ("Hidden", -1)], if e.pointer.hidden { -1 } else { e.pointer.style }, |e, v| {
                e.pointer.hidden = v < 0;
                if v >= 0 {
                    e.pointer.style = v;
                }
            }, cx))
            .child(self.chips("ed-psize", &[("Small", 10), ("Medium", 15), ("Large", 25), ("Huge", 40)], (e.pointer.size * 10.0).round() as i32, |e, v| e.pointer.size = v as f32 / 10.0, cx))
            .child(label("SMOOTHING"))
            .child(self.chips("ed-smooth", &[("Off", 0), ("Some", 5), ("Lots", 9)], (e.pointer.smoothing * 10.0).round() as i32, |e, v| e.pointer.smoothing = v as f32 / 10.0, cx))
            .child(label("CLICKS"))
            .child(self.chips("ed-clicks", &[("None", 0i64), ("Circle", 1), ("Ripple", 2)], e.pointer.clicks, |e, v| e.pointer.clicks = v, cx))
            .child(label("SIZE"))
            .child(self.chips("ed-size", &[("720p", (1280u32, 720u32)), ("1080p", (1920, 1080)), ("4K", (3840, 2160)), ("Vertical", (1080, 1920))], e.size, |e, v| e.size = v, cx))
            .when_some(o.transcript.as_ref(), |d, t| {
                // the words, where they are in the result; a click goes there
                let mut words = div().flex().flex_wrap().gap_x_1().text_size(px(12.));
                for (n, w) in t.words.iter().enumerate() {
                    let Some(at) = timeline::result_at(e, w.start_ms) else { continue };
                    let here = self.playhead >= at && self.playhead < at + (w.end_ms - w.start_ms).max(1);
                    words = words.child(
                        div()
                            .id(("ed-word", n))
                            .cursor_pointer()
                            .rounded(px(3.))
                            .when(here, |d| d.bg(tok::bg_selected()))
                            .hover(|d| d.text_color(tok::text()))
                            .text_color(tok::text_secondary())
                            .child(w.text.clone())
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.stop(cx);
                                this.playhead = at;
                                this.request(at);
                                cx.notify();
                            })),
                    );
                }
                d.child(label("TRANSCRIPT")).child(div().id("ed-words").flex_1().min_h_0().overflow_y_scroll().child(words))
            })
    }

    fn timeline(&self, o: &Open, cx: &mut Context<Self>) -> impl IntoElement {
        let total = o.duration.max(1) as f32;
        let e = &o.edit;
        let track = self.track.clone();
        let head = self.playhead as f32 / total;
        // clips, as they play
        let mut clips = div().relative().h(px(36.)).w_full();
        let mut at = 0u64;
        for (n, c) in e.clips.iter().enumerate() {
            let len = ((c.end_ms - c.start_ms) as f32 / c.speed) as u64;
            let on = self.selected_clip == Some(n);
            clips = clips.child(
                div()
                    .id(("ed-clip", n))
                    .absolute()
                    .top_0()
                    .h_full()
                    .left(relative(at as f32 / total))
                    .w(relative(len as f32 / total))
                    .px_1()
                    .rounded(px(6.))
                    .border_2()
                    .border_color(if on { tok::accent_primary() } else { tok::bg_app() })
                    .bg(tok::bg_raised())
                    .text_size(px(11.))
                    .text_color(tok::text_secondary())
                    .overflow_hidden()
                    .child(if (c.speed - 1.0).abs() > 0.01 { format!("Clip · {}×", c.speed) } else { "Clip".into() })
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.selected_clip = Some(n);
                        this.selected_zoom = None;
                        cx.notify();
                    })),
            );
            at += len;
        }
        // zooms, where they land in the result
        let mut zooms = div().relative().h(px(28.)).w_full();
        for (n, z) in e.zooms.iter().enumerate() {
            let (Some(a), Some(b)) = (timeline::result_at(e, z.start_ms), timeline::result_at(e, z.end_ms.saturating_sub(1))) else { continue };
            let on = self.selected_zoom == Some(n);
            zooms = zooms.child(
                div()
                    .id(("ed-zoom", n))
                    .absolute()
                    .top_0()
                    .h_full()
                    .left(relative(a as f32 / total))
                    .w(relative((b.saturating_sub(a)).max(1) as f32 / total))
                    .px_1()
                    .rounded(px(6.))
                    .border_2()
                    .border_color(if on { tok::accent_primary() } else { tok::bg_app() })
                    .bg(rgba(0x7C3AED80))
                    .text_size(px(11.))
                    .overflow_hidden()
                    .child(format!("{}× {}", z.scale, if z.auto { "auto" } else { "" }))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.selected_zoom = Some(n);
                        this.selected_clip = None;
                        cx.notify();
                    })),
            );
        }
        let lane = |label: &'static str, content: Div| {
            div().flex().items_center().gap_2().child(div().w(px(56.)).flex_none().text_size(px(11.)).text_color(tok::text_tertiary()).child(label)).child(content)
        };
        div()
            .id("ed-timeline")
            .relative()
            .flex()
            .flex_col()
            .gap_1p5()
            .p_3()
            .border_t_1()
            .border_color(tok::border())
            .child(lane("", div().flex_1().h(px(18.)).relative().child(canvas(move |b, _, _| *track.borrow_mut() = Some(b), |_, _, _, _| {}).absolute().size_full()).on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, ev: &MouseDownEvent, _, cx| {
                    let Some(b) = *this.track.borrow() else { return };
                    let k = (f32::from(ev.position.x - b.origin.x) / f32::from(b.size.width).max(1.0)).clamp(0.0, 1.0);
                    let total = this.open.as_ref().map_or(0, |o| o.duration);
                    this.stop(cx);
                    this.playhead = (k * total as f32) as u64;
                    this.request(this.playhead);
                    cx.notify();
                }),
            ).child(div().absolute().top_0().h_full().left(relative(head)).w(px(2.)).bg(tok::live())).child(div().text_size(px(11.)).text_color(tok::text_tertiary()).child(format!("{} / {}", clock(self.playhead), clock(o.duration))))))
            .child(lane("Clips", clips.flex_1()))
            .child(lane("Zooms", zooms.flex_1()))
    }

    fn tools(&self, o: &Open, cx: &mut Context<Self>) -> impl IntoElement {
        let source = timeline::source_at(&o.edit, self.playhead).unwrap_or(0);
        let mut row = div()
            .flex()
            .items_center()
            .gap_2()
            .px_3()
            .py_1p5()
            .child(ui::icon_button("ed-play", if self.playing.is_some() { "pause" } else { "play" }, tok::text()).on_click(cx.listener(|this, _, _, cx| this.play(cx))))
            .child(Button::new("ed-split").label("Split").icon(ui::icon("scissors")).xsmall().ghost().on_click(cx.listener(move |this, _, _, cx| this.change(|e| e.split(source), cx))))
            .child(Button::new("ed-zoom-add").label("Add zoom").icon(ui::icon("zoom-in")).xsmall().ghost().on_click(cx.listener(move |this, _, _, cx| {
                this.change(|e| e.zooms.push(Zoom { start_ms: source, end_ms: source + 3_000, scale: zoom::DEFAULT_SCALE, focus: Focus::Pointer, auto: false }), cx)
            })))
            .child(Button::new("ed-zoom-auto").label("Auto zooms").icon(ui::icon("sparkles")).xsmall().ghost().on_click(cx.listener(|this, _, _, cx| {
                let Some(o) = &this.open else { return };
                let Some(log) = o.source.log.as_ref().and_then(|p| lightair_store::input_log::Log::read(p).ok()) else {
                    this.note = Some("Automatic zooms need a Studio take (they follow its clicks and typing).".into());
                    cx.notify();
                    return;
                };
                let duration = o.edit.clips.iter().map(|c| c.end_ms).max().unwrap_or(0);
                let auto = zoom::auto(&log, duration);
                this.change(|e| {
                    e.zooms.retain(|z| !z.auto);
                    e.zooms.extend(auto);
                    e.zooms.sort_by_key(|z| z.start_ms);
                }, cx)
            })))
            .child(match (&o.transcript, model().is_file()) {
                (Some(_), _) => Button::new("ed-tighten")
                    .label("Remove fillers and pauses")
                    .icon(ui::icon("wand-sparkles"))
                    .xsmall()
                    .ghost()
                    .on_click(cx.listener(|this, _, _, cx| {
                        let Some(t) = this.open.as_ref().and_then(|o| o.transcript.clone()) else { return };
                        this.change(|e| *e = lightair_render::speech::tighten(e, &t, 900, 300), cx)
                    })),
                (None, true) => Button::new("ed-transcribe")
                    .label(if self.transcribing { "Transcribing…" } else { "Transcribe" })
                    .icon(ui::icon("captions"))
                    .xsmall()
                    .ghost()
                    .loading(self.transcribing)
                    .on_click(cx.listener(|this, _, _, cx| this.transcribe(cx))),
                (None, false) => Button::new("ed-model")
                    .label(if self.downloading { "Downloading…" } else { "Get speech model" })
                    .icon(ui::icon("download"))
                    .xsmall()
                    .ghost()
                    .loading(self.downloading)
                    .tooltip("142 MB, once: speech is recognised on this computer, never sent anywhere")
                    .on_click(cx.listener(|this, _, _, cx| this.download_model(cx))),
            })
            .child(div().flex_1());
        if let Some(n) = self.selected_clip.filter(|n| *n < o.edit.clips.len()) {
            row = row
                .child(self.chips("ed-speed", &[("1×", 10), ("1.5×", 15), ("2×", 20), ("4×", 40)], (o.edit.clips[n].speed * 10.0).round() as i32, move |e, v| {
                    if let Some(c) = e.clips.get_mut(n) {
                        c.speed = v as f32 / 10.0;
                    }
                }, cx))
                .child(Button::new("ed-clip-del").label("Delete clip").icon(ui::icon("trash")).xsmall().ghost().on_click(cx.listener(move |this, _, _, cx| {
                    this.selected_clip = None;
                    this.change(|e| {
                        if e.clips.len() > 1 && n < e.clips.len() {
                            e.clips.remove(n);
                        }
                    }, cx)
                })));
        }
        if let Some(n) = self.selected_zoom.filter(|n| *n < o.edit.zooms.len()) {
            row = row
                .child(self.chips("ed-zscale", &[("1.5×", 15), ("2×", 20), ("2.5×", 25), ("3×", 30)], (o.edit.zooms[n].scale * 10.0).round() as i32, move |e, v| {
                    if let Some(z) = e.zooms.get_mut(n) {
                        z.scale = v as f32 / 10.0;
                        z.auto = false;
                    }
                }, cx))
                .child(Button::new("ed-zoom-del").label("Delete zoom").icon(ui::icon("trash")).xsmall().ghost().on_click(cx.listener(move |this, _, _, cx| {
                    this.selected_zoom = None;
                    this.change(|e| {
                        if n < e.zooms.len() {
                            e.zooms.remove(n);
                        }
                    }, cx)
                })));
        }
        row
    }
}

impl Render for Editor {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(o) = self.open.take() else {
            return div()
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .gap_2()
                .size_full()
                .child(ui::icon("scissors").size(px(36.)).text_color(tok::text_tertiary()))
                .child(div().text_size(px(15.)).font_weight(FontWeight::SEMIBOLD).child("Nothing open"))
                .child(
                    div()
                        .max_w(px(420.))
                        .text_center()
                        .text_color(tok::text_secondary())
                        .child(self.note.clone().unwrap_or_else(|| "Open a recording or a Studio take from the Library: right-click it, Open in Editor.".into())),
                )
                .into_any_element();
        };
        let el = div()
            .flex()
            .flex_col()
            .size_full()
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .px_3()
                    .py_2()
                    .border_b_1()
                    .border_color(tok::border())
                    .child(div().text_size(px(15.)).font_weight(FontWeight::SEMIBOLD).child(o.item.title.clone()))
                    .child(div().text_size(px(12.)).text_color(tok::text_tertiary()).child(o.item.kind.label()))
                    .child(div().flex_1())
                    .child(ui::icon_button("ed-undo", "undo-2", tok::text_secondary()).on_click(cx.listener(|this, _, _, cx| this.undo(true, cx))))
                    .child(ui::icon_button("ed-redo", "redo-2", tok::text_secondary()).on_click(cx.listener(|this, _, _, cx| this.undo(false, cx))))
                    .child(match self.exporting {
                        Some(p) => Button::new("ed-export").label(format!("Exporting {:.0}%", p * 100.0)).small().outline().loading(true).into_any_element(),
                        None => div()
                            .flex()
                            .gap_1()
                            .child(Button::new("ed-export-gif").label("GIF").small().ghost().on_click(cx.listener(|this, _, _, cx| this.export(Codec::Gif, cx))))
                            .child(Button::new("ed-export-prores").label("ProRes").small().ghost().on_click(cx.listener(|this, _, _, cx| this.export(Codec::ProRes, cx))))
                            .child(Button::new("ed-export").label("Export MP4").small().primary().on_click(cx.listener(|this, _, _, cx| this.export(Codec::H264, cx))))
                            .into_any_element(),
                    }),
            )
            .child(
                div()
                    .flex()
                    .flex_1()
                    .min_h_0()
                    .child(
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
                                    .m_3()
                                    .rounded(px(8.))
                                    .overflow_hidden()
                                    .bg(tok::bg_canvas_well())
                                    .when_some(self.preview.clone(), |d, img_| d.child(img(ImageSource::Render(img_)).absolute().size_full().object_fit(ObjectFit::Contain))),
                            )
                            .child(self.tools(&o, cx))
                            .child(self.timeline(&o, cx)),
                    )
                    .child(self.side(&o, cx)),
            )
            .when_some(self.note.clone(), |d, note| {
                d.child(div().px_3().py_1().border_t_1().border_color(tok::border()).text_size(px(12.)).text_color(tok::text_secondary()).child(note))
            })
            .into_any_element();
        self.open = Some(o);
        el
    }
}
