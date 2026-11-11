//! An imported OBS collection reconciles into libobs, including plugin sources and filters
//! that aren't installed (they must not take the engine down). No cameras or microphones here,
//! so the test never triggers a permission prompt.

use lightair_core::{Document, Project, diff};
use lightair_engine::{Engine, EngineConfig};

const COLLECTION: &str = r#"{
  "name": "Engine import",
  "current_program_scene": "Main",
  "current_transition": "Cut",
  "scene_order": [{"name": "Main"}, {"name": "Other"}],
  "sources": [
    {"id": "color_source", "versioned_id": "color_source_v3", "name": "Red", "uuid": "red",
     "settings": {"color": 4278190335, "width": 640, "height": 360},
     "filters": [
       {"id": "color_filter", "versioned_id": "color_filter_v2", "name": "Grade", "settings": {"saturation": -0.5}},
       {"id": "not_installed_filter", "versioned_id": "not_installed_filter", "name": "Ghost", "settings": {}}
     ]},
    {"id": "not_installed_source", "versioned_id": "not_installed_source", "name": "Ghost source", "uuid": "ghost", "settings": {"a": 1}},
    {"id": "scene", "name": "Main", "uuid": "main", "settings": {"items": [
      {"name": "Red", "source_uuid": "red", "align": 5, "pos": {"x": 0, "y": 0}, "scale": {"x": 1, "y": 1}, "visible": true},
      {"name": "Ghost source", "source_uuid": "ghost", "align": 5, "pos": {"x": 10, "y": 10}, "scale": {"x": 1, "y": 1}, "visible": true}
    ]}},
    {"id": "scene", "name": "Other", "uuid": "other", "settings": {"items": [
      {"name": "Red", "source_uuid": "red", "align": 0, "bounds_type": 2, "bounds": {"x": 320, "y": 180}, "pos": {"x": 320, "y": 180}, "scale": {"x": 1, "y": 1}, "visible": true}
    ]}}
  ]
}"#;

#[test]
fn imported_collection_reconciles() {
    let _ = tracing_subscriber::fmt().with_env_filter("warn").with_test_writer().try_init();
    let engine = Engine::start(EngineConfig { canvas: (640, 360), output: (640, 360), ..Default::default() }).unwrap();
    let mut doc = Document::new(Project::new("Import"));
    let (cmd, report) = lightair_store::import_obs_collection(COLLECTION, doc.project()).unwrap();
    assert_eq!(report.kept_plugin.len(), 2, "{report:#?}");
    let change = doc.apply(&cmd).unwrap();
    let patches = diff(&change.before, &change.after);
    let project = change.after.clone();
    engine.call(move |s| s.reconciler.apply(&project, &patches));
    std::thread::sleep(std::time::Duration::from_millis(200));
    let described = engine.call(|s| s.reconciler.describe());
    eprintln!("{described:#?}");
    let (inputs, scenes, items) = engine.call(|s| s.reconciler.counts());
    assert_eq!((inputs, scenes), (2, 2), "both inputs and scenes exist in libobs");
    assert_eq!(items, 3);

    // Undo removes everything it added.
    let change = doc.undo().unwrap();
    let patches = diff(&change.before, &change.after);
    let project = change.after.clone();
    engine.call(move |s| s.reconciler.apply(&project, &patches));
    assert_eq!(engine.call(|s| s.reconciler.counts()), (0, 0, 0));
    drop(engine);
}
