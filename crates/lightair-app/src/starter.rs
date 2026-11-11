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
    let icon_path = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.ancestors().nth(3).map(|root| root.join("branding/final/icon/lightair-icon-512.png")))
        .filter(|p| p.exists())
        .unwrap_or_default();
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
