//! The shape mask (Lightair's libobs module) cuts layers into circles and rounded boxes, with a
//! border. Skipped when the module isn't built (`cargo xtask modules`).

use std::path::PathBuf;
use std::time::Duration;

use lightair_core::{Command, Document, Effect, Input, Layer, Layout, Project, Scene, diff};
use lightair_engine::{Engine, EngineConfig};
use lightair_preview::{Preview, sample_ycbcr};
use serde_json::json;

fn apply(engine: &Engine, doc: &mut Document, cmd: Command) {
    let change = doc.apply(&cmd).unwrap();
    let patches = diff(&change.before, &change.after);
    let project = change.after.clone();
    engine.call(move |s| s.reconciler.apply(&project, &patches));
}

fn at(preview: &Preview, x: f32, y: f32) -> (u8, u8, u8) {
    let n = preview.frames();
    while preview.frames() < n + 3 {
        std::thread::sleep(Duration::from_millis(10));
    }
    sample_ycbcr(&preview.latest_frame().unwrap(), x, y).unwrap()
}

fn red(p: (u8, u8, u8)) -> bool {
    p.2 > 200 && p.1 < 128
}
fn black(p: (u8, u8, u8)) -> bool {
    p.0 < 30 && (118..138).contains(&p.1) && (118..138).contains(&p.2)
}
fn white(p: (u8, u8, u8)) -> bool {
    p.0 > 200 && (118..138).contains(&p.1) && (118..138).contains(&p.2)
}

#[test]
fn circle_rounded_and_border() {
    let _ = tracing_subscriber::fmt().with_env_filter("warn").with_test_writer().try_init();
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/modules/lightair-camera-fx");
    if !dir.join("lightair-camera-fx.plugin").exists() {
        eprintln!("module not built; run `cargo xtask modules`");
        return;
    }
    let engine = Engine::start(EngineConfig {
        canvas: (320, 180),
        output: (320, 180),
        extra_plugin_dirs: vec![dir],
        ..Default::default()
    })
    .unwrap();
    assert!(engine.capabilities.filters.iter().any(|f| f == "lightair_shape_mask"), "filter registered");
    let mut doc = Document::new(Project::new("Mask"));
    let f = doc.project().primary_format().id;
    let r = Input::new("Red", "color_source_v3", json!({"color": 0xFF0000FFu32, "width": 320, "height": 180}));
    let rid = r.id;
    let mut scene = Scene::new("Main");
    scene.layers.push(Layer::new("Red", rid, [(f, Layout::boxed(0.0, 0.0, 320.0, 180.0))]));
    apply(&engine, &mut doc, Command::AddInput { input: r });
    apply(&engine, &mut doc, Command::AddScene { scene, index: None });
    let preview = engine.call(|_| Preview::attach());
    assert!(red(at(&preview, 0.03, 0.05)), "unmasked: the corner is red");

    let mask = Effect::new("Shape", "lightair_shape_mask", json!({"shape": 1, "feather": 1.0}));
    let mid = mask.id;
    apply(&engine, &mut doc, Command::AddInputEffect { input: rid, effect: mask, index: None });
    let (c, corner) = (at(&preview, 0.5, 0.5), at(&preview, 0.03, 0.05));
    eprintln!("circle: centre {c:?}, corner {corner:?}");
    assert!(red(c), "inside the circle stays");
    assert!(black(corner), "outside the circle is cut away");
    // The circle is the largest that fits (90 px radius in 320×180, centred), so x = 60 — inside
    // the frame but left of the circle — is cut too: a circle, not an ellipse.
    assert!(black(at(&preview, 60.0 / 320.0, 0.5)), "a true circle, not an ellipse");

    let rounded = json!({"shape": 0, "radius": 0.0});
    apply(&engine, &mut doc, Command::SetInputEffectSettings { input: rid, effect: mid, settings: rounded });
    assert!(red(at(&preview, 0.03, 0.05)), "a square-cornered box keeps the corner");

    let ringed = json!({"shape": 1, "border": 12.0, "border_color": 0xFFFFFFFFu32});
    apply(&engine, &mut doc, Command::SetInputEffectSettings { input: rid, effect: mid, settings: ringed });
    let ring = at(&preview, (160.0 - 84.0) / 320.0, 0.5);
    eprintln!("border: {ring:?}");
    assert!(white(ring), "a white border inside the circle's edge");
    assert!(red(at(&preview, 0.5, 0.5)), "the middle is still the picture");
    drop(preview);
    drop(engine);
}
