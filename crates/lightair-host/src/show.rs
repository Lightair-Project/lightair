//! The show as Lightair's compositors draw it: the document and the switch under way, published
//! by the engine thread each time they change, read by each canvas's compositor every frame; and
//! what each timer shows now.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock, OnceLock};
use std::time::Instant;

use lightair_model::{Id, TransitionSettings, Workspace};
use parking_lot::Mutex;

/// Set by the engine without libobs ([`crate::engine::Engine`]): Lightair's own compositor is
/// all there is, whatever `LIGHTAIR_ENGINE` says.
static ALWAYS_OWN: AtomicBool = AtomicBool::new(false);

/// Whether Lightair's own compositor draws the program (`LIGHTAIR_ENGINE=own`, or the engine
/// without libobs running). Read once.
pub fn own() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    ALWAYS_OWN.load(Ordering::Relaxed)
        || *ON.get_or_init(|| std::env::var("LIGHTAIR_ENGINE").is_ok_and(|v| v.trim().eq_ignore_ascii_case("own")))
}

/// The engine without libobs is starting: Lightair's own compositor, mixer and outputs from
/// now on. Call before anything reads [`own`].
pub fn set_own() {
    ALWAYS_OWN.store(true, Ordering::Relaxed);
}

/// Whether a source is a link from another computer (a media source with a `link_port`) that
/// Lightair's compositor receives itself (with the switch on): the reconciler then gives its
/// libobs media source no address, so only the compositor's receiver listens on the port.
pub fn link_received_by_compositor(source: &lightair_model::Source) -> bool {
    own()
        && source.kind == "ffmpeg_source"
        && source.settings["link_port"].as_u64().is_some_and(|p| u16::try_from(p).is_ok())
}

// ---------------------------------------------------------------- the document

/// A scene switch under way: what the compositor needs to draw it.
#[derive(Debug, Clone, Copy)]
pub struct Switch {
    pub from: Id,
    pub to: Id,
    pub settings: TransitionSettings,
    pub started: Instant,
}

impl Switch {
    /// How far through it is at `now` (0..1; 1 once it's over).
    pub fn progress(&self, now: Instant) -> f32 {
        let ms = now.saturating_duration_since(self.started).as_secs_f32() * 1000.0;
        (ms / self.settings.duration_ms.max(1) as f32).clamp(0.0, 1.0)
    }
}

/// The document as the engine last applied it.
pub struct Snapshot {
    pub workspace: Arc<Workspace>,
    /// The switch the last program change started (cleared by a cut).
    pub switch: Option<Switch>,
}

static SNAPSHOT: Mutex<Option<Arc<Snapshot>>> = Mutex::new(None);
static SWITCH: Mutex<Option<Switch>> = Mutex::new(None);

/// The document as the engine last applied it (`None` before the first change, or with the
/// switch off). The lock is held only to clone an `Arc`.
pub fn snapshot() -> Option<Arc<Snapshot>> {
    SNAPSHOT.lock().clone()
}

/// Engine thread, after applying a change: `workspace` is what libobs now shows.
pub fn publish(workspace: &Workspace) {
    if !own() {
        return;
    }
    let switch = *SWITCH.lock();
    *SNAPSHOT.lock() = Some(Arc::new(Snapshot { workspace: Arc::new(workspace.clone()), switch }));
}

/// Engine thread, when the program changes: the switch it starts (`None`: a cut).
pub fn set_switch(switch: Option<Switch>) {
    if own() {
        *SWITCH.lock() = switch;
    }
}

/// The show is let go of (another workspace, shutdown): no document, no switch.
pub fn clear() {
    *SWITCH.lock() = None;
    *SNAPSHOT.lock() = None;
}

// ---------------------------------------------------------------- timers

/// What each timer shows now (the engine's timers write it once a second, while the switch is
/// on), so the compositor can draw them itself.
static TIMER_TEXTS: LazyLock<Mutex<HashMap<Id, String>>> = LazyLock::new(Default::default);

/// The text the timer `source` shows now, once its first tick has made one.
pub fn timer_text(source: Id) -> Option<String> {
    TIMER_TEXTS.lock().get(&source).cloned()
}

/// The engine's timers: `source` now shows `text` (`None`: the timer is gone).
pub fn set_timer_text(source: Id, text: Option<&str>) {
    if !own() {
        return;
    }
    let mut texts = TIMER_TEXTS.lock();
    match text {
        Some(t) => {
            texts.insert(source, t.to_string());
        }
        None => {
            texts.remove(&source);
        }
    }
}

/// The engine's timers were all removed.
pub fn clear_timer_texts() {
    if own() {
        TIMER_TEXTS.lock().clear();
    }
}

/// The object being cropped (scene, canvas, object), shown whole as a faint ghost around its
/// crop, and whether the ghost is fading out (crop view closing).
static CROP: Mutex<Option<((Id, Id, Id), bool)>> = Mutex::new(None);

/// Engine thread: crop view for `target` (`None`: none), fading its ghost out or not.
pub fn set_crop_view(target: Option<(Id, Id, Id)>, fading: bool) {
    *CROP.lock() = target.map(|t| (t, fading));
}

/// The object in crop view, and whether its ghost is fading out. Any thread.
pub fn crop_view() -> Option<((Id, Id, Id), bool)> {
    *CROP.lock()
}
