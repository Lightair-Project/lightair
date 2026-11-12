//! Camera Studio (docs/09-camera-studio.md): the effect, its looks, and helpers. The pipeline
//! itself is the `lightair_camera_studio` libobs filter (modules/lightair-camera-fx), attached as
//! an input effect; the Inspector's section lives in `shell/inspector.rs`.

use std::path::PathBuf;
use std::sync::OnceLock;

use lightair_core::{Command, Effect, Input};
use serde_json::{Value, json};

pub const KIND: &str = "lightair_camera_studio";

/// The effect new camera inputs get: present but off until a look is chosen.
pub fn default_effect() -> Effect {
    let mut e = Effect::new("Camera Studio", KIND, json!({}));
    e.enabled = false;
    e
}

/// Where the Camera Studio module bundle lives: `target/modules/lightair-camera-fx` in
/// development (found from the executable, also inside the development `Lightair.app`).
pub fn plugin_dir() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    exe.ancestors()
        .map(|a| a.join("modules/lightair-camera-fx"))
        .find(|dir| dir.join("lightair-camera-fx.plugin").exists())
}

#[derive(Clone)]
pub struct Look {
    pub name: String,
    pub settings: Value,
}

/// Looks by category, from the module's `looks.json`.
pub fn looks() -> &'static [(String, Vec<Look>)] {
    static LOOKS: OnceLock<Vec<(String, Vec<Look>)>> = OnceLock::new();
    LOOKS.get_or_init(|| {
        let Some(path) = plugin_dir().map(|d| d.join("lightair-camera-fx.plugin/Contents/Resources/looks.json")) else {
            return vec![];
        };
        let Ok(text) = std::fs::read_to_string(path) else { return vec![] };
        let Ok(v) = serde_json::from_str::<Value>(&text) else { return vec![] };
        v["categories"]
            .as_array()
            .map(|cats| {
                cats.iter()
                    .map(|c| {
                        let looks = c["looks"]
                            .as_array()
                            .map(|ls| {
                                ls.iter()
                                    .map(|l| Look {
                                        name: l["name"].as_str().unwrap_or("").into(),
                                        settings: l["settings"].clone(),
                                    })
                                    .collect()
                            })
                            .unwrap_or_default();
                        (c["name"].as_str().unwrap_or("").to_string(), looks)
                    })
                    .collect()
            })
            .unwrap_or_default()
    })
}

/// (key, label, default, signed)
/// Adds Camera Studio to an input that doesn't have it yet.
pub fn ensure_effect(input: &Input) -> Option<Command> {
    (!input.effects.iter().any(|e| e.kind == KIND)).then(|| Command::AddInputEffect {
        input: input.id,
        effect: default_effect(),
        index: Some(0),
    })
}

pub fn is_camera(kind: &str) -> bool {
    matches!(kind, "macos-avcapture" | "macos-avcapture-fast" | "av_capture_input_v2" | "av_capture_input")
}
