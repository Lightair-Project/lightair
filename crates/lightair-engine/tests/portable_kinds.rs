//! Portable input kinds play as the platform's libobs sources, and an input whose kind maps to
//! another libobs type after an edit gets a new source, still in its scenes.
//!
//! Windows only: there a screen input's mode picks the libobs type. (On macOS the one such change,
//! desktop audio to a loopback device, needs the screen recording permission in the test.)
#![cfg(windows)]

use lightair_core::{Command, Document, Input, Layer, Layout, Workspace, Scene, diff, kinds};
use lightair_engine::{Engine, EngineConfig};
use serde_json::json;

#[test]
fn a_screen_input_changing_mode_is_recreated_in_place() {
    let _ = tracing_subscriber::fmt().with_env_filter("warn").with_test_writer().try_init();
    let engine = Engine::start(EngineConfig { canvas: (1280, 720), output: (1280, 720), ..Default::default() })
        .expect("engine starts");
    let mut doc = Document::new(Workspace::new("Kinds"));
    let canvas = doc.workspace().primary_canvas().id;
    let screen = Input::new("Screen", kinds::SCREEN, json!({"mode": kinds::DISPLAY}));
    let id = screen.id;
    let scene = Scene::new("Main");
    let sid = scene.id;
    let layer = Layer::new("Screen", id, [(canvas, Layout::boxed(0.0, 0.0, 1280.0, 720.0))]);
    let mut apply = |cmd: Command| {
        let change = doc.apply(&cmd).unwrap();
        let patches = diff(&change.before, &change.after);
        let workspace = change.after.clone();
        engine.call(move |s| s.reconciler.apply(&workspace, &patches));
    };
    apply(Command::AddInput { input: screen });
    apply(Command::AddScene { scene, index: None });
    apply(Command::AddLayer { scene: sid, layer, index: None });
    assert_eq!(engine.call(move |s| s.reconciler.input_type(id)).as_deref(), Some("monitor_capture"));

    apply(Command::SetInputSettings { input: id, settings: json!({"mode": kinds::WINDOW}) });
    assert_eq!(engine.call(move |s| s.reconciler.input_type(id)).as_deref(), Some("window_capture"));
    let items = engine.call(|s| s.reconciler.describe());
    assert_eq!(items.iter().filter(|l| l.contains(&id.to_string())).count(), 1, "{items:?}");
}
