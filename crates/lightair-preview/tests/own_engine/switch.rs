//! A scene switch as the compositor draws it.

use std::time::Duration;

use lightair_obs::{Engine, EngineConfig};
use lightair_model::{Command, Document, Layout, Object, Scene, Source, TransitionKind, TransitionSettings, Workspace};
use lightair_preview::Preview;
use serde_json::json;

use crate::common::{H, SMALL, W, apply, rgb, switch_on, until, watch};

fn red(c: [u8; 3]) -> bool {
    c[0] > 200 && c[1] < 60 && c[2] < 60
}
fn blue(c: [u8; 3]) -> bool {
    c[2] > 200 && c[0] < 60 && c[1] < 60
}
/// Partway from red to blue: some of each, all of neither.
fn mixed(c: [u8; 3]) -> bool {
    (50..=210).contains(&c[0]) && (50..=210).contains(&c[2]) && c[1] < 60
}

#[test]
fn a_fade_shows_both_scenes_mixed_partway_through() {
    if lightair_obs::testing::ran_in_own_process() {
        return;
    }
    switch_on();
    let _ = tracing_subscriber::fmt().with_env_filter("warn").with_test_writer().try_init();
    let engine = Engine::start(EngineConfig { canvas: (W, H), output: (W, H), ..Default::default() }).unwrap();
    let mut ws = Workspace::new("Fade");
    ws.canvases[0].width = W;
    ws.canvases[0].height = H;
    let mut doc = Document::new(ws);
    let c = doc.workspace().primary_canvas().id;
    let solid =
        |name: &str, color: u32| Source::new(name, "color_source_v3", json!({"color": color, "width": W, "height": H}));
    let (r, b) = (solid("Red", 0xFF0000FF), solid("Blue", 0xFFFF0000));
    let mut a_scene = Scene::new("A");
    a_scene.objects.push(Object::new("Red", r.id, [(c, Layout::boxed(0.0, 0.0, W as f32, H as f32))]));
    let mut b_scene = Scene::new("B");
    b_scene.objects.push(Object::new("Blue", b.id, [(c, Layout::boxed(0.0, 0.0, W as f32, H as f32))]));
    let (a, bb) = (a_scene.id, b_scene.id);
    for cmd in [
        Command::AddSource { source: r },
        Command::AddSource { source: b },
        Command::AddScene { scene: a_scene, index: None },
        Command::AddScene { scene: b_scene, index: None },
        Command::SetProgram { scene: Some(a) },
    ] {
        apply(&engine, &mut doc, cmd);
    }
    // the compositor's frames are the canvas's size; libobs's the smaller one asked for
    let preview = engine.call(|_| Preview::attach());
    preview.set_size(Some(SMALL));
    assert!(preview.compose_program(), "the compositor starts");
    let centre = |f: &[u8]| rgb(f, 0.5, 0.5);
    until(&preview, Duration::from_secs(10), |f| red(centre(f)))
        .unwrap_or_else(|last| panic!("starts on A (red): {:?}", last.map(|f| centre(&f))));

    let fade = TransitionSettings { kind: TransitionKind::Fade, duration_ms: 1000, ..Default::default() };
    apply(&engine, &mut doc, Command::SetTransition { settings: fade });
    apply(&engine, &mut doc, Command::SetProgram { scene: Some(bb) });
    // every frame of the switch, and a little after
    let seen: Vec<[u8; 3]> = watch(&preview, Duration::from_millis(1300)).iter().map(|f| centre(f)).collect();
    assert!(seen.iter().any(|c| mixed(*c)), "partway through, red and blue mixed: {seen:?}");
    assert!(seen.last().is_some_and(|c| blue(*c)), "lands on B (blue): {seen:?}");
    drop(preview);
    drop(engine);
}
