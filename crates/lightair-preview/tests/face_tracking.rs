//! The face tracker finds a face and the mesh-drawn makeup reaches it. Needs a picture with a
//! face: set LIGHTAIR_FACE_IMAGE (not checked in); skipped otherwise.

use std::path::PathBuf;
use std::time::Duration;

use lightair_core::{Command, Document, Effect, Fit, Id, Input, Layer, Layout, Project, Scene, diff};
use lightair_engine::{Engine, EngineConfig};
use lightair_preview::{Preview, sample_ycbcr};
use serde_json::json;

fn apply(engine: &Engine, doc: &mut Document, cmd: Command) {
    let change = doc.apply(&cmd).unwrap();
    let patches = diff(&change.before, &change.after);
    let project = change.after.clone();
    engine.call(move |s| s.reconciler.apply(&project, &patches));
}

/// Luma on a 24×24 grid of the newest frame.
fn grid(preview: &Preview) -> Vec<u8> {
    let n = preview.frames();
    while preview.frames() < n + 5 {
        std::thread::sleep(Duration::from_millis(10));
    }
    let f = preview.latest_frame().unwrap();
    (0..24 * 24)
        .map(|i| sample_ycbcr(&f, (i % 24) as f32 / 24.0 + 0.02, (i / 24) as f32 / 24.0 + 0.02).unwrap().0)
        .collect()
}

#[test]
fn the_tracker_finds_the_face() {
    let _ = tracing_subscriber::fmt().with_env_filter("warn").with_test_writer().try_init();
    let Some(image) = std::env::var_os("LIGHTAIR_FACE_IMAGE").map(PathBuf::from) else {
        eprintln!("LIGHTAIR_FACE_IMAGE not set; skipping");
        return;
    };
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/modules/lightair-camera-fx");
    if !dir.join("lightair-camera-fx.plugin").exists() {
        eprintln!("module not built; skipping");
        return;
    }
    let engine = Engine::start(EngineConfig {
        canvas: (1280, 720),
        output: (1280, 720),
        extra_plugin_dirs: vec![dir],
        ..Default::default()
    })
    .unwrap();
    let mut doc = Document::new(Project::new("Face"));
    let f = doc.project().primary_format().id;
    let mut input = Input::new("Face", "image_source", json!({"file": image.to_string_lossy()}));
    let mut studio = Effect::new("Camera Studio", "lightair_camera_studio", json!({"debug_view": 2}));
    studio.enabled = false;
    let eid = studio.id;
    input.effects.push(studio);
    let iid = input.id;
    let mut scene = Scene::new("Main");
    scene.id = Id::new();
    let sid = scene.id;
    scene.layers =
        vec![Layer::new("Face", iid, [(f, Layout { fit: Fit::Fit, ..Layout::boxed(0.0, 0.0, 1280.0, 720.0) })])];
    apply(&engine, &mut doc, Command::AddInput { input });
    apply(&engine, &mut doc, Command::AddScene { scene, index: None });
    let preview = engine.call(|_| Preview::attach());
    std::thread::sleep(Duration::from_millis(800));
    let plain = grid(&preview);
    apply(&engine, &mut doc, Command::SetInputEffectEnabled { input: iid, effect: eid, enabled: true });
    // Models load on the tracker's threads (the first load also compiles them for this Mac).
    let mut present = false;
    for _ in 0..60 {
        std::thread::sleep(Duration::from_millis(500));
        present = engine.call(move |s| s.reconciler.face_present(iid)).unwrap_or(false);
        if present {
            break;
        }
    }
    let changed = plain.iter().zip(&grid(&preview)).filter(|(a, b)| (**a as i32 - **b as i32).abs() > 12).count();
    eprintln!("face present: {present}; {changed} of 576 grid points changed with face regions shown");
    let expect = std::env::var("LIGHTAIR_FACE_EXPECTED").map_or(true, |v| v != "0");
    assert_eq!(present, expect, "the tracker sees a face when there is one");
    let _ = sid;
    drop(preview);
    drop(engine);
}

/// Cb/Cr on a 24×24 grid of the newest frame.
fn chroma(preview: &Preview) -> Vec<(u8, u8)> {
    let n = preview.frames();
    while preview.frames() < n + 5 {
        std::thread::sleep(Duration::from_millis(10));
    }
    let f = preview.latest_frame().unwrap();
    (0..24 * 24)
        .map(|i| {
            let (_, cb, cr) = sample_ycbcr(&f, (i % 24) as f32 / 24.0 + 0.02, (i / 24) as f32 / 24.0 + 0.02).unwrap();
            (cb, cr)
        })
        .collect()
}

#[test]
fn the_background_is_replaced_around_the_person() {
    let _ = tracing_subscriber::fmt().with_env_filter("warn").with_test_writer().try_init();
    let Some(image) = std::env::var_os("LIGHTAIR_FACE_IMAGE").map(PathBuf::from) else {
        eprintln!("LIGHTAIR_FACE_IMAGE not set; skipping");
        return;
    };
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/modules/lightair-camera-fx");
    if !dir.join("lightair-camera-fx.plugin").exists() {
        eprintln!("module not built; skipping");
        return;
    }
    let engine = Engine::start(EngineConfig {
        canvas: (1280, 720),
        output: (1280, 720),
        extra_plugin_dirs: vec![dir],
        ..Default::default()
    })
    .unwrap();
    let mut doc = Document::new(Project::new("Background"));
    let f = doc.project().primary_format().id;
    let mut input = Input::new("Person", "image_source", json!({"file": image.to_string_lossy()}));
    // Pure red behind the person (OBS colours are 0xAABBGGRR).
    let studio = Effect::new("Background", "studio.bg_replace", json!({"bg_mode": 3, "bg_color": 0xFF0000FFu32}));
    let eid = studio.id;
    input.effects.push(studio);
    let iid = input.id;
    let mut scene = Scene::new("Main");
    scene.layers =
        vec![Layer::new("Person", iid, [(f, Layout { fit: Fit::Fill, ..Layout::boxed(0.0, 0.0, 1280.0, 720.0) })])];
    apply(&engine, &mut doc, Command::AddInput { input });
    apply(&engine, &mut doc, Command::AddScene { scene, index: None });
    let preview = engine.call(|_| Preview::attach());
    let mut person = false;
    for _ in 0..60 {
        std::thread::sleep(Duration::from_millis(500));
        person = engine.call(move |s| s.reconciler.person_present(iid)).unwrap_or(false);
        if person {
            break;
        }
    }
    std::thread::sleep(Duration::from_millis(500));
    let red = |c: &[(u8, u8)]| c.iter().filter(|(cb, cr)| *cr > 200 && *cb < 120).count();
    let replaced = red(&chroma(&preview));
    eprintln!("person present: {person}; {replaced} of 576 grid points red");
    let expect = std::env::var("LIGHTAIR_FACE_EXPECTED").map_or(true, |v| v != "0");
    assert_eq!(person, expect, "the segmenter sees a person when there is one");
    if expect {
        assert!(replaced > 40, "the background turned red");
        assert!(replaced < 540, "the person kept their colours");
    }

    // Blur keeps the background's own colours: nothing red.
    // (one face effect for another: the same Camera Studio filter, updated in place)
    let blur = Effect::new("Blur", "studio.bg_blur", json!({"bg_blur": 80}));
    apply(&engine, &mut doc, Command::AddInputEffect { input: iid, effect: blur, index: None });
    apply(&engine, &mut doc, Command::RemoveInputEffect { input: iid, effect: eid });
    std::thread::sleep(Duration::from_millis(400));
    assert!(red(&chroma(&preview)) < 5, "blur shows the real background");
    drop(preview);
    drop(engine);
}
