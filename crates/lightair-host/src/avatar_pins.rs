//! Where things pinned to an avatar go on the canvas (docs/plans/avatars.md, "Accessories and
//! pins"): an anchor's place in the avatar's picture ([`Spot`]) and the avatar's box on the
//! canvas ([`Placement`]), and a point of the picture on the canvas ([`on_canvas`]).
//!
//! What a model can be pinned to: a VRM's bones ([`vrm_anchors`]); a flat model's meshes
//! ([`flat_anchors`]: a Live2D model's art meshes by part, an Inochi2D puppet's parts, an Umamo
//! puppet's art meshes, a PNGtuber's anchors and layers), the face and head ones first as
//! suggestions. [`AtRest`] keeps a model's anchors as they are at rest (its idle pose, nothing
//! tracked), to give each one's place at rest and as the model is posed now.

use std::collections::HashMap;

use glam::{Mat4, Vec3};
use lightair_avatar::pose::Camera;
use lightair_avatar::source::{Flat, Pngtuber, Posable};
use lightair_avatar::vrm::Model;
use lightair_avatar::{Avatar, Controls, Face, Framing};
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

// ------------------------------------------------------------------------------- flat models

/// A flat model's meshes' names and groups, in its own order.
fn flat_names(f: &Flat) -> Vec<(String, String)> {
    match f {
        Flat::Live2d(p) => p
            .mesh_ids()
            .into_iter()
            // (a part's id as its name: "PartRobe" → "Robe")
            .map(|(id, part)| {
                let name = part.strip_prefix("Part").filter(|r| !r.is_empty()).unwrap_or(&part).to_string();
                (id, if name.is_empty() { "Art meshes".into() } else { name })
            })
            .collect(),
        Flat::Inochi2d(p) => p.parts().iter().map(|x| (x.name.clone(), "Parts".into())).collect(),
        Flat::Uma(p) => p.drawables().iter().map(|d| (d.name.clone(), "Art meshes".into())).collect(),
        Flat::Png(p) => png_names(p),
    }
}

/// A PNGtuber's anchors then its layers, by name.
fn png_names(p: &Pngtuber) -> Vec<(String, String)> {
    let a = p.puppet.avatar();
    a.anchors
        .iter()
        .map(|x| (x.name.clone(), "Anchors".to_string()))
        .chain(a.layers.iter().map(|l| (l.name.clone(), "Layers".to_string())))
        .collect()
}

/// A PNGtuber's anchor's or layer's points this frame (canvas pixels, y up): a little cross at
/// an anchor, a layer's corners.
fn png_points(p: &Pngtuber, i: usize) -> Vec<[f32; 2]> {
    let a = p.puppet.avatar();
    let h = a.height as f32;
    let up = |q: [f32; 2]| [q[0], h - q[1]];
    let n = a.anchors.len();
    if i < n {
        let x = &a.anchors[i];
        let pose = x
            .layer
            .as_ref()
            .and_then(|id| a.layers.iter().position(|l| l.id == *id))
            .or((!a.layers.is_empty()).then_some(0))
            .and_then(|j| p.puppet.pose().layers.get(j).copied());
        let place = |q: [f32; 2]| pose.map_or(q, |l| l.place(q));
        vec![up(place([x.x, x.y])), up(place([x.x + 12.0, x.y])), up(place([x.x, x.y + 12.0]))]
    } else {
        p.puppet.layer_corners(i - n).map(|c| c.iter().map(|q| up(*q)).collect()).unwrap_or_default()
    }
}

/// Anchors from a flat model's meshes' names and groups, each with its mesh's index; the face and
/// head ones first, as suggestions (listed again in their own group).
pub fn mesh_anchors(names: Vec<(String, String)>) -> Vec<(Anchor, usize)> {
    let keys = keyed(names.iter().map(|(n, _)| n.clone()));
    let all: Vec<(Anchor, usize)> = names
        .into_iter()
        .zip(keys)
        .enumerate()
        .map(|(i, ((name, group), key))| (Anchor { key, label: name, group }, i))
        .collect();
    let suggested: Vec<(Anchor, usize)> = all
        .iter()
        .filter(|(a, _)| {
            let l = a.label.to_lowercase();
            ["face", "head", "顔", "頭"].iter().any(|w| l.contains(w))
        })
        .take(6)
        .map(|(a, i)| (Anchor { group: "Suggested".into(), ..a.clone() }, *i))
        .collect();
    suggested.into_iter().chain(all).collect()
}

/// A flat model's anchors (its meshes), each with its mesh's index.
pub fn flat_anchors(f: &Flat) -> Vec<(Anchor, usize)> {
    mesh_anchors(flat_names(f))
}

/// A flat model's mesh, as posed now: its points (model units, y up).
pub fn flat_points(f: &Flat, i: usize) -> Vec<[f32; 2]> {
    match f {
        Flat::Live2d(p) => {
            p.meshes().get(i).map(|m| m.vertices().iter().map(|v| v.position()).collect()).unwrap_or_default()
        }
        Flat::Inochi2d(p) => p.positions().get(i).cloned().unwrap_or_default(),
        Flat::Uma(p) => p.drawables().get(i).filter(|d| d.posed).map(|d| d.positions.clone()).unwrap_or_default(),
        Flat::Png(p) => png_points(p, i),
    }
}

/// A mesh at rest (the model's idle pose): its middle, the point farthest from it (its turn is
/// read from that one), and how far its points lie from the middle (model units).
#[derive(Clone, Copy, Debug)]
pub struct MeshRest {
    pub far: usize,
    centre: [f32; 2],
    far_point: [f32; 2],
    rms: f32,
}

pub fn mesh_rest(points: &[[f32; 2]]) -> MeshRest {
    let c = middle(points);
    let far = points.iter().enumerate().max_by(|a, b| dist(*a.1, c).total_cmp(&dist(*b.1, c))).map_or(0, |(i, _)| i);
    let rms = (points.iter().map(|p| dist(*p, c).powi(2)).sum::<f32>() / points.len().max(1) as f32).sqrt();
    MeshRest { far, centre: c, far_point: points.get(far).copied().unwrap_or(c), rms }
}

/// A mesh at rest in the picture, from the frame it's drawn in (`w`×`h` pixels).
pub fn rest_spot(r: &MeshRest, frame: (f32, f32, f32, f32), w: f32, h: f32) -> Spot {
    let (l, rt, t, b) = frame;
    let px = |p: [f32; 2]| [(p[0] - l) / (rt - l) * w, (t - p[1]) / (t - b) * h];
    let (c, f) = (px(r.centre), px(r.far_point));
    Spot { x: c[0], y: c[1], angle: (f[1] - c[1]).atan2(f[0] - c[0]), scale: (r.rms * w / (rt - l)).abs().max(1e-3) }
}

fn middle(points: &[[f32; 2]]) -> [f32; 2] {
    let n = points.len().max(1) as f32;
    let s = points.iter().fold([0.0, 0.0], |a, p| [a[0] + p[0], a[1] + p[1]]);
    [s[0] / n, s[1] / n]
}

fn dist(a: [f32; 2], b: [f32; 2]) -> f32 {
    (a[0] - b[0]).hypot(a[1] - b[1])
}

/// A mesh in the picture, from its points and the frame it's drawn in (`w`×`h` pixels).
pub fn mesh_spot(points: &[[f32; 2]], far: usize, frame: (f32, f32, f32, f32), w: f32, h: f32) -> Option<Spot> {
    if points.is_empty() {
        return None;
    }
    let (l, r, t, b) = frame;
    let px = |p: [f32; 2]| [(p[0] - l) / (r - l) * w, (t - p[1]) / (t - b) * h];
    let pts: Vec<[f32; 2]> = points.iter().map(|p| px(*p)).collect();
    let c = middle(&pts);
    let f = pts.get(far).copied().unwrap_or(c);
    let rms = (pts.iter().map(|p| dist(*p, c).powi(2)).sum::<f32>() / pts.len() as f32).sqrt();
    Some(Spot { x: c[0], y: c[1], angle: (f[1] - c[1]).atan2(f[0] - c[0]), scale: rms.max(1e-3) })
}

// --------------------------------------------------------------------------------- at rest

const FRAMINGS: [Framing; 4] = [Framing::Head, Framing::Bust, Framing::Half, Framing::Full];

/// A model's anchors at rest (its idle pose, nothing tracked).
pub enum AtRest {
    /// A VRM posed at rest (sharing the model), and each anchor's node.
    Vrm { avatar: Box<Avatar>, anchors: HashMap<String, usize> },
    /// Each anchor's mesh and that mesh at rest; and for each framing, the frame's middle across,
    /// its top and its height at zoom 1 (every flat model frames that way, as wide as the
    /// picture's aspect asks).
    Flat { meshes: HashMap<String, (usize, MeshRest)>, frames: [(f32, f32, f32); 4] },
}

impl AtRest {
    /// `model`'s anchors at rest. A flat model is posed once with nothing tracked to read them,
    /// so this comes before it's posed for real.
    pub fn of(model: &mut Posable) -> AtRest {
        match model {
            Posable::Vrm(a) => {
                let mut avatar = Box::new(Avatar::new(a.model.clone()));
                avatar.set_cpu_skinning(false);
                avatar.update(&Controls::default(), 0.0);
                let anchors = vrm_anchors(&a.model).into_iter().map(|(anchor, n)| (anchor.key, n)).collect();
                AtRest::Vrm { avatar, anchors }
            }
            Posable::Flat(f) => {
                f.update(&Face::default(), None, 0.0);
                let meshes = flat_anchors(f)
                    .into_iter()
                    .map(|(anchor, i)| (anchor.key, (i, mesh_rest(&flat_points(f, i)))))
                    .collect();
                let frames = FRAMINGS.map(|framing| {
                    let (l, r, t, b) = f.frame(framing, 1.0, 1.0);
                    ((l + r) * 0.5, t, t - b)
                });
                AtRest::Flat { meshes, frames }
            }
        }
    }

    /// Where anchor `key` is at rest in the avatar's picture (`w`×`h` pixels), framed so.
    pub fn spot(&self, key: &str, framing: Framing, zoom: f32, w: f32, h: f32) -> Option<Spot> {
        let aspect = w / h.max(1.0);
        match self {
            AtRest::Vrm { avatar, anchors } => {
                let m = avatar.world.get(*anchors.get(key)?)?;
                let vp = view_projection(&avatar.camera(framing, aspect, zoom), aspect);
                bone_spot(m, &vp, w, h, best_axis(m))
            }
            AtRest::Flat { meshes, frames } => {
                let (_, rest) = meshes.get(key)?;
                let i = FRAMINGS.iter().position(|f| *f == framing).unwrap_or(1);
                let (middle, top, tall) = frames[i];
                let height = tall / zoom.clamp(0.25, 4.0);
                let width = height * aspect.max(0.05);
                let frame = (middle - width * 0.5, middle + width * 0.5, top, top - height);
                Some(rest_spot(rest, frame, w, h))
            }
        }
    }

    /// Where each anchor of `keys` is in the avatar's picture (`w`×`h` pixels), framed so, at
    /// rest and as `model` (the model this was made from) is posed now: (key, at rest, now).
    pub fn pins(
        &self,
        model: &Posable,
        keys: &[String],
        framing: Framing,
        zoom: f32,
        w: f32,
        h: f32,
    ) -> Vec<(String, Spot, Spot)> {
        let aspect = w / h.max(1.0);
        match (self, model) {
            (AtRest::Vrm { avatar: rest, anchors }, Posable::Vrm(a)) => {
                let vp_rest = view_projection(&rest.camera(framing, aspect, zoom), aspect);
                let vp_now = view_projection(&a.camera(framing, aspect, zoom), aspect);
                keys.iter()
                    .filter_map(|key| {
                        let n = *anchors.get(key)?;
                        let axis = best_axis(rest.world.get(n)?);
                        let at_rest = bone_spot(rest.world.get(n)?, &vp_rest, w, h, axis)?;
                        let now = bone_spot(a.world.get(n)?, &vp_now, w, h, axis)?;
                        Some((key.clone(), at_rest, now))
                    })
                    .collect()
            }
            (AtRest::Flat { meshes, .. }, Posable::Flat(f)) => {
                let frame = f.frame(framing, aspect, zoom);
                keys.iter()
                    .filter_map(|key| {
                        let (i, rest) = meshes.get(key)?;
                        let now = mesh_spot(&flat_points(f, *i), rest.far, frame, w, h)?;
                        Some((key.clone(), rest_spot(rest, frame, w, h), now))
                    })
                    .collect()
            }
            _ => Vec::new(),
        }
    }
}
