//! An input's list choices are read from the input itself (cameras, displays, windows, mics use
//! the same path; a slideshow's list needs no permission).

use lightair_core::{Command, Document, Input, Workspace, diff};
use lightair_engine::{Engine, EngineConfig};
use serde_json::json;

#[test]
fn list_choices_come_from_the_input() {
    let _ = tracing_subscriber::fmt().with_env_filter("warn").with_test_writer().try_init();
    let engine = Engine::start(EngineConfig { canvas: (320, 180), output: (320, 180), ..Default::default() }).unwrap();
    let mut doc = Document::new(Workspace::new("Options"));
    let show = Input::new("Slides", "slideshow_v2", json!({}));
    let id = show.id;
    let change = doc.apply(&Command::AddInput { input: show }).unwrap();
    let patches = diff(&change.before, &change.after);
    let workspace = change.after.clone();
    engine.call(move |s| s.reconciler.apply(&workspace, &patches));
    let options = engine.call(move |s| s.reconciler.input_options(id, "transition"));
    eprintln!("{options:?}");
    assert!(options.len() >= 3, "slideshow offers its transitions: {options:?}");
    assert!(options.iter().all(|(label, v)| !label.is_empty() && v.is_string()));
    assert!(engine.call(move |s| s.reconciler.input_options(id, "nope")).is_empty());
    drop(engine);
}
