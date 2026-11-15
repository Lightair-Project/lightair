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
        "virtual-camera" => virtual_camera(engine),
        "web" => web(engine),
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
    // LIGHTAIR_SET_LATE=1: turned on while the camera runs (as a scene change does)
    let late = std::env::var("LIGHTAIR_SET_LATE").is_ok_and(|v| v == "1");
    if was_on == Some(false) && !late {
        println!("SELFTEST turn backgrounds on: {} -> {:?}", sys::set_backgrounds_on(true), sys::backgrounds_on());
    }
    let before = sys::background_picture();
    println!("SELFTEST picture before: {before:?}");
    // LIGHTAIR_KEY=blue: a blue screen
    let blue = std::env::var("LIGHTAIR_KEY").is_ok_and(|k| k == "blue");
    if !late {
        println!("SELFTEST set {}: {}", if blue { "blue" } else { "green" }, sys::set_background_colour(if blue { sys::BLUE } else { sys::GREEN }));
    }
    println!("SELFTEST picture now: {:?}", sys::background_picture());
    let report = engine.call(move |s| {
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
        look.set_int("bg_key", i64::from(blue));
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
    if late {
        std::thread::sleep(Duration::from_secs(3));
        println!("SELFTEST late: colour {} on {}", sys::set_background_colour(if blue { sys::BLUE } else { sys::GREEN }), sys::set_backgrounds_on(true));
    }
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
            } else if (cb < 100 && cr < 100) || (blue && cb > 170 && cr < 110) {
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
            "SELFTEST frame: {:.0}% keyed out (magenta shows), {:.1}% key colour left, {:.0}% neither (the person)",
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

/// `LIGHTAIR_SELFTEST=virtual-camera`: the virtual camera started on a magenta canvas, then
/// the OBS Virtual Camera opened as a camera here: what comes back should be that magenta.
fn virtual_camera(engine: &Engine) {
    let started = engine.call(|s| {
        let magenta = Data::from_json(&serde_json::json!({"color": 0xFFFF00FFu32, "width": 1920, "height": 1080}));
        let back = ObsSource::create("color_source_v3", "Self-test magenta", Some(&magenta)).unwrap();
        let scene = ObsScene::create("Self-test").unwrap();
        scene.add(&back);
        s.sources.insert("back".into(), back);
        s.scenes.insert("selftest".into(), scene);
        s.set_program("selftest");
        s.outputs.start_virtual_camera(None, None)
    });
    println!("SELFTEST start: {started:?}");
    std::thread::sleep(Duration::from_secs(1));
    println!("SELFTEST state: {:?}", lightair_engine::outputs::runtime().get(lightair_engine::outputs::OutputKey::VirtualCamera).state);
    // the virtual camera's device: Lightair's own when installed, else OBS's (by the UUID its
    // plugin declares)
    let ours = "2DF109AC-DE07-42DF-A0D7-098033FFE986".to_string();
    let uuid = if lightair_engine::kinds::cameras().iter().any(|(_, u)| u.eq_ignore_ascii_case(&ours)) {
        ours
    } else {
        std::process::Command::new("defaults")
            .args(["read", "/Applications/OBS.app/Contents/PlugIns/mac-virtualcam.plugin/Contents/Info", "OBSCameraDeviceUUID"])
            .output()
            .ok()
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .unwrap_or_default()
    };
    let found = engine.call(move |s| {
        let devices = lightair_engine::kinds::cameras();
        let Some((name, uid)) = devices.into_iter().find(|(_, u)| u.eq_ignore_ascii_case(&uuid)) else {
            return format!("no camera with uid {uuid}");
        };
        let (kind, settings) = lightair_engine::kinds::resolve(lightair_core::kinds::CAMERA, &serde_json::json!({ "device": uid })).unwrap();
        let cam = ObsSource::create(kind, "Self-test virtual camera in", Some(&Data::from_json(&settings))).unwrap();
        // read back on a canvas of its own (on the program it would loop into itself)
        let canvas = lightair_engine::obs::ObsCanvas::create("Self-test readback", 640, 360, 30, 1).unwrap();
        let scene = canvas.create_scene("Self-test in").unwrap();
        scene.add(&cam);
        unsafe { lightair_engine::sys::obs_canvas_set_channel(canvas.as_ptr(), 0, scene.source_ptr()) };
        s.sources.insert("cam".into(), cam);
        s.scenes.insert("selftest-in".into(), scene);
        let ptr = canvas.as_ptr() as usize;
        std::mem::forget(canvas);
        format!("{name}\u{1f}{ptr}")
    });
    let (found, canvas) = found.split_once('\u{1f}').map_or((found.clone(), 0usize), |(a, b)| (a.to_string(), b.parse().unwrap_or(0)));
    println!("SELFTEST reading back from: {found}");
    std::thread::sleep(Duration::from_secs(4));
    let (w, h) = engine.call(|s| s.sources.get("cam").map_or((0, 0), |c| (c.width(), c.height())));
    println!("SELFTEST frames back: {w}x{h}");
    if canvas != 0 {
        let preview = engine.call(move |_| lightair_preview::Preview::attach_canvas(canvas as *mut lightair_engine::sys::obs_canvas_t));
        std::thread::sleep(Duration::from_secs(1));
        match preview.latest_frame() {
            Some(f) => {
                let at = |x: f32, y: f32| lightair_preview::sample_ycbcr(&f, x, y);
                println!("SELFTEST colour back (YCbCr, magenta is about 73/227/244): centre {:?} corner {:?}", at(0.5, 0.5), at(0.1, 0.1));
            }
            None => println!("SELFTEST colour back: no frame"),
        }
        // one scene, not the one on air: a green scene while magenta stays on air
        engine.call(|s| {
            s.outputs.stop_virtual_camera();
            let green = Data::from_json(&serde_json::json!({"color": 0xFF00FF00u32, "width": 1920, "height": 1080}));
            let src = ObsSource::create("color_source_v3", "Self-test green", Some(&green)).unwrap();
            let scene = ObsScene::create("Self-test green scene").unwrap();
            scene.add(&src);
            let pick = lightair_engine::outputs::VirtualCameraScene { source: scene.source_ptr(), width: 1920, height: 1080, fps_num: 30, fps_den: 1 };
            s.sources.insert("green".into(), src);
            s.scenes.insert("green".into(), scene);
            println!("SELFTEST start on a scene: {:?}", s.outputs.start_virtual_camera(None, Some(pick)));
        });
        std::thread::sleep(Duration::from_secs(2));
        match preview.latest_frame() {
            Some(f) => {
                let at = |x: f32, y: f32| lightair_preview::sample_ycbcr(&f, x, y);
                println!("SELFTEST scene back (YCbCr, green is about 182/30/12): centre {:?}", at(0.5, 0.5));
            }
            None => println!("SELFTEST scene back: no frame"),
        }
        drop(preview);
    }
    engine.call(|s| {
        s.outputs.stop_virtual_camera();
        s.sources.remove("cam");
    });
    std::thread::sleep(Duration::from_millis(500));
    println!("SELFTEST stopped: {:?}", lightair_engine::outputs::runtime().get(lightair_engine::outputs::OutputKey::VirtualCamera).state);
}

/// `LIGHTAIR_SELFTEST=web`: a web page (magenta) on air, Chromium driven from this thread; the
/// program's colour.
fn web(engine: &Engine) {
    // the application object Chromium works with on macOS (the window loop makes it otherwise)
    #[cfg(target_os = "macos")]
    {
        use objc2::msg_send;
        use objc2::runtime::{AnyClass, AnyObject};
        let _app: *mut AnyObject = unsafe { msg_send![AnyClass::get(c"NSApplication").unwrap(), sharedApplication] };
    }
    println!("SELFTEST Chromium bundled: {}", lightair_web::available());
    engine.call(|s| {
        let url = "data:text/html,<html><body style='margin:0;background:%23ff00ff'></body></html>";
        let settings = Data::from_json(&serde_json::json!({"url": url, "width": 1920, "height": 1080, "fps": 30}));
        let src = ObsSource::create(lightair_engine::web::KIND, "Self-test page", Some(&settings)).unwrap();
        let scene = ObsScene::create("Self-test").unwrap();
        scene.add(&src);
        s.sources.insert("page".into(), src);
        s.scenes.insert("selftest".into(), scene);
        s.set_program("selftest");
    });
    let preview = engine.call(|_| lightair_preview::Preview::attach());
    let start = std::time::Instant::now();
    let mut next = 1;
    while start.elapsed() < Duration::from_secs(8) {
        lightair_web::pump();
        std::thread::sleep(Duration::from_millis(8));
        if start.elapsed() >= Duration::from_secs(next) {
            next += 1;
            let (w, h) = engine.call(|s| s.sources.get("page").map_or((0, 0), |c| (c.width(), c.height())));
            let colour = preview.latest_frame().and_then(|f| lightair_preview::sample_ycbcr(&f, 0.3, 0.3));
            println!("SELFTEST {} s: page {w}x{h}, program YCbCr {colour:?} (magenta is about 73/227/244)", next - 1);
        }
    }
}
