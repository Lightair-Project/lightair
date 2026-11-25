//! The compositors' frames on their way to Lightair's outputs: each canvas's shape as its outputs
//! take it, and scenes the compositor draws for an output alone (the virtual camera's one scene).

use std::collections::HashMap;

use lightair_model::{Canvas, Id};
use parking_lot::Mutex;

/// A canvas's size and frame rate as its outputs take them (even sizes).
pub fn shape_of(canvas: &Canvas) -> (u32, u32, u32, u32) {
    (canvas.width.max(2) & !1, canvas.height.max(2) & !1, canvas.fps_num.max(1), canvas.fps_den.max(1))
}

static OUTPUT_SHAPES: Mutex<Option<HashMap<Id, (u32, u32, u32, u32)>>> = Mutex::new(None);

/// The shape of each canvas's output as it is now (an output outlives its canvas in the
/// document while another workspace opens).
pub fn set_output_shapes(shapes: Vec<(Id, (u32, u32, u32, u32))>) {
    *OUTPUT_SHAPES.lock() = Some(shapes.into_iter().collect());
}

/// `canvas`'s output's shape, when it has one.
pub fn output_shape(canvas: Id) -> Option<(u32, u32, u32, u32)> {
    OUTPUT_SHAPES.lock().as_ref()?.get(&canvas).copied()
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
