//! macOS, with `LIGHTAIR_ENGINE=own`: each canvas's program drawn by Lightair's own compositor
//! (`lightair-render`'s `SceneRenderer`) instead of libobs, for the canvas's previews and its
//! outputs.
//!
//! A hub thread follows the document's canvases and runs one compositor thread per canvas, at
//! the canvas's frame rate. A canvas's compositor draws while one of the canvas's previews shows
//! its program ([`watch`]) or something records or streams from the canvas's output
//! (`program_source::in_use`, asked by the hub a few times a second), and idles otherwise, so it
//! keeps drawing for a recording while the window's preview is hidden.
//!
//! Each frame it reads the document the engine last applied (`bridge::snapshot`), makes the
//! pictures of the sources the program and the show's overlays show, and draws the program (or
//! the switch under way) on the GPU, then the overlays ("Show over every scene") over it, as
//! libobs draws its overlay channels over what's on air. The frame is copied into a BGRA
//! IOSurface from a small ring: the previews show that buffer as they show libobs's Metal frames
//! (the window's layer takes it as it is, GPUI's `surface()` an NV12 copy), and the canvas's
//! `lightair_program` sources draw it for libobs's outputs.
//!
//! The pictures ([`Native`]):
//!
//! - **Made here**, with `lightair-engine`'s pictures: images (GIFs animated), colours (and so
//!   the shapes, which are colours cut by their layout); text and timers (their text from the
//!   engine's timers) as libobs's text source draws them, in the faces it finds (`text`); and
//!   the built-in widgets the native widgets draw (chat box, alert box, event list, goal,
//!   label, counter, subathon timer), from the app's chat hub, the widget server's storage and
//!   its alert queue (`widget_feed`). Until one is ready (an image decoding, a timer before its
//!   first tick) libobs's picture stands in.
//! - **From libobs** (`bridge`), for everything else: captures, cameras, media files, web pages,
//!   other widgets, avatars, devices, NDI…
//!
//! Effects: the compositor draws a source's effects when it can draw all of them: the
//! renderer's (shader effects, the colour filter, a `.cube` LUT) and the engine's sharpen and
//! keyers (chroma, colour, luma key), the keyers first in the stack, as they're drawn first
//! here. Such a source's picture is made bare (the bridge draws it without its libobs
//! filters), keyed on the GPU, then given to the renderer with its effects. A source with any
//! other effect (the Effect Stack's looks, the camera studio filter, masks, freeze, scroll…)
//! is drawn by libobs through all its filters, and the renderer is given it without its effects
//! so nothing is drawn twice. The renderer does the rest: position, fit, crop, turn, flip,
//! tilt, shape, opacity, and the switch between scenes.
//!
//! Switches are drawn as libobs draws them: a stinger plays its video (`stinger`, decoded here)
//! for the video's length, cutting at its cut point; a stinger with no video, and a Scene
//! transition with no scene to play, are crossfades. A stinger video the compositor can't read
//! is drawn by libobs: the compositor steps aside for that switch.

use std::collections::{BTreeSet, HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock, OnceLock, Weak};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use lightair_obs::{bridge, program_source};
use lightair_model::{Canvas, Id, Scene, Source, Stinger, TransitionKind, TransitionSettings, Workspace};
use lightair_render::SharedGpu;
use lightair_render::media::Picture;
use lightair_render::scene::{Pictures, SceneRenderer, Shot};
use lightair_engine::gpu::Gpu;
use lightair_engine::hot::{Hot, Program};
use lightair_engine::{keying, templates, widgets};
use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2_core_video::{CVPixelBufferGetIOSurface, kCVPixelFormatType_32BGRA};
use objc2_metal::{MTLDevice, MTLPixelFormat, MTLTexture, MTLTextureDescriptor, MTLTextureType, MTLTextureUsage};
use parking_lot::Mutex;

use crate::mac::{Frame, Shared, pixel_buffer};
use crate::stinger::Player;
use crate::widget_feed;

/// Output buffers at most: the newest (the previews' and libobs's), the one before it (a window
/// may still be showing it), one being drawn, and spares while something holds on to older ones.
const MOST: usize = 6;
/// Buffers not drawn into again for this many frames after they're handed on (a window's layer
/// shows a surface without holding its buffer).
const RECENT: usize = 2;
/// The longest a frame's GPU work is waited for before the frame is dropped.
const GPU_WAIT: Duration = Duration::from_secs(1);
/// How often the hub looks for canvases added or removed and outputs started or stopped.
const HUB_PERIOD: Duration = Duration::from_millis(100);
/// Overlay scenes drawn at most, bottom first (as many as libobs has overlay channels for).
const OVERLAYS: usize = 8;
/// A widget's picture is made again at most this often (a busy chat changes it many times a
/// second).
const WIDGET_PERIOD: Duration = Duration::from_millis(100);
/// How long a stinger switch is drawn while its video's length isn't known yet.
const UNKNOWN_LENGTH_MS: u32 = 3_600_000;

// ---------------------------------------------------------------- the hub

/// The canvas a preview shows.
#[derive(Clone, PartialEq, Eq)]
pub(crate) enum Target {
    /// The primary canvas (libobs's main canvas), whichever it is.
    Primary,
    /// Another canvas, by its id as text (the name the engine gives its libobs canvas).
    Canvas(String),
}

struct Viewer {
    target: Target,
    shared: Weak<Shared>,
}

#[derive(Default)]
struct Hub {
    /// The previews given the compositors' frames.
    viewers: Vec<Viewer>,
    /// Canvases something records or streams from.
    in_use: HashSet<Id>,
}

static HUB: LazyLock<Mutex<Hub>> = LazyLock::new(Default::default);

/// Starts the hub (once); false when there's no GPU to draw with.
pub(crate) fn start() -> bool {
    static STARTED: OnceLock<bool> = OnceLock::new();
    *STARTED.get_or_init(|| {
        if let Err(e) = lightair_render::shared_gpu() {
            tracing::error!(%e, "compositor: no GPU; libobs draws the program");
            return false;
        }
        std::thread::Builder::new()
            .name("lightair-compose-hub".into())
            .spawn(run_hub)
            .map_err(|e| tracing::error!(%e, "compositor: no thread"))
            .is_ok()
    })
}

/// Gives `preview` the program frames of the canvas `target` from now on.
pub(crate) fn watch(target: Target, preview: &Arc<Shared>) {
    let mut hub = HUB.lock();
    hub.viewers.retain(|v| !std::ptr::eq(v.shared.as_ptr(), Arc::as_ptr(preview)));
    hub.viewers.push(Viewer { target, shared: Arc::downgrade(preview) });
}

/// Stops giving `preview` frames: libobs draws its program again.
pub(crate) fn unwatch(preview: &Arc<Shared>) {
    HUB.lock().viewers.retain(|v| !std::ptr::eq(v.shared.as_ptr(), Arc::as_ptr(preview)));
    preview.unfeed();
}

/// The previews of `canvas` (`name`: its id as text), and whether something records or streams
/// from it.
fn viewers(canvas: Id, name: &str, primary: bool) -> (Vec<Arc<Shared>>, bool) {
    let hub = HUB.lock();
    let viewers = hub
        .viewers
        .iter()
        .filter(|v| match &v.target {
            Target::Primary => primary,
            Target::Canvas(n) => n == name,
        })
        .filter_map(|v| v.shared.upgrade())
        .collect();
    (viewers, hub.in_use.contains(&canvas))
}

/// The hub's thread: a compositor for each canvas of the document, and which are in use.
fn run_hub() {
    // the faces libobs's text finds, before any compositor draws text
    crate::text::install_fonts();
    let mut running: HashMap<Id, Compositor> = HashMap::new();
    loop {
        let canvases: Vec<Id> =
            bridge::snapshot().map(|s| s.workspace.canvases.iter().map(|c| c.id).collect()).unwrap_or_default();
        // a canvas removed: its thread stops (and is waited for, outside the hub's lock)
        let gone: Vec<Id> = running.keys().filter(|id| !canvases.contains(id)).copied().collect();
        for id in gone {
            drop(running.remove(&id));
        }
        for id in &canvases {
            if !running.contains_key(id)
                && let Some(c) = Compositor::start(*id)
            {
                running.insert(*id, c);
            }
        }
        let in_use: HashSet<Id> = canvases.iter().copied().filter(|id| program_source::in_use(*id)).collect();
        {
            let mut hub = HUB.lock();
            hub.in_use = in_use;
            hub.viewers.retain(|v| v.shared.strong_count() > 0);
        }
        std::thread::sleep(HUB_PERIOD);
    }
}

// ---------------------------------------------------------------- a canvas's compositor

/// A canvas's compositor thread. Dropping it stops the thread (and waits for it).
struct Compositor {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Compositor {
    /// Starts drawing `canvas`'s program when it's wanted.
    fn start(canvas: Id) -> Option<Compositor> {
        let stop = Arc::new(AtomicBool::new(false));
        let thread = std::thread::Builder::new()
            .name("lightair-compose".into())
            .spawn({
                let stop = stop.clone();
                move || run(canvas, &stop)
            })
            .map_err(|e| tracing::error!(%e, "compositor: no thread"))
            .ok()?;
        Some(Compositor { stop, thread: Some(thread) })
    }
}

impl Drop for Compositor {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

fn run(canvas: Id, stop: &AtomicBool) {
    let gpu = match lightair_render::shared_gpu() {
        Ok(gpu) => gpu,
        Err(e) => {
            tracing::error!(%e, "compositor: no GPU; libobs draws the program");
            return;
        }
    };
    let mut state = State {
        canvas,
        name: canvas.to_string(),
        gpu,
        drawer: None,
        failed: None,
        doc: Doc::default(),
        native: None,
        native_failed: false,
        missing: HashSet::new(),
        wanted: None,
        fed: Vec::new(),
        clock: Instant::now(),
        stingers: HashMap::new(),
        stingers_of: 0,
        playing: None,
    };
    let mut next = Instant::now();
    while !stop.load(Ordering::Acquire) {
        let period = state.tick();
        next += period;
        let now = Instant::now();
        if next <= now {
            // behind: the frames missed are dropped, not caught up
            next = now;
        } else {
            std::thread::sleep(next - now);
        }
    }
    state.idle();
    state.unfeed(&[]);
}

/// What the thread keeps between ticks.
struct State {
    canvas: Id,
    /// The canvas's id as text (how previews of it are known).
    name: String,
    gpu: SharedGpu,
    /// The renderers and buffers for the canvas's current size.
    drawer: Option<Drawer>,
    /// A size nothing could be made for (not tried again).
    failed: Option<(u32, u32)>,
    doc: Doc,
    /// The pictures made here and the keyers (made at the first frame).
    native: Option<Native>,
    /// No GPU for them: every picture comes from libobs, with its filters unless it has no keys.
    native_failed: bool,
    /// Sources made here whose picture wasn't there last frame: libobs's stands in meanwhile.
    missing: HashSet<Id>,
    /// The sources last asked of the bridge, and whether each was asked for without its filters.
    wanted: Option<HashMap<Id, bool>>,
    /// The previews given frames.
    fed: Vec<Weak<Shared>>,
    /// The clock moving effects run on.
    clock: Instant,
    /// The stinger videos the workspace's switches may play, by path, read ahead.
    stingers: HashMap<String, Player>,
    /// The workspace they were last kept for.
    stingers_of: usize,
    /// The stinger switch being drawn: when it started, and its video's path.
    playing: Option<(Instant, String)>,
}

impl State {
    /// Draws a frame if one is wanted; the time until the next.
    fn tick(&mut self) -> Duration {
        let Some(snapshot) = bridge::snapshot() else {
            self.idle();
            return frame_period(None);
        };
        let Some(canvas) = snapshot.workspace.canvas(self.canvas).cloned() else {
            self.idle();
            return frame_period(None);
        };
        let period = frame_period(Some(&canvas));
        let primary = snapshot.workspace.canvases.first().is_some_and(|c| c.id == self.canvas);
        let (viewers, in_use) = viewers(self.canvas, &self.name, primary);
        // previews that no longer look at this canvas show libobs's frames again
        self.unfeed(&viewers);
        let showing: Vec<&Arc<Shared>> = viewers.iter().filter(|v| v.wants_program()).collect();
        if showing.is_empty() && !in_use {
            self.idle();
            return period;
        }
        let base = self.doc.update(&snapshot.workspace);
        let now = Instant::now();
        self.keep_stingers(&base);
        // the switch to what's on air, while it runs, as libobs draws it
        let switch = snapshot.switch.filter(|s| base.program == Some(s.to)).and_then(|s| self.plan(&base, s, now));
        self.played(switch.as_ref().and_then(|p| p.stinger.as_ref().map(|(st, _)| (p.started, st.path.as_str()))));
        if switch.as_ref().is_some_and(|p| p.stand_in) {
            // libobs draws this switch (a stinger video the compositor can't read)
            self.idle();
            self.unfeed(&[]);
            return period;
        }
        let mut scenes: Vec<Id> = match &switch {
            Some(p) => [Some(p.from), Some(p.to), p.over].into_iter().flatten().collect(),
            None => base.program.into_iter().collect(),
        };
        let overlays = self.doc.overlays;
        scenes.extend(overlays);
        let size = (canvas.width.max(2) & !1, canvas.height.max(2) & !1);
        if self.drawer.as_ref().is_none_or(|d| d.size != size) {
            self.drawer = None;
            if self.failed != Some(size) {
                self.drawer = Drawer::new(&self.gpu, size);
                if self.drawer.is_none() {
                    tracing::error!(?size, "compositor: no renderer at this size; libobs draws the program");
                    self.failed = Some(size);
                }
            }
            if self.drawer.is_none() {
                self.idle();
                self.unfeed(&[]);
                return period;
            }
        }
        let shown = self.doc.shown(&base, canvas.id, &scenes);
        if self.native.is_none() && !self.native_failed {
            self.native = Native::new();
            self.native_failed = self.native.is_none();
        }
        let keyers = self.native.is_some();
        self.missing.retain(|id| shown.contains(id));
        // what libobs is asked for: the sources not made here (bare when the compositor draws
        // all their effects), and those made here whose picture isn't ready
        let want: HashMap<Id, bool> = shown
            .iter()
            .filter_map(|id| base.sources.get(id))
            .filter_map(|source| {
                let effects = effects_drawn(source, keyers);
                let made = keyers && effects && made_here(source) && !self.missing.contains(&source.id);
                (!made).then_some((source.id, effects))
            })
            .collect();
        if self.wanted.as_ref() != Some(&want) {
            bridge::want(canvas.id, want.clone());
            self.wanted = Some(want);
        }

        let Some(drawer) = self.drawer.as_mut() else { return period };

        let time = self.clock.elapsed().as_secs_f32();
        // every picture, before drawing: which sources keep their effects depends on them
        let mut shots: HashMap<Id, Shot> = HashMap::new();
        let mut kept: BTreeSet<Id> = BTreeSet::new();
        // libobs's buffers, held until the GPU is done with the frame
        let mut held: Vec<bridge::Buffer> = Vec::new();
        let pts_ns = self.clock.elapsed().as_nanos() as i64;
        if let Some(native) = self.native.as_mut() {
            native.keep(&snapshot.workspace);
        }
        for id in &shown {
            let Some(source) = base.sources.get(id) else { continue };
            let effects = effects_drawn(source, keyers);
            if effects
                && made_here(source)
                && let Some(native) = self.native.as_mut()
            {
                match native.picture(source, pts_ns) {
                    Some(shot) => {
                        self.missing.remove(id);
                        shots.insert(*id, shot);
                        kept.insert(*id);
                        continue;
                    }
                    None => {
                        self.missing.insert(*id);
                    }
                }
            }
            let Some(picture) = bridge::picture(*id) else { continue };
            let steps = if picture.unfiltered { keying::steps(source) } else { Vec::new() };
            let shot = if steps.is_empty() {
                picture.pixel_buffer().map(|b| Shot { size: b.size(), picture: Picture::Buffer(b), premultiplied: true })
            } else {
                // keyed here; without the keyers it isn't drawn (it wasn't asked for bare)
                self.native.as_mut().and_then(|n| n.keyed(source.id, &picture.buffer, &steps))
            };
            let Some(shot) = shot else { continue };
            if picture.unfiltered {
                kept.insert(*id);
            }
            shots.insert(*id, shot);
            held.push(picture.buffer);
        }
        if let Some(native) = self.native.as_mut() {
            native.end_frame();
        }
        let ws = self.doc.drawn(kept);
        // a stinger switch: the renderer is given the stinger it plays (the scene's own, or the
        // workspace's) and its frame now, held until the GPU is done with it
        let playing = switch.as_ref().and_then(|p| p.stinger.as_ref());
        let ws = match playing {
            Some((st, _)) if ws.stinger.as_ref() != Some(st) => {
                let mut with = Workspace::clone(&ws);
                with.stinger = Some(st.clone());
                Arc::new(with)
            }
            _ => ws,
        };
        let stinger = playing
            .and_then(|(st, at)| self.stingers.get(&st.path)?.frame(*at))
            .map(|b| Shot { size: b.size(), picture: Picture::Buffer(b), premultiplied: false });
        let mut pictures = Ready { shots: &shots, stinger: stinger.clone() };
        match (&switch, ws.program) {
            (Some(p), _) => {
                let _ = drawer.renderer.transition(
                    &ws,
                    p.from,
                    p.to,
                    canvas.id,
                    &p.settings,
                    p.progress,
                    &mut pictures,
                    time,
                );
            }
            // nothing on air: black, as libobs shows it
            (None, program) => {
                let _ = drawer.renderer.frame(&ws, program.unwrap_or(Id::nil()), canvas.id, &mut pictures, time);
            }
        }
        // the show's overlays, over whatever is on air
        if overlays.is_some() && drawer.over.is_none() {
            drawer.over = overlay_renderer(size);
        }
        let over = match (overlays, drawer.over.as_mut()) {
            (Some(scene), Some(r)) => {
                let _ = r.frame(&ws, scene, canvas.id, &mut pictures, time);
                Some(&*r)
            }
            _ => None,
        };
        // every buffer still in use: this frame is dropped rather than waited for
        let Some(i) = drawer.ring.free(&self.gpu.device) else { return period };
        let target = drawer.renderer.target();
        if !drawer.blit.copy(&self.gpu, target, over.map(|r| r.target()), &drawer.ring.slots[i].texture) {
            return period;
        }
        drawer.ring.handed(i);
        // the GPU is done with the sources' buffers and the stinger's frame
        drop(held);
        drop(stinger);
        let frame = &drawer.ring.slots[i].buffer;
        for preview in showing {
            preview.feed(frame);
            if !self.fed.iter().any(|w| std::ptr::eq(w.as_ptr(), Arc::as_ptr(preview))) {
                self.fed.push(Arc::downgrade(preview));
            }
        }
        program_source::publish(canvas.id, frame);
        period
    }

    /// Draws nothing: the bridge stops drawing sources for it, and its outputs show libobs's
    /// picture.
    fn idle(&mut self) {
        if self.wanted.take().is_some() {
            bridge::want(self.canvas, HashMap::new());
        }
        program_source::withdraw(self.canvas);
    }

    /// Previews given frames that aren't among `viewers` any more: libobs draws them again.
    fn unfeed(&mut self, viewers: &[Arc<Shared>]) {
        self.fed.retain(|w| {
            let keep = viewers.iter().any(|v| std::ptr::eq(w.as_ptr(), Arc::as_ptr(v)));
            if !keep && let Some(preview) = w.upgrade() {
                preview.unfeed();
            }
            keep
        });
    }

    /// The switch `s` as it's drawn at `now`, as libobs plays it: `None` once it's over.
    fn plan(&mut self, ws: &Workspace, s: bridge::Switch, now: Instant) -> Option<Planned> {
        let elapsed = now.saturating_duration_since(s.started).as_millis().min(u32::MAX as u128) as u32;
        let mut settings = s.settings;
        let mut length = settings.duration_ms.max(1);
        let mut over = None;
        let mut stinger = None;
        let mut stand_in = false;
        match settings.kind {
            TransitionKind::Stinger => match stinger_for(ws, s.to) {
                // no video: libobs crossfades
                None => settings.kind = TransitionKind::Fade,
                Some(st) => {
                    let player = self.stingers.entry(st.path.clone()).or_insert_with(|| Player::new(&st.path));
                    // it lasts the video's length, as libobs's stinger does
                    let duration = player.duration_ms();
                    if player.failed() {
                        stand_in = true;
                        length = duration.unwrap_or(length).max(1);
                    } else {
                        length = duration.unwrap_or(UNKNOWN_LENGTH_MS).max(1);
                        stinger = Some((st, elapsed));
                    }
                }
            },
            TransitionKind::Scene => {
                over = ws
                    .scene(s.to)
                    .and_then(|sc| sc.transition_scene)
                    .or(ws.show.transition_scene)
                    .filter(|o| *o != s.from && *o != s.to && ws.scene(*o).is_some());
                // no scene to play: libobs crossfades
                if over.is_none() {
                    settings.kind = TransitionKind::Fade;
                }
            }
            _ => {}
        }
        if elapsed >= length {
            return None;
        }
        settings.duration_ms = length;
        Some(Planned {
            from: s.from,
            to: s.to,
            settings,
            progress: elapsed as f32 / length as f32,
            over,
            stinger,
            started: s.started,
            stand_in,
        })
    }

    /// The stinger drawn now (its switch's start and its path): one whose switch is over is
    /// read from its start again, ready for the next.
    fn played(&mut self, now: Option<(Instant, &str)>) {
        if let Some((started, path)) = &self.playing
            && now.is_none_or(|(t, p)| t != *started || p != path)
        {
            if let Some(player) = self.stingers.get(path) {
                player.rewind();
            }
            self.playing = None;
        }
        if let Some((started, path)) = now {
            self.playing = Some((started, path.to_string()));
        }
    }

    /// Reads ahead the stinger videos `ws`'s switches may play, and lets go of the others
    /// (checked when the workspace changes).
    fn keep_stingers(&mut self, ws: &Arc<Workspace>) {
        let at = Arc::as_ptr(ws) as usize;
        if at == self.stingers_of {
            return;
        }
        self.stingers_of = at;
        let wanted = stingers_in_use(ws);
        let playing = self.playing.as_ref().map(|(_, p)| p.clone());
        self.stingers.retain(|path, _| wanted.contains(path) || playing.as_ref() == Some(path));
        for path in wanted {
            self.stingers.entry(path.clone()).or_insert_with(|| Player::new(&path));
        }
    }
}

/// A switch as the compositor draws it.
struct Planned {
    from: Id,
    to: Id,
    /// What's drawn: the kind libobs falls back to when it can't play the one asked for, and a
    /// stinger's length as its video's.
    settings: TransitionSettings,
    progress: f32,
    /// The transition scene drawn over a Scene transition.
    over: Option<Id>,
    /// A stinger switch: its video, and how far into it (ms).
    stinger: Option<(Stinger, u32)>,
    started: Instant,
    /// libobs draws it (a stinger video the compositor can't read).
    stand_in: bool,
}

/// The stinger a switch to `to` plays, as libobs's is chosen: the scene's own when it has its
/// own transition, else the workspace's.
fn stinger_for(ws: &Workspace, to: Id) -> Option<Stinger> {
    ws.scene(to)
        .filter(|s| s.transition.is_some())
        .and_then(|s| s.stinger.clone())
        .or_else(|| ws.stinger.clone())
        .filter(|s| !s.path.is_empty())
}

/// The stinger videos `ws`'s switches may play: the workspace's when its transition is a
/// stinger, and each scene's (or the workspace's) when the scene switches in with one.
fn stingers_in_use(ws: &Workspace) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    if ws.transition.kind == TransitionKind::Stinger
        && let Some(st) = &ws.stinger
    {
        out.insert(st.path.clone());
    }
    for scene in &ws.scenes {
        let stinger = scene.transition.is_some_and(|t| t.kind == TransitionKind::Stinger)
            || scene.transitions_from.values().any(|t| t.kind == TransitionKind::Stinger);
        if stinger && let Some(st) = stinger_for(ws, scene.id) {
            out.insert(st.path);
        }
    }
    out
}

/// The canvas's frame interval (60 a second without one).
fn frame_period(canvas: Option<&Canvas>) -> Duration {
    let fps = canvas.map_or(60.0, |c| c.fps()).clamp(1.0, 240.0);
    Duration::from_secs_f64(1.0 / fps)
}

// ---------------------------------------------------------------- the document

/// The workspace as the renderer is given it, made again only when the engine's changes or the
/// sources that keep their effects do.
struct Doc {
    from: Option<Arc<Workspace>>,
    /// The engine's workspace with the overlays' scene, effects as they are.
    base: Option<Arc<Workspace>>,
    /// What the renderer was last given: `base` with effects only on the sources listed (the
    /// ones whose pictures came without them).
    drawn: Option<(BTreeSet<Id>, Arc<Workspace>)>,
    /// The scene the drawn workspace has for the show's overlays (every overlay scene's objects,
    /// bottom first), when there are any.
    overlays: Option<Id>,
    /// The id that scene is given.
    overlays_id: Id,
    /// The sources the last scenes asked about show: (canvas, scenes) → sources.
    shown: Option<(Id, Vec<Id>, HashSet<Id>)>,
}

impl Default for Doc {
    fn default() -> Doc {
        Doc { from: None, base: None, drawn: None, overlays: None, overlays_id: Id::new(), shown: None }
    }
}

impl Doc {
    /// The engine's workspace `ws` with the overlays' scene.
    fn update(&mut self, ws: &Arc<Workspace>) -> Arc<Workspace> {
        match (&self.from, &self.base) {
            (Some(from), Some(base)) if Arc::ptr_eq(from, ws) => base.clone(),
            _ => {
                let mut base = Workspace::clone(ws);
                self.overlays = overlay_scene(ws, self.overlays_id).map(|scene| {
                    base.scenes.push(Arc::new(scene));
                    self.overlays_id
                });
                let base = Arc::new(base);
                self.from = Some(ws.clone());
                self.base = Some(base.clone());
                self.drawn = None;
                self.shown = None;
                base
            }
        }
    }

    /// The workspace for the renderer: effects only on the sources in `kept` (their pictures
    /// came without them); every other source's are already in its picture.
    fn drawn(&mut self, kept: BTreeSet<Id>) -> Arc<Workspace> {
        let Some(base) = &self.base else { return Arc::new(Workspace::new("")) };
        // only sources with effects tell two workspaces apart
        let kept: BTreeSet<Id> =
            kept.into_iter().filter(|id| base.sources.get(id).is_some_and(|s| !s.effects.is_empty())).collect();
        match &self.drawn {
            Some((k, ws)) if *k == kept => ws.clone(),
            _ => {
                let ws = Arc::new(without_effects(base, &kept));
                self.drawn = Some((kept, ws.clone()));
                ws
            }
        }
    }

    /// The sources `scenes` show in `canvas` (visible, not see-through).
    fn shown(&mut self, ws: &Workspace, canvas: Id, scenes: &[Id]) -> HashSet<Id> {
        if let Some((c, s, shown)) = &self.shown
            && *c == canvas
            && s == scenes
        {
            return shown.clone();
        }
        let shown: HashSet<Id> = scenes
            .iter()
            .filter_map(|id| ws.scene(*id))
            .flat_map(|scene| scene.flatten().objects)
            .filter(|o| o.layouts.get(&canvas).is_some_and(|l| l.visible && l.opacity > 0.0))
            .filter_map(|o| o.source)
            .collect();
        self.shown = Some((canvas, scenes.to_vec(), shown.clone()));
        shown
    }
}

/// One scene (`id`) holding the objects of every overlay scene of the show, bottom first (a
/// scene that is also on air is drawn once more on top, as libobs does); `None` when no overlay
/// shows anything.
fn overlay_scene(ws: &Workspace, id: Id) -> Option<Scene> {
    let objects: Vec<_> = ws
        .show
        .overlays
        .iter()
        .take(OVERLAYS)
        .filter_map(|o| ws.scene(*o))
        .flat_map(|scene| scene.flatten().objects)
        .collect();
    if objects.is_empty() {
        return None;
    }
    let mut scene = Scene::new("");
    scene.id = id;
    scene.objects = objects;
    Some(scene)
}

/// `ws` with no effects on the sources not in `kept`: libobs has drawn them into those
/// sources' pictures already.
fn without_effects(ws: &Workspace, kept: &BTreeSet<Id>) -> Workspace {
    let mut out = ws.clone();
    for source in out.sources.values_mut() {
        if !source.effects.is_empty() && !kept.contains(&source.id) {
            let mut bare = Source::clone(source);
            bare.effects.clear();
            *source = Arc::new(bare);
        }
    }
    out
}

/// The frame's pictures, made before it's drawn, and the stinger's frame during a stinger.
struct Ready<'a> {
    shots: &'a HashMap<Id, Shot>,
    stinger: Option<Shot>,
}

impl Pictures for Ready<'_> {
    fn picture(&mut self, source: &Source) -> Option<Shot> {
        self.shots.get(&source.id).cloned()
    }

    fn stinger(&mut self, _path: &str, _at_ms: u32) -> Option<Shot> {
        self.stinger.clone()
    }
}

// ---------------------------------------------------------------- pictures made here

/// Whether the compositor makes `source`'s picture itself (when it draws all its effects).
fn made_here(source: &Source) -> bool {
    let s = &source.settings;
    match source.kind.as_str() {
        templates::IMAGE => s["file"].as_str().is_some_and(|f| !f.is_empty()),
        templates::COLOUR | templates::TIMER => true,
        // text read from a file stays libobs's
        templates::TEXT => !s["from_file"].as_bool().unwrap_or(false),
        // the built-in widgets the native widgets draw, once the app has given what they show
        widgets::KIND => widgets::instance(s).is_some_and(|i| widget_feed::drawn_here(&i)),
        _ => false,
    }
}

/// Whether the compositor draws every effect of `source` that changes its picture: the
/// renderer's (shader effects, the colour filter, a LUT from a `.cube` file, as
/// `lightair_render::scene::unsupported` counts them) and, with `keyers`, sharpen and the
/// keyers, which must come before the renderer's in the stack (they're drawn first here).
/// Sound effects don't count.
fn effects_drawn(source: &Source, keyers: bool) -> bool {
    let mut renderer_seen = false;
    for e in source.effects.iter().filter(|e| e.enabled && !e.kind.starts_with("audio.")) {
        if keying::KINDS.contains(&e.kind.as_str()) {
            if !keyers || renderer_seen {
                return false;
            }
        } else if renderer_draws(e) {
            renderer_seen = true;
        } else {
            return false;
        }
    }
    true
}

/// An effect lightair-render's scene renderer draws.
fn renderer_draws(e: &lightair_model::Effect) -> bool {
    let cube = || {
        e.settings.get("image_path").and_then(|p| p.as_str()).is_some_and(|p| p.to_ascii_lowercase().ends_with(".cube"))
    };
    e.kind.starts_with("fx.") || e.kind == "color_filter_v2" || (e.kind == "clut_filter" && cube())
}

/// The pictures made here, with `lightair-engine`'s pictures (decoded images and GIFs, colours),
/// text as libobs's text source draws it and the native widgets (kept as textures while they're
/// the same), and the keyers for libobs's bare pictures, on the renderer's GPU.
struct Native {
    gpu: Gpu,
    cache: lightair_engine::pictures::Cache,
    /// What the engine's pictures read besides the source (no feeds, chat or alerts here).
    hot: Hot,
    program: Program,
    keying: keying::Pipeline,
    keyed: keying::Targets,
    /// The workspace the per-source textures were last kept for.
    sources_of: usize,
    /// Each image file's own size (a GIF's frames are kept smaller; the layout's crop is in the
    /// file's pixels, as libobs has it).
    sizes: HashMap<PathBuf, Option<(u32, u32)>>,
    /// Text and timers drawn here, by what decides their picture (`text::key`), and the ones
    /// this frame used.
    texts: HashMap<String, (wgpu::Texture, (u32, u32))>,
    texts_used: HashSet<String>,
    /// The widgets drawn here, by source.
    widgets: HashMap<Id, Widget>,
}

/// A widget's picture and what it was made from.
struct Widget {
    /// What decided it (`widget_feed::probe`): made again when that changes.
    probe: String,
    texture: wgpu::Texture,
    size: (u32, u32),
    made: Instant,
}

impl Native {
    fn new() -> Option<Native> {
        let gpu = Gpu::new().map_err(|e| tracing::error!(%e, "compositor: no GPU for its own pictures")).ok()?;
        let program = Program {
            workspace: Arc::new(Workspace::new("")),
            folder: PathBuf::new(),
            scene: Id::nil(),
            canvas: Id::nil(),
            switch: None,
            privacy_scene: None,
            feeds: HashMap::new(),
            on_air_ns: 0,
            utc_offset_min: 0,
            frozen: Vec::new(),
        };
        let hot = Hot::new(program.clone(), 0);
        let keying = keying::Pipeline::new(gpu.device());
        Some(Native {
            gpu,
            cache: Default::default(),
            hot,
            program,
            keying,
            keyed: Default::default(),
            sources_of: 0,
            sizes: HashMap::new(),
            texts: HashMap::new(),
            texts_used: HashSet::new(),
            widgets: HashMap::new(),
        })
    }

    /// Lets go of what sources no longer in `ws` kept (checked when the workspace changes).
    fn keep(&mut self, ws: &Arc<Workspace>) {
        self.cache.keep_sources(ws);
        let at = Arc::as_ptr(ws) as usize;
        if at != self.sources_of {
            self.sources_of = at;
            let keep: Vec<Id> = ws.sources.keys().copied().collect();
            self.keyed.forget_others(&keep);
            self.widgets.retain(|id, _| ws.sources.contains_key(id));
            self.sizes.retain(|path, _| {
                ws.sources.values().any(|s| s.settings["file"].as_str().is_some_and(|f| Path::new(f) == path))
            });
        }
    }

    /// `source`'s picture made here, keyed if it has keys; `None` while it isn't ready.
    fn picture(&mut self, source: &Source, pts_ns: i64) -> Option<Shot> {
        let made = match source.kind.as_str() {
            // a timer: its text as the engine's timers have it now, drawn as its text source does
            templates::TIMER => {
                let text = bridge::timer_text(source.id)?;
                Some(self.text(&source.settings, &text)?)
            }
            templates::TEXT => Some(self.text(&source.settings, source.settings["text"].as_str().unwrap_or(""))?),
            widgets::KIND => Some(self.widget(source)?),
            _ => None,
        };
        if let Some(shot) = made {
            let steps = keying::steps(source);
            if steps.is_empty() {
                return Some(shot);
            }
            let Picture::Texture(texture) = &shot.picture else { return Some(shot) };
            let out =
                keying::draw(&self.gpu, &self.keying, &mut self.keyed, source.id, texture, shot.premultiplied, &steps, None);
            return Some(Shot { picture: Picture::Texture(out), size: shot.size, premultiplied: false });
        }
        let mut frame = lightair_engine::pictures::Frame {
            gpu: &self.gpu,
            cache: &mut self.cache,
            hot: &self.hot,
            program: &self.program,
            host_ns: 0,
            pts_ns,
            used: Vec::new(),
        };
        let mut shot = frame.picture(source)?;
        if source.kind == templates::IMAGE
            && let Some(file) = source.settings["file"].as_str()
        {
            let path = PathBuf::from(file);
            // the file's header only
            let size = *self.sizes.entry(path).or_insert_with_key(|p| image::image_dimensions(p).ok());
            if let Some(size) = size {
                shot.size = size;
            }
        }
        Some(shot)
    }

    /// libobs's bare picture of source `id` (colours multiplied by alpha) through its sharpen and
    /// keyer passes, as a straight-alpha texture. The buffer must be held until the GPU is done.
    fn keyed(&mut self, id: Id, buffer: &bridge::Buffer, steps: &[keying::Step]) -> Option<Shot> {
        let texture = lightair_engine::gpu::apple::sampled(self.gpu.device(), buffer)?;
        let out = keying::draw(&self.gpu, &self.keying, &mut self.keyed, id, &texture, true, steps, None);
        let size = (out.width(), out.height());
        Some(Shot { picture: Picture::Texture(out), size, premultiplied: false })
    }

    /// A text object's picture (`settings` showing `text`), drawn as libobs's text source draws
    /// it; `None` when it draws nothing.
    fn text(&mut self, settings: &serde_json::Value, text: &str) -> Option<Shot> {
        let key = crate::text::key(settings, text);
        if !self.texts.contains_key(&key) {
            let (rgba, size) = crate::text::draw(settings, text);
            if size.0 == 0 || size.1 == 0 {
                return None;
            }
            let texture = self.gpu.texture(&rgba, size);
            self.texts.insert(key.clone(), (texture, size));
        }
        let (texture, size) = self.texts.get(&key)?;
        let shot = Shot { picture: Picture::Texture(texture.clone()), size: *size, premultiplied: true };
        self.texts_used.insert(key);
        Some(shot)
    }

    /// A built-in widget's picture, drawn by the native widgets from what its page would show;
    /// made again when that changes (at most every [`WIDGET_PERIOD`]).
    fn widget(&mut self, source: &Source) -> Option<Shot> {
        let live = widget_feed::live()?;
        let instance = widgets::instance(&source.settings)?;
        let s = &source.settings;
        let size = (s["width"].as_u64().unwrap_or(800) as u32, s["height"].as_u64().unwrap_or(600) as u32);
        live.refresh();
        let probe = widget_feed::probe(&live, &instance, size);
        let old = self.widgets.get(&source.id);
        let same = old.is_some_and(|w| w.probe == probe);
        let recent = old.is_some_and(|w| w.made.elapsed() < WIDGET_PERIOD);
        if !same && !recent {
            let (rgba, size) = widget_feed::prepare(&live, &instance, size).draw();
            let texture = self.gpu.texture(&rgba, size);
            self.widgets.insert(source.id, Widget { probe, texture, size, made: Instant::now() });
        }
        let w = self.widgets.get(&source.id)?;
        Some(Shot { picture: Picture::Texture(w.texture.clone()), size: w.size, premultiplied: true })
    }

    /// Drops the made pictures no frame used lately.
    fn end_frame(&mut self) {
        self.cache.end_frame();
        if self.texts.len() > 32 {
            let used = std::mem::take(&mut self.texts_used);
            self.texts.retain(|k, _| used.contains(k));
        } else {
            self.texts_used.clear();
        }
    }
}

// ---------------------------------------------------------------- drawing

/// The renderers and output buffers for one canvas size.
struct Drawer {
    size: (u32, u32),
    renderer: SceneRenderer,
    /// The show's overlays, drawn on transparent black (made when there are some).
    over: Option<SceneRenderer>,
    ring: Ring,
    blit: Blit,
}

impl Drawer {
    fn new(gpu: &SharedGpu, size: (u32, u32)) -> Option<Drawer> {
        let mut renderer =
            SceneRenderer::new(size).map_err(|e| tracing::error!(%e, "compositor: SceneRenderer")).ok()?;
        renderer.keep_on_gpu(true);
        let blit = Blit::new(&gpu.device, renderer.target().format().is_srgb());
        let mut ring = Ring { size, slots: Vec::new(), recent: VecDeque::new() };
        // one to start with: none means this device can't draw into IOSurfaces
        ring.free(&gpu.device)?;
        Some(Drawer { size, renderer, over: None, ring, blit })
    }
}

/// A renderer for the show's overlays at `size`: frames start see-through, so only what the
/// overlays cover is laid over the program.
fn overlay_renderer(size: (u32, u32)) -> Option<SceneRenderer> {
    let mut renderer =
        SceneRenderer::new(size).map_err(|e| tracing::error!(%e, "compositor: SceneRenderer for the overlays")).ok()?;
    renderer.keep_on_gpu(true);
    renderer.compositor().set_transparent(true);
    Some(renderer)
}

/// An output buffer: a BGRA IOSurface-backed pixel buffer, and a texture over its memory that
/// frames are drawn into.
struct Out {
    buffer: Frame,
    texture: wgpu::Texture,
}

impl Out {
    fn new(device: &wgpu::Device, size: (u32, u32)) -> Option<Out> {
        let (w, h) = size;
        let buffer = pixel_buffer(w, h, kCVPixelFormatType_32BGRA)?;
        let iosurface = CVPixelBufferGetIOSurface(Some(&buffer))?;
        let raw: Retained<ProtocolObject<dyn MTLTexture>> = {
            // SAFETY: the raw Metal device is only used to make a texture here.
            let hal = unsafe { device.as_hal::<wgpu::hal::api::Metal>() }?;
            // SAFETY: a plain 2D BGRA descriptor.
            let desc = unsafe {
                MTLTextureDescriptor::texture2DDescriptorWithPixelFormat_width_height_mipmapped(
                    MTLPixelFormat::BGRA8Unorm,
                    w as usize,
                    h as usize,
                    false,
                )
            };
            desc.setUsage(MTLTextureUsage::RenderTarget | MTLTextureUsage::ShaderRead);
            hal.raw_device().newTextureWithDescriptor_iosurface_plane(&desc, &iosurface, 0)?
        };
        // SAFETY: `raw` is a BGRA8Unorm 2D texture of `size` on this device; the pixel buffer that
        // owns its memory lives as long as the texture (both in `Out`).
        let hal_texture = unsafe {
            wgpu::hal::metal::Device::texture_from_raw(
                raw,
                wgpu::TextureFormat::Bgra8Unorm,
                MTLTextureType::Type2D,
                1,
                1,
                wgpu::hal::CopyExtent { width: w, height: h, depth: 1 },
                None,
            )
        };
        // SAFETY: the descriptor matches the texture made above.
        let texture = unsafe {
            device.create_texture_from_hal::<wgpu::hal::api::Metal>(
                hal_texture,
                &wgpu::TextureDescriptor {
                    label: Some("lightair-compose-out"),
                    size: wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
                    mip_level_count: 1,
                    sample_count: 1,
                    dimension: wgpu::TextureDimension::D2,
                    format: wgpu::TextureFormat::Bgra8Unorm,
                    usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
                    view_formats: &[],
                },
                wgpu::TextureUses::UNINITIALIZED,
            )
        };
        Some(Out { buffer, texture })
    }
}

/// The output buffers.
struct Ring {
    size: (u32, u32),
    slots: Vec<Out>,
    /// The ones handed on last, newest first.
    recent: VecDeque<usize>,
}

impl Ring {
    /// A buffer nothing else holds and not handed on lately; a new one while there are fewer
    /// than [`MOST`].
    fn free(&mut self, device: &wgpu::Device) -> Option<usize> {
        let free =
            (0..self.slots.len()).find(|i| !self.recent.contains(i) && self.slots[*i].buffer.retain_count() == 1);
        match free {
            Some(i) => Some(i),
            None if self.slots.len() < MOST => {
                self.slots.push(Out::new(device, self.size)?);
                Some(self.slots.len() - 1)
            }
            None => None,
        }
    }

    fn handed(&mut self, i: usize) {
        self.recent.push_front(i);
        self.recent.truncate(RECENT);
    }
}

/// Copies the renderer's frame into an output buffer, with the overlays' frame laid over it
/// when there is one (both premultiplied), sRGB-encoding it on the way when the renderers'
/// targets hold linear light.
struct Blit {
    pipeline: wgpu::RenderPipeline,
    layout: wgpu::BindGroupLayout,
    over_pipeline: wgpu::RenderPipeline,
    over_layout: wgpu::BindGroupLayout,
}

const BLIT: &str = r#"
@group(0) @binding(0) var frame: texture_2d<f32>;
@group(0) @binding(1) var over: texture_2d<f32>;

@vertex
fn vs(@builtin(vertex_index) i: u32) -> @builtin(position) vec4<f32> {
    // one triangle over the whole target
    let p = vec2<f32>(f32((i << 1u) & 2u), f32(i & 2u));
    return vec4<f32>(p * 2.0 - 1.0, 0.0, 1.0);
}

fn encode(c: vec3<f32>) -> vec3<f32> {
    let lo = c * 12.92;
    let hi = 1.055 * pow(max(c, vec3<f32>(0.0)), vec3<f32>(1.0 / 2.4)) - 0.055;
    return select(hi, lo, c <= vec3<f32>(0.0031308));
}

@fragment
fn fs_encode(@builtin(position) at: vec4<f32>) -> @location(0) vec4<f32> {
    let c = textureLoad(frame, vec2<i32>(at.xy), 0);
    return vec4<f32>(encode(c.rgb), c.a);
}

@fragment
fn fs_copy(@builtin(position) at: vec4<f32>) -> @location(0) vec4<f32> {
    return textureLoad(frame, vec2<i32>(at.xy), 0);
}

// the overlays (premultiplied) over the frame
fn overlaid(at: vec4<f32>) -> vec4<f32> {
    let p = textureLoad(frame, vec2<i32>(at.xy), 0);
    let o = textureLoad(over, vec2<i32>(at.xy), 0);
    return o + p * (1.0 - o.a);
}

@fragment
fn fs_over_encode(@builtin(position) at: vec4<f32>) -> @location(0) vec4<f32> {
    let c = overlaid(at);
    return vec4<f32>(encode(c.rgb), c.a);
}

@fragment
fn fs_over_copy(@builtin(position) at: vec4<f32>) -> @location(0) vec4<f32> {
    return overlaid(at);
}
"#;

impl Blit {
    fn new(device: &wgpu::Device, encode: bool) -> Blit {
        let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("lightair-compose-blit"),
            source: wgpu::ShaderSource::Wgsl(BLIT.into()),
        });
        let texture = |binding| wgpu::BindGroupLayoutEntry {
            binding,
            visibility: wgpu::ShaderStages::FRAGMENT,
            ty: wgpu::BindingType::Texture {
                sample_type: wgpu::TextureSampleType::Float { filterable: false },
                view_dimension: wgpu::TextureViewDimension::D2,
                multisampled: false,
            },
            count: None,
        };
        let layout =
            device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor { label: None, entries: &[texture(0)] });
        let over_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: None,
            entries: &[texture(0), texture(1)],
        });
        let pipeline = |layout: &wgpu::BindGroupLayout, entry: &str| {
            let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: None,
                bind_group_layouts: &[Some(layout)],
                immediate_size: 0,
            });
            device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some("lightair-compose-blit"),
                layout: Some(&pipeline_layout),
                vertex: wgpu::VertexState {
                    module: &module,
                    entry_point: Some("vs"),
                    buffers: &[],
                    compilation_options: Default::default(),
                },
                fragment: Some(wgpu::FragmentState {
                    module: &module,
                    entry_point: Some(entry),
                    targets: &[Some(wgpu::ColorTargetState {
                        format: wgpu::TextureFormat::Bgra8Unorm,
                        blend: None,
                        write_mask: wgpu::ColorWrites::ALL,
                    })],
                    compilation_options: Default::default(),
                }),
                primitive: wgpu::PrimitiveState::default(),
                depth_stencil: None,
                multisample: wgpu::MultisampleState::default(),
                multiview_mask: None,
                cache: None,
            })
        };
        Blit {
            pipeline: pipeline(&layout, if encode { "fs_encode" } else { "fs_copy" }),
            over_pipeline: pipeline(&over_layout, if encode { "fs_over_encode" } else { "fs_over_copy" }),
            layout,
            over_layout,
        }
    }

    /// Draws `frame`, with `over` laid over it if given, into `out` and waits for the GPU to
    /// finish (so the buffer is complete when it's handed on). False when the GPU didn't finish
    /// in time.
    fn copy(&self, gpu: &SharedGpu, frame: &wgpu::Texture, over: Option<&wgpu::Texture>, out: &wgpu::Texture) -> bool {
        let source = frame.create_view(&Default::default());
        let over = over.map(|t| t.create_view(&Default::default()));
        let target = out.create_view(&Default::default());
        let group = match &over {
            Some(over) => gpu.device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: None,
                layout: &self.over_layout,
                entries: &[
                    wgpu::BindGroupEntry { binding: 0, resource: wgpu::BindingResource::TextureView(&source) },
                    wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::TextureView(over) },
                ],
            }),
            None => gpu.device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: None,
                layout: &self.layout,
                entries: &[wgpu::BindGroupEntry { binding: 0, resource: wgpu::BindingResource::TextureView(&source) }],
            }),
        };
        let mut encoder = gpu.device.create_command_encoder(&Default::default());
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: None,
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &target,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            pass.set_pipeline(if over.is_some() { &self.over_pipeline } else { &self.pipeline });
            pass.set_bind_group(0, &group, &[]);
            pass.draw(0..3, 0..1);
        }
        let submitted = gpu.queue.submit([encoder.finish()]);
        gpu.device.poll(wgpu::PollType::Wait { submission_index: Some(submitted), timeout: Some(GPU_WAIT) }).is_ok()
    }
}
