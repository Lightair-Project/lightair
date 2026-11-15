//! Flip and 3D tilt (Lightair's transform filter): a half-red, half-blue picture swaps sides when
//! flipped and narrows into perspective when tilted. Needs the module (`cargo xtask modules`) and
//! ffmpeg to make the picture; skipped otherwise.

use std::path::PathBuf;
use std::time::Duration;

use lightair_core::{Command, Document, Fit, Id, Source, Object, Layout, Workspace, Scene, Shape, ShapeKind, diff};
use lightair_engine::{Engine, EngineConfig};
use lightair_preview::{Preview, sample_ycbcr};
use serde_json::json;

fn apply(engine: &Engine, doc: &mut Document, cmd: Command) {
    let change = doc.apply(&cmd).unwrap();
    let patches = diff(&change.before, &change.after);
    let workspace = change.after.clone();
    engine.call(move |s| s.reconciler.apply(&workspace, &patches));
}

fn at(preview: &Preview, x: f32, y: f32) -> (u8, u8, u8) {
    let n = preview.frames();
    while preview.frames() < n + 3 {
        std::thread::sleep(Duration::from_millis(10));
    }
    sample_ycbcr(&preview.latest_frame().unwrap(), x / 320.0, y / 180.0).unwrap()
}

fn red(p: (u8, u8, u8)) -> bool {
    p.2 > 200 && p.1 < 128
}
fn blue(p: (u8, u8, u8)) -> bool {
    p.1 > 200 && p.2 < 128
}
fn black(p: (u8, u8, u8)) -> bool {
    p.0 < 30 && (118..138).contains(&p.1) && (118..138).contains(&p.2)
}

#[test]
fn flip_and_tilt() {
    let _ = tracing_subscriber::fmt().with_env_filter("warn").with_test_writer().try_init();
    let module = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/modules/lightair-camera-fx");
    if !module.join("lightair-camera-fx.plugin").exists() {
        eprintln!("module not built; skipping");
        return;
    }
    let dir = std::env::temp_dir().join(format!("lightair-3d-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let png = dir.join("halves.png");
    let made = std::process::Command::new("ffmpeg")
        .args(["-nostdin", "-loglevel", "error", "-y", "-f", "lavfi", "-i"])
        .arg("color=c=red:s=320x180,drawbox=x=160:y=0:w=160:h=180:color=blue:t=fill")
        .args(["-frames:v", "1"])
        .arg(&png)
        .status()
        .is_ok_and(|s| s.success());
    if !made {
        eprintln!("ffmpeg not available; skipping");
        return;
    }
    let engine = Engine::start(EngineConfig {
        canvas: (320, 180),
        output: (320, 180),
        extra_plugin_dirs: vec![module],
        ..Default::default()
    })
    .unwrap();
    assert!(engine.capabilities.filters.iter().any(|f| f == "lightair_transform"), "filter registered");
    let mut doc = Document::new(Workspace::new("3D"));
    let f = doc.workspace().primary_canvas().id;
    let image = Source::new("Halves", "image_source", json!({"file": png.to_string_lossy()}));
    let full = Layout { fit: Fit::Stretch, ..Layout::boxed(0.0, 0.0, 320.0, 180.0) };
    let object = Object::new("Halves", image.id, [(f, full)]);
    let (lid, sid) = (object.id, Id::new());
    let mut scene = Scene::new("Main");
    scene.id = sid;
    scene.objects.push(object);
    apply(&engine, &mut doc, Command::AddSource { source: image });
    apply(&engine, &mut doc, Command::AddScene { scene, index: None });
    let preview = engine.call(|_| Preview::attach());
    let set = |doc: &mut Document, layout: Layout| {
        apply(&engine, doc, Command::SetLayout { scene: sid, object: lid, canvas: f, layout })
    };
    std::thread::sleep(Duration::from_millis(300));
    assert!(red(at(&preview, 80.0, 90.0)) && blue(at(&preview, 240.0, 90.0)), "red left, blue right");

    set(&mut doc, Layout { flip_h: true, ..full });
    let (l, r) = (at(&preview, 80.0, 90.0), at(&preview, 240.0, 90.0));
    eprintln!("flipped: {l:?} {r:?}");
    assert!(blue(l) && red(r), "flip swaps the halves");

    set(&mut doc, Layout { tilt_y: 60.0, ..full });
    let (edge, inner, middle_right) = (at(&preview, 4.0, 90.0), at(&preview, 140.0, 90.0), at(&preview, 180.0, 90.0));
    eprintln!("tilt y: edge {edge:?}, inner {inner:?}, right of middle {middle_right:?}");
    assert!(black(edge), "the picture narrows away from the left edge");
    assert!(red(inner) && blue(middle_right), "the middle is still the picture, in order");

    // Which side recedes: the farther edge is shorter (measured, for the editor's outline).
    let column = |x: f32| (0..180).step_by(6).filter(|y| !black(at(&preview, x, *y as f32))).count() * 6;
    let (near_left, near_right) = (column(115.0), column(205.0));
    eprintln!("tilt y 60: column heights near left {near_left}, near right {near_right}");
    assert!(near_left < near_right, "positive tilt Y: the left side recedes");
    set(&mut doc, Layout { tilt_x: 60.0, ..full });
    let row = |y: f32| (0..320).step_by(8).filter(|x| !black(at(&preview, *x as f32, y))).count() * 8;
    let (near_top, near_bottom) = (row(65.0), row(115.0));
    eprintln!("tilt x 60: row widths near top {near_top}, near bottom {near_bottom}");
    assert!(near_bottom < near_top, "positive tilt X: the bottom recedes");
    let (top, mid) = (at(&preview, 80.0, 3.0), at(&preview, 80.0, 90.0));
    eprintln!("tilt x: top {top:?}, middle {mid:?}");
    assert!(black(top), "the picture narrows away from the top");
    assert!(red(mid));

    // With a shape too: the circle tilts with it (corners stay cut, the middle stays).
    set(&mut doc, Layout { tilt_y: 30.0, shape: Some(Shape::new(ShapeKind::Circle)), ..full });
    assert!(black(at(&preview, 100.0, 4.0)), "outside the circle");
    assert!(red(at(&preview, 140.0, 90.0)), "inside it");

    set(&mut doc, full);
    assert!(red(at(&preview, 80.0, 90.0)) && blue(at(&preview, 240.0, 90.0)), "back to normal");
    drop(preview);
    drop(engine);
    let _ = std::fs::remove_dir_all(dir);
}
