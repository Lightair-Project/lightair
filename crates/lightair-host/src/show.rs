//! The show as Lightair's compositors draw it: the document and the switch under way, published
//! by the engine thread each time they change, read by each canvas's compositor every frame; and
//! what each timer shows now.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock};
use std::time::Instant;

use lightair_model::{Id, TransitionSettings, Workspace};
use parking_lot::Mutex;

/// Set when the engine starts ([`crate::engine::Engine`]).
static ALWAYS_OWN: AtomicBool = AtomicBool::new(false);

/// Whether the engine runs, so Lightair's own compositor draws the program.
pub fn own() -> bool {
    ALWAYS_OWN.load(Ordering::Relaxed)
}

/// The engine is starting: Lightair's own compositor, mixer and outputs from now on. Call
/// before anything reads [`own`].
pub fn set_own() {
    ALWAYS_OWN.store(true, Ordering::Relaxed);
}

/// Whether a source is a link from another computer (a media source with a `link_port`) that
/// Lightair's compositor receives itself.
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

/// Engine thread, after applying a change: `workspace` is what the show now is.
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

/// Effects switched off live, leaving the document alone (the eyedropper picking a colour
/// under a key): (source, effect; `lightair_model::catalog::STUDIO_ID` for its looks).
static LIVE_OFF: Mutex<Vec<(Id, Id)>> = Mutex::new(Vec::new());

/// Switches `source`'s effect `filter` off live (`enabled` false), or back as the document has it.
pub fn set_live(source: Id, filter: Id, enabled: bool) {
    let mut off = LIVE_OFF.lock();
    off.retain(|e| *e != (source, filter));
    if !enabled {
        off.push((source, filter));
    }
}

/// The effects switched off live. Any thread.
pub fn live_off() -> Vec<(Id, Id)> {
    LIVE_OFF.lock().clone()
}

/// The sources each canvas's compositor shows now (the program, scenes in previews, outputs'
/// scenes), and how many times that has changed.
static SHOWN: LazyLock<Mutex<HashMap<Id, std::collections::HashSet<Id>>>> = LazyLock::new(Default::default);
static SHOWN_CHANGES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// The compositor of `canvas` shows `sources` now (empty: nothing). The compositor's thread.
pub fn set_shown(canvas: Id, sources: &std::collections::HashSet<Id>) {
    let mut all = SHOWN.lock();
    if all.get(&canvas).map_or(sources.is_empty(), |s| s == sources) {
        return;
    }
    if sources.is_empty() {
        all.remove(&canvas);
    } else {
        all.insert(canvas, sources.clone());
    }
    SHOWN_CHANGES.fetch_add(1, Ordering::Relaxed);
}

/// Every source a compositor shows now. Any thread.
pub fn shown() -> std::collections::HashSet<Id> {
    SHOWN.lock().values().flatten().copied().collect()
}

/// Counts the changes of what's shown: a different value, something to look at again.
pub fn shown_changes() -> u64 {
    SHOWN_CHANGES.load(Ordering::Relaxed)
}
