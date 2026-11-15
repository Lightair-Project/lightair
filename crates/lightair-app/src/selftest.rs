//! Headless self-tests run inside the app bundle (so they use Lightair's own macOS privacy
//! permissions). `LIGHTAIR_SELFTEST=camera` opens the first camera with Camera Studio, waits for
//! frames, prints a report to stdout and exits without opening a window.
//! `LIGHTAIR_SELFTEST=green-key` checks Background removal on macOS's own effect end to end:
//! Lightair's Backgrounds picture set to green, the camera keyed over magenta, and how much of
//! the frame is keyed out, green or neither (numbers only: the frames are never saved).

use std::time::Duration;

use lightair_engine::Engine;
use lightair_engine::obs::{Data, ObsScene, ObsSource};

pub fn run(engine: &Engine, what: &str) {
    match what {
        "camera" => camera(engine),
        "green-key" => green_key(engine),
        // puts Lightair's Backgrounds back: LIGHTAIR_BG_PICTURE and LIGHTAIR_BG_ON (0/1)
        "restore-backgrounds" => {
            use crate::system_effects as sys;
            if let Ok(p) = std::env::var("LIGHTAIR_BG_PICTURE") {
                sys::set_background_picture(std::path::Path::new(&p));
            }
            if let Ok(on) = std::env::var("LIGHTAIR_BG_ON") {
                sys::set_backgrounds_on(on == "1");
            }
            println!("SELFTEST backgrounds now: {:?} {:?}", sys::backgrounds_on(), sys::background_picture());
        }
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

fn green_key(engine: &Engine) {
    use crate::system_effects as sys;
    let was_on = sys::backgrounds_on();
    println!("SELFTEST backgrounds on for Lightair: {was_on:?}");
    if was_on == Some(false) {
        println!("SELFTEST turn backgrounds on: {} -> {:?}", sys::set_backgrounds_on(true), sys::backgrounds_on());
    }
    let before = sys::background_picture();
    println!("SELFTEST picture before: {before:?}");
    println!("SELFTEST set green: {}", sys::set_background_colour(sys::GREEN));
    println!("SELFTEST picture now: {:?}", sys::background_picture());
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
        look.set_int("bg_mode", 2);
        look.set_int("bg_model", lightair_core::catalog::BG_MODEL_SYSTEM);
        look.set_int("bg_feather", 30);
        if let Some(fx) = ObsSource::create_private(crate::camera_studio::KIND, "Self-test green key", Some(&look)) {
            unsafe { lightair_engine::sys::obs_source_filter_add(cam.as_ptr(), fx.as_ptr()) };
            s.sources.insert("fx".into(), fx);
        }
        let magenta = Data::from_json(&serde_json::json!({"color": 0xFFFF00FFu32, "width": 1920, "height": 1080}));
        let back = ObsSource::create("color_source_v3", "Self-test magenta", Some(&magenta)).unwrap();
        let scene = ObsScene::create("Self-test").unwrap();
        scene.add(&back);
        scene.add(&cam);
        s.sources.insert("back".into(), back);
        s.sources.insert("cam".into(), cam);
        s.scenes.insert("selftest".into(), scene);
        s.set_program("selftest");
        format!("{name}\u{1f}{uid}")
    });
    let (report, uid) = report.split_once('\u{1f}').map_or((report.clone(), String::new()), |(a, b)| (a.to_string(), b.to_string()));
    println!("SELFTEST camera device: {report}");
    let preview = engine.call(|_| lightair_preview::Preview::attach());
    // the camera starting, and macOS's effect warming up
    std::thread::sleep(Duration::from_secs(6));
    let (w, h) = engine.call(|s| s.sources.get("cam").map_or((0, 0), |c| (c.width(), c.height())));
    println!("SELFTEST camera frames: {w}x{h}");
    println!("SELFTEST camera backgrounds (active, format supports): {:?}", sys::camera_backgrounds(&uid));
    if let Some(frame) = preview.latest_frame() {
        // the camera fills the middle of the canvas (it's placed at the top-left at its own size):
        // sample where it is
        let (cw, ch) = (w.min(1920) as f32 / 1920.0, h.min(1080) as f32 / 1080.0);
        let (mut keyed, mut green, mut other) = (0, 0, 0);
        for i in 0..48 * 27 {
            let (fx, fy) = ((i % 48) as f32 / 48.0 + 0.01, (i / 48) as f32 / 27.0 + 0.01);
            let Some((_, cb, cr)) = lightair_preview::sample_ycbcr(&frame, fx * cw, fy * ch) else { continue };
            if cb > 170 && cr > 170 {
                keyed += 1;
            } else if cb < 100 && cr < 100 {
                green += 1;
            } else {
                other += 1;
            }
        }
        // the top corners (most likely background): their mean colour
        let mut sum = (0u32, 0u32, 0u32, 0u32);
        for i in 0..10 * 6 {
            for fx in [(i % 10) as f32 / 10.0 * 0.15, 0.85 + (i % 10) as f32 / 10.0 * 0.15] {
                let fy = (i / 10) as f32 / 6.0 * 0.2;
                if let Some((y, cb, cr)) = lightair_preview::sample_ycbcr(&frame, fx * cw, fy * ch) {
                    sum = (sum.0 + y as u32, sum.1 + cb as u32, sum.2 + cr as u32, sum.3 + 1);
                }
            }
        }
        let k = sum.3.max(1);
        println!("SELFTEST top corners mean YCbCr: ({}, {}, {})", sum.0 / k, sum.1 / k, sum.2 / k);
        let n = (keyed + green + other).max(1) as f32;
        println!(
            "SELFTEST frame: {:.0}% keyed out (magenta shows), {:.1}% green left, {:.0}% neither (the person)",
            keyed as f32 * 100.0 / n,
            green as f32 * 100.0 / n,
            other as f32 * 100.0 / n
        );
    } else {
        println!("SELFTEST frame: none");
    }
    drop(preview);
    engine.call(|s| {
        if let (Some(cam), Some(fx)) = (s.sources.get("cam"), s.sources.get("fx")) {
            unsafe { lightair_engine::sys::obs_source_filter_remove(cam.as_ptr(), fx.as_ptr()) };
        }
        s.sources.remove("fx");
    });
    // the picture as it was (the app sets it again while a camera keys the green)
    if let Some(b) = before {
        sys::set_background_picture(std::path::Path::new(&b));
    }
    println!("SELFTEST picture restored: {:?}", sys::background_picture());
    if was_on == Some(false) {
        sys::set_backgrounds_on(false);
        println!("SELFTEST backgrounds back off: {:?}", sys::backgrounds_on());
    }
}
