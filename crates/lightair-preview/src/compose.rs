//! Each canvas's program drawn by Lightair's own compositor (`lightair-render`'s
//! `SceneRenderer`) instead of libobs, for the canvas's previews and its outputs: on macOS with
//! Metal into IOSurfaces, on Windows with Direct3D 12 into shared Direct3D 11 textures.
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
//! Windows: the ring's buffers are Lightair's own pictures there (`lightair_host::buffers`: BGRA
//! Direct3D 11 textures with NT shared handles), opened on the compositor's Direct3D 12 device
//! (`crate::win_gpu`); the window's preview copies each into its swap chain
//! (`readback::own`). Captured frames and pictures from the providers are opened the same way and
//! drawn as textures (`crate::buffer_gpu`). What has no Windows side yet (the camera looks, face
//! detection, stinger videos, web pages, media files and the other providers) is drawn as
//! nothing, or without that effect, and says so in the log once.
//!
//! The pictures ([`Native`]):
//!
//! - **Made here**, with `lightair-engine`'s pictures: images (GIFs animated), colours (and so
//!   the shapes, which are colours cut by their layout); text and timers (their text from the
//!   engine's timers) as libobs's text source draws them, in the faces it finds (`text`); and
//!   the built-in widgets the native widgets draw (chat box, alert box, event list, goal,
//!   label, counter, subathon timer), from the app's chat hub, the widget server's storage and
//!   its alert queue (`widget_feed`); and avatars (`avatars::AvatarRenderer`), posed from what
//!   their libobs sources publish each tick (the face, body, co-host mood, mic and push to talk
//!   their camera, trackers and calibration give: `bridge::avatar_posing`); and image
//!   slideshows, their slides and the switches between them (`slideshow`). Until one is ready
//!   (an image decoding, a timer before its first tick, an avatar's model loading) libobs's
//!   picture stands in.
//! - **Captured here** (`capture`, with `lightair-capture`): screens (displays, windows, apps)
//!   and cameras, one capture per source for every canvas, running while a compositor draws a
//!   scene that shows the source; their frames are drawn straight from the capture's IOSurfaces.
//!   The source's libobs wrapper runs no capture of its own meanwhile and draws these frames, so
//!   a source whose effects libobs draws is still captured once. Until a capture's first frame
//!   (or where it stays libobs's: access not yet given, a capture card with its sound in the same
//!   stream), libobs's picture stands in.
//! - **From libobs** (`bridge`), for everything else: media files, web pages, other widgets,
//!   devices, NDI, Syphon, the phone camera…
//!
//! Objects pinned to an avatar's anchors follow them as libobs's pinned items do: each frame the
//! anchors' places the avatar's libobs source published move the pinned objects' layouts in the
//! document the renderer is given (`Doc::follow_pins`).
//!
//! Effects: the compositor draws a source's effects when it can draw all of them: the
//! renderer's (shader effects, the colour filter, a `.cube` LUT), the engine's sharpen and
//! keyers (chroma, colour, luma key), and the engine's camera looks (the Effect Stack's
//! beauty, makeup, face shape, light, grade and background effects, with faces and masks from
//! the module's own models: `looks`); the looks first in the stack, then the keyers, as
//! they're drawn in that order here. Such a source's picture is made bare (the bridge draws it
//! without its libobs filters), looked and keyed on the GPU, then given to the renderer with
//! its effects. A source with any other effect (stickers, camera motion, ambience, masks,
//! freeze, scroll…) is drawn by libobs through all its filters, and the renderer is given it
//! without its effects so nothing is drawn twice. The renderer does the rest: position, fit,
//! crop, turn, flip, tilt, shape, opacity, and the switch between scenes.
//!
//! Scenes shown in the program's place (`Preview::set_source` with a scene: Studio mode's
//! preview of the scene not on air, the phones' and the remote's shots, thumbnails, a
//! multiview's views) are drawn by the compositor of the canvas they're laid out in, each by a
//! renderer of its own at the view's size ([`View`]), from the same pictures the program is
//! drawn from that frame: their sources are gathered, captured and asked of libobs with the
//! program's, so nothing is captured or drawn by libobs twice. A view is drawn only while its
//! preview is active, at its own rate (`Preview::set_rate`; a view a quarter of the canvas's
//! area or smaller at most [`THUMBNAIL_FPS`] a second), without the show's overlays, as libobs
//! draws a scene there. A source other than a scene shown there is still libobs's.
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

use lightair_avatar::source::Input;
use lightair_engine::gpu::Gpu;
use lightair_engine::hot::{Hot, Program};
use lightair_engine::{keying, templates, widgets};
use lightair_host::avatar_pins::{Placement, Spot, on_canvas};
use lightair_host::program as program_source;
use lightair_host::{avatar, bridge};
use lightair_model::{Canvas, Fit, Id, Layout, Scene, Source, Stinger, TransitionKind, TransitionSettings, Workspace};
use lightair_render::SharedGpu;
use lightair_render::media::Picture;
use lightair_render::scene::{Pictures, SceneRenderer, Shot};
use parking_lot::Mutex;

use crate::avatars::{AvatarRenderer, TrackingView};
use crate::buffer_gpu;
use crate::capture;
use crate::looks;
#[cfg(target_os = "macos")]
use crate::mac::{Frame, SceneView, Shared};
#[cfg(windows)]
use crate::readback::{SceneView, Shared};
/// A finished frame: one of Lightair's own pictures.
#[cfg(windows)]
type Frame = lightair_host::buffers::Buffer;
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
/// An avatar's posing older than this is stale (its libobs source stopped ticking): the
/// avatar is drawn as with nothing tracked.
const POSING_STALE: Duration = Duration::from_millis(500);
/// The most frames a second a small view of a scene (a quarter of the canvas's area or less) is
/// drawn, unless its preview asks for another rate.
const THUMBNAIL_FPS: f64 = 30.0;

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
    /// The previews whose scenes shown in the program's place the compositors draw.
    scenes: Vec<Weak<Shared>>,
    /// Canvases something records or streams from.
    in_use: HashSet<Id>,
}

static HUB: LazyLock<Mutex<Hub>> = LazyLock::new(Default::default);

/// Windows: Lightair's own pictures and captures are made on the compositor's graphics adapter
/// from now on (once), so its device can open them.
#[cfg(windows)]
pub(crate) fn share_adapter() {
    static SHARED: OnceLock<()> = OnceLock::new();
    SHARED.get_or_init(|| {
        let Ok(gpu) = lightair_render::shared_gpu() else { return };
        if !crate::win_gpu::share_adapter(&gpu.device) {
            tracing::error!("compositor: its device isn't Direct3D 12; it can't open Lightair's pictures");
        }
    });
}

/// Starts the hub (once); false when there's no GPU to draw with.
pub(crate) fn start() -> bool {
    static STARTED: OnceLock<bool> = OnceLock::new();
    *STARTED.get_or_init(|| {
        if let Err(e) = lightair_render::shared_gpu() {
            tracing::error!(%e, "compositor: no GPU; libobs draws the program");
            return false;
        }
        #[cfg(windows)]
        share_adapter();
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

/// Draws the scenes `preview` shows in the program's place from now on (whenever it shows one).
pub(crate) fn watch_scenes(preview: &Arc<Shared>) {
    let mut hub = HUB.lock();
    if !hub.scenes.iter().any(|w| std::ptr::eq(w.as_ptr(), Arc::as_ptr(preview))) {
        hub.scenes.push(Arc::downgrade(preview));
    }
}

/// Stops giving `preview` frames: libobs draws its program, and any scene in its place, again.
pub(crate) fn unwatch(preview: &Arc<Shared>) {
    {
        let mut hub = HUB.lock();
        hub.viewers.retain(|v| !std::ptr::eq(v.shared.as_ptr(), Arc::as_ptr(preview)));
        hub.scenes.retain(|w| !std::ptr::eq(w.as_ptr(), Arc::as_ptr(preview)));
    }
    preview.unfeed();
    preview.unfeed_scene();
}

/// The previews showing a scene of `canvas` in the program's place, and what each shows.
fn scene_views(canvas: Id) -> Vec<(Arc<Shared>, SceneView)> {
    let hub = HUB.lock();
    hub.scenes
        .iter()
        .filter_map(|w| w.upgrade())
        .filter_map(|preview| {
            let view = preview.scene_view()?;
            (view.canvas == canvas).then_some((preview, view))
        })
        .collect()
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
            hub.scenes.retain(|w| w.strong_count() > 0);
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
        captures: None,
        captures_of: None,
        fed: Vec::new(),
        clock: Instant::now(),
        stingers: HashMap::new(),
        ghost: None,
        prepared: None,
        stingers_of: 0,
        playing: None,
        views: HashMap::new(),
        handover: None,
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
    state.stop_captures();
    state.unfeed(&[]);
    state.drop_views();
}

/// What the thread keeps between ticks.
/// The document as drawn, from the engine's document and the effects switched off live
/// (eyedropper): what it was made from, those switches, and the result.
type Prepared = (Arc<Workspace>, Vec<(Id, Id)>, Arc<Workspace>);

/// An avatar's anchors at rest and now, by key, and its picture's size.
type AvatarPins = (Vec<(String, Spot, Spot)>, (u32, u32));

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
    /// The screens and cameras the scenes drawn show, as last given to the captures (`None`:
    /// none asked for).
    captures: Option<HashMap<Id, capture::Plan>>,
    /// What those were worked out from: the engine's workspace (by address) and the sources shown.
    captures_of: Option<(usize, HashSet<Id>)>,
    /// The previews given frames.
    fed: Vec<Weak<Shared>>,
    /// The clock moving effects run on.
    clock: Instant,
    /// The stinger videos the workspace's switches may play, by path, read ahead.
    stingers: HashMap<String, Player>,
    /// Crop view's ghost: its object's id, its opacity now, and when that last moved.
    ghost: Option<(Id, f32, Instant)>,
    /// The document as drawn (live switches and makeup LUTs), and what it was made from.
    prepared: Option<Prepared>,
    /// The workspace they were last kept for.
    stingers_of: usize,
    /// The stinger switch being drawn: when it started, and its video's path.
    playing: Option<(Instant, String)>,
    /// The scenes previews show in the program's place, and those drawn for outputs.
    views: HashMap<ViewKey, View>,
    /// Hands each program frame on once the GPU has finished it (made at the first frame).
    handover: Option<Handover>,
}

/// Whose a view is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum ViewKey {
    /// A preview's (its address).
    Preview(usize),
    /// An output's: a scene drawn for it alone (`program_source::SceneTarget`'s key).
    Output(Id),
}

/// Where a view's frames go.
enum ViewTo {
    Preview(Weak<Shared>),
    /// Handed to the outputs under this key.
    Output(Id),
}

impl ViewTo {
    /// The preview stops being given frames of this view: libobs draws its scene again.
    fn unfeed(&self) {
        if let ViewTo::Preview(w) = self
            && let Some(preview) = w.upgrade()
        {
            preview.unfeed_scene();
        }
    }
}

/// A scene a preview shows in the program's place, or one drawn for an output, drawn for it.
struct View {
    to: ViewTo,
    scene: Id,
    /// What the preview showed when this was worked out (`SceneView::generation`).
    generation: u64,
    /// The size it's drawn at, and the time between its frames.
    size: (u32, u32),
    period: Duration,
    /// When it was last drawn (`None`: not since what's shown changed).
    drawn: Option<Instant>,
    /// Its renderer and buffers (no overlays drawn).
    drawer: Option<Drawer>,
    /// A size nothing could be made for (not tried again; libobs draws it).
    failed: Option<(u32, u32)>,
}

impl View {
    /// Whether a frame is due at `now`, the compositor ticking every `tick`.
    fn due(&self, now: Instant, tick: Duration) -> bool {
        self.drawn.is_none_or(|t| now.saturating_duration_since(t) + tick / 2 >= self.period)
    }
}

/// The size a view of `canvas` is drawn at: the size its preview asks for, at most the canvas's
/// (its pictures are no larger), even.
fn view_size(view: &SceneView, canvas: &Canvas) -> (u32, u32) {
    let whole = (canvas.width.max(2), canvas.height.max(2));
    let (w, h) = match view.size {
        Some((w, h)) if w <= whole.0 && h <= whole.1 => (w, h),
        _ => whole,
    };
    (w.max(2) & !1, h.max(2) & !1)
}

/// The time between a view's frames: the rate its preview asks for, else at most
/// [`THUMBNAIL_FPS`] for a small one; never faster than the canvas (`tick`).
fn view_period(view: &SceneView, size: (u32, u32), canvas: &Canvas, tick: Duration) -> Duration {
    let small = size.0 as u64 * size.1 as u64 * 4 <= canvas.width as u64 * canvas.height as u64;
    let fps = match view.rate {
        Some(fps) => fps as f64,
        None if small => THUMBNAIL_FPS,
        None => return tick,
    };
    Duration::from_secs_f64(1.0 / fps.max(1.0)).max(tick)
}

impl State {
    /// Draws a frame of the program, and of the scenes previews show in its place, if one is
    /// wanted; the time until the next.
    fn tick(&mut self) -> Duration {
        let started = Instant::now();
        let Some(snapshot) = bridge::snapshot() else {
            self.idle();
            self.stop_captures();
            self.drop_views();
            return frame_period(None);
        };
        let Some(canvas) = snapshot.workspace.canvas(self.canvas).cloned() else {
            self.idle();
            self.stop_captures();
            self.drop_views();
            return frame_period(None);
        };
        let period = frame_period(Some(&canvas));
        let primary = snapshot.workspace.canvases.first().is_some_and(|c| c.id == self.canvas);
        let (viewers, in_use) = viewers(self.canvas, &self.name, primary);
        // previews that no longer look at this canvas show libobs's frames again
        self.unfeed(&viewers);
        let showing: Vec<&Arc<Shared>> = viewers.iter().filter(|v| v.wants_program()).collect();
        // the program is drawn for its previews and the canvas's outputs
        let program = !showing.is_empty() || in_use;
        // effects switched off live (the eyedropper picking a colour under a key), and the looks'
        // makeup LUTs as the renderer's (made once per change of the document)
        let off = bridge::live_off();
        let prepared = match &self.prepared {
            Some((from, was, out)) if Arc::ptr_eq(from, &snapshot.workspace) && *was == off => out.clone(),
            _ => {
                let out = with_makeup_luts(live_off(snapshot.workspace.clone(), &off));
                self.prepared = Some((snapshot.workspace.clone(), off, out.clone()));
                out
            }
        };
        let base = self.doc.update(&prepared);
        // the scenes previews show in the program's place (those still in the document)
        let wanted: Vec<(Arc<Shared>, SceneView)> =
            scene_views(self.canvas).into_iter().filter(|(_, v)| base.scene(v.scene).is_some()).collect();
        // and those drawn for outputs on their own (the virtual camera's one scene)
        let targets: Vec<program_source::SceneTarget> =
            program_source::scene_targets(self.canvas).into_iter().filter(|t| base.scene(t.scene).is_some()).collect();
        self.keep_views(&wanted, &targets, &canvas, period);
        if !program && self.views.is_empty() {
            self.idle();
            self.stop_captures();
            return period;
        }
        let now = Instant::now();
        let due: Vec<ViewKey> = self.views.iter().filter(|(_, v)| v.due(now, period)).map(|(k, _)| *k).collect();
        if !program {
            // only views: the outputs show libobs's picture
            program_source::withdraw(self.canvas);
            if due.is_empty() {
                // nothing to draw this tick; what's asked for stays asked for
                return period;
            }
        }
        self.keep_stingers(&base);
        // the switch to what's on air, while it runs, as libobs draws it
        let switch =
            snapshot.switch.filter(|s| program && base.program == Some(s.to)).and_then(|s| self.plan(&base, s, now));
        self.played(switch.as_ref().and_then(|p| p.stinger.as_ref().map(|(st, _)| (p.started, st.path.as_str()))));
        if switch.as_ref().is_some_and(|p| p.stand_in) {
            // libobs draws this switch (a stinger video the compositor can't read), and the views
            // meanwhile; the captures keep running (libobs's wrappers draw their frames), so they
            // don't restart after it
            self.idle();
            self.unfeed(&[]);
            self.drop_views();
            return period;
        }
        let mut scenes: Vec<Id> = match (&switch, program) {
            (Some(p), _) => [Some(p.from), Some(p.to), p.over].into_iter().flatten().collect(),
            (None, true) => base.program.into_iter().collect(),
            (None, false) => Vec::new(),
        };
        let overlays = if program { self.doc.overlays } else { None };
        scenes.extend(overlays);
        // the views' scenes, in a steady order (the sources shown are worked out again when it
        // changes)
        let viewed: BTreeSet<Id> = self.views.values().map(|v| v.scene).collect();
        for scene in viewed {
            if !scenes.contains(&scene) {
                scenes.push(scene);
            }
        }
        let size = (canvas.width.max(2) & !1, canvas.height.max(2) & !1);
        if program && self.drawer.as_ref().is_none_or(|d| d.size != size) {
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
                self.stop_captures();
                self.unfeed(&[]);
                self.drop_views();
                return period;
            }
        }
        let shown = self.doc.shown(&base, canvas.id, &scenes);
        // (what's shown runs: a web page or an avatar's camera in a preview, not only on air)
        lightair_host::show::set_shown(canvas.id, &shown);
        if self.native.is_none() && !self.native_failed {
            self.native = Native::new();
            self.native_failed = self.native.is_none();
        }
        let keyers = self.native.is_some();
        // the camera looks: with the engine's pictures, and a module that gives faces and masks
        let looked = keyers && looks::available();
        // Lightair's own pointer and zoom (a screen look, a drawn pointer effect): drawn here on a
        // source with no other effect, else by libobs with its filters
        let pointed: HashMap<Id, lightair_host::pointer::Style> = crate::pointer::styles(&base, &shown);
        let pointer_here = |source: &Source| {
            !pointed.contains_key(&source.id) || (keyers && !crate::pointer::has_other_effects(source))
        };
        // with nothing else to draw a source (no libobs), every source is drawn here, with the
        // effects the compositor draws (the others are left out)
        let alone = !bridge::elsewhere();
        let drawn_here = |source: &Source| {
            let all = effects_drawn(source, keyers, looked) && pointer_here(source);
            if !all && alone {
                skipped_effects(source);
            }
            all || alone
        };
        if let Some(native) = self.native.as_mut() {
            native.pointers.keep(&pointed);
            native.timed.keep(&shown);
        }
        self.missing.retain(|id| shown.contains(id));
        // the screens and cameras shown, captured here (once for every canvas)
        self.keep_captures(&snapshot.workspace, &shown);
        let captured: HashMap<Id, capture::Captured> = self
            .captures
            .iter()
            .flat_map(|plans| plans.keys())
            .filter_map(|id| Some((*id, capture::frame(*id)?)))
            .collect();
        // the sources a provider makes (web pages, NDI, Syphon…), told what's shown here
        let shown_sources: Vec<&Source> = shown.iter().filter_map(|id| base.sources.get(id).map(|s| &**s)).collect();
        crate::providers::want(canvas.id, &shown_sources);
        let provided: HashMap<Id, bridge::Picture> = crate::providers::frames(&shown_sources);
        // what libobs is asked for: the sources not made or captured here (bare when the
        // compositor draws all their effects), and those made here whose picture isn't ready
        let want: HashMap<Id, bool> = shown
            .iter()
            .filter_map(|id| base.sources.get(id))
            .filter_map(|source| {
                let effects = drawn_here(source);
                let made = keyers && effects && made_here(source) && !self.missing.contains(&source.id);
                let taken = effects && (captured.contains_key(&source.id) || provided.contains_key(&source.id));
                (!made && !taken).then_some((source.id, effects))
            })
            .collect();
        if self.wanted.as_ref() != Some(&want) {
            bridge::want(canvas.id, want.clone());
            self.wanted = Some(want);
        }

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
        // the pictures object masks cut to
        let masks = mask_pictures(self.native.as_mut(), &base, &shown, &captured, &provided, pts_ns);
        for id in &shown {
            let Some(source) = base.sources.get(id) else { continue };
            let effects = drawn_here(source);
            if effects
                && made_here(source)
                && let Some(native) = self.native.as_mut()
            {
                match native.picture(source, pts_ns) {
                    Some(shot) => {
                        let shot = with_timed(Some(native), source, None, shot, &masks);
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
            // a screen or camera captured here, every effect drawn here: its frame as captured
            if effects
                && let Some(c) = captured.get(id)
                && let Some(shot) = shot_of(self.native.as_mut(), source, &c.picture())
            {
                let shot = with_pointer(self.native.as_mut(), &pointed, source, &c.buffer, shot, program);
                let shot = with_timed(self.native.as_mut(), source, Some(&c.buffer), shot, &masks);
                kept.insert(*id);
                shots.insert(*id, shot);
                held.push(c.buffer.clone());
                continue;
            }
            // a source a provider makes, every effect drawn here: its picture as provided
            if effects
                && let Some(p) = provided.get(id)
                && let Some(shot) = shot_of(self.native.as_mut(), source, p)
            {
                let shot = with_pointer(self.native.as_mut(), &pointed, source, &p.buffer, shot, program);
                let shot = with_timed(self.native.as_mut(), source, Some(&p.buffer), shot, &masks);
                kept.insert(*id);
                shots.insert(*id, shot);
                held.push(p.buffer.clone());
                continue;
            }
            let Some(picture) = bridge::picture(*id) else {
                // Windows: nothing else draws a kind the compositor doesn't make (screens and
                // cameras are the captures', which say why they don't run)
                #[cfg(windows)]
                // (a source a runtime plays has no picture until its first: not missing)
                if alone
                    && !made_here(source)
                    && !crate::providers::handled(source)
                    && !lightair_host::host::runs_here(source)
                    && !matches!(source.kind.as_str(), lightair_model::kinds::SCREEN | lightair_model::kinds::CAMERA)
                {
                    buffer_gpu::not_yet(&format!("{} sources in the compositor", source.kind));
                }
                continue;
            };
            let Some(shot) = shot_of(self.native.as_mut(), source, &picture) else { continue };
            let shot = if picture.unfiltered && effects {
                let shot = with_pointer(self.native.as_mut(), &pointed, source, &picture.buffer, shot, program);
                with_timed(self.native.as_mut(), source, Some(&picture.buffer), shot, &masks)
            } else {
                shot
            };
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
        // objects pinned to avatars, where their anchors are now
        let native = &mut self.native;
        let ws = self.doc.follow_pins(ws, canvas.id, &scenes, |avatar, keys| {
            native.as_mut().and_then(|n| n.avatar_pins(avatar, keys))
        });
        // the object being cropped: the rest of its picture as a ghost around the crop
        let ws = self.crop_ghost(ws, canvas.id, &shots);
        // a stinger switch: the renderer is given the stinger it plays (the scene's own, or the
        // workspace's) and its frame now, held until the GPU is done with it
        let playing = switch.as_ref().and_then(|p| p.stinger.as_ref());
        let stinger = playing.and_then(|(st, at)| crate::stinger::shot(self.stingers.get(&st.path)?.frame(*at)?));
        if program && let Some(drawer) = self.drawer.as_mut() {
            let ws = match playing {
                Some((st, _)) if ws.stinger.as_ref() != Some(st) => {
                    let mut with = Workspace::clone(&ws);
                    with.stinger = Some(st.clone());
                    Arc::new(with)
                }
                _ => ws.clone(),
            };
            let mut pictures = Ready { shots: &shots, stinger: stinger.clone() };
            let drawn = draw_program(&self.gpu, drawer, &ws, switch.as_ref(), overlays, canvas.id, &mut pictures, time);
            if let Some((frame, submitted)) = drawn {
                for preview in &showing {
                    if !self.fed.iter().any(|w| std::ptr::eq(w.as_ptr(), Arc::as_ptr(preview))) {
                        self.fed.push(Arc::downgrade(preview));
                    }
                }
                // the time this thread spent on the frame; the GPU finishes it on its own
                lightair_host::program::count_frame(canvas.id, started.elapsed(), period);
                let previews: Vec<Arc<Shared>> = showing.iter().map(|p| Arc::clone(p)).collect();
                let handover = self.handover.get_or_insert_with(|| Handover::start(&self.gpu));
                handover.send(Handed { frame, submitted, previews, canvas: canvas.id });
            }
        }
        // the scenes shown in the program's place that are due, from the same pictures
        for key in &due {
            let Some(view) = self.views.get_mut(key) else { continue };
            let preview = match &view.to {
                ViewTo::Preview(w) => match w.upgrade() {
                    Some(p) => Some(p),
                    None => continue,
                },
                ViewTo::Output(_) => None,
            };
            if view.drawer.as_ref().is_none_or(|d| d.size != view.size) {
                view.drawer = None;
                if view.failed == Some(view.size) {
                    continue;
                }
                view.drawer = Drawer::new(&self.gpu, view.size);
                if view.drawer.is_none() {
                    tracing::error!(size = ?view.size, "compositor: no renderer for a scene's view; libobs draws it");
                    view.failed = Some(view.size);
                    continue;
                }
            }
            let Some(drawer) = view.drawer.as_mut() else { continue };
            let mut pictures = Ready { shots: &shots, stinger: None };
            let _ = drawer.renderer.frame(&ws, view.scene, canvas.id, &mut pictures, time);
            let Some(i) = drawer.ring.free(&self.gpu.device) else { continue };
            if !drawer.blit.copy(&self.gpu, drawer.renderer.target(), None, &drawer.ring.slots[i].texture) {
                continue;
            }
            drawer.ring.handed(i);
            view.drawn = Some(now);
            match (&view.to, preview) {
                (_, Some(preview)) => preview.feed_scene(&drawer.ring.slots[i].buffer, view.generation),
                (ViewTo::Output(key), None) => program_source::publish_scene(*key, &drawer.ring.slots[i].buffer),
                (ViewTo::Preview(_), None) => {}
            }
        }
        // the GPU is done with the sources' buffers and the stinger's frame (each copy above
        // waited for it)
        drop(held);
        drop(stinger);
        if let Some(native) = self.native.as_mut() {
            native.frame_done();
        }
        period
    }

    /// Draws nothing: the bridge stops drawing sources for it, and its outputs show libobs's
    /// picture.
    /// `ws` with a faint copy of the object in crop view under it, whole, where it sits
    /// uncropped (its picture's size from `shots`); fading in, and out as crop view closes.
    fn crop_ghost(&mut self, ws: Arc<Workspace>, canvas: Id, shots: &HashMap<Id, Shot>) -> Arc<Workspace> {
        let Some(((scene, on, object), fading)) = bridge::crop_view().filter(|((_, c, _), _)| *c == canvas) else {
            self.ghost = None;
            return ws;
        };
        let now = Instant::now();
        let (ghost, opacity, last) = self.ghost.get_or_insert((Id::new(), 0.0, now));
        let want = if fading { 0.0 } else { CROP_GHOST };
        // a quarter of a second either way
        let step = now.duration_since(*last).as_secs_f32() * 4.0 * CROP_GHOST;
        *last = now;
        *opacity = if *opacity < want { (*opacity + step).min(want) } else { (*opacity - step).max(want) };
        let (ghost, opacity) = (*ghost, *opacity);
        if opacity <= 0.001 {
            return ws;
        }
        let Some(sc) = ws.scenes.iter().find(|s| s.id == scene) else { return ws };
        let Some(index) = sc.objects.iter().position(|o| o.id == object) else { return ws };
        let o = &sc.objects[index];
        let (Some(layout), Some(size)) = (o.layouts.get(&on), o.source.and_then(|s| shots.get(&s)).map(|s| s.size))
        else {
            return ws;
        };
        let mut whole = layout.uncropped((size.0 as f32, size.1 as f32));
        whole.opacity = layout.opacity * opacity;
        let mut copy = o.clone();
        copy.id = ghost;
        copy.link = None;
        copy.layouts.insert(on, whole);
        let mut w = (*ws).clone();
        if let Some(s) = w.scenes.iter_mut().find(|s| s.id == scene) {
            Arc::make_mut(s).objects.insert(index, copy);
        }
        Arc::new(w)
    }

    fn idle(&mut self) {
        if self.wanted.take().is_some() {
            bridge::want(self.canvas, HashMap::new());
        }
        if let Some(native) = self.native.as_mut() {
            native.forget_looks();
        }
        program_source::withdraw(self.canvas);
        lightair_host::show::set_shown(self.canvas, &HashSet::new());
    }

    /// The compositor draws nothing that shows a screen or camera any more: their captures are let
    /// go (a second later, unless another canvas's compositor still shows them).
    fn stop_captures(&mut self) {
        self.captures_of = None;
        if self.captures.take().is_some() {
            capture::want(self.canvas, HashMap::new());
        }
        // and the providers' sources: nothing shown here now
        crate::providers::want(self.canvas, &[]);
    }

    /// Tells the captures which screens and cameras the scenes drawn show (`shown`, in the
    /// engine's workspace `ws`), when that changes.
    fn keep_captures(&mut self, ws: &Arc<Workspace>, shown: &HashSet<Id>) {
        let at = Arc::as_ptr(ws) as usize;
        if self.captures_of.as_ref().is_some_and(|(a, s)| *a == at && s == shown) {
            return;
        }
        self.captures_of = Some((at, shown.clone()));
        let plans: HashMap<Id, capture::Plan> = shown
            .iter()
            .filter_map(|id| ws.sources.get(id))
            .filter_map(|source| Some((source.id, capture::Plan::of(source, ws)?)))
            .collect();
        if self.captures.as_ref() == Some(&plans) || (self.captures.is_none() && plans.is_empty()) {
            return;
        }
        capture::want(self.canvas, plans.clone());
        self.captures = (!plans.is_empty()).then_some(plans);
    }

    /// Keeps a view for each scene shown in the program's place in `wanted`, and for each scene
    /// drawn for an output in `targets`: one no longer wanted (its preview inactive, showing
    /// something else, or gone; its output stopped) is let go, and libobs draws a preview's
    /// again; one whose preview shows something new is drawn at the next tick.
    fn keep_views(
        &mut self,
        wanted: &[(Arc<Shared>, SceneView)],
        targets: &[program_source::SceneTarget],
        canvas: &Canvas,
        tick: Duration,
    ) {
        self.views.retain(|key, view| {
            let keep = match key {
                ViewKey::Preview(at) => wanted.iter().any(|(p, _)| Arc::as_ptr(p) as usize == *at),
                ViewKey::Output(k) => targets.iter().any(|t| t.key == *k),
            };
            if !keep {
                view.to.unfeed();
            }
            keep
        });
        for (preview, shown) in wanted {
            let size = view_size(shown, canvas);
            let period = view_period(shown, size, canvas, tick);
            let view = self.views.entry(ViewKey::Preview(Arc::as_ptr(preview) as usize)).or_insert_with(|| View {
                to: ViewTo::Preview(Arc::downgrade(preview)),
                scene: shown.scene,
                generation: shown.generation,
                size,
                period,
                drawn: None,
                drawer: None,
                failed: None,
            });
            if view.generation != shown.generation {
                view.generation = shown.generation;
                view.scene = shown.scene;
                view.drawn = None;
            }
            view.size = size;
            view.period = period;
        }
        // an output's scene: at the canvas's size, at most at its rate
        let size = (canvas.width.max(2) & !1, canvas.height.max(2) & !1);
        for t in targets {
            let (num, den) = (t.fps.0.max(1) as f64, t.fps.1.max(1) as f64);
            let period = Duration::from_secs_f64(den / num).max(tick);
            let view = self.views.entry(ViewKey::Output(t.key)).or_insert_with(|| View {
                to: ViewTo::Output(t.key),
                scene: t.scene,
                generation: 0,
                size,
                period,
                drawn: None,
                drawer: None,
                failed: None,
            });
            if view.scene != t.scene {
                view.scene = t.scene;
                view.drawn = None;
            }
            view.size = size;
            view.period = period;
        }
    }

    /// Draws no scene in the program's place any more: libobs draws them again.
    fn drop_views(&mut self) {
        for (_, view) in self.views.drain() {
            view.to.unfeed();
        }
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

/// Draws the program into `drawer`: the switch under way (`switch`) or the scene on air, the
/// show's overlays (`overlays`) over it, and copies it into a free output buffer, which it
/// returns once the GPU has finished it; `None` when every buffer is still in use (the frame is
/// dropped rather than waited for) or the GPU didn't finish in time.
#[allow(clippy::too_many_arguments)]
fn draw_program(
    gpu: &SharedGpu,
    drawer: &mut Drawer,
    ws: &Workspace,
    switch: Option<&Planned>,
    overlays: Option<Id>,
    canvas: Id,
    pictures: &mut Ready,
    time: f32,
) -> Option<(Frame, wgpu::SubmissionIndex)> {
    match switch {
        Some(p) => {
            let _ = drawer.renderer.transition(ws, p.from, p.to, canvas, &p.settings, p.progress, pictures, time);
        }
        // nothing on air: black, as libobs shows it
        None => {
            let _ = drawer.renderer.frame(ws, ws.program.unwrap_or(Id::nil()), canvas, pictures, time);
        }
    }
    // the show's overlays, over whatever is on air
    if overlays.is_some() && drawer.over.is_none() {
        drawer.over = overlay_renderer(drawer.size);
    }
    let over = match (overlays, drawer.over.as_mut()) {
        (Some(scene), Some(r)) => {
            let _ = r.frame(ws, scene, canvas, pictures, time);
            Some(&*r)
        }
        _ => None,
    };
    let i = drawer.ring.free(&gpu.device)?;
    let submitted =
        drawer.blit.submit(gpu, drawer.renderer.target(), over.map(|r| r.target()), &drawer.ring.slots[i].texture);
    drawer.ring.handed(i);
    Some((drawer.ring.slots[i].buffer.clone(), submitted))
}

/// A program frame handed on once the GPU has finished it.
struct Handed {
    frame: Frame,
    submitted: wgpu::SubmissionIndex,
    /// The previews showing the program.
    previews: Vec<Arc<Shared>>,
    canvas: Id,
}

// SAFETY: CoreVideo pixel buffers (and Lightair's Windows pictures) are reference counted
// thread-safely, and a finished frame isn't written while it's handed on (`program::publish`).
unsafe impl Send for Handed {}

/// The thread that waits for each program frame's GPU work and then hands the frame to the
/// previews and Lightair's outputs, so the compositor's thread goes on to the next frame meanwhile.
/// Frames are handed on in the order drawn; the thread ends with its compositor.
struct Handover {
    tx: std::sync::mpsc::Sender<Handed>,
}

impl Handover {
    fn start(gpu: &SharedGpu) -> Handover {
        let (tx, rx) = std::sync::mpsc::channel::<Handed>();
        let device = gpu.device.clone();
        let spawned = std::thread::Builder::new().name("lightair-handover".into()).spawn(move || {
            while let Ok(h) = rx.recv() {
                let done =
                    device.poll(wgpu::PollType::Wait { submission_index: Some(h.submitted), timeout: Some(GPU_WAIT) });
                if done.is_err() {
                    // the GPU didn't finish it in time: dropped, as a frame not drawn
                    continue;
                }
                for preview in &h.previews {
                    preview.feed(&h.frame);
                }
                program_source::publish(h.canvas, &h.frame);
            }
        });
        if let Err(e) = spawned {
            tracing::error!(%e, "compositor: no hand-over thread");
        }
        Handover { tx }
    }

    fn send(&self, handed: Handed) {
        let _ = self.tx.send(handed);
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
    /// The objects pinned to avatars' anchors in a canvas: (canvas, pins).
    pins: Option<(Id, Vec<Pinned>)>,
}

impl Default for Doc {
    fn default() -> Doc {
        Doc { from: None, base: None, drawn: None, overlays: None, overlays_id: Id::new(), shown: None, pins: None }
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
                self.pins = None;
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

    /// `ws` with the objects pinned to avatars' anchors in `scenes` moved to where the anchors are
    /// now, as the avatars' libobs sources move their pinned scene items: each anchor's place at
    /// rest and now (the avatar's last tick) gives a move, a turn and a change of size, kept
    /// relative to the anchor. A scene with a moved object is given flattened (each picture at
    /// its world layout), the moved ones at their new layouts.
    /// (`local`: an avatar's pins as the compositor's own renderer poses it, when whatever poses
    /// it gives none: the anchors at rest and now, and the picture's size.)
    fn follow_pins(
        &mut self,
        ws: Arc<Workspace>,
        canvas: Id,
        scenes: &[Id],
        mut local: impl FnMut(Id, &[String]) -> Option<(Vec<(String, Spot, Spot)>, (u32, u32))>,
    ) -> Arc<Workspace> {
        let Some(base) = &self.base else { return ws };
        if self.pins.as_ref().is_none_or(|(c, _)| *c != canvas) {
            self.pins = Some((canvas, pins_of(base, canvas)));
        }
        let Some((_, pins)) = &self.pins else { return ws };
        if pins.is_empty() {
            return ws;
        }
        type Spots = Option<(Vec<(String, Spot, Spot)>, (u32, u32))>;
        let mut spots: HashMap<Id, Spots> = HashMap::new();
        let mut moved: HashMap<Id, Layout> = HashMap::new();
        for pin in pins {
            // (held where it was last moved while the avatar doesn't tick, as libobs's items are)
            let avatar_spots = spots.entry(pin.avatar).or_insert_with(|| {
                let posed = bridge::avatar_posing(pin.avatar)
                    .map(|(p, _)| p)
                    .filter(|p| !p.pins.is_empty())
                    .map(|p| (p.pins.clone(), p.size));
                posed.or_else(|| {
                    let keys: Vec<String> =
                        pins.iter().filter(|p| p.avatar == pin.avatar).map(|p| p.anchor.clone()).collect();
                    local(pin.avatar, &keys)
                })
            });
            let Some((list, size)) = avatar_spots else { continue };
            let Some((_, rest, now)) = list.iter().find(|(k, ..)| *k == pin.anchor) else { continue };
            moved.insert(pin.object, pin.follow(*rest, *now, (size.0 as f32, size.1 as f32)));
        }
        if moved.is_empty() {
            return ws;
        }
        let mut out = Workspace::clone(&ws);
        for scene in out.scenes.iter_mut() {
            if !scenes.contains(&scene.id) || !scene.objects.iter().any(|o| moved.contains_key(&o.id)) {
                continue;
            }
            let mut flat = scene.flatten();
            for o in &mut flat.objects {
                if let Some(to) = moved.get(&o.id)
                    && let Some(l) = o.layouts.get_mut(&canvas)
                {
                    // (visibility and opacity as flattened)
                    l.x = to.x;
                    l.y = to.y;
                    l.width = to.width;
                    l.height = to.height;
                    l.rotation = to.rotation;
                }
            }
            *scene = Arc::new(flat);
        }
        Arc::new(out)
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

/// An object pinned to an avatar's anchor, or under one that is, in one canvas.
struct Pinned {
    /// The avatar's source.
    avatar: Id,
    /// The anchor's key.
    anchor: String,
    /// Where the anchor was at rest in the avatar's picture when the object was pinned (`None`:
    /// where it's at rest now).
    pinned_at: Option<Spot>,
    /// The avatar's object, in canvas terms.
    host: Placement,
    /// The object, and its layout in canvas terms with the avatar at rest.
    object: Id,
    base: Layout,
}

impl Pinned {
    /// The object's layout in canvas terms with the anchor at `now` (`rest`: at rest), in the
    /// avatar's picture of `size`: moved and turned with the anchor, its size changed with the
    /// anchor's (an Original-sized picture isn't resized).
    fn follow(&self, rest: Spot, now: Spot, (w, h): (f32, f32)) -> Layout {
        let rest = self.pinned_at.unwrap_or(rest);
        let p0 = on_canvas(&self.host, w, h, rest.x, rest.y);
        let p1 = on_canvas(&self.host, w, h, now.x, now.y);
        let turn = now.angle - rest.angle;
        let ratio = (now.scale / rest.scale).clamp(0.2, 5.0);
        let (s, c) = turn.sin_cos();
        let base = Placement::of(&self.base);
        let (dx, dy) = ((base.cx - p0.0) * ratio, (base.cy - p0.1) * ratio);
        let (x, y) = (p1.0 + dx * c - dy * s, p1.1 + dx * s + dy * c);
        let mut l = self.base;
        if l.fit != Fit::Original {
            l.width *= ratio;
            l.height *= ratio;
        }
        l.x = x - l.width / 2.0;
        l.y = y - l.height / 2.0;
        l.rotation = self.base.rotation + turn.to_degrees();
        l
    }
}

/// The objects pinned to avatars in `ws`'s scenes, for `canvas`: an object under an avatar that
/// the avatar's `pins` setting names ({object id: anchor, or {"anchor", "at"}}), and everything
/// under that object.
fn pins_of(ws: &Workspace, canvas: Id) -> Vec<Pinned> {
    let mut out = Vec::new();
    for scene in &ws.scenes {
        for o in &scene.objects {
            let Some(parent) = o.parent.and_then(|p| scene.object(p)) else { continue };
            let Some(host) = parent.source.and_then(|i| ws.sources.get(&i)).filter(|s| s.kind == avatar::KIND) else {
                continue;
            };
            let pin = &host.settings["pins"][o.id.to_string()];
            let Some(anchor) = pin.as_str().or_else(|| pin["anchor"].as_str()).filter(|a| !a.is_empty()) else {
                continue;
            };
            let pinned_at = pin["at"].as_array().filter(|a| a.len() == 4).map(|a| {
                let f = |i: usize| a[i].as_f64().unwrap_or(0.0) as f32;
                Spot { x: f(0), y: f(1), angle: f(2), scale: f(3).max(1e-6) }
            });
            let Some(at) = scene.world(parent.id, canvas) else { continue };
            // it and what's under it
            let mut ids = vec![o.id];
            let mut i = 0;
            while i < ids.len() {
                let under: Vec<Id> = scene.children(Some(ids[i])).map(|c| c.id).collect();
                ids.extend(under);
                i += 1;
            }
            for id in ids {
                let Some(base) = scene.world(id, canvas) else { continue };
                out.push(Pinned {
                    avatar: host.id,
                    anchor: anchor.to_string(),
                    pinned_at,
                    host: Placement::of(&at),
                    object: id,
                    base,
                });
            }
        }
    }
    out
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

/// `source`'s picture from `picture` (libobs's, or a capture's): drawn as it is when it came with
/// its effects in it or has none the engine draws before the renderer; else (bare) through its
/// looks, then its keys, by the engine (`None` without the engine's pictures: a bare picture is
/// asked for only with them). The buffer must be held until the GPU is done.
/// `shot` (of `buffer`, `source`'s picture) with Lightair's own pointer drawn over it and zoomed,
/// when the compositor draws one on `source` (`pointed`); `on_air`: it counts for the badge.
fn with_pointer(
    native: Option<&mut Native>,
    pointed: &HashMap<Id, lightair_host::pointer::Style>,
    source: &Source,
    buffer: &bridge::Buffer,
    shot: Shot,
    on_air: bool,
) -> Shot {
    let (Some(native), Some(style)) = (native, pointed.get(&source.id)) else { return shot };
    let Native { pointers, gpu, .. } = native;
    pointers.draw(gpu, source, *style, buffer, on_air).unwrap_or(shot)
}

/// `shot` (of `buffer`, `source`'s picture, if it's libobs's or captured) with the effects that
/// work across frames drawn on it (`crate::timed`), when `source` has any.
fn with_timed(
    native: Option<&mut Native>,
    source: &Source,
    buffer: Option<&bridge::Buffer>,
    shot: Shot,
    masks: &HashMap<Id, wgpu::Texture>,
) -> Shot {
    let Some(native) = native else { return shot };
    let Native { timed, gpu, .. } = native;
    timed.draw(gpu, source, &shot, buffer, masks).unwrap_or(shot)
}

/// `ws` with the effects in `off` (source, effect; `catalog::STUDIO_ID`: its looks) switched
/// off.
fn live_off(ws: Arc<Workspace>, off: &[(Id, Id)]) -> Arc<Workspace> {
    if off.is_empty() {
        return ws;
    }
    let mut w = (*ws).clone();
    for (source, filter) in off {
        let Some(s) = w.sources.get_mut(source) else { continue };
        let s = Arc::make_mut(s);
        for e in s.effects.iter_mut() {
            let looks = *filter == lightair_model::catalog::STUDIO_ID && e.kind.starts_with("studio.");
            if e.id == *filter || looks {
                e.enabled = false;
            }
        }
    }
    Arc::new(w)
}

/// `ws` with each source's looks' makeup LUT (a `.cube` file) drawn by the renderer right
/// after its looks, at the look's filter amount.
fn with_makeup_luts(ws: Arc<Workspace>) -> Arc<Workspace> {
    let luts: Vec<(Id, String, f32)> = ws
        .sources
        .values()
        .filter_map(|s| {
            let look = lightair_engine::looks::render::Settings::from_source(s)?;
            let lut = look.makeup.lut.trim().to_string();
            let amount = if look.enhance_on { look.makeup.filter_amt } else { 0.0 };
            (lut.to_ascii_lowercase().ends_with(".cube") && amount > 0.0).then_some((s.id, lut, amount))
        })
        .collect();
    if luts.is_empty() {
        return ws;
    }
    let mut w = (*ws).clone();
    for (id, lut, amount) in luts {
        let Some(s) = w.sources.get_mut(&id) else { continue };
        let s = Arc::make_mut(s);
        let after = s.effects.iter().rposition(|e| e.enabled && e.kind.starts_with("studio.")).map_or(0, |i| i + 1);
        let e = lightair_model::Effect::new(
            "Makeup LUT",
            "clut_filter",
            serde_json::json!({"image_path": lut, "clut_amount": amount}),
        );
        s.effects.insert(after, e);
    }
    Arc::new(w)
}

/// How much of the cut-away picture shows around the crop while cropping.
const CROP_GHOST: f32 = 0.42;

/// The pictures object masks cut to this frame, by source: made here, captured or provided
/// (a source drawn only by libobs can't be one).
fn mask_pictures(
    native: Option<&mut Native>,
    base: &Workspace,
    shown: &HashSet<Id>,
    captured: &HashMap<Id, capture::Captured>,
    provided: &HashMap<Id, bridge::Picture>,
    pts_ns: i64,
) -> HashMap<Id, wgpu::Texture> {
    let mut out = HashMap::new();
    let Some(native) = native else { return out };
    let wanted: HashSet<Id> = shown
        .iter()
        .filter_map(|id| base.sources.get(id))
        .flat_map(|s| s.effects.iter().filter(|e| e.enabled && e.kind == crate::timed::MASK))
        .filter_map(crate::timed::mask_source)
        .collect();
    for id in wanted {
        let Some(source) = base.sources.get(&id) else { continue };
        let device = native.gpu.device();
        let texture = if let Some(c) = captured.get(&id) {
            buffer_gpu::sampled(device, &c.buffer)
        } else if let Some(p) = provided.get(&id) {
            buffer_gpu::sampled(device, &p.buffer)
        } else if made_here(source) {
            match native.picture(source, pts_ns).map(|s| s.picture) {
                Some(Picture::Texture(t)) => Some(t),
                #[cfg(target_os = "macos")]
                Some(Picture::Buffer(b)) => b.rgba().map(|rgba| native.gpu.texture(&rgba, b.size())),
                _ => None,
            }
        } else {
            None
        };
        if let Some(t) = texture {
            out.insert(id, t);
        }
    }
    out
}

fn shot_of(native: Option<&mut Native>, source: &Source, picture: &bridge::Picture) -> Option<Shot> {
    let steps = if picture.unfiltered { keying::steps(source) } else { Vec::new() };
    if picture.unfiltered && looks::has_looks(source) {
        // its looks (then its keys) drawn by the engine
        native?.looked(source, picture)
    } else if steps.is_empty() {
        buffer_gpu::shot(&picture.buffer, true)
    } else {
        native?.keyed(source.id, &picture.buffer, &steps)
    }
}

/// Whether the compositor makes `source`'s picture itself (when it draws all its effects). A
/// source with camera looks isn't: the engine draws looks on a feed, which is given libobs's bare
/// picture.
fn made_here(source: &Source) -> bool {
    if looks::has_looks(source) {
        return false;
    }
    let s = &source.settings;
    match source.kind.as_str() {
        templates::IMAGE => s["file"].as_str().is_some_and(|f| !f.is_empty()),
        templates::COLOUR | templates::TIMER => true,
        templates::TEXT => true,
        // the built-in widgets the native widgets draw, once the app has given what they show
        widgets::KIND => widgets::instance(s).is_some_and(|i| widget_feed::drawn_here(&i)),
        avatar::KIND => s["file"].as_str().is_some_and(|f| !f.is_empty()),
        crate::backdrop::KIND => true,
        _ => crate::slideshow::drawn(source),
    }
}

/// Whether the compositor draws every effect of `source` that changes its picture: the
/// renderer's (shader effects, the colour filter, a LUT from a `.cube` file, as
/// `lightair_render::scene::unsupported` counts them); with `keyers`, sharpen and the keyers,
/// which must come before the renderer's in the stack (they're drawn first here); and with
/// `looks` too, the camera looks the engine draws (`looks::drawable`), which libobs draws as one
/// filter at the first one's place: that place must come before any keyer or renderer effect
/// (they're drawn before the keys here). With `keyers`, freeze, render delay and scroll too
/// (`crate::timed`), before any renderer effect. Sound effects don't count.
fn effects_drawn(source: &Source, keyers: bool, looks: bool) -> bool {
    let (mut renderer_seen, mut keyer_seen, mut studio_seen) = (false, false, false);
    // (the drawn pointer is drawn with the source's pointer, `crate::pointer`)
    for e in
        source.effects.iter().filter(|e| e.enabled && !e.kind.starts_with("audio.") && !crate::pointer::is_pointer(e))
    {
        if crate::timed::is_timed(e) {
            // drawn on the picture as made, keyed and looked, before the renderer's
            if !keyers || renderer_seen {
                return false;
            }
        } else if e.kind.starts_with("studio.") {
            if !studio_seen && (renderer_seen || keyer_seen) {
                return false;
            }
            studio_seen = true;
        } else if keying::KINDS.contains(&e.kind.as_str()) {
            if !keyers || renderer_seen {
                return false;
            }
            keyer_seen = true;
        } else if renderer_draws(e) {
            renderer_seen = true;
        } else {
            return false;
        }
    }
    !studio_seen || (keyers && looks && looks::drawable(source))
}

/// A text source's file as text (read again when it changes, looked at twice a second); empty
/// when it can't be read.
fn file_text(path: &str) -> String {
    type Read = (Instant, Option<std::time::SystemTime>, String);
    static FILES: std::sync::LazyLock<parking_lot::Mutex<HashMap<String, Read>>> =
        std::sync::LazyLock::new(Default::default);
    if path.is_empty() {
        return String::new();
    }
    let mut files = FILES.lock();
    if let Some((looked, _, text)) = files.get(path)
        && looked.elapsed() < Duration::from_millis(500)
    {
        return text.clone();
    }
    let modified = std::fs::metadata(path).and_then(|m| m.modified()).ok();
    if let Some((looked, was, text)) = files.get_mut(path)
        && *was == modified
    {
        *looked = Instant::now();
        return text.clone();
    }
    // (as the text source reads it: UTF-8, a byte-order mark left out)
    let text = std::fs::read(path)
        .map(|b| String::from_utf8_lossy(b.strip_prefix(b"\xEF\xBB\xBF".as_slice()).unwrap_or(&b)).into_owned())
        .unwrap_or_default();
    files.insert(path.to_string(), (Instant::now(), modified, text.clone()));
    text
}

/// Says once per source that some of its effects aren't drawn (no libobs to draw them).
fn skipped_effects(source: &Source) {
    static SAID: std::sync::LazyLock<parking_lot::Mutex<HashSet<Id>>> = std::sync::LazyLock::new(Default::default);
    if SAID.lock().insert(source.id) {
        let kinds: Vec<&str> = source.effects.iter().filter(|e| e.enabled).map(|e| e.kind.as_str()).collect();
        tracing::info!(source = %source.id, name = source.name, ?kinds, "compositor: some of these effects aren't drawn without libobs yet");
    }
}

/// An effect lightair-render's scene renderer draws.
fn renderer_draws(e: &lightair_model::Effect) -> bool {
    let cube = || {
        e.settings.get("image_path").and_then(|p| p.as_str()).is_some_and(|p| p.to_ascii_lowercase().ends_with(".cube"))
    };
    e.kind.starts_with("fx.")
        || e.kind == "color_filter_v2"
        || (e.kind == "clut_filter" && cube())
        || lightair_render::scene::wgsl_of(e).is_some()
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
    /// Backdrops as drawn, by source (drawn again when their settings change).
    backdrops: HashMap<Id, (crate::backdrop::Params, wgpu::Texture)>,
    texts_used: HashSet<String>,
    /// The widgets drawn here, by source.
    widgets: HashMap<Id, Widget>,
    /// The cameras drawn with looks: their feeds (in `hot`, mapped in `program`) and analysis.
    looks: looks::Hosts,
    /// The avatars drawn here, by source.
    avatars: HashMap<Id, Avatar>,
    /// Lightair's own pointer and zoom on the screens drawn here.
    pointers: crate::pointer::Pointers,
    /// Freeze, render delay and scroll on the sources drawn here.
    timed: crate::timed::Timed,
    /// The image slideshows drawn here.
    slideshows: crate::slideshow::Slideshows,
    /// The person masks last looked at for presence, by camera: their serials, and whether
    /// they showed a person.
    persons: HashMap<Id, ((u64, u64), bool)>,
}

/// An avatar source drawn here: its renderer (`None`: none could be made, libobs's picture
/// stands in) and the settings it was last given.
struct Avatar {
    renderer: Option<AvatarRenderer>,
    settings: serde_json::Value,
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
            backdrops: HashMap::new(),
            texts_used: HashSet::new(),
            widgets: HashMap::new(),
            looks: looks::Hosts::default(),
            avatars: HashMap::new(),
            pointers: Default::default(),
            timed: Default::default(),
            slideshows: Default::default(),
            persons: HashMap::new(),
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
            self.backdrops.retain(|id, _| ws.sources.contains_key(id));
            self.slideshows
                .keep(|id| ws.sources.get(&id).is_some_and(|s| crate::slideshow::KINDS.contains(&s.kind.as_str())));
            self.persons.retain(|id, _| ws.sources.contains_key(id));
            self.avatars.retain(|id, _| ws.sources.get(id).is_some_and(|s| s.kind == avatar::KIND));
            let gone = self.looks.sweep(|id| ws.sources.contains_key(&id), &self.hot, &mut self.program.feeds);
            for feed in gone {
                self.cache.forget_feed(feed);
            }
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
            templates::TEXT if source.settings["from_file"].as_bool().unwrap_or(false) => {
                let text = file_text(source.settings["text_file"].as_str().unwrap_or(""));
                Some(self.text(&source.settings, &text)?)
            }
            templates::TEXT => Some(self.text(&source.settings, source.settings["text"].as_str().unwrap_or(""))?),
            widgets::KIND => Some(self.widget(source)?),
            avatar::KIND => Some(self.avatar(source)?),
            crate::backdrop::KIND => Some(self.backdrop(source)?),
            _ if crate::slideshow::drawn(source) => Some(self.slideshows.picture(&self.gpu, source)?),
            _ => None,
        };
        if let Some(shot) = made {
            let steps = keying::steps(source);
            if steps.is_empty() {
                return Some(shot);
            }
            let Picture::Texture(texture) = &shot.picture else { return Some(shot) };
            let out = keying::draw(
                &self.gpu,
                &self.keying,
                &mut self.keyed,
                source.id,
                texture,
                shot.premultiplied,
                &steps,
                None,
            );
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

    /// libobs's bare `picture` of `source` through its looks, then its sharpen and keyer passes,
    /// drawn by the engine from the camera's feed (given the picture here; the faces and masks
    /// its analysis pushed); the bare picture as it is if that can't be drawn. The buffer must
    /// be held until the GPU is done.
    fn looked(&mut self, source: &Source, picture: &bridge::Picture) -> Option<Shot> {
        let bare = || buffer_gpu::shot(&picture.buffer, true);
        if !self.looks.push(source, picture, &self.hot, &mut self.program.feeds) {
            return bare();
        }
        self.person_seen(source.id);
        // the engine draws looks on a camera's feed, whatever made the picture
        let mut camera = Source::clone(source);
        camera.kind = templates::CAMERA.into();
        // a face stickers preset's face distortion, for the looks' warp
        looks::with_sticker_shape(&mut camera);
        let mut frame = lightair_engine::pictures::Frame {
            gpu: &self.gpu,
            cache: &mut self.cache,
            hot: &self.hot,
            program: &self.program,
            host_ns: lightair_engine::clock::host_now_ns(),
            pts_ns: 0,
            used: Vec::new(),
        };
        frame.picture(&camera).or_else(bare)
    }

    /// Whether the newest masks the looks' analysis gave camera `source` show a person (more
    /// than 1% of the picture, as the Effect Stack's filter counts it): the person mask of a
    /// background effect, else the multiclass mask's person channel. Published for the effects'
    /// "No person" hint; nothing is published while there's no mask.
    fn person_seen(&mut self, source: Id) {
        let Some(feed) = self.program.feeds.get(&source).and_then(|f| self.hot.feeds.read().get(f).cloned()) else {
            return;
        };
        let Some(v) = feed.video() else { return };
        let mask = v.mask.lock().clone();
        let segmentation = if mask.is_none() { v.segmentation.lock().clone() } else { None };
        let serials = (mask.as_ref().map_or(0, |m| m.serial), segmentation.as_ref().map_or(0, |s| s.serial));
        if serials == (0, 0) {
            return;
        }
        if let Some((last, seen)) = self.persons.get(&source)
            && *last == serials
        {
            // the same masks: told again, so it stays current
            bridge::set_person_seen(source, *seen);
            return;
        }
        // every fourth pixel across and down, as the filter counts
        let count = |bytes: &[u8], (w, h): (u32, u32), step: usize, at: usize| {
            let (mut hits, mut n) = (0usize, 0usize);
            for y in (0..h as usize).step_by(4) {
                for x in (0..w as usize).step_by(4) {
                    n += 1;
                    hits += bytes.get((y * w as usize + x) * step + at).is_some_and(|b| *b > 127) as usize;
                }
            }
            n > 0 && hits * 100 > n
        };
        let seen = match (&mask, &segmentation) {
            (Some(m), _) => count(&m.bytes, m.size, 1, 0),
            (None, Some(s)) => count(&s.rgba, s.size, 4, 3),
            _ => return,
        };
        self.persons.insert(source, (serials, seen));
        bridge::set_person_seen(source, seen);
    }

    /// The frame's GPU work is done: the cameras' feeds let go of their pictures.
    fn frame_done(&mut self) {
        self.looks.frame_done();
    }

    /// While nothing is drawn: the analyses of cameras not drawn lately stop (a short pause,
    /// such as a switch libobs draws, keeps them).
    fn forget_looks(&mut self) {
        for feed in self.looks.sweep(|_| true, &self.hot, &mut self.program.feeds) {
            self.cache.forget_feed(feed);
        }
    }

    /// libobs's bare picture of source `id` (colours multiplied by alpha) through its sharpen and
    /// keyer passes, as a straight-alpha texture. The buffer must be held until the GPU is done.
    fn keyed(&mut self, id: Id, buffer: &bridge::Buffer, steps: &[keying::Step]) -> Option<Shot> {
        let texture = buffer_gpu::sampled(self.gpu.device(), buffer)?;
        let out = keying::draw(&self.gpu, &self.keying, &mut self.keyed, id, &texture, true, steps, None);
        let size = (out.width(), out.height());
        Some(Shot { picture: Picture::Texture(out), size, premultiplied: false })
    }

    /// A text object's picture (`settings` showing `text`), drawn as libobs's text source draws
    /// it; `None` when it draws nothing.
    /// A backdrop's picture, drawn again when its settings change.
    fn backdrop(&mut self, source: &Source) -> Option<Shot> {
        let params = crate::backdrop::Params::of(&source.settings);
        if self.backdrops.get(&source.id).is_none_or(|(p, _)| *p != params) {
            let texture = self.gpu.texture(&params.draw(), (params.width, params.height));
            self.backdrops.insert(source.id, (params, texture));
        }
        let (p, texture) = self.backdrops.get(&source.id)?;
        Some(Shot { picture: Picture::Texture(texture.clone()), size: (p.width, p.height), premultiplied: true })
    }

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

    /// An avatar's picture: posed from what its libobs source published last tick (nothing
    /// tracked when it published nothing lately) and drawn on the GPU; `None` while its model
    /// loads or if it can't be drawn (libobs's picture stands in).
    /// Where `keys` (anchors of avatar `source`) are in its picture at rest and now, as this
    /// compositor's renderer poses it, and the picture's size.
    fn avatar_pins(&mut self, source: Id, keys: &[String]) -> Option<AvatarPins> {
        let renderer = self.avatars.get_mut(&source)?.renderer.as_mut()?;
        let pins = renderer.pins(keys);
        (!pins.is_empty()).then(|| (pins, renderer.size()))
    }

    fn avatar(&mut self, source: &Source) -> Option<Shot> {
        let a = self.avatars.entry(source.id).or_insert_with(|| Avatar {
            renderer: AvatarRenderer::new(&source.settings)
                .map_err(|e| tracing::error!(%e, "compositor: no avatar renderer; libobs draws the avatar"))
                .ok(),
            settings: source.settings.clone(),
        });
        let renderer = a.renderer.as_mut()?;
        if a.settings != source.settings {
            renderer.set_settings(&source.settings);
            a.settings = source.settings.clone();
        }
        let fed = bridge::avatar_posing(source.id).filter(|(p, _)| p.at.elapsed() < POSING_STALE);
        let Some((posing, picture)) = &fed else { return renderer.draw(&Input::default(), None) };
        let input = Input {
            face: posing.face,
            body: posing.body.as_ref(),
            expression: posing.mood,
            mic_db: posing.mic_db,
            push_to_talk_held: posing.push_to_talk_held,
        };
        let view = picture.as_ref().map(|p| TrackingView {
            bgra: &p.bgra,
            width: p.width,
            height: p.height,
            face: &posing.view_face,
            body: posing.body.as_ref(),
        });
        renderer.draw(&input, view.as_ref())
    }

    /// Drops the made pictures no frame used lately, and the looks' hosts of cameras not drawn
    /// with looks lately.
    fn end_frame(&mut self) {
        self.cache.end_frame();
        for feed in self.looks.sweep(|_| true, &self.hot, &mut self.program.feeds) {
            self.cache.forget_feed(feed);
        }
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
        // one to start with: none means this device can't draw into Lightair's pictures
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

/// An output buffer: a BGRA IOSurface-backed pixel buffer (macOS) or shared Direct3D 11 texture
/// (Windows), and a texture over its memory that frames are drawn into.
struct Out {
    buffer: Frame,
    texture: wgpu::Texture,
}

impl Out {
    #[cfg(windows)]
    fn new(device: &wgpu::Device, size: (u32, u32)) -> Option<Out> {
        let (buffer, texture) = crate::win_gpu::new_output(device, size)?;
        Some(Out { buffer, texture })
    }

    #[cfg(target_os = "macos")]
    fn new(device: &wgpu::Device, size: (u32, u32)) -> Option<Out> {
        use objc2::rc::Retained;
        use objc2::runtime::ProtocolObject;
        use objc2_core_video::{CVPixelBufferGetIOSurface, kCVPixelFormatType_32BGRA};
        use objc2_metal::{
            MTLDevice, MTLPixelFormat, MTLTexture, MTLTextureDescriptor, MTLTextureType, MTLTextureUsage,
        };

        let (w, h) = size;
        let buffer = crate::mac::pixel_buffer(w, h, kCVPixelFormatType_32BGRA)?;
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
        let free = (0..self.slots.len())
            .find(|i| !self.recent.contains(i) && !buffer_gpu::held_elsewhere(&self.slots[*i].buffer));
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

    /// Draws `frame`, with `over` laid over it if given, into `out`: submitted, not waited for.
    fn submit(
        &self,
        gpu: &SharedGpu,
        frame: &wgpu::Texture,
        over: Option<&wgpu::Texture>,
        out: &wgpu::Texture,
    ) -> wgpu::SubmissionIndex {
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
        gpu.queue.submit([encoder.finish()])
    }

    /// [`Blit::submit`], then waits for the GPU to finish (so the buffer is complete when it's
    /// handed on). False when the GPU didn't finish in time.
    fn copy(&self, gpu: &SharedGpu, frame: &wgpu::Texture, over: Option<&wgpu::Texture>, out: &wgpu::Texture) -> bool {
        let submitted = self.submit(gpu, frame, over, out);
        gpu.device.poll(wgpu::PollType::Wait { submission_index: Some(submitted), timeout: Some(GPU_WAIT) }).is_ok()
    }
}
