//! Without `LIGHTAIR_ENGINE=own` (the default), Lightair's own compositor stays out of the way:
//! the engine gives it nothing to draw, so even one that's started leaves the preview to libobs.
//! (The switch-on tests are their own program, `own_engine`.)

use std::time::{Duration, Instant};

use lightair_engine::{Engine, EngineConfig};
use lightair_model::{Command, Document, Layout, Object, Scene, Source, Workspace, diff};
use lightair_preview::{Preview, frame_size, sample_ycbcr};
use serde_json::json;

fn apply(engine: &Engine, doc: &mut Document, cmd: Command) {
    let change = doc.apply(&cmd).unwrap();
    let patches = diff(&change.before, &change.after);
    let workspace = change.after.clone();
    engine.call(move |s| s.reconciler.apply(&workspace, &patches));
}

#[test]
fn without_the_switch_libobs_draws_the_preview() {
    if lightair_engine::testing::ran_in_own_process() {
        return;
    }
    if lightair_engine::bridge::own() {
        eprintln!("LIGHTAIR_ENGINE=own is set; skipping");
        return;
    }
    let _ = tracing_subscriber::fmt().with_env_filter("warn").with_test_writer().try_init();
    let engine = Engine::start(EngineConfig { canvas: (320, 180), output: (320, 180), ..Default::default() }).unwrap();
    let mut ws = Workspace::new("Switch off");
    ws.canvases[0].width = 320;
    ws.canvases[0].height = 180;
    let mut doc = Document::new(ws);
    let c = doc.workspace().primary_canvas().id;
    let red = Source::new("Red", "color_source_v3", json!({"color": 0xFF0000FFu32, "width": 320, "height": 180}));
    let mut scene = Scene::new("A");
    scene.objects.push(Object::new("Red", red.id, [(c, Layout::boxed(0.0, 0.0, 320.0, 180.0))]));
    let a = scene.id;
    for cmd in [
        Command::AddSource { source: red },
        Command::AddScene { scene, index: None },
        Command::SetProgram { scene: Some(a) },
    ] {
        apply(&engine, &mut doc, cmd);
    }
    // libobs draws the preview at the size asked for; the compositor would at the canvas's
    let preview = engine.call(|_| Preview::attach());
    preview.set_size(Some((160, 90)));
    preview.compose_program();

    // a second of frames: every one libobs's, showing the program
    let end = Instant::now() + Duration::from_secs(1);
    let (mut seen, mut frames) = (Vec::new(), preview.frames());
    while Instant::now() < end {
        if preview.frames() != frames {
            frames = preview.frames();
            let f = preview.latest_frame().expect("a frame");
            seen.push((frame_size(&f), sample_ycbcr(&f, 0.5, 0.5).unwrap()));
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    assert!(seen.len() > 10, "libobs keeps drawing the preview: {seen:?}");
    // red, as (Y, Cb, Cr)
    let libobs_red = |&(size, (_, cb, cr)): &((usize, usize), (u8, u8, u8))| size == (160, 90) && cr > 200 && cb < 128;
    assert!(seen.iter().all(libobs_red), "every frame libobs's picture of the program: {seen:?}");
    drop(preview);
    drop(engine);
}
