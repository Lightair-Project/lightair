//! The drawn pointer (docs/plans/screen-studio.md, "Cursor"): a filter on a screen capture that
//! draws the pointer itself, so it can be bigger, smoother and styled. The capture's own cursor
//! is turned off while it's on (`reconcile::source_settings`).
//!
//! - The pointer comes from `lightair_platform::source` each frame and is followed by a critically
//!   damped spring (smoothing), tilted by its speed, and faded out when it rests.
//! - Shapes are Lightair's own, drawn here at a high resolution (an arrow, an I-beam, a
//!   crosshair; a dot and a ring as styles), so they stay sharp at any size. Any other pointer
//!   (a hand, a resize arrow, an app's own) is the system's own picture (macOS,
//!   `lightair_platform::system_pointer`).
//! - Clicks (when the OS lets Lightair see them) draw a circle or a ripple.
//!
//! - The follow-zoom (docs/plans/screen-studio.md, "Auto zoom", live): in to where a click was
//!   and out again after a while without one, or always in, following the pointer. Scale and
//!   centre move on critically damped springs; the view keeps the pointer inside it and never
//!   leaves the capture. The pointer and clicks are drawn zoomed too.
//!
//! A scene's [`lightair_model::ScreenZoomPointer`] puts one of these on each of its screen captures
//! (`reconcile`); the effect catalog's "Drawn pointer" is the same filter without the zoom.
//!
//! Positions map from display points to the capture's pixels through the frame of what it
//! captures: its display, or its window where the window is now (read a few times a second);
//! the main display when that can't be told (`capture_frame`).
//!
//! The state lives here (`Pointer`, `Style`, the clicks, the badge); libobs's filter
//! (lightair-obs) and the compositor (lightair-preview) draw from it.

use std::collections::VecDeque;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use lightair_platform::input::{self, Event, Recorder, Shape};
use lightair_platform::system_pointer;

pub const KIND: &str = lightair_model::catalog::POINTER_KIND;

use lightair_render::shapes::{arrow, crosshair, ibeam};
/// The pointer's size in display points at size 1.
const POINTS: f32 = 22.0;
/// How long a click's effect lasts.
const CLICK: Duration = Duration::from_millis(450);

// ------------------------------------------------------------------ clicks

/// Clicks seen lately, for every pointer filter (one listener for all of them).
struct Clicks {
    recorder: Option<Recorder>,
    started: Instant,
    recent: VecDeque<(Instant, f32, f32)>,
    /// The last key typed (not a shortcut: ⌘ or Ctrl held), and the last click (where typing
    /// most likely goes).
    typed: Option<Instant>,
    last_click: Option<(f32, f32)>,
    users: usize,
}

static CLICKS: Mutex<Option<Clicks>> = Mutex::new(None);

fn clicks_join() {
    let mut c = CLICKS.lock().unwrap();
    let c = c.get_or_insert_with(|| Clicks {
        recorder: None,
        started: Instant::now(),
        recent: VecDeque::new(),
        typed: None,
        last_click: None,
        users: 0,
    });
    c.users += 1;
    if c.recorder.is_none() && input::can_listen() {
        c.started = Instant::now();
        c.recorder = Some(Recorder::start(30));
    }
}

fn clicks_leave() {
    let mut guard = CLICKS.lock().unwrap();
    if let Some(c) = guard.as_mut() {
        c.users = c.users.saturating_sub(1);
        if c.users == 0 {
            *guard = None;
        }
    }
}

/// Clicks in the last `CLICK`, oldest first.
fn recent_clicks() -> Vec<(Instant, f32, f32)> {
    let mut guard = CLICKS.lock().unwrap();
    let Some(c) = guard.as_mut() else { return Vec::new() };
    if let Some(r) = &c.recorder {
        for s in r.take() {
            match s.event {
                Event::Down { x, y, .. } => {
                    c.recent.push_back((c.started + s.at, x, y));
                    c.last_click = Some((x, y));
                }
                Event::KeyDown { modifiers, .. }
                    if modifiers & (input::modifiers::COMMAND | input::modifiers::CONTROL) == 0 =>
                {
                    c.typed = Some(c.started + s.at);
                }
                _ => {}
            }
        }
    }
    let now = Instant::now();
    while c.recent.front().is_some_and(|(t, ..)| now.duration_since(*t) > CLICK) {
        c.recent.pop_front();
    }
    c.recent.iter().copied().collect()
}

/// When a key was last typed, and where the last click was (display points).
fn typing() -> (Option<Instant>, Option<(f32, f32)>) {
    recent_clicks();
    let guard = CLICKS.lock().unwrap();
    guard.as_ref().map_or((None, None), |c| (c.typed, c.last_click))
}

// ------------------------------------------------------------------ following, on a key

/// Zoomed in, following the pointer, on looks whose zoom is "toggle" (a shortcut turns it on
/// and off again).
static FOLLOW: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Turns following on or off: whether it's on now.
pub fn toggle_follow() -> bool {
    !FOLLOW.fetch_xor(true, std::sync::atomic::Ordering::Relaxed)
}

pub fn following() -> bool {
    FOLLOW.load(std::sync::atomic::Ordering::Relaxed)
}

/// How a drawn pointer and its follow-zoom look and behave: a pointer filter's settings, or a
/// scene's [`lightair_model::ScreenZoomPointer`] as the reconciler writes it for the filter.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Style {
    style: i64,
    size: f32,
    smoothing: f32,
    tilt: bool,
    idle: f32,
    clicks: i64,
    arrow_only: bool,
    /// 0xAABBGGRR
    color: u32,
    /// Draws the pointer (off: only zooms, the capture keeps its own).
    draw: bool,
    /// 0 off, 1 on clicks, 2 always, 3 on a shortcut (`ScreenZoomPointer`'s).
    zoom: i64,
    zoom_scale: f32,
    zoom_hold: f32,
    zoom_motion: f32,
    /// On clicks: typing zooms in too, looking where the last click was.
    zoom_typing: bool,
    /// Tells the badge by the real pointer when it's live and zoomed in.
    zoom_badge: bool,
}

impl Style {
    /// From the settings as JSON (an effect's, or `reconcile`'s for a scene's look), with the
    /// filter's defaults for what's missing.
    pub fn from_json(s: &serde_json::Value) -> Style {
        let float = |k: &str, d: f64| s.get(k).and_then(serde_json::Value::as_f64).unwrap_or(d) as f32;
        let int = |k: &str, d: i64| s.get(k).and_then(serde_json::Value::as_i64).unwrap_or(d);
        let flag = |k: &str, d: bool| s.get(k).and_then(serde_json::Value::as_bool).unwrap_or(d);
        Style {
            style: int("style", 0),
            size: float("size", 1.5).clamp(0.25, 6.0),
            smoothing: float("smoothing", 0.5).clamp(0.0, 1.0),
            tilt: flag("tilt", true),
            idle: float("idle_s", 3.0).max(0.0),
            clicks: int("clicks", 2),
            arrow_only: flag("arrow_only", false),
            color: int("color", 0xFFFF_9000) as u32,
            draw: flag("draw", true),
            zoom: int("zoom", 0),
            zoom_scale: float("zoom_scale", 2.0).clamp(1.0, 6.0),
            zoom_hold: float("zoom_hold_s", 2.0).max(0.2),
            zoom_motion: float("zoom_motion", 0.5).clamp(0.0, 1.0),
            zoom_typing: flag("zoom_typing", true),
            zoom_badge: flag("zoom_badge", true),
        }
    }

    /// The colour of the dot, ring and click disc (0xAABBGGRR).
    pub fn color(&self) -> u32 {
        self.color
    }

    /// Whether the zoom badge by the real pointer counts this pointer.
    pub fn badge(&self) -> bool {
        self.zoom_badge
    }
}

/// The follow-zoom's camera (capture pixels).
#[derive(Clone, Copy, Debug, PartialEq)]
struct Cam {
    scale: f32,
    scale_v: f32,
    center: (f32, f32),
    center_v: (f32, f32),
    /// Where it's heading (the pointer kept inside the view).
    aim: (f32, f32),
}

impl Cam {
    fn new(size: (f32, f32)) -> Cam {
        let c = (size.0 / 2.0, size.1 / 2.0);
        Cam { scale: 1.0, scale_v: 0.0, center: c, center_v: (0.0, 0.0), aim: c }
    }
}

/// Where the view heads: as it was while the pointer stays within its middle, else moved just
/// enough to bring it back in; kept inside the capture.
fn aim_at(aim: (f32, f32), pointer: (f32, f32), scale: f32, size: (f32, f32)) -> (f32, f32) {
    let scale = scale.max(1.0);
    let half = (size.0 / scale / 2.0, size.1 / scale / 2.0);
    // the middle 60% of the view
    let slack = (half.0 * 0.6, half.1 * 0.6);
    let follow = |a: f32, p: f32, s: f32| {
        if p > a + s {
            p - s
        } else if p < a - s {
            p + s
        } else {
            a
        }
    };
    let a = (follow(aim.0, pointer.0, slack.0), follow(aim.1, pointer.1, slack.1));
    (a.0.clamp(half.0, (size.0 - half.0).max(half.0)), a.1.clamp(half.1, (size.1 - half.1).max(half.1)))
}

/// The part of the capture shown (x, y, w, h in its pixels) by a camera at `center`, `scale`.
fn view_rect(center: (f32, f32), scale: f32, size: (f32, f32)) -> (f32, f32, f32, f32) {
    let scale = scale.max(1.0);
    let (w, h) = (size.0 / scale, size.1 / scale);
    let x = (center.0 - w / 2.0).clamp(0.0, size.0 - w);
    let y = (center.1 - h / 2.0).clamp(0.0, size.1 - h);
    (x, y, w, h)
}

/// One step of the camera towards `scale` and the pointer (capture pixels).
fn cam_step(
    cam: &mut Cam,
    want_scale: f32,
    pointer: (f32, f32),
    size: (f32, f32),
    motion: f32,
    dt: f32,
    snap_aim: bool,
) {
    cam.aim = if snap_aim {
        aim_at(pointer, pointer, want_scale, size)
    } else {
        aim_at(cam.aim, pointer, want_scale.max(cam.scale), size)
    };
    // when out, it heads back to the middle
    let aim = if want_scale <= 1.001 { (size.0 / 2.0, size.1 / 2.0) } else { cam.aim };
    let omega = 14.0 - 10.0 * motion.clamp(0.0, 1.0);
    (cam.scale, cam.scale_v) = spring(cam.scale, cam.scale_v, want_scale, omega, dt);
    let (x, vx) = spring(cam.center.0, cam.center_v.0, aim.0, omega, dt);
    let (y, vy) = spring(cam.center.1, cam.center_v.1, aim.1, omega, dt);
    cam.center = (x, y);
    cam.center_v = (vx, vy);
}

/// What a pointer draws: the part of the capture shown (x, y, w, h as fractions of it; `None`:
/// all of it, not zoomed), then the sprites over it, in order, in the capture's pixels.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Drawing {
    pub view: Option<(f32, f32, f32, f32)>,
    pub sprites: Vec<Sprite>,
}

/// A picture drawn square, `size` pixels on a side, with its hot spot (a fraction of the size)
/// at `at`, turned by `angle` degrees about it, at `opacity`. Art is premultiplied.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Sprite {
    pub art: Art,
    pub at: (f32, f32),
    pub size: f32,
    pub hot: (f32, f32),
    pub angle: f32,
    pub opacity: f32,
}

/// Which picture a sprite draws: Lightair's own shapes (`lightair_render::shapes`, the dot, ring
/// and click disc in the style's colour), or the system pointer's own ([`Pointer::picture`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Art {
    Arrow,
    IBeam,
    Crosshair,
    Dot,
    Ring,
    ClickRing,
    ClickDisc,
    System,
}

/// The hot spots of the arrow, the I-beam and the crosshair.
fn hots() -> [(f32, f32); 3] {
    static HOTS: std::sync::OnceLock<[(f32, f32); 3]> = std::sync::OnceLock::new();
    *HOTS.get_or_init(|| [arrow().hot, ibeam().hot, crosshair().hot])
}

/// A drawn pointer's state, stepped each frame ([`Pointer::tick`]): where it is (followed on a
/// spring), how it's turned and faded, its shape, and the follow-zoom's camera. libobs's filter
/// and Lightair's compositor both draw from it ([`Pointer::drawing`]).
pub struct Pointer {
    look: Style,
    /// Followed position (display points), its velocity, the tilt.
    pos: Option<(f32, f32)>,
    vel: (f32, f32),
    tilt: f32,
    /// When the pointer last moved, and the opacity now.
    moved: Instant,
    last_target: (f32, f32),
    opacity: f32,
    shape: Shape,
    /// The follow-zoom: its camera, until when it stays in, the newest click seen.
    cam: Option<Cam>,
    zoom_until: Option<Instant>,
    last_click: Option<Instant>,
    /// The newest key seen (typing zooms in too).
    last_typed: Option<Instant>,
    /// What the capture shows, in global display points (its display, or its window where it
    /// is now), and when that was last read.
    frame: Option<(f32, f32, f32, f32)>,
    frame_read: Option<Instant>,
    /// The system pointer's own picture, when it's a shape Lightair has no art for (a hand, a
    /// resize arrow, an app's own pointer).
    picture: Option<system_pointer::Picture>,
}

impl Pointer {
    /// A pointer drawn as `look` says (clicks are listened for while any pointer exists).
    pub fn new(look: Style) -> Pointer {
        clicks_join();
        Pointer {
            look,
            pos: None,
            vel: (0.0, 0.0),
            tilt: 0.0,
            moved: Instant::now(),
            last_target: (f32::NAN, f32::NAN),
            opacity: 1.0,
            shape: Shape::Arrow,
            cam: None,
            zoom_until: None,
            last_click: None,
            last_typed: None,
            frame: None,
            frame_read: None,
            picture: None,
        }
    }

    pub fn look(&self) -> &Style {
        &self.look
    }

    pub fn set_look(&mut self, look: Style) {
        self.look = look;
    }

    /// The follow-zoom's scale now (`None`: not zooming).
    pub fn zoom(&self) -> Option<f32> {
        self.cam.map(|c| c.scale)
    }

    /// The system pointer's own picture, which [`Art::System`] draws.
    pub fn picture(&self) -> Option<&system_pointer::Picture> {
        self.picture.as_ref()
    }

    /// One step of `dt` seconds, for a capture `size` pixels big. `frame` says where the capture
    /// is (its display, or its window, in display points; `None`: the main display): asked a few
    /// times a second.
    pub fn tick(&mut self, dt: f32, size: (u32, u32), frame: impl FnOnce() -> Option<(f32, f32, f32, f32)>) {
        let dt = dt.clamp(1e-4, 0.1);
        let target = input::pointer();
        if target != self.last_target {
            self.last_target = target;
            self.moved = Instant::now();
        }
        // smoothing 0: stiff (follows at once); 1: a slow glide
        let omega = 60.0 - 52.0 * self.look.smoothing;
        let (x, y) = self.pos.unwrap_or(target);
        let (nx, vx) = spring(x, self.vel.0, target.0, omega, dt);
        let (ny, vy) = spring(y, self.vel.1, target.1, omega, dt);
        self.pos = Some((nx, ny));
        self.vel = (vx, vy);
        let want_tilt = if self.look.tilt { (vx * 0.012).clamp(-14.0, 14.0) } else { 0.0 };
        self.tilt += (want_tilt - self.tilt) * (dt * 12.0).min(1.0);
        let idle = self.look.idle > 0.0 && self.moved.elapsed().as_secs_f32() > self.look.idle;
        let want = if idle { 0.0 } else { 1.0 };
        self.opacity += (want - self.opacity) * (dt * 8.0).min(1.0);
        self.shape = if self.look.arrow_only { Shape::Arrow } else { input::shape() };
        self.picture = if self.look.arrow_only {
            None
        } else {
            system_pointer::now().filter(|p| !matches!(p.shape, Some(Shape::Arrow | Shape::IBeam | Shape::Crosshair)))
        };
        // where the capture is (a window moves): read a few times a second
        if self.frame_read.is_none_or(|t| t.elapsed() > Duration::from_millis(250)) {
            self.frame = frame();
            self.frame_read = Some(Instant::now());
        }
        self.zoom_tick(size, dt);
    }

    /// What to draw over a capture `size` pixels big, now.
    pub fn drawing(&self, size: (u32, u32)) -> Drawing {
        let (tw, th) = size;
        if tw == 0 || th == 0 {
            return Drawing::default();
        }
        let size_px = (tw as f32, th as f32);
        let view_px = self.cam.filter(|c| c.scale > 1.001).map(|c| view_rect(c.center, c.scale, size_px));
        let view = view_px.map(|(x, y, w, h)| (x / size_px.0, y / size_px.1, w / size_px.0, h / size_px.1));
        let mut sprites = Vec::new();
        let Some((px, py)) = self.pos.filter(|_| self.look.draw) else { return Drawing { view, sprites } };
        // display points → the capture's pixels → the view's
        let (dx, dy, dw, _) = self.frame.unwrap_or_else(input::main_display);
        let scale = if dw > 0.0 { tw as f32 / dw } else { 1.0 };
        let (vx, vy, vk) = match view_px {
            Some((vx, vy, vw, _)) => (vx, vy, size_px.0 / vw),
            None => (0.0, 0.0, 1.0),
        };
        let place = |x: f32, y: f32| (((x - dx) * scale - vx) * vk, ((y - dy) * scale - vy) * vk);
        let size = POINTS * self.look.size * scale * vk;
        // clicks under the pointer
        if self.look.clicks > 0 {
            let now = Instant::now();
            for (at, cx, cy) in recent_clicks() {
                let k = (now.duration_since(at).as_secs_f32() / CLICK.as_secs_f32()).clamp(0.0, 1.0);
                let (art, grow, fade) = if self.look.clicks == 1 {
                    (Art::ClickDisc, 0.9 + 0.3 * k, 0.45)
                } else {
                    (Art::ClickRing, 0.6 + 1.4 * k, 0.9)
                };
                sprites.push(Sprite {
                    art,
                    at: place(cx, cy),
                    size: size * 1.6 * grow,
                    hot: (0.5, 0.5),
                    angle: 0.0,
                    opacity: fade * (1.0 - k) * self.opacity,
                });
            }
        }
        let [arrow_hot, ibeam_hot, cross_hot] = hots();
        let (art, hot, angle) = match (self.look.style, self.shape) {
            (1, _) => (Art::Dot, (0.5, 0.5), 0.0),
            (2, _) => (Art::Ring, (0.5, 0.5), 0.0),
            (_, _) if let Some(p) = &self.picture => (Art::System, p.hot, 0.0),
            (_, Shape::IBeam) => (Art::IBeam, ibeam_hot, 0.0),
            (_, Shape::Crosshair) => (Art::Crosshair, cross_hot, 0.0),
            _ => (Art::Arrow, arrow_hot, self.tilt),
        };
        sprites.push(Sprite { art, at: place(px, py), size, hot, angle, opacity: self.opacity });
        Drawing { view, sprites }
    }

    /// The follow-zoom, a step on.
    fn zoom_tick(&mut self, size: (u32, u32), dt: f32) {
        // toggle: in, following, while the shortcut has it on; else out
        let mode = match self.look.zoom {
            3 if following() => 2,
            3 => 1,
            m => m,
        };
        if mode == 0 || size.0 == 0 || size.1 == 0 {
            self.cam = None;
            self.zoom_until = None;
            return;
        }
        let size = (size.0 as f32, size.1 as f32);
        let (dx, dy, dw, _) = self.frame.unwrap_or_else(input::main_display);
        let k = if dw > 0.0 { size.0 / dw } else { 1.0 };
        let Some((px, py)) = self.pos else { return };
        let pointer = ((px - dx) * k, (py - dy) * k);
        // a new click: in (again) for a while, looking at it
        let newest = recent_clicks().last().map(|c| c.0);
        let clicked = newest.is_some_and(|n| self.last_click.is_none_or(|l| n > l));
        if clicked {
            self.last_click = newest;
        }
        let now = Instant::now();
        let was_in = self.zoom_until.is_some_and(|u| u > now);
        if clicked {
            self.zoom_until = Some(now + Duration::from_secs_f32(self.look.zoom_hold));
        }
        // typing (on clicks): in as well, looking where the last click went (the text field), and
        // staying there while the keys keep coming
        let (typed, click_at) = if mode == 1 && self.look.zoom_typing { typing() } else { (None, None) };
        let new_key = typed.is_some_and(|t| self.last_typed.is_none_or(|l| t > l));
        if new_key {
            self.last_typed = typed;
            self.zoom_until = Some(now + Duration::from_secs_f32(self.look.zoom_hold));
        }
        let typing_now = typed.is_some_and(|t| now.duration_since(t) < Duration::from_secs_f32(self.look.zoom_hold));
        let focus = match click_at.filter(|_| typing_now && !clicked) {
            Some((cx_, cy_)) => ((cx_ - dx) * k, (cy_ - dy) * k),
            None => pointer,
        };
        let zoomed = match (mode, self.look.zoom) {
            (2, _) => true,
            // toggled out: clicks don't zoom
            (_, 3) => false,
            _ => self.zoom_until.is_some_and(|u| u > now),
        };
        let want = if zoomed { self.look.zoom_scale } else { 1.0 };
        // "zoom to here" (PiAir, crate::zoom_to): in to that point while it lasts
        let asked = crate::zoom_to::current();
        let (want, focus) = match asked {
            Some(r) => (r.scale.unwrap_or(self.look.zoom_scale.max(1.5)), ((r.at.0 - dx) * k, (r.at.1 - dy) * k)),
            None => (want, focus),
        };
        let snap = asked.is_some_and(|r| now.duration_since(r.since) < Duration::from_millis(50));
        let cam = self.cam.get_or_insert_with(|| Cam::new(size));
        cam_step(
            cam,
            want,
            focus,
            size,
            self.look.zoom_motion,
            dt,
            snap || ((clicked || new_key) && !was_in && mode == 1),
        );
    }
}

impl Drop for Pointer {
    fn drop(&mut self) {
        clicks_leave();
    }
}

// ------------------------------------------------------------------ the badge

/// Each zoomed-in filter on a live capture (with its badge on): its zoom, and when it said so
/// (by filter).
static LIVE_ZOOMS: std::sync::Mutex<Vec<(usize, f32, Instant)>> = std::sync::Mutex::new(Vec::new());

/// Says how far a pointer on a live capture is zoomed in now (`None`: not, or gone), by a key of
/// its own (a filter's address, a compositor's source).
pub fn report_zoom(filter: usize, zoom: Option<f32>) {
    let mut all = LIVE_ZOOMS.lock().unwrap_or_else(|e| e.into_inner());
    all.retain(|z| z.0 != filter);
    if let Some(scale) = zoom {
        all.push((filter, scale, Instant::now()));
    }
}

/// What viewers see now, for the badge by the real pointer (`lightair_platform::pointer_badge`):
/// the most a live capture is zoomed in; `None` when none is.
pub fn live_zoom() -> Option<f32> {
    let all = LIVE_ZOOMS.lock().unwrap_or_else(|e| e.into_inner());
    all.iter().filter(|z| z.2.elapsed() < Duration::from_millis(500) && z.1 > 1.02).map(|z| z.1).reduce(f32::max)
}

/// One step of a critically damped spring towards `target` (implicit, stable at any `dt`).
fn spring(x: f32, v: f32, target: f32, omega: f32, dt: f32) -> (f32, f32) {
    let f = 1.0 + 2.0 * dt * omega;
    let oo = omega * omega;
    let (hoo, hhoo) = (dt * oo, dt * dt * oo);
    let inv = 1.0 / (f + hhoo);
    ((f * x + dt * v + hhoo * target) * inv, (v + hoo * (target - x)) * inv)
}

/// A scene's `ScreenZoomPointer` as the pointer filter's settings.
pub fn look_settings(l: &lightair_model::ScreenZoomPointer) -> serde_json::Value {
    serde_json::json!({
        "draw": l.pointer_drawn && !l.pointer_hidden,
        // the capture's own pointer goes when Lightair draws one, or when there's to be none
        "hide_capture": l.pointer_drawn || l.pointer_hidden,
        "style": l.pointer_style,
        "size": l.pointer_size,
        "smoothing": l.pointer_smoothing,
        "tilt": l.pointer_tilt,
        "idle_s": l.pointer_idle_s,
        // clicks are drawn with the pointer
        "clicks": if l.pointer_drawn && !l.pointer_hidden { l.clicks } else { 0 },
        "arrow_only": false,
        "color": l.pointer_color,
        "zoom": l.zoom,
        "zoom_scale": l.zoom_scale,
        "zoom_hold_s": l.zoom_hold_s,
        "zoom_motion": l.zoom_motion,
        "zoom_typing": l.zoom_typing,
        "zoom_badge": l.zoom_badge,
    })
}

/// The side of the system pointer's picture ([`Pointer::picture`]), in pixels.
pub fn system_side() -> u32 {
    system_pointer::SIDE as u32
}

/// The pointer `source` draws, when it draws one: the screen look of the scene it's in (the one
/// on air first, as the reconciler puts it on the capture), else its own drawn pointer effect.
pub fn style_for(ws: &lightair_model::Workspace, source: &lightair_model::Source) -> Option<Style> {
    if is_screen(source) {
        let on_air = ws.program.and_then(|p| ws.scene(p));
        let look = on_air
            .into_iter()
            .chain(ws.scenes.iter())
            .filter_map(|scene| scene.screen.map(|look| (scene, look)))
            .find(|(scene, _)| scene.objects.iter().any(|o| o.source == Some(source.id)));
        if let Some((_, look)) = look {
            return Some(Style::from_json(&look_settings(&look)));
        }
    }
    source
        .effects
        .iter()
        .find(|e| e.enabled && (e.kind == KIND || e.kind == lightair_model::catalog::OLD_POINTER_KIND))
        .map(|e| Style::from_json(&e.settings))
}

/// What a document screen source captures, in global display points: its window where it is
/// now, or its display. `None`: the main display.
pub fn frame_of(source: &lightair_model::Source) -> Option<(f32, f32, f32, f32)> {
    use lightair_model::kinds;
    let s = &source.settings;
    let text = |k: &str| match &s[k] {
        serde_json::Value::String(t) => t.trim().to_string(),
        serde_json::Value::Number(n) => n.to_string(),
        _ => String::new(),
    };
    match s["mode"].as_i64().unwrap_or(kinds::DISPLAY) {
        kinds::WINDOW | kinds::GAME => input::window_frame(&text("window")),
        _ => input::display_frame(&text("display")),
    }
}

/// Whether `source` is a screen capture (the document's kind, or libobs's own kinds).
pub fn is_screen(source: &lightair_model::Source) -> bool {
    source.kind == lightair_model::kinds::SCREEN
        || matches!(
            source.kind.as_str(),
            "screen_capture" | "display_capture" | "monitor_capture" | "window_capture" | "game_capture"
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_spring_settles_without_overshoot() {
        let (mut x, mut v) = (0.0f32, 0.0f32);
        let mut max = 0.0f32;
        for _ in 0..240 {
            (x, v) = spring(x, v, 100.0, 20.0, 1.0 / 60.0);
            max = max.max(x);
        }
        assert!((x - 100.0).abs() < 0.5, "{x}");
        assert!(max <= 100.01, "critically damped: no overshoot ({max})");
    }

    #[test]
    fn the_zoom_keeps_the_pointer_in_view_and_the_view_on_the_capture() {
        let size = (1920.0, 1080.0);
        // in the middle, a small move doesn't move it; a big one brings the pointer back in
        let aim = (960.0, 540.0);
        assert_eq!(aim_at(aim, (1000.0, 560.0), 2.0, size), aim);
        let moved = aim_at(aim, (1500.0, 540.0), 2.0, size);
        let (x, _, w, _) = view_rect(moved, 2.0, size);
        assert!(1500.0 > x && 1500.0 < x + w, "the pointer is in the view: {moved:?}");
        // at the corner it stops at the edge
        let (x, y, w, h) = view_rect(aim_at(aim, (0.0, 0.0), 2.0, size), 2.0, size);
        assert_eq!((x, y, w, h), (0.0, 0.0, 960.0, 540.0));
        let (x, y, ..) = view_rect((1900.0, 1070.0), 3.0, size);
        assert_eq!((x, y), (1920.0 - 640.0, 1080.0 - 360.0));
    }

    #[test]
    fn the_zoom_goes_in_and_comes_back_out() {
        let size = (1920.0, 1080.0);
        let mut cam = Cam::new(size);
        let step = 1.0 / 60.0;
        cam_step(&mut cam, 2.0, (1600.0, 300.0), size, 0.5, step, true);
        for _ in 0..120 {
            cam_step(&mut cam, 2.0, (1600.0, 300.0), size, 0.5, step, false);
        }
        assert!((cam.scale - 2.0).abs() < 0.02, "in: {cam:?}");
        let (x, y, w, h) = view_rect(cam.center, cam.scale, size);
        assert!(1600.0 > x && 1600.0 < x + w && 300.0 > y && 300.0 < y + h, "looking at it: {cam:?}");
        for _ in 0..180 {
            cam_step(&mut cam, 1.0, (1600.0, 300.0), size, 0.5, step, false);
        }
        assert!((cam.scale - 1.0).abs() < 0.02, "out again: {cam:?}");
    }
}
