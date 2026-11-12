//! The project a first launch starts with (until onboarding generates one from the user's
//! devices; docs/05-interface-shell.md, "Onboarding").

use lightair_core::{Command, Input, Layer, Layout, Project, Scene, Track};
use serde_json::json;

pub fn starter_project() -> Project {
    let mut p = Project::new("My Show");
    let format = p.primary_format().clone();
    let (w, h) = (format.width as f32, format.height as f32);

    let background = Input::new(
        "Background",
        "color_source_v3",
        json!({"color": 0xFF1B1B1Cu32, "width": format.width, "height": format.height}),
    );
    let mut display_settings = json!({"type": 0, "show_cursor": true, "hide_obs": true});
    if let Some(uuid) = lightair_engine::displays::main_display_uuid() {
        display_settings["display_uuid"] = uuid.into();
    }
    let display = Input::new("Main display", "screen_capture", display_settings);
    let icon_path = brand_icon();
    let logo = Input::new("Lightair logo", "image_source", json!({"file": icon_path.to_string_lossy()}));

    let mut main = Scene::new("Main");
    main.layers = vec![
        Layer::new("Background", background.id, [(format.id, Layout::boxed(0.0, 0.0, w, h))]),
        Layer::new("Main display", display.id, [(format.id, Layout::boxed(w * 0.05, h * 0.08, w * 0.9, h * 0.84))]),
        Layer::new("Logo", logo.id, [(format.id, Layout::boxed(w - 200.0, h - 200.0, 160.0, 160.0))]),
    ];
    let mut soon = Scene::new("Starting Soon");
    soon.layers = vec![
        Layer::new("Background", background.id, [(format.id, Layout::boxed(0.0, 0.0, w, h))]),
        Layer::new("Logo", logo.id, [(format.id, Layout::boxed(w / 2.0 - 160.0, h / 2.0 - 160.0, 320.0, 320.0))]),
    ];
    let mut brb = Scene::new("Be Right Back");
    brb.layers = vec![
        Layer::new("Background", background.id, [(format.id, Layout::boxed(0.0, 0.0, w, h))]),
        Layer::new("Logo", logo.id, [(format.id, Layout::boxed(80.0, h - 240.0, 160.0, 160.0))]),
    ];

    let mic = Input::new("Mic", "coreaudio_input_capture", json!({"device_id": "default"}));
    let desktop = Input::new("Desktop audio", "sck_audio_capture", json!({"type": 0}));
    let mut mic_track = Track::new("Mic", mic.id);
    mic_track.effects = crate::mixer::voice_clarity();
    let desktop_track = Track::new("Desktop audio", desktop.id);

    for cmd in [
        Command::AddInput { input: background },
        Command::AddInput { input: display },
        Command::AddInput { input: logo },
        Command::AddScene { scene: soon, index: None },
        Command::AddScene { scene: main, index: None },
        Command::AddScene { scene: brb, index: None },
        Command::AddInput { input: mic },
        Command::AddInput { input: desktop },
        Command::AddTrack { track: mic_track, index: None },
        Command::AddTrack { track: desktop_track, index: None },
    ] {
        cmd.apply(&mut p).expect("starter project is valid");
    }
    p
}

/// A starter with no capture sources (colour, text, a timer and silent tracks), so it never
/// triggers a camera, screen or microphone permission prompt. Used for screenshots and demos
/// with `LIGHTAIR_STARTER=plain`.
pub fn plain_project() -> Project {
    let mut p = Project::new("Demo");
    let format = p.primary_format().clone();
    let (w, h) = (format.width as f32, format.height as f32);
    let bg = Input::new("Background", "color_source_v3", json!({"color": 0xFF2A1A12u32, "width": w, "height": h}));
    let card = Input::new("Card", "color_source_v3", json!({"color": 0xFF3B4AFFu32, "width": 960, "height": 540}));
    let title = Input::new(
        "Title",
        "text_ft2_source_v2",
        json!({"text": "Lightair", "font": {"face": "Helvetica Neue", "size": 144, "flags": 1}, "color1": 0xFFFFFFFFu32, "color2": 0xFFFFFFFFu32}),
    );
    let timer = Input::new(
        "Countdown",
        lightair_engine::timers::TIMER_KIND,
        json!({"mode": "countdown", "seconds": 300, "font": {"face": "Helvetica Neue", "size": 96, "flags": 0}, "color1": 0xFFFFFFFFu32, "color2": 0xFFFFFFFFu32}),
    );
    let icon = brand_icon();
    let logo = Input::new("Logo", "image_source", json!({"file": icon.to_string_lossy()}));
    let f = format.id;
    let mut main = Scene::new("Main");
    main.layers.push(Layer::new("Background", bg.id, [(f, Layout::boxed(0.0, 0.0, w, h))]));
    main.layers.push(Layer::new("Card", card.id, [(f, Layout::boxed(480.0, 270.0, 960.0, 540.0))]));
    main.layers.push(Layer::new("Title", title.id, [(f, Layout::boxed(660.0, 440.0, 600.0, 200.0))]));
    let mut soon = Scene::new("Starting Soon");
    soon.layers.push(Layer::new("Background", bg.id, [(f, Layout::boxed(0.0, 0.0, w, h))]));
    soon.layers.push(Layer::new("Countdown", timer.id, [(f, Layout::boxed(760.0, 470.0, 400.0, 140.0))]));
    let logo_box = Layout {
        shape: Some(lightair_core::Shape::new(lightair_core::ShapeKind::Circle)),
        crop: lightair_core::Crop { left: 64, top: 64, right: 64, bottom: 64 },
        tilt_y: 20.0,
        ..Layout::boxed(1500.0, 640.0, 360.0, 360.0)
    };
    soon.layers.push(Layer::new("Logo", logo.id, [(f, logo_box)]));
    let tone = |name: &str| Input::new(name, "color_source_v3", json!({}));
    let (a, b) = (tone("Mic (demo)"), tone("Music (demo)"));
    let tracks = [Track::new("Mic", a.id), Track::new("Music", b.id)];
    let mut commands = vec![
        Command::AddInput { input: bg },
        Command::AddInput { input: card },
        Command::AddInput { input: title },
        Command::AddInput { input: timer },
        Command::AddInput { input: logo },
        Command::AddInput { input: a },
        Command::AddInput { input: b },
        Command::AddScene { scene: soon, index: None },
        Command::AddScene { scene: main, index: None },
    ];
    commands.extend(tracks.into_iter().map(|track| Command::AddTrack { track, index: None }));
    for c in commands {
        c.apply(&mut p).expect("plain starter is valid");
    }
    p
}

/// The brand icon in the repository, found from the running executable whether it runs from
/// `target/debug` or inside the development app bundle.
fn brand_icon() -> std::path::PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|exe| {
            exe.ancestors().map(|a| a.join("branding/final/icon/lightair-icon-512.png")).find(|p| p.exists())
        })
        .unwrap_or_default()
}
