//! Headless engine smoke test (IMPLEMENTATION.md, M1).
//! libobs is a process-wide singleton, so everything runs in one test.

use std::time::Duration;

use lightair_engine::obs::{Data, Scene, Source};
use lightair_engine::{Engine, EngineConfig};

#[test]
fn engine_starts_renders_and_stops() {
    let _ = tracing_subscriber::fmt().with_env_filter("info,libobs=warn").with_test_writer().try_init();

    // Every default module, even when the test runner skips some elsewhere.
    let engine = Engine::start(EngineConfig {
        canvas: (1280, 720),
        output: (1280, 720),
        allowed_modules: lightair_engine::DEFAULT_MODULES.iter().map(|m| m.to_string()).collect(),
        ..Default::default()
    })
    .expect("engine starts");
    assert!(engine.version.starts_with("33."), "libobs version {}", engine.version);
    for id in ["color_source_v3", "image_source", "display_capture", "av_capture_input_v2"] {
        assert!(engine.capabilities.inputs.iter().any(|i| i == id), "missing input type {id}");
    }
    assert!(engine.capabilities.encoders.iter().any(|e| e.contains("x264")), "x264 encoder missing");
    assert!(!engine.capabilities.modules.iter().any(|m| m.contains("frontend-tools")), "UI module loaded");

    let ok = engine.call(|s| {
        let mut settings = Data::new();
        settings.set_int("color", 0xFF4A3BFF_u32 as i64).set_int("width", 1280).set_int("height", 720);
        let Some(colour) = Source::create("color_source_v3", "Colour", Some(&settings)) else { return false };
        let Some(scene) = Scene::create("Main") else { return false };
        scene.add(&colour);
        s.sources.insert("colour".into(), colour);
        s.scenes.insert("main".into(), scene);
        s.set_program("main")
    });
    assert!(ok, "scene set up");

    std::thread::sleep(Duration::from_millis(600));
    let (stats, width) = engine.call(|s| (s.stats(), s.sources["colour"].width()));
    assert_eq!(width, 1280);
    assert!(stats.total_frames > 10, "rendered {} frames", stats.total_frames);
    eprintln!("stats after 600 ms: {stats:?}");

    drop(engine);
}
