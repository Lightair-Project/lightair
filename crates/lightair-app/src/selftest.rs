//! Headless self-tests run inside the app bundle (so they use Lightair's own macOS privacy
//! permissions). `LIGHTAIR_SELFTEST=camera` opens the first camera with Camera Studio, waits for
//! frames, prints a report to stdout and exits without opening a window.

use std::time::Duration;

use lightair_engine::Engine;
use lightair_engine::obs::{Data, ObsScene, ObsSource};

pub fn run(engine: &Engine, what: &str) {
    match what {
        "camera" => camera(engine),
        other => println!("SELFTEST unknown: {other}"),
    }
}

fn camera(engine: &Engine) {
    let report = engine.call(|s| {
        let devices = lightair_engine::kinds::cameras();
        let Some((name, uid)) = devices.into_iter().find(|(n, _)| !n.contains("Virtual") && !n.contains("VCam")) else {
            return "no camera device".to_string();
        };
        let Some((kind, settings)) = lightair_engine::kinds::resolve(lightair_core::kinds::CAMERA, &serde_json::json!({ "device": uid }))
        else {
            return "no camera source on this platform".into();
        };
        let Some(cam) = ObsSource::create(kind, "Self-test camera", Some(&Data::from_json(&settings))) else {
            return "could not create the camera source".into();
        };
        let mut look = Data::new();
        look.set_int("smooth", 60);
        if let Some(fx) = ObsSource::create_private(crate::camera_studio::KIND, "Self-test Camera Studio", Some(&look)) {
            unsafe { lightair_engine::sys::obs_source_filter_add(cam.as_ptr(), fx.as_ptr()) };
            s.sources.insert("fx".into(), fx);
        }
        let scene = ObsScene::create("Self-test").unwrap();
        scene.add(&cam);
        s.sources.insert("cam".into(), cam);
        s.scenes.insert("selftest".into(), scene);
        s.set_program("selftest");
        name
    });
    println!("SELFTEST camera device: {report}");
    let mut ok = false;
    for i in 1..=10 {
        std::thread::sleep(Duration::from_millis(500));
        let (w, h) = engine.call(|s| s.sources.get("cam").map_or((0, 0), |c| (c.width(), c.height())));
        if w > 0 {
            println!("SELFTEST camera frames: {w}x{h} after {} ms", i * 500);
            ok = true;
            break;
        }
    }
    if !ok {
        println!("SELFTEST camera frames: none after 5 s (camera permission not granted?)");
    }
    let stats = engine.call(|s| s.stats());
    println!("SELFTEST stats: {stats:?}");
    engine.call(|s| {
        if let (Some(cam), Some(fx)) = (s.sources.get("cam"), s.sources.get("fx")) {
            unsafe { lightair_engine::sys::obs_source_filter_remove(cam.as_ptr(), fx.as_ptr()) };
        }
        s.sources.remove("fx");
    });
}
