//! The compositors' frames on their way to Lightair's outputs: each canvas's shape as its outputs
//! take it, and scenes the compositor draws for an output alone (the virtual camera's one scene).

use std::collections::HashMap;

use lightair_model::{Canvas, Id};
use parking_lot::Mutex;

/// A canvas's size and frame rate as its outputs take them (even sizes).
pub fn shape_of(canvas: &Canvas) -> (u32, u32, u32, u32) {
    (canvas.width.max(2) & !1, canvas.height.max(2) & !1, canvas.fps_num.max(1), canvas.fps_den.max(1))
}

/// Width, height and frame rate (numerator, denominator).
type Shape = (u32, u32, u32, u32);

static OUTPUT_SHAPES: Mutex<Option<HashMap<Id, Shape>>> = Mutex::new(None);

/// The shape of each canvas's output as it is now (an output outlives its canvas in the
/// document while another workspace opens).
pub fn set_output_shapes(shapes: Vec<(Id, (u32, u32, u32, u32))>) {
    *OUTPUT_SHAPES.lock() = Some(shapes.into_iter().collect());
}

/// Shapes of feeds that aren't canvases (a Studio take's sources), by their key.
static FEED_SHAPES: Mutex<Option<HashMap<Id, Shape>>> = Mutex::new(None);

/// `canvas`'s output's shape, when it has one (or a feed's: [`set_feed_shape`]).
pub fn output_shape(canvas: Id) -> Option<(u32, u32, u32, u32)> {
    if let Some(s) = FEED_SHAPES.lock().as_ref().and_then(|f| f.get(&canvas)) {
        return Some(*s);
    }
    OUTPUT_SHAPES.lock().as_ref()?.get(&canvas).copied()
}

/// The shape of a feed of frames that isn't a canvas's (`None`: it's gone).
pub fn set_feed_shape(key: Id, shape: Option<Shape>) {
    let mut f = FEED_SHAPES.lock();
    let f = f.get_or_insert_with(HashMap::new);
    match shape {
        Some(s) => f.insert(key, s),
        None => f.remove(&key),
    };
}

// ------------------------------------------------------------------ one scene for an output

/// A scene the compositor draws on its own for an output (the virtual camera's "one scene"), at
/// its canvas's size, whatever is on air. Its frames go to the outputs under `key` (a feed of
/// its own, not a canvas's).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SceneTarget {
    pub key: Id,
    pub scene: Id,
    /// The canvas the scene is laid out in (whose compositor draws it).
    pub canvas: Id,
    /// The most frames a second it's drawn at (numerator, denominator).
    pub fps: (u32, u32),
}

static SCENE_TARGETS: Mutex<Vec<SceneTarget>> = Mutex::new(Vec::new());

/// The compositor of `target.canvas` draws `target.scene` for `target.key` from now on.
pub fn add_scene_target(target: SceneTarget) {
    let mut t = SCENE_TARGETS.lock();
    t.retain(|x| x.key != target.key);
    t.push(target);
}

/// Stops drawing the scene for `key`.
pub fn remove_scene_target(key: Id) {
    SCENE_TARGETS.lock().retain(|t| t.key != key);
}

/// The scenes `canvas`'s compositor draws for outputs. Any thread.
pub fn scene_targets(canvas: Id) -> Vec<SceneTarget> {
    SCENE_TARGETS.lock().iter().filter(|t| t.canvas == canvas).copied().collect()
}

/// Hands `frame` (BGRA, IOSurface-backed, complete on the GPU) over as the newest picture of the
/// scene drawn for `key`. Any thread.
#[cfg(target_os = "macos")]
pub fn publish_scene(key: Id, frame: &crate::own_outputs::Buffer) {
    crate::own_outputs::frame(key, frame);
}

// ------------------------------------------------------------------ each canvas's program

#[cfg(target_os = "macos")]
pub use frames::{newest, publish, set_libobs_in_use, withdraw};

#[cfg(target_os = "macos")]
mod frames {
    use std::collections::HashMap;
    use std::sync::OnceLock;
    use std::time::{Duration, Instant};

    use lightair_model::Id;
    use parking_lot::Mutex;

    use crate::buffers::Buffer;

    /// A compositor frame older than this is stale ([`newest`] with `fresh`).
    const STALE: Duration = Duration::from_millis(500);

    /// A finished frame and when it was handed over.
    struct Published {
        buffer: Buffer,
        at: Instant,
    }

    /// The newest finished program picture of each canvas.
    struct Programs(Option<HashMap<Id, Published>>);

    // SAFETY: CoreVideo's buffers are reference counted thread-safely, and a finished picture
    // isn't written while it's handed over.
    unsafe impl Send for Programs {}
    unsafe impl Sync for Programs {}

    static PROGRAMS: Mutex<Programs> = Mutex::new(Programs(None));

    /// Hands `frame` (BGRA, IOSurface-backed, complete on the GPU) over as `canvas`'s newest
    /// program picture, to Lightair's outputs and anything drawing it. Any thread.
    pub fn publish(canvas: Id, frame: &Buffer) {
        PROGRAMS
            .lock()
            .0
            .get_or_insert_with(HashMap::new)
            .insert(canvas, Published { buffer: frame.clone(), at: Instant::now() });
        crate::own_outputs::frame(canvas, frame);
    }

    /// Forgets `canvas`'s picture (its compositor stopped).
    pub fn withdraw(canvas: Id) {
        if let Some(p) = PROGRAMS.lock().0.as_mut() {
            p.remove(&canvas);
        }
    }

    /// `canvas`'s newest picture; with `fresh`, only one handed over in the last half second.
    pub fn newest(canvas: Id, fresh: bool) -> Option<Buffer> {
        let programs = PROGRAMS.lock();
        let p = programs.0.as_ref()?.get(&canvas)?;
        (!fresh || p.at.elapsed() < STALE).then(|| p.buffer.clone())
    }

    static LIBOBS_IN_USE: OnceLock<fn(Id) -> bool> = OnceLock::new();

    /// Whether outputs libobs still makes take a canvas (installed by libobs's side, once).
    pub fn set_libobs_in_use(f: fn(Id) -> bool) {
        let _ = LIBOBS_IN_USE.set(f);
    }

    /// Whether something records or streams `canvas`'s program: Lightair's own encoders, or
    /// libobs's outputs while there are any. Any thread.
    pub fn in_use(canvas: Id) -> bool {
        crate::own_outputs::uses(canvas) || LIBOBS_IN_USE.get().is_some_and(|f| f(canvas))
    }
}

#[cfg(target_os = "macos")]
pub use frames::in_use;

/// The primary canvas (the document's first), as the show has it now.
pub fn primary_canvas() -> Option<Id> {
    crate::show::snapshot().and_then(|s| s.workspace.canvases.first().map(|c| c.id))
}
