//! Reconciler idempotence (docs/04-engine-libobs.md, "Reconciler"): applying the edits one by
//! one must leave libobs in exactly the same state as rebuilding from the final document.

use lightair_core::{Command, Document, Input, Layer, Layout, Project, Scene, diff};
use lightair_engine::{Engine, EngineConfig};
use serde_json::json;

#[test]
fn incremental_reconcile_matches_full_rebuild() {
    let _ = tracing_subscriber::fmt().with_env_filter("warn").with_test_writer().try_init();
    let engine = Engine::start(EngineConfig { canvas: (1280, 720), output: (1280, 720), ..Default::default() })
        .expect("engine starts");

    let mut doc = Document::new(Project::new("Test"));
    let format = doc.project().primary_format().id;
    let red = Input::new("Red", "color_source_v3", json!({"color": 0xFF0000FFu32, "width": 400, "height": 300}));
    let blue = Input::new("Blue", "color_source_v3", json!({"color": 0xFFFF0000u32, "width": 400, "height": 300}));
    let (red_id, blue_id) = (red.id, blue.id);
    let main = Scene::new("Main");
    let brb = Scene::new("BRB");
    let (main_id, brb_id) = (main.id, brb.id);
    let l1 = Layer::new("Red", red_id, [(format, Layout::boxed(0.0, 0.0, 640.0, 360.0))]);
    let l2 = Layer::new("Blue", blue_id, [(format, Layout::boxed(320.0, 180.0, 640.0, 360.0))]);
    let l3 = Layer::new("Red again", red_id, [(format, Layout::boxed(10.0, 10.0, 100.0, 100.0))]);
    let (l1_id, l2_id) = (l1.id, l2.id);

    let edits = vec![
        Command::AddInput { input: red },
        Command::AddInput { input: blue },
        Command::AddScene { scene: main, index: None },
        Command::AddScene { scene: brb, index: None },
        Command::AddLayer { scene: main_id, layer: l1, index: None },
        Command::AddLayer { scene: main_id, layer: l2, index: None },
        Command::AddLayer { scene: brb_id, layer: l3, index: None },
        Command::SetLayout {
            scene: main_id,
            layer: l1_id,
            format,
            layout: Layout { rotation: 15.0, ..Layout::boxed(50.0, 60.0, 300.0, 200.0) },
        },
        Command::MoveLayer { scene: main_id, layer: l2_id, to: 0 },
        Command::SetLayerVisible { scene: main_id, layer: l1_id, format, visible: false },
        Command::SetInputSettings {
            input: blue_id,
            settings: json!({"color": 0xFF00FF00u32, "width": 800, "height": 600}),
        },
        Command::SetProgram { scene: Some(brb_id) },
        Command::RemoveLayer { scene: brb_id, layer: doc_layer_placeholder() },
    ];

    let mut applied = 0;
    for edit in edits {
        let Ok(change) = doc.apply(&edit) else { continue };
        let patches = diff(&change.before, &change.after);
        let project = change.after.clone();
        engine.call(move |s| s.reconciler.apply(&project, &patches));
        applied += 1;
    }
    assert!(applied >= 12, "edits applied: {applied}");
    // Undo two steps and redo one, through the same path.
    for change in [doc.undo().unwrap(), doc.undo().unwrap(), doc.redo().unwrap()] {
        let patches = diff(&change.before, &change.after);
        let project = change.after.clone();
        engine.call(move |s| s.reconciler.apply(&project, &patches));
    }

    let incremental = engine.call(|s| s.reconciler.describe());
    let final_project = doc.project().clone();
    let rebuilt = engine.call(move |s| {
        s.reconciler.clear();
        let empty = final_project.empty_like();
        let patches = diff(&empty, &final_project);
        s.reconciler.apply(&final_project, &patches);
        s.reconciler.describe()
    });
    eprintln!("{}", incremental.join("\n"));
    assert_eq!(incremental, rebuilt);
    assert!(incremental.iter().any(|l| l.contains("(program)")));
    assert_eq!(engine.call(|s| s.reconciler.counts()), (2, 2, 3));
}

/// A layer id that doesn't exist: the last edit must fail validation and change nothing.
fn doc_layer_placeholder() -> lightair_core::Id {
    lightair_core::Id::new()
}
