//! Things pinned to an avatar (docs/plans/avatars.md, "Accessories and pins"): an object whose
//! parent is an avatar can follow one of the avatar's anchors, which is what each kind of model
//! really has:
//!
//! - **VRM**: its bones (the skeleton's nodes), the humanoid ones grouped as head, body, arms,
//!   hands and legs, the rest (hair, clothes, the artist's own) as other bones;
//! - **Live2D**: its art meshes, grouped by the part each sits in;
//! - **Inochi2D** and **Umamo**: their parts (Inochi2D) or art meshes (Umamo).
//!
//! A 2D model's meshes whose names say face or head come first, as suggestions.
//!
//! Following: each frame, where the anchor is in the avatar's picture now against where it is at
//! rest (the model's idle pose, nothing tracked) gives a move, a turn and a change of size. The
//! pinned object's own layout is where it sits with the avatar at rest, so it keeps its offset,
//! rotation and size relative to the anchor. A bone's turn is taken from one of its axes as seen
//! (the one that shows longest at rest), its size from its distance from the camera; a mesh's
//! from its points about their middle.
//!
//! The reconciler hands each avatar source its pinned scene items ([`set_pins`]); the avatar's
//! own tick moves them after posing ([`apply`]), so they move in the same frame as the avatar.

use std::collections::HashMap;

use parking_lot::Mutex;

pub use lightair_host::avatar_pins::{Placement, Spot, on_canvas};

use crate::avatar2d::Flat;
use crate::sys;

pub use lightair_host::avatar_pins::Anchor;

use lightair_host::avatar_pins::mesh_anchors;
pub(crate) use lightair_host::avatar_pins::vrm_anchors;

/// A flat model's meshes' names and groups, in its own order.
fn flat_names(f: &Flat) -> Vec<(String, String)> {
    match f {
        Flat::Live2d(l) => l
            .puppet
            .mesh_ids()
            .into_iter()
            // (a part's id as its name: "PartRobe" → "Robe")
            .map(|(id, part)| {
                let name = part.strip_prefix("Part").filter(|r| !r.is_empty()).unwrap_or(&part).to_string();
                (id, if name.is_empty() { "Art meshes".into() } else { name })
            })
            .collect(),
        Flat::Inochi2d(l) => l.puppet.parts().iter().map(|p| (p.name.clone(), "Parts".into())).collect(),
        Flat::Uma(l) => l.puppet.drawables().iter().map(|d| (d.name.clone(), "Art meshes".into())).collect(),
        Flat::Png(l) => l.anchor_names(),
    }
}

/// A flat model's anchors (its meshes), each with its mesh's index; the face and head ones first,
/// as suggestions (listed again in their own group).
pub(crate) fn flat_anchors(f: &Flat) -> Vec<(Anchor, usize)> {
    mesh_anchors(flat_names(f))
}

/// A flat model's mesh, posed: its points (model units, y up).
pub(crate) fn flat_points(f: &Flat, i: usize) -> Vec<[f32; 2]> {
    match f {
        Flat::Live2d(l) => {
            l.puppet.meshes().get(i).map(|m| m.vertices().iter().map(|v| v.position()).collect()).unwrap_or_default()
        }
        Flat::Inochi2d(l) => l.puppet.positions().get(i).cloned().unwrap_or_default(),
        Flat::Uma(l) => {
            l.puppet.drawables().get(i).filter(|d| d.posed).map(|d| d.positions.clone()).unwrap_or_default()
        }
        Flat::Png(l) => l.anchor_points(i),
    }
}

/// The frame a flat model is drawn in (left, right, top, bottom; model units, y up).
pub(crate) fn flat_frame(f: &Flat, framing: lightair_avatar::Framing, aspect: f32, zoom: f32) -> (f32, f32, f32, f32) {
    match f {
        Flat::Live2d(l) => l.puppet.frame(framing, aspect, zoom),
        Flat::Inochi2d(l) => l.puppet.frame(framing, aspect, zoom),
        Flat::Uma(l) => l.puppet.frame(framing, aspect, zoom),
        Flat::Png(l) => l.frame(framing, aspect, zoom),
    }
}

pub(crate) use lightair_host::avatar_pins::{best_axis, bone_spot, view_projection};

pub(crate) use lightair_host::avatar_pins::{MeshRest, mesh_rest, mesh_spot, rest_spot};

// ------------------------------------------------------------------------------------- pins

/// A scene item following an anchor: the item (a reference held), the anchor's key, where the
/// item sits with the avatar at rest, and where the avatar's own item is.
pub struct PinTarget {
    pub item: *mut sys::obs_sceneitem_t,
    pub anchor: String,
    /// Where the anchor was at rest in the avatar's picture when the object was pinned: its offset
    /// is from there, whatever the framing or zoom since (`None`: from where it's at rest now).
    pub pinned_at: Option<Spot>,
    pub base: Placement,
    pub host: Placement,
}

// SAFETY: scene items are reference counted and thread-safe in libobs.
unsafe impl Send for PinTarget {}

static PINS: Mutex<Option<HashMap<usize, Vec<PinTarget>>>> = Mutex::new(None);

/// The pinned items, by avatar source (each item with a reference added by the caller, let go
/// here when replaced).
pub fn set_pins(pins: HashMap<usize, Vec<PinTarget>>) {
    let old = PINS.lock().replace(pins);
    for t in old.into_iter().flat_map(|m| m.into_values()).flatten() {
        unsafe { sys::obs_sceneitem_release(t.item) };
    }
}

/// The anchors pinned to on this avatar source now.
pub(crate) fn anchors_pinned(source: usize) -> Vec<String> {
    let pins = PINS.lock();
    let mut keys: Vec<String> = pins
        .as_ref()
        .and_then(|m| m.get(&source))
        .map(|v| v.iter().map(|t| t.anchor.clone()).collect())
        .unwrap_or_default();
    keys.sort();
    keys.dedup();
    keys
}

/// Moves this avatar's pinned items: `spot` gives an anchor's place at rest and now, in the
/// avatar's picture (`w`×`h`). (The avatar's tick, after posing.)
pub(crate) fn apply(source: usize, w: f32, h: f32, spot: impl Fn(&str) -> Option<(Spot, Spot)>) {
    let pins = PINS.lock();
    let Some(targets) = pins.as_ref().and_then(|m| m.get(&source)) else { return };
    let mut cache: HashMap<&str, Option<(Spot, Spot)>> = HashMap::new();
    for t in targets {
        let Some((rest, now)) = *cache.entry(t.anchor.as_str()).or_insert_with(|| spot(&t.anchor)) else { continue };
        let rest = t.pinned_at.unwrap_or(rest);
        let p0 = on_canvas(&t.host, w, h, rest.x, rest.y);
        let p1 = on_canvas(&t.host, w, h, now.x, now.y);
        let turn = now.angle - rest.angle;
        let ratio = (now.scale / rest.scale).clamp(0.2, 5.0);
        let (s, c) = turn.sin_cos();
        let (dx, dy) = ((t.base.cx - p0.0) * ratio, (t.base.cy - p0.1) * ratio);
        let (x, y) = (p1.0 + dx * c - dy * s, p1.1 + dx * s + dy * c);
        unsafe {
            sys::obs_sceneitem_set_pos(t.item, &crate::obs::vec2(x, y));
            sys::obs_sceneitem_set_rot(t.item, t.base.rot + turn.to_degrees());
            if t.base.fit == lightair_model::Fit::Original {
                sys::obs_sceneitem_set_scale(t.item, &crate::obs::vec2(ratio, ratio));
            } else {
                sys::obs_sceneitem_set_bounds(t.item, &crate::obs::vec2(t.base.w * ratio, t.base.h * ratio));
            }
        }
    }
}
