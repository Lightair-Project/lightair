//! Where things pinned to an avatar go on the canvas (docs/plans/avatars.md, "Accessories and
//! pins"): an anchor's place in the avatar's picture ([`Spot`]) and the avatar's box on the
//! canvas ([`Placement`]), and a point of the picture on the canvas ([`on_canvas`]).

use std::collections::HashMap;

use glam::{Mat4, Vec3};
use lightair_avatar::pose::Camera;
use lightair_avatar::vrm::Model;
use lightair_model::Fit;

/// A point of an avatar something can be pinned to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Anchor {
    /// How the pin names it (a node's or mesh's name; `name#2` for the second of that name).
    pub key: String,
    /// What the picker shows ("Left hand"; a mesh's own name).
    pub label: String,
    /// Where the picker files it ("Head", "Left hand", a Live2D part's id, …).
    pub group: String,
}

/// An anchor in the avatar's picture: pixels, its turn (radians, clockwise as seen) and its size
/// (only ever compared with itself at rest).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Spot {
    pub x: f32,
    pub y: f32,
    pub angle: f32,
    pub scale: f32,
}

/// A box in canvas pixels: its centre, size and rotation (degrees clockwise).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Placement {
    pub cx: f32,
    pub cy: f32,
    pub w: f32,
    pub h: f32,
    pub rot: f32,
    pub fit: Fit,
}

impl Placement {
    pub fn of(l: &lightair_model::Layout) -> Placement {
        Placement {
            cx: l.x + l.width / 2.0,
            cy: l.y + l.height / 2.0,
            w: l.width,
            h: l.height,
            rot: l.rotation,
            fit: l.fit,
        }
    }
}

/// A point of the avatar's picture (`w`×`h`) on the canvas, where its item is.
pub fn on_canvas(host: &Placement, w: f32, h: f32, x: f32, y: f32) -> (f32, f32) {
    let (kx, ky) = match host.fit {
        Fit::Fit => {
            let k = (host.w / w).min(host.h / h);
            (k, k)
        }
        Fit::Fill => {
            let k = (host.w / w).max(host.h / h);
            (k, k)
        }
        Fit::Stretch => (host.w / w, host.h / h),
        Fit::Original => (1.0, 1.0),
    };
    let (lx, ly) = ((x - w / 2.0) * kx, (y - h / 2.0) * ky);
    let (s, c) = host.rot.to_radians().sin_cos();
    (host.cx + lx * c - ly * s, host.cy + lx * s + ly * c)
}

/// Keys from names, unique: the second "Hair" is `Hair#2`.
pub fn keyed(names: impl Iterator<Item = String>) -> Vec<String> {
    let mut seen: HashMap<String, usize> = HashMap::new();
    names
        .map(|n| {
            let k = seen.entry(n.clone()).or_insert(0);
            *k += 1;
            if *k == 1 { n } else { format!("{n}#{k}") }
        })
        .collect()
}

/// "leftUpperArm" → "Left upper arm".
fn words(camel: &str) -> String {
    let mut out = String::new();
    for (i, c) in camel.chars().enumerate() {
        if i == 0 {
            out.extend(c.to_uppercase());
        } else if c.is_uppercase() {
            out.push(' ');
            out.extend(c.to_lowercase());
        } else {
            out.push(c);
        }
    }
    out
}

/// Where a humanoid bone is filed.
fn humanoid_group(name: &str) -> &'static str {
    let left = name.starts_with("left");
    let right = name.starts_with("right");
    let finger = ["Thumb", "Index", "Middle", "Ring", "Little", "Hand"].iter().any(|f| name.contains(f));
    if ["head", "neck", "jaw", "leftEye", "rightEye"].contains(&name) {
        "Head"
    } else if ["hips", "spine", "chest", "upperChest"].contains(&name) {
        "Body"
    } else if finger && left {
        "Left hand"
    } else if finger && right {
        "Right hand"
    } else if name.contains("Leg") || name.contains("Foot") || name.contains("Toes") {
        "Legs"
    } else if left {
        "Left arm"
    } else if right {
        "Right arm"
    } else {
        "Body"
    }
}

const VRM_GROUPS: [&str; 9] =
    ["Head", "Body", "Left arm", "Left hand", "Right arm", "Right hand", "Legs", "Other bones", ""];

/// A VRM model's anchors (its bones), with each one's node.
pub fn vrm_anchors(m: &Model) -> Vec<(Anchor, usize)> {
    let humanoid: HashMap<usize, &str> = m.humanoid.iter().map(|(name, &n)| (n, name.as_str())).collect();
    let joints: std::collections::HashSet<usize> = m.skins.iter().flat_map(|s| s.joints.iter().copied()).collect();
    let bones: Vec<usize> = (0..m.nodes.len()).filter(|n| humanoid.contains_key(n) || joints.contains(n)).collect();
    let keys = keyed(bones.iter().map(|&n| m.nodes[n].name.clone()));
    let mut out: Vec<(Anchor, usize)> = bones
        .iter()
        .zip(keys)
        .map(|(&n, key)| {
            let (label, group) = match humanoid.get(&n) {
                Some(h) => (words(h), humanoid_group(h).to_string()),
                None => (m.nodes[n].name.clone(), "Other bones".to_string()),
            };
            (Anchor { key, label, group }, n)
        })
        .collect();
    let rank = |g: &str| VRM_GROUPS.iter().position(|x| *x == g).unwrap_or(VRM_GROUPS.len());
    out.sort_by_key(|(a, n)| (rank(&a.group), *n));
    out
}

/// The camera's view and projection, as the avatar is drawn.
pub fn view_projection(cam: &Camera, aspect: f32) -> Mat4 {
    Mat4::perspective_rh_gl(cam.fovy.to_radians(), aspect, cam.near, cam.far) * cam.view()
}

fn project(p: Vec3, vp: &Mat4, w: f32, h: f32) -> Option<(f32, f32, f32)> {
    let c = *vp * p.extend(1.0);
    (c.w > 1e-5).then(|| ((c.x / c.w * 0.5 + 0.5) * w, (0.5 - c.y / c.w * 0.5) * h, c.w))
}

/// Which of a bone's axes lies flattest across the picture at rest (its turn is read from that
/// one): the one pointing least toward the camera, which always looks along the model's depth, so
/// the choice doesn't depend on framing.
pub fn best_axis(rest_world: &Mat4) -> usize {
    (0..3)
        .min_by(|a, b| {
            let z = |i: usize| {
                [rest_world.x_axis, rest_world.y_axis, rest_world.z_axis][i].truncate().normalize_or_zero().z.abs()
            };
            z(*a).total_cmp(&z(*b))
        })
        .unwrap_or(1)
}

/// A bone in the picture.
pub fn bone_spot(world: &Mat4, vp: &Mat4, w: f32, h: f32, axis: usize) -> Option<Spot> {
    let o = world.w_axis.truncate();
    let a = [world.x_axis, world.y_axis, world.z_axis][axis.min(2)].truncate().normalize_or_zero() * 0.1;
    let (x, y, depth) = project(o, vp, w, h)?;
    let (x2, y2, _) = project(o + a, vp, w, h)?;
    Some(Spot { x, y, angle: (y2 - y).atan2(x2 - x), scale: 1.0 / depth })
}
