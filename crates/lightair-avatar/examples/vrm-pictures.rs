//! Pictures of a VRM model posed by hand and by a tracked body, to check by eye (these were
//! tests that couldn't fail). LIGHTAIR_VRM: the model; LIGHTAIR_VRM_OUT: the folder the
//! pictures go to; LIGHTAIR_BODY_JSON: a tracked body frame for the second picture.
//! `cargo run -p lightair-avatar --example vrm-pictures`

use std::path::PathBuf;
use std::sync::Arc;

use glam::{Mat4, Quat, Vec2, Vec3, Vec4};
use lightair_avatar::vrm::Model;
use lightair_avatar::{Avatar, Controls, Framing};

fn main() {
    body_poses();
    body_from_tracking();
}

fn model() -> Option<Arc<Model>> {
    let path = PathBuf::from(std::env::var_os("LIGHTAIR_VRM")?);
    let bytes = std::fs::read(&path).unwrap();
    Some(Arc::new(Model::load(&bytes).unwrap()))
}

/// A software picture of the avatar: textured, unlit, depth-tested.
fn draw(a: &Avatar, w: usize, h: usize, framing: Framing) -> Vec<u8> {
    let cam = a.camera(framing, w as f32 / h as f32, 1.0);
    let vp = Mat4::perspective_rh(cam.fovy.to_radians(), w as f32 / h as f32, cam.near, cam.far) * cam.view();
    let mut color = vec![0u8; w * h * 4];
    let mut depth = vec![f32::INFINITY; w * h];
    let m = &a.model;
    for (pi, p) in m.primitives.iter().enumerate() {
        let mat = &m.materials[p.material];
        let tex = mat.base_texture.map(|t| &m.images[t]);
        let screen: Vec<(Vec2, f32)> = a.positions[pi]
            .iter()
            .map(|v| {
                let c = vp * Vec4::new(v.x, v.y, v.z, 1.0);
                let n = c.truncate() / c.w;
                (Vec2::new((n.x * 0.5 + 0.5) * w as f32, (0.5 - n.y * 0.5) * h as f32), n.z)
            })
            .collect();
        for t in p.indices.as_chunks::<3>().0 {
            let [i0, i1, i2] = [t[0] as usize, t[1] as usize, t[2] as usize];
            let (a0, a1, a2) = (screen[i0], screen[i1], screen[i2]);
            let area = (a1.0 - a0.0).perp_dot(a2.0 - a0.0);
            if area.abs() < 1e-6 {
                continue;
            }
            let min = a0.0.min(a1.0).min(a2.0).max(Vec2::ZERO);
            let max = a0.0.max(a1.0).max(a2.0).min(Vec2::new(w as f32 - 1.0, h as f32 - 1.0));
            for y in min.y as usize..=max.y as usize {
                for x in min.x as usize..=max.x as usize {
                    let pt = Vec2::new(x as f32 + 0.5, y as f32 + 0.5);
                    let b0 = (a1.0 - pt).perp_dot(a2.0 - pt) / area;
                    let b1 = (a2.0 - pt).perp_dot(a0.0 - pt) / area;
                    let b2 = 1.0 - b0 - b1;
                    if b0 < 0.0 || b1 < 0.0 || b2 < 0.0 {
                        continue;
                    }
                    let z = a0.1 * b0 + a1.1 * b1 + a2.1 * b2;
                    let k = y * w + x;
                    if z >= depth[k] {
                        continue;
                    }
                    let uv = Vec2::from(p.uvs[i0]) * b0 + Vec2::from(p.uvs[i1]) * b1 + Vec2::from(p.uvs[i2]) * b2;
                    let mut c = [
                        mat.base_color[0] * 255.0,
                        mat.base_color[1] * 255.0,
                        mat.base_color[2] * 255.0,
                        mat.base_color[3] * 255.0,
                    ];
                    if let Some(img) = tex {
                        let tx = ((uv.x.rem_euclid(1.0)) * img.width as f32) as usize % img.width as usize;
                        let ty = ((uv.y.rem_euclid(1.0)) * img.height as f32) as usize % img.height as usize;
                        let o = (ty * img.width as usize + tx) * 4;
                        for (i, ch) in c.iter_mut().enumerate() {
                            *ch *= img.rgba[o + i] as f32 / 255.0;
                        }
                    }
                    if c[3] < 128.0 {
                        continue;
                    }
                    depth[k] = z;
                    let n = (a.normals[pi][i0] * b0 + a.normals[pi][i1] * b1 + a.normals[pi][i2] * b2)
                        .normalize_or(Vec3::Z);
                    let light = 0.7 + 0.3 * n.dot(Vec3::new(0.3, 0.5, 0.8).normalize()).max(0.0);
                    color[k * 4..k * 4 + 4].copy_from_slice(&[
                        (c[0] * light) as u8,
                        (c[1] * light) as u8,
                        (c[2] * light) as u8,
                        255,
                    ]);
                }
            }
        }
    }
    color
}

fn save(name: &str, rgba: &[u8], w: usize, h: usize) {
    if let Some(dir) = std::env::var_os("LIGHTAIR_VRM_OUT") {
        let path = PathBuf::from(dir).join(name);
        image::save_buffer(&path, rgba, w as u32, h as u32, image::ColorType::Rgba8).unwrap();
    }
}

/// Poses for checking a body tracker by eye (LIGHTAIR_VRM_OUT): the left arm forward, the body
/// turned to its left.
fn body_poses() {
    let Some(model) = model() else { return };
    if std::env::var_os("LIGHTAIR_VRM_OUT").is_none() {
        return;
    }
    let mut a = Avatar::new(model);
    let (w, h) = (480, 640);
    let poses: [(&str, Vec<(String, Quat)>); 4] = [
        ("arm_forward.png", vec![("leftUpperArm".into(), Quat::from_rotation_y(-90f32.to_radians()))]),
        (
            "pose.png",
            vec![
                (
                    "leftUpperArm".into(),
                    Quat::from_rotation_y(-60f32.to_radians()) * Quat::from_rotation_z(-30f32.to_radians()),
                ),
                ("rightUpperArm".into(), Quat::from_rotation_z(50f32.to_radians())),
                ("rightLowerArm".into(), Quat::from_rotation_z(80f32.to_radians())),
                ("leftUpperLeg".into(), Quat::from_rotation_x(-40f32.to_radians())),
                ("leftLowerLeg".into(), Quat::from_rotation_x(60f32.to_radians())),
            ],
        ),
        ("arm_up.png", vec![("leftUpperArm".into(), Quat::from_rotation_z(70f32.to_radians()))]),
        ("turned_body.png", vec![("hips".into(), Quat::from_rotation_y(20f32.to_radians()))]),
    ];
    for (name, bones) in poses {
        let c = Controls { bones, ..Controls::default() };
        for _ in 0..30 {
            a.update(&c, 1.0 / 60.0);
        }
        save(name, &draw(&a, w, h, Framing::Full), w, h);
        if name == "pose.png" {
            let hips = a.bone_position("hips").unwrap();
            for b in [
                "leftUpperArm",
                "leftLowerArm",
                "leftHand",
                "rightUpperArm",
                "rightLowerArm",
                "rightHand",
                "leftUpperLeg",
                "leftLowerLeg",
                "leftFoot",
            ] {
                eprintln!("true {b:14} {:.3}", a.bone_position(b).unwrap() - hips);
            }
        }
    }
}

/// A body tracker's joints (LIGHTAIR_BODY_JSON: {"j3": [[x,y,z]|null ×17] | null, "j2":
/// [[x,y,conf]|null ×17], "aspect"}) put on the avatar and drawn (LIGHTAIR_VRM_OUT), to check
/// the round trip: a pose drawn, tracked, and drawn again from what was tracked.
fn body_from_tracking() {
    use lightair_avatar::body::{BodyFrame, BodyMode, BodyState, JOINTS};
    let (Some(model), Some(json)) = (model(), std::env::var_os("LIGHTAIR_BODY_JSON")) else { return };
    let v: serde_json::Value = serde_json::from_slice(&std::fs::read(json).unwrap()).unwrap();
    let f3 = |x: &serde_json::Value| x.as_f64().unwrap() as f32;
    let mut j2 = [None; JOINTS];
    for (i, p) in v["j2"].as_array().unwrap().iter().enumerate() {
        if let Some(a) = p.as_array() {
            j2[i] = Some((glam::Vec2::new(f3(&a[0]), f3(&a[1])), f3(&a[2])));
        }
    }
    let joints3d = v["j3"].as_array().map(|a| {
        let mut j = [None; JOINTS];
        for (i, p) in a.iter().enumerate() {
            if let Some(p) = p.as_array() {
                j[i] = Some(Vec3::new(f3(&p[0]), f3(&p[1]), f3(&p[2])));
            }
        }
        j
    });
    let frame = |id| BodyFrame {
        joints3d,
        joints2d: j2,
        hands: [None, None],
        hands3d: [None, None],
        aspect: f3(&v["aspect"]),
        frame_id: id,
    };
    let mut a = Avatar::new(model);
    let mut state = BodyState::default();
    for i in 0..60 {
        state.step(Some(&frame(i)), false, 1.0 / 30.0);
        let (bones, torso) = a.body_bones(&state, BodyMode::Full);
        a.update(&Controls { bones, torso, ..Controls::default() }, 1.0 / 30.0);
    }
    eprintln!("seen {:?}", state.seen);
    save("retarget.png", &draw(&a, 480, 640, Framing::Half), 480, 640);
    for (x, y, jx, jy) in [
        ("leftUpperArm", "leftLowerArm", 11, 12),
        ("leftLowerArm", "leftHand", 12, 13),
        ("rightUpperArm", "rightLowerArm", 14, 15),
        ("rightLowerArm", "rightHand", 15, 16),
        ("leftUpperLeg", "leftLowerLeg", 1, 2),
        ("leftLowerLeg", "leftFoot", 2, 3),
    ] {
        let got = (a.bone_position(y).unwrap() - a.bone_position(x).unwrap()).normalize();
        let (Some(b), Some(a0)) = (state.joints[jy], state.joints[jx]) else { continue };
        let want = (b - a0).normalize();
        eprintln!("{x:14} got {got:.2} want {want:.2} off {:.1}°", got.angle_between(want).to_degrees());
    }
}
