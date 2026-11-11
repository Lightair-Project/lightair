//! The Move transition (docs/07-scenes-layers-canvas.md, "Move transition details").
//!
//! Switching from scene A to scene B builds a temporary scene in each format's canvas: B's
//! layers that match a layer in A (same input first, then same name) start at A's layout and
//! glide to B's; layers only in B fade in; layers only in A fade out. A libobs tick callback
//! drives the glide every frame; when it finishes, each canvas cuts to B (which now looks
//! identical) and the temporary scenes are released on the engine thread.

use std::time::Instant;

use libobs_sys as sys;
use lightair_core::{Id, Layout, Scene as DocScene};

use crate::obs::{Scene, Source};

pub(crate) struct Tween {
    pub item: *mut sys::obs_sceneitem_t,
    pub from: Layout,
    pub to: Layout,
}

pub(crate) struct FormatMove {
    /// Owned to keep the temporary scene alive until the Move is cleaned up.
    #[allow(dead_code)]
    pub temp: Scene,
    pub tweens: Vec<Tween>,
    /// The format's transition and the scene to land on.
    pub transition: *mut sys::obs_source_t,
    pub dest: *mut sys::obs_source_t,
    /// Per-item fade transitions, kept alive for the animation.
    #[allow(dead_code)]
    pub fades: Vec<Source>,
}

pub(crate) struct MoveAnim {
    pub formats: Vec<FormatMove>,
    pub start: Instant,
    pub duration: f32,
    pub done: bool,
}

unsafe impl Send for MoveAnim {}

/// Which layer in `from` a layer of `to` continues: same input, else same name.
pub(crate) fn match_layer<'a>(from: &'a DocScene, layer: &lightair_core::Layer) -> Option<&'a lightair_core::Layer> {
    from.layers.iter().find(|l| l.input == layer.input).or_else(|| from.layers.iter().find(|l| l.name == layer.name))
}

fn ease_in_out(t: f32) -> f32 {
    if t < 0.5 { 4.0 * t * t * t } else { 1.0 - (-2.0 * t + 2.0).powi(3) / 2.0 }
}

fn lerp(a: f32, b: f32, t: f32) -> f32 {
    a + (b - a) * t
}

pub(crate) fn lerp_layout(a: &Layout, b: &Layout, t: f32) -> Layout {
    let crop = |x: u32, y: u32| lerp(x as f32, y as f32, t).round() as u32;
    Layout {
        x: lerp(a.x, b.x, t),
        y: lerp(a.y, b.y, t),
        width: lerp(a.width, b.width, t),
        height: lerp(a.height, b.height, t),
        rotation: lerp(a.rotation, b.rotation, t),
        crop: lightair_core::Crop {
            left: crop(a.crop.left, b.crop.left),
            top: crop(a.crop.top, b.crop.top),
            right: crop(a.crop.right, b.crop.right),
            bottom: crop(a.crop.bottom, b.crop.bottom),
        },
        ..*b
    }
}

impl MoveAnim {
    /// Advances the animation; returns true when it has finished (and landed on B).
    pub(crate) fn step(&mut self) -> bool {
        if self.done {
            return true;
        }
        let t = (self.start.elapsed().as_secs_f32() / self.duration.max(0.001)).min(1.0);
        let e = ease_in_out(t);
        for f in &self.formats {
            for tw in &f.tweens {
                unsafe { crate::reconcile::apply_layout(tw.item, &lerp_layout(&tw.from, &tw.to, e)) };
            }
        }
        if t >= 1.0 {
            self.finish();
        }
        self.done
    }

    /// Lands every canvas on B immediately.
    pub(crate) fn finish(&mut self) {
        if self.done {
            return;
        }
        for f in &self.formats {
            unsafe { sys::obs_transition_set(f.transition, f.dest) };
        }
        self.done = true;
    }
}

/// Fade used for layers entering or leaving during a Move.
pub(crate) fn item_fade(item: *mut sys::obs_sceneitem_t, show: bool, ms: u32, keep: &mut Vec<Source>) {
    if let Some(fade) = Source::create_private("fade_transition", &format!("move-fade-{}", Id::new()), None) {
        unsafe {
            sys::obs_sceneitem_set_transition(item, show, fade.as_ptr());
            sys::obs_sceneitem_set_transition_duration(item, show, ms);
        }
        keep.push(fade);
    }
}
