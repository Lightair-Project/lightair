//! Screens (displays, windows, apps; games on Windows) and cameras captured by Lightair itself
//! (`lightair-capture`: ScreenCaptureKit and AVFoundation on macOS; Windows Graphics Capture,
//! Desktop Duplication and Media Foundation on Windows) for the compositor, instead of through
//! libobs.
//!
//! - **One capture per source**, shared by every canvas's compositor. A compositor says which
//!   sources the scenes it draws show ([`want`]); a capture runs while any compositor wants it and
//!   is let go a second after none does (as libobs's own wrappers do, `lightair_obs::on_screen`),
//!   so a scene being rebuilt or a switch's hand-over doesn't restart it.
//! - **Off the compositor's and the main thread.** Starting, changing and stopping a capture can
//!   block (the window server answering, a camera opening or closing), so a thread of this
//!   module's own looks after the captures and each start, change and stop runs on a short-lived
//!   thread. Nothing here asks the user for access: a capture starts only once screen recording or
//!   the camera is already allowed, and until then libobs's capture runs (and asks, as it always
//!   has). Nothing is listed or opened at startup: only when a compositor draws the source.
//! - **Pictures.** Each new frame (a BGRA IOSurface-backed pixel buffer, never copied) becomes the
//!   source's newest picture ([`frame`]) and is handed to the source's libobs wrapper
//!   (`on_screen::publish`), which then runs no capture of its own and draws these frames instead:
//!   libobs's picture of the source (through its filters, in its own canvases, thumbnails and
//!   Studio mode) stays right with one capture running. On Windows a frame is the capture's
//!   shared Direct3D 11 texture (or, from some cameras, bytes), copied on the GPU into one of
//!   Lightair's own pictures (`lightair_host::buffers::Copier`, after waiting on the GPU for the
//!   capture's shared fence): the capture gets its texture back at once.
//! - **Windows's settings:** a display is its monitor's device path (none, or a Mac's display:
//!   the primary monitor); a window, an app or a game is libobs's window string
//!   (`title:class:executable`), the window found again when its capture starts; a camera is
//!   Media Foundation's symbolic link, or libobs's DirectShow id (`name:path`) of an older
//!   project.
//! - **What stays libobs's:** a capture card whose sound comes in the same stream as its pictures
//!   (a muxed device: libobs's capture carries that sound into the mix), screens and cameras
//!   before access is given, and a capture that couldn't start or ended by itself (a window
//!   closed, a camera unplugged), tried again every few seconds.
//!
//! The pointer: a capture draws the system's pointer as the source's `cursor` setting says,
//! except while Lightair draws its own (the drawn pointer effect, or a scene's screen look that
//! draws or hides it), as the libobs capture is set up (`reconcile::source_settings`).

use std::collections::HashMap;
#[cfg(target_os = "macos")]
use std::ptr::NonNull;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, LazyLock};
use std::time::{Duration, Instant};

use lightair_capture::{Error, Frame, PixelFormat, VideoSink, camera, screen};
use lightair_host::bridge;
use lightair_model::{Id, Source, Workspace, catalog, kinds};
#[cfg(target_os = "macos")]
use objc2_core_foundation::CFRetained;
#[cfg(target_os = "macos")]
use objc2_core_video::CVPixelBuffer;
use parking_lot::Mutex;

/// How long a capture keeps running once no compositor wants it.
const LINGER: Duration = Duration::from_secs(1);
/// How often the captures are looked after.
const PERIOD: Duration = Duration::from_millis(50);
/// A capture that ended by itself (a window closed, a camera unplugged) is tried again after this.
const RETRY_ENDED: Duration = Duration::from_secs(3);
/// Pixel buffers a screen capture may have in flight: the newest picture, the one libobs's
/// wrapper draws, one per compositor drawing it, the looks' analysis, and spares for the window
/// server.
#[cfg(target_os = "macos")]
const SCREEN_QUEUE_DEPTH: u32 = 8;
/// Windows: shared textures a screen capture may have in flight (each is copied into Lightair's
/// own picture as it arrives, and given back at once).
#[cfg(windows)]
const SCREEN_QUEUE_DEPTH: u32 = 4;
/// A window capture's picture follows the window's size when they differ by more than this
/// (pixels).
const RESIZE_SLACK: u32 = 2;

// ---------------------------------------------------------------- what a capture is

/// What a screen capture takes.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Aim {
    /// A display by its UUID or device path ("" for the main display), a window by its id, or
    /// an app's windows.
    Target(screen::Target),
    /// Windows: a window by libobs's window string, found when its capture starts.
    #[cfg(windows)]
    Window(String),
    /// Windows: a game, by its executable (its file name), captured as the game capture sees
    /// fit.
    #[cfg(windows)]
    Game(String),
    /// Windows: a game with none chosen: whatever application is full screen in front.
    #[cfg(windows)]
    AnyGame,
}

/// What a source's capture is: what it takes and how.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Plan {
    Screen {
        aim: Aim,
        /// Lightair's own windows left out of a display capture (`hide_obs`).
        exclude_own: bool,
        fps: u32,
        /// The system's pointer drawn into the picture.
        cursor: bool,
    },
    Camera {
        device: String,
        /// The short side asked for (`height`, or the preset of an older project).
        height: Option<u32>,
        /// The frame rate asked for: the canvases' fastest (the camera's format nearest it).
        fps: f64,
    },
}

impl Plan {
    /// The capture of `source` (in `ws`), when it's a screen or a camera Lightair captures
    /// itself; `None` for every other kind, and for a screen or camera with nothing chosen.
    pub(crate) fn of(source: &Source, ws: &Workspace) -> Option<Plan> {
        let s = &source.settings;
        let fps = ws.canvases.iter().map(|c| c.fps()).fold(0.0, f64::max);
        let fps = if fps > 0.0 { fps.clamp(1.0, 240.0) } else { 60.0 };
        let text = |key: &str| match &s[key] {
            serde_json::Value::String(t) if !t.trim().is_empty() => Some(t.trim().to_string()),
            serde_json::Value::Number(n) => Some(n.to_string()),
            _ => None,
        };
        match source.kind.as_str() {
            kinds::SCREEN => {
                let aim = screen_aim(s["mode"].as_i64().unwrap_or(kinds::DISPLAY), text)?;
                Some(Plan::Screen {
                    aim,
                    exclude_own: s["hide_obs"].as_bool().unwrap_or(false),
                    fps: fps.ceil() as u32,
                    cursor: s["cursor"].as_bool().unwrap_or(true) && !pointer_hidden(ws, source),
                })
            }
            kinds::CAMERA => {
                let device = text("device").filter(|d| d != "default")?;
                let height = s["height"].as_u64().map(|h| h as u32).or_else(|| preset_height(s));
                Some(Plan::Camera { device, height, fps })
            }
            _ => None,
        }
    }

    /// Whether `self` can be had from a running capture of `other` by changing its frame rate or
    /// pointer (the same screen target), rather than starting again.
    fn reconfigures(&self, other: &Plan) -> bool {
        match (self, other) {
            (Plan::Screen { aim: a, exclude_own: ea, .. }, Plan::Screen { aim: b, exclude_own: eb, .. }) => {
                a == b && ea == eb
            }
            _ => false,
        }
    }

    fn is_window(&self) -> bool {
        match self {
            Plan::Screen { aim: Aim::Target(screen::Target::Window(_)), .. } => true,
            #[cfg(windows)]
            Plan::Screen { aim: Aim::Window(_), .. } => true,
            _ => false,
        }
    }
}

/// What a screen source in `mode` takes, from its settings (`text`); `None` with nothing chosen.
#[cfg(target_os = "macos")]
fn screen_aim(mode: i64, text: impl Fn(&str) -> Option<String>) -> Option<Aim> {
    Some(Aim::Target(match mode {
        // a game is a window here
        kinds::WINDOW | kinds::GAME => screen::Target::Window(text("window")?.parse().ok()?),
        kinds::APP => screen::Target::App { bundle: text("app")?, display: text("display") },
        _ => screen::Target::Display(text("display").unwrap_or_default()),
    }))
}

/// What a screen source in `mode` takes, from its settings (`text`); `None` with nothing chosen.
/// A window, app or game is libobs's window string (a window's id is taken too); an app or game
/// by its executable. A game with none chosen is "any full-screen application".
#[cfg(windows)]
fn screen_aim(mode: i64, text: impl Fn(&str) -> Option<String>) -> Option<Aim> {
    use lightair_host::displays::parse_window_string;
    let exe = |value: String| match parse_window_string(&value) {
        Some((_, _, exe)) => exe,
        None => value,
    };
    Some(match mode {
        kinds::WINDOW => {
            let window = text("window")?;
            match window.parse() {
                Ok(id) => Aim::Target(screen::Target::Window(id)),
                Err(_) => Aim::Window(window),
            }
        }
        kinds::APP => Aim::Target(screen::Target::App { bundle: exe(text("app")?), display: None }),
        kinds::GAME => match text("window") {
            Some(window) => Aim::Game(exe(window)),
            None => Aim::AnyGame,
        },
        _ => Aim::Target(screen::Target::Display(text("display").unwrap_or_default())),
    })
}

/// The short side an older project's camera preset asks for (`use_preset` with `preset`; on
/// Windows, `res_type` 1 with `resolution`).
fn preset_height(s: &serde_json::Value) -> Option<u32> {
    if s["res_type"].as_i64() == Some(1) {
        let (w, h) = s["resolution"].as_str()?.split_once('x')?;
        let (w, h): (u32, u32) = (w.trim().parse().ok()?, h.trim().parse().ok()?);
        return Some(w.min(h));
    }
    if s["use_preset"].as_bool() != Some(true) {
        return None;
    }
    match s["preset"].as_str()? {
        "AVCaptureSessionPreset640x480" => Some(480),
        "AVCaptureSessionPreset960x540" => Some(540),
        "AVCaptureSessionPreset1280x720" => Some(720),
        "AVCaptureSessionPreset1920x1080" => Some(1080),
        "AVCaptureSessionPreset3840x2160" => Some(2160),
        _ => None,
    }
}

/// Whether Lightair draws (or hides) the pointer over `source`, so its capture leaves the
/// system's out: the drawn pointer effect, or the screen look of the scene that has it (the scene
/// on air first, else the first scene with a look that shows the source), as the reconciler sets
/// libobs's capture up.
fn pointer_hidden(ws: &Workspace, source: &Source) -> bool {
    let drawn = source
        .effects
        .iter()
        .any(|e| e.enabled && (e.kind == catalog::POINTER_KIND || e.kind == catalog::OLD_POINTER_KIND));
    if drawn {
        return true;
    }
    let on_air = ws.program.and_then(|p| ws.scene(p));
    on_air
        .into_iter()
        .chain(ws.scenes.iter())
        .filter_map(|scene| scene.screen.map(|look| (scene, look)))
        .find(|(scene, _)| scene.objects.iter().any(|o| o.source == Some(source.id)))
        .is_some_and(|(_, look)| look.pointer_drawn || look.pointer_hidden)
}

/// The screen capture's settings for `plan` at `size` (`None`: the target's own).
fn screen_config(plan: &Plan, size: Option<(u32, u32)>) -> Option<screen::Config> {
    let Plan::Screen { exclude_own, fps, cursor, .. } = plan else { return None };
    Some(screen::Config {
        size,
        fps: *fps,
        format: PixelFormat::Bgra,
        cursor: *cursor,
        exclude_own: *exclude_own,
        queue_depth: SCREEN_QUEUE_DEPTH,
        // Windows Graphics Capture, Desktop Duplication where it isn't there
        #[cfg(windows)]
        backend: screen::Backend::Auto,
    })
}

// ---------------------------------------------------------------- pictures

/// A captured picture: BGRA; IOSurface-backed as the capture made it (macOS), or a copy of it
/// in a shared texture (Windows).
#[derive(Clone)]
pub(crate) struct Captured {
    pub(crate) buffer: bridge::Buffer,
    /// Counts the capture's frames: a different value is a new picture.
    pub(crate) serial: u64,
}

impl Captured {
    /// As the bridge's pictures are given to the keyers and the looks: without any libobs
    /// filter.
    pub(crate) fn picture(&self) -> bridge::Picture {
        bridge::Picture { buffer: self.buffer.clone(), unfiltered: true, serial: self.serial }
    }
}

// SAFETY: CoreVideo's buffers are reference counted thread-safely, and a captured picture isn't
// written again once it's handed over.
#[cfg(target_os = "macos")]
unsafe impl Send for Captured {}
#[cfg(target_os = "macos")]
unsafe impl Sync for Captured {}

/// What a running capture's sink shares with the rest.
struct Latest {
    /// The newest picture, and whether pictures are still taken (false once the capture is
    /// being let go: a late frame isn't handed to libobs's wrapper after it got its capture back).
    frame: Mutex<(bool, Option<Captured>)>,
    serial: AtomicU64,
    /// The capture ended by itself.
    ended: AtomicBool,
    /// A window capture: the size its picture should be to fit the window (w << 32 | h; 0 none).
    resize: AtomicU64,
    /// The size the screen capture was last asked for (w << 32 | h; 0: the target's own).
    size: AtomicU64,
    /// A change of size or settings is under way.
    changing: AtomicBool,
}

impl Latest {
    fn new() -> Arc<Latest> {
        Arc::new(Latest {
            frame: Mutex::new((true, None)),
            serial: AtomicU64::new(0),
            ended: AtomicBool::new(false),
            resize: AtomicU64::new(0),
            size: AtomicU64::new(0),
            changing: AtomicBool::new(false),
        })
    }

    /// No more pictures are taken, and libobs's wrapper gets its capture back.
    fn let_go(&self, source: Id) {
        let mut f = self.frame.lock();
        f.0 = false;
        let old = f.1.take();
        bridge::release_capture(source);
        drop(f);
        drop(old);
    }

    /// No more pictures are taken, but libobs's wrapper keeps drawing the last one: another
    /// capture of the same source is starting (a camera isn't opened by libobs in between).
    fn quiet(&self) {
        let mut f = self.frame.lock();
        f.0 = false;
        let old = f.1.take();
        drop(f);
        drop(old);
    }

    fn asked_size(&self) -> Option<(u32, u32)> {
        unpack(self.size.load(Ordering::Acquire))
    }
}

fn pack((w, h): (u32, u32)) -> u64 {
    ((w as u64) << 32) | h as u64
}

fn unpack(v: u64) -> Option<(u32, u32)> {
    (v != 0).then_some(((v >> 32) as u32, v as u32))
}

/// Where a capture delivers: its frames become the source's newest picture.
struct Sink {
    source: Id,
    latest: Arc<Latest>,
    window: bool,
    /// Windows: copies each frame into Lightair's own pictures.
    #[cfg(windows)]
    copier: lightair_host::buffers::Copier,
}

impl VideoSink for Sink {
    fn frame(&mut self, frame: Frame) {
        if frame.format != PixelFormat::Bgra {
            return;
        }
        if self.window {
            self.follow_window(&frame);
        }
        let Some(buffer) = self.buffer_of(frame) else { return };
        let serial = self.latest.serial.fetch_add(1, Ordering::AcqRel) + 1;
        let mut f = self.latest.frame.lock();
        if !f.0 {
            return;
        }
        bridge::publish_capture(self.source, &buffer);
        let old = f.1.replace(Captured { buffer, serial });
        drop(f);
        drop(old);
    }

    fn stopped(&mut self, reason: &str) {
        tracing::info!(source = %self.source, reason, "capture: ended by itself; libobs captures it until it's back");
        self.latest.ended.store(true, Ordering::Release);
    }
}

impl Sink {
    /// The frame as Lightair's picture: the pixel buffer itself.
    #[cfg(target_os = "macos")]
    fn buffer_of(&mut self, frame: Frame) -> Option<bridge::Buffer> {
        let raw = NonNull::new(frame.into_raw().cast::<CVPixelBuffer>())?;
        // SAFETY: `into_raw` gave up the frame's reference, now this one's
        Some(unsafe { CFRetained::from_raw(raw) })
    }

    /// The frame copied into Lightair's own picture (none while pictures aren't taken); the
    /// capture has its texture back once this returns.
    #[cfg(windows)]
    fn buffer_of(&mut self, frame: Frame) -> Option<bridge::Buffer> {
        use lightair_capture::Image;
        if !self.latest.frame.lock().0 {
            return None;
        }
        match frame.image() {
            Image::Texture(t) => {
                self.copier.copy(t.shared_handle(), frame.size, t.fence().map(|f| (f.handle, f.value)))
            }
            Image::Memory(m) => self.copier.from_memory(&m.data, m.stride, frame.size),
        }
    }

    /// A window drawn into a picture of another size (it was resized): the size that fits it,
    /// for the capture to be changed to (the window's content in points, ÷ how much it was
    /// scaled to fit, × the display's pixels to a point).
    fn follow_window(&self, frame: &Frame) {
        let (Some(content), Some(content_scale), Some(scale)) = (frame.content, frame.content_scale, frame.scale)
        else {
            return;
        };
        if content_scale <= 0.0 || scale <= 0.0 {
            return;
        }
        let px = |points: f64| ((points / content_scale * scale).round().clamp(16.0, 8192.0) as u32) & !1;
        let fits = (px(content.width), px(content.height));
        let (w, h) = frame.size;
        if fits.0.abs_diff(w) > RESIZE_SLACK || fits.1.abs_diff(h) > RESIZE_SLACK {
            self.latest.resize.store(pack(fits), Ordering::Release);
        }
    }
}

// ---------------------------------------------------------------- the captures

/// A running capture.
enum Running {
    Screen(screen::ScreenCapture),
    Camera(camera::CameraCapture),
    #[cfg(windows)]
    Game(lightair_capture::game::GameCapture),
    #[cfg(windows)]
    AnyGame(lightair_capture::game::FullscreenCapture),
}

impl Running {
    /// What it captures at (for the log).
    fn describe(&self) -> String {
        match self {
            Running::Screen(c) => format!("screen {}x{}", c.size().0, c.size().1),
            #[cfg(windows)]
            Running::Game(c) => format!("game {}x{}", c.size().0, c.size().1),
            #[cfg(windows)]
            Running::AnyGame(c) => format!("the full-screen application {}x{}", c.size().0, c.size().1),
            Running::Camera(c) => {
                let f = c.format();
                format!("camera {}x{} {} at {:.2} fps", f.size.0, f.size.1, f.fourcc_text(), c.fps())
            }
        }
    }
}

enum State {
    /// A thread is starting it.
    Starting,
    Running(Arc<Running>),
    /// libobs captures it: until then (`None`: until its plan changes).
    Libobs(Option<Instant>),
}

struct Entry {
    plan: Plan,
    /// Changes with each start, so a starting thread's late answer about an older one is let go.
    generation: u64,
    state: State,
    latest: Arc<Latest>,
    /// Since when no compositor wants it.
    unwanted_since: Option<Instant>,
}

/// Work done on a thread of its own (it may block).
enum Job {
    Start { source: Id, plan: Plan, generation: u64, latest: Arc<Latest> },
    Stop(Arc<Running>),
    Change { capture: Arc<Running>, config: screen::Config, latest: Arc<Latest> },
}

#[derive(Default)]
struct Host {
    /// What each compositor (by canvas) wants captured.
    wanted: HashMap<Id, HashMap<Id, Plan>>,
    entries: HashMap<Id, Entry>,
    generation: u64,
    started: bool,
}

static HOST: LazyLock<Mutex<Host>> = LazyLock::new(Default::default);

/// The compositor of `canvas` draws scenes showing these screens and cameras now (replacing what
/// it said before; empty when it draws nothing). Doesn't block.
pub(crate) fn want(canvas: Id, plans: HashMap<Id, Plan>) {
    let mut host = HOST.lock();
    if plans.is_empty() {
        host.wanted.remove(&canvas);
    } else {
        host.wanted.insert(canvas, plans);
    }
    if !host.started && !host.wanted.is_empty() {
        host.started = std::thread::Builder::new()
            .name("lightair-captures".into())
            .spawn(run)
            .map_err(|e| tracing::error!(%e, "capture: no thread; libobs captures every screen and camera"))
            .is_ok();
    }
}

/// `source`'s newest picture, while Lightair captures it.
pub(crate) fn frame(source: Id) -> Option<Captured> {
    let host = HOST.lock();
    let entry = host.entries.get(&source)?;
    if !matches!(entry.state, State::Running(_)) {
        return None;
    }
    entry.latest.frame.lock().1.clone()
}

/// The captures' thread: starts what's wanted, lets go of what isn't, follows changes.
fn run() {
    loop {
        std::thread::sleep(PERIOD);
        let jobs = HOST.lock().step(Instant::now());
        for job in jobs {
            spawn(job);
        }
    }
}

impl Host {
    /// Brings the captures in line with what's wanted at `now`; the work to do off this lock.
    fn step(&mut self, now: Instant) -> Vec<Job> {
        let mut jobs = Vec::new();
        let mut wanted: HashMap<Id, Plan> = HashMap::new();
        for plans in self.wanted.values() {
            for (id, plan) in plans {
                wanted.entry(*id).or_insert_with(|| plan.clone());
            }
        }
        // not wanted for a while: let go
        let mut gone = Vec::new();
        for (id, entry) in self.entries.iter_mut() {
            if wanted.contains_key(id) {
                entry.unwanted_since = None;
            } else if now.duration_since(*entry.unwanted_since.get_or_insert(now)) >= LINGER {
                gone.push(*id);
            }
        }
        for id in gone {
            if let Some(entry) = self.entries.remove(&id) {
                entry.latest.let_go(id);
                if let State::Running(capture) = entry.state {
                    jobs.push(Job::Stop(capture));
                }
            }
        }
        for (id, plan) in wanted {
            let Some(entry) = self.entries.get_mut(&id) else {
                self.generation += 1;
                let latest = Latest::new();
                jobs.push(Job::Start {
                    source: id,
                    plan: plan.clone(),
                    generation: self.generation,
                    latest: latest.clone(),
                });
                self.entries.insert(
                    id,
                    Entry { plan, generation: self.generation, state: State::Starting, latest, unwanted_since: None },
                );
                continue;
            };
            if entry.plan != plan {
                // the same screen at another frame rate or with the pointer on or off: changed live
                if let State::Running(capture) = &entry.state
                    && plan.reconfigures(&entry.plan)
                    && !entry.latest.changing.load(Ordering::Acquire)
                    && let Some(config) = screen_config(&plan, entry.latest.asked_size())
                {
                    entry.latest.changing.store(true, Ordering::Release);
                    jobs.push(Job::Change { capture: capture.clone(), config, latest: entry.latest.clone() });
                    entry.plan = plan;
                    continue;
                }
                if matches!(entry.state, State::Running(_)) && plan.reconfigures(&entry.plan) {
                    // a change already under way: this one next time round
                    continue;
                }
                // anything else: started again (libobs's wrapper keeps the last picture meanwhile)
                entry.latest.quiet();
                if let State::Running(capture) = std::mem::replace(&mut entry.state, State::Starting) {
                    jobs.push(Job::Stop(capture));
                }
                self.generation += 1;
                entry.generation = self.generation;
                entry.latest = Latest::new();
                entry.plan = plan.clone();
                entry.state = State::Starting;
                jobs.push(Job::Start { source: id, plan, generation: entry.generation, latest: entry.latest.clone() });
                continue;
            }
            match &entry.state {
                State::Running(capture) if entry.latest.ended.load(Ordering::Acquire) => {
                    entry.latest.let_go(id);
                    jobs.push(Job::Stop(capture.clone()));
                    entry.state = State::Libobs(Some(now + RETRY_ENDED));
                }
                State::Running(capture) if entry.plan.is_window() && !entry.latest.changing.load(Ordering::Acquire) => {
                    // the window changed size: the picture follows
                    let Some(fits) = unpack(entry.latest.resize.swap(0, Ordering::AcqRel)) else { continue };
                    if entry.latest.asked_size() == Some(fits) {
                        continue;
                    }
                    if let Some(config) = screen_config(&entry.plan, Some(fits)) {
                        entry.latest.changing.store(true, Ordering::Release);
                        entry.latest.size.store(pack(fits), Ordering::Release);
                        jobs.push(Job::Change { capture: capture.clone(), config, latest: entry.latest.clone() });
                    }
                }
                State::Libobs(Some(at)) if now >= *at => {
                    self.generation += 1;
                    entry.generation = self.generation;
                    entry.latest = Latest::new();
                    entry.state = State::Starting;
                    jobs.push(Job::Start {
                        source: id,
                        plan: entry.plan.clone(),
                        generation: entry.generation,
                        latest: entry.latest.clone(),
                    });
                }
                _ => {}
            }
        }
        jobs
    }
}

/// Runs `job` on a thread of its own.
fn spawn(job: Job) {
    let name = match &job {
        Job::Start { .. } => "lightair-capture-start",
        Job::Stop(_) => "lightair-capture-stop",
        Job::Change { .. } => "lightair-capture-change",
    };
    let spawned = std::thread::Builder::new().name(name.into()).spawn(move || match job {
        Job::Start { source, plan, generation, latest } => start(source, &plan, generation, latest),
        // (a camera's session stops before this returns)
        Job::Stop(capture) => drop(capture),
        Job::Change { capture, config, latest } => {
            let changed = match &*capture {
                Running::Screen(c) => c.reconfigure(&config),
                #[cfg(windows)]
                Running::Game(c) => c.reconfigure(&config),
                #[cfg(windows)]
                Running::AnyGame(c) => c.reconfigure(&config),
                Running::Camera(_) => Ok(()),
            };
            if let Err(e) = changed {
                tracing::warn!(%e, "capture: couldn't change a screen capture");
            }
            latest.changing.store(false, Ordering::Release);
        }
    });
    if let Err(e) = spawned {
        tracing::error!(%e, "capture: no thread");
    }
}

/// Why a capture didn't start, and when to try again (`None`: not until its plan changes).
struct Refused {
    reason: String,
    retry: Option<Duration>,
}

impl From<Error> for Refused {
    fn from(e: Error) -> Refused {
        let retry = match &e {
            Error::Permission(_) | Error::Failed(_) => Some(Duration::from_secs(10)),
            Error::NotFound(_) => Some(Duration::from_secs(5)),
            Error::Unsupported(_) => None,
        };
        Refused { reason: e.to_string(), retry }
    }
}

/// Starts `source`'s capture (a thread of its own) and records the outcome, unless the plan was
/// changed or the capture let go meanwhile (then it's let go again at once).
fn start(source: Id, plan: &Plan, generation: u64, latest: Arc<Latest>) {
    let outcome = open(source, plan, &latest);
    let (unwanted, replaced) = {
        let mut host = HOST.lock();
        let replaced = host.entries.contains_key(&source);
        let unwanted = match (host.entries.get_mut(&source).filter(|e| e.generation == generation), outcome) {
            (Some(entry), Ok(capture)) => {
                tracing::info!(source = %source, how = capture.describe(), "capture: Lightair captures it (libobs's capture stops)");
                entry.state = State::Running(Arc::new(capture));
                None
            }
            (Some(entry), Err(refused)) => {
                tracing::info!(source = %source, reason = refused.reason, "capture: libobs captures it");
                latest.let_go(source);
                entry.state = State::Libobs(refused.retry.map(|d| Instant::now() + d));
                None
            }
            (None, Ok(capture)) => Some(capture),
            (None, Err(_)) => None,
        };
        (unwanted, replaced)
    };
    // let go meanwhile, or another capture of the source started since: stopped here, off the
    // lock (the source given back to libobs only when nothing captures it now)
    if let Some(capture) = unwanted {
        if replaced {
            latest.quiet();
        } else {
            latest.let_go(source);
        }
        drop(capture);
    }
}

/// Opens `plan`'s capture into `latest`. Blocks; never asks for access.
fn open(source: Id, plan: &Plan, latest: &Arc<Latest>) -> Result<Running, Refused> {
    let later = |reason: &str| Refused { reason: reason.into(), retry: Some(Duration::from_secs(5)) };
    let sink = Sink {
        source,
        latest: latest.clone(),
        window: plan.is_window(),
        #[cfg(windows)]
        copier: lightair_host::buffers::Copier::new(),
    };
    // the capture's textures on the graphics adapter of Lightair's own pictures, so they can be
    // copied there
    #[cfg(windows)]
    lightair_capture::set_adapter(lightair_host::buffers::adapter());
    match plan {
        Plan::Screen { aim, .. } => {
            #[cfg(target_os = "macos")]
            if !screen::access() {
                return Err(later("screen recording isn't allowed yet"));
            }
            let config = screen_config(plan, None).ok_or_else(|| later("not a screen"))?;
            let target = match aim {
                Aim::Target(screen::Target::Display(d)) => {
                    screen::Target::Display(display_of(d).ok_or_else(|| later("no main display"))?)
                }
                Aim::Target(t) => t.clone(),
                #[cfg(windows)]
                Aim::Window(named) => screen::Target::Window(
                    lightair_host::displays::find_window(named).ok_or_else(|| later("the window isn't open"))?,
                ),
                #[cfg(windows)]
                Aim::AnyGame => {
                    return Ok(Running::AnyGame(lightair_capture::game::FullscreenCapture::start(&config, sink)?));
                }
                #[cfg(windows)]
                Aim::Game(exe) => {
                    return Ok(Running::Game(lightair_capture::game::GameCapture::start(
                        &game_path(exe),
                        &config,
                        sink,
                    )?));
                }
            };
            Ok(Running::Screen(screen::ScreenCapture::start(&target, &config, sink)?))
        }
        Plan::Camera { device, height, fps } => {
            let access = camera::access();
            // Windows asks desktop apps nothing: only its privacy setting says no
            let allowed = if cfg!(windows) {
                matches!(access, camera::Access::Allowed | camera::Access::NotAsked)
            } else {
                access == camera::Access::Allowed
            };
            if !allowed {
                return Err(later("the camera isn't allowed yet"));
            }
            let Some(found) = camera::cameras().into_iter().find(|c| same_camera(device, c)) else {
                return Err(later("the camera isn't connected"));
            };
            if found.muxed {
                // its sound comes in the same stream: libobs's capture keeps both
                return Err(Refused { reason: "its sound comes with its pictures".into(), retry: None });
            }
            let config =
                camera::Config { format: None, height: *height, fps: Some(*fps), pixel_format: PixelFormat::Bgra };
            Ok(Running::Camera(camera::CameraCapture::start(&found.id, &config, sink)?))
        }
    }
}

/// The display a setting names: the main one for none.
#[cfg(target_os = "macos")]
fn display_of(display: &str) -> Option<String> {
    if display.is_empty() { lightair_host::displays::main_display_uuid() } else { Some(display.to_string()) }
}

/// The display a setting names: the primary monitor for none, or for one that isn't a Windows
/// monitor's (a project from a Mac).
#[cfg(windows)]
fn display_of(display: &str) -> Option<String> {
    let monitor = display.starts_with(r"\\") || display.parse::<u32>().is_ok();
    if monitor { Some(display.to_string()) } else { lightair_host::displays::main_display_uuid() }
}

/// A game's executable (its file name, as libobs's window string has it) as a full path, from
/// the game running now (its file name while it isn't: the game capture waits for it).
#[cfg(windows)]
fn game_path(exe: &str) -> String {
    let name = |path: &str| path.rsplit(['\\', '/']).next().unwrap_or(path).to_ascii_lowercase();
    lightair_capture::game::running()
        .into_iter()
        .find(|g| name(&g.exe) == name(exe))
        .map_or_else(|| exe.to_string(), |g| g.exe)
}

/// Whether `camera` is the one a `device` setting names: its id.
#[cfg(target_os = "macos")]
fn same_camera(device: &str, camera: &camera::Camera) -> bool {
    camera.id == device
}

/// Whether `camera` is the one a `device` setting names: its symbolic link; or libobs's
/// DirectShow id of an older project (`name:path`, `:` written `#3A`): the same device (its
/// path but for its device class, the last `#{…}` part), else the same name.
#[cfg(windows)]
fn same_camera(device: &str, camera: &camera::Camera) -> bool {
    if camera.id.eq_ignore_ascii_case(device) {
        return true;
    }
    let Some((name, path)) = device.split_once(':') else { return false };
    let decode = |s: &str| s.replace("#3A", ":").replace("#22", "#");
    let (name, path) = (decode(name), decode(path));
    let device_of = |p: &str| p.rsplit_once("#{").map_or(p, |(d, _)| d).to_ascii_lowercase();
    if !path.is_empty() && device_of(&path) == device_of(&camera.id) {
        return true;
    }
    !name.is_empty() && camera.name == name
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;
    use serde_json::json;

    fn game(settings: serde_json::Value) -> Option<Plan> {
        let source = Source { id: Id::new(), name: "Game".into(), kind: kinds::SCREEN.into(), settings, effects: vec![] };
        Plan::of(&source, &Workspace::new("Show"))
    }

    fn aim(plan: Option<Plan>) -> Option<Aim> {
        match plan? {
            Plan::Screen { aim, .. } => Some(aim),
            _ => None,
        }
    }

    /// A Game source with no game chosen (no window, an empty one, or the picker's `null`)
    /// captures any full-screen application; with one chosen, that game by its executable.
    #[test]
    fn a_game_with_none_chosen_is_any_full_screen_application() {
        for settings in [
            json!({ "mode": kinds::GAME }),
            json!({ "mode": kinds::GAME, "window": null }),
            json!({ "mode": kinds::GAME, "window": "" }),
            json!({ "mode": kinds::GAME, "window": "   " }),
        ] {
            assert_eq!(aim(game(settings.clone())), Some(Aim::AnyGame), "{settings}");
        }
        let chosen = json!({ "mode": kinds::GAME, "window": "Elden Ring:ELDEN RING#3A NIGHTREIGN:eldenring.exe" });
        assert_eq!(aim(game(chosen)), Some(Aim::Game("eldenring.exe".into())));
        assert_eq!(aim(game(json!({ "mode": kinds::GAME, "window": "game.exe" }))), Some(Aim::Game("game.exe".into())));
    }

    /// Only a game falls back to any full-screen application: a window capture with no window
    /// chosen captures nothing.
    #[test]
    fn a_window_with_none_chosen_is_nothing() {
        assert_eq!(aim(game(json!({ "mode": kinds::WINDOW }))), None);
        assert_eq!(screen_aim(kinds::WINDOW, |_| None), None);
        assert_eq!(screen_aim(kinds::GAME, |_| None), Some(Aim::AnyGame));
    }
}
