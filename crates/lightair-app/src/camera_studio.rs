//! Camera Studio's data for the effect stack (docs/plans/effects-revamp.md): the module bundle and
//! its preset collections (makeup looks, stickers, lights, grades, ambience, motion). The face effects themselves are in the
//! catalog (`lightair_core::catalog`); the engine merges them into one `lightair_camera_studio`
//! filter.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::OnceLock;

use lightair_core::catalog::{self, Presets};
use serde_json::{Value, json};

/// The libobs filter all face effects run on.
pub const KIND: &str = catalog::STUDIO_FILTER;

/// Where the Camera Studio module bundle lives: `target/modules/lightair-camera-fx` in
/// development (found from the executable, also inside the development `Lightair.app`; a copy
/// of that app elsewhere, in /Applications say, looks in the checkout it was built from).
pub fn plugin_dir() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    // macOS: the .plugin bundle; Windows: OBS's layout (obs-plugins/64bit, data/obs-plugins)
    let (dir, module) = if cfg!(windows) {
        ("modules/lightair-camera-fx/obs-plugins/64bit", "lightair-camera-fx.dll")
    } else {
        ("modules/lightair-camera-fx", "lightair-camera-fx.plugin")
    };
    let checkout = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target");
    exe.ancestors().chain([checkout.as_path()]).map(|a| a.join(dir)).find(|dir| dir.join(module).exists())
}

/// The module's data folder (presets, shaders, models).
fn data_dir() -> Option<PathBuf> {
    let dir = plugin_dir()?;
    Some(if cfg!(windows) {
        dir.parent()?.parent()?.join("data/obs-plugins/lightair-camera-fx")
    } else {
        dir.join("lightair-camera-fx.plugin/Contents/Resources")
    })
}

fn resource(name: &str) -> Option<Value> {
    let path = data_dir()?.join(name);
    serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()
}

/// Preset names by group ("" when a collection has no groups).
pub type Groups = Vec<(String, Vec<String>)>;

/// A preset collection's names, by group.
pub fn presets(set: Presets) -> &'static [(String, Vec<String>)] {
    static ALL: OnceLock<HashMap<u8, Groups>> = OnceLock::new();
    let all = ALL.get_or_init(|| {
        let names = |v: &Value| -> Vec<String> {
            v.as_array().map(|a| a.iter().filter_map(|x| x["name"].as_str().map(String::from)).collect()).unwrap_or_default()
        };
        let grouped = |file: &str, items: &str| -> Groups {
            resource(file)
                .and_then(|v| {
                    v["categories"].as_array().map(|cs| {
                        cs.iter().map(|c| (c["name"].as_str().unwrap_or("").to_string(), names(&c[items]))).collect()
                    })
                })
                .unwrap_or_default()
        };
        let flat = |file: &str, key: &str| -> Groups {
            resource(file).map(|v| vec![(String::new(), names(&v[key]))]).unwrap_or_default()
        };
        let mut m = HashMap::new();
        m.insert(Presets::Makeup as u8, flat("makeup/presets.json", "presets"));
        m.insert(Presets::Stickers as u8, grouped("effects.json", "effects"));
        m.insert(Presets::Light as u8, flat("lights.json", "lights"));
        m.insert(Presets::Grade as u8, grouped("filters.json", "filters"));
        m.insert(Presets::Ambience as u8, flat("atmosphere.json", "ambience"));
        m.insert(Presets::Motion as u8, flat("atmosphere.json", "motion"));
        m
    });
    all.get(&(set as u8)).map(Vec::as_slice).unwrap_or(&[])
}

/// A makeup look's entry from `makeup/presets.json`.
pub fn makeup_preset(name: &str) -> Option<Value> {
    static PRESETS: OnceLock<Vec<Value>> = OnceLock::new();
    PRESETS
        .get_or_init(|| {
            resource("makeup/presets.json").and_then(|v| v["presets"].as_array().cloned()).unwrap_or_default()
        })
        .iter()
        .find(|p| p["name"] == name)
        .cloned()
}

/// The settings with one key set.
pub fn with(settings: &Value, key: &str, value: Value) -> Value {
    let mut s = if settings.is_object() { settings.clone() } else { json!({}) };
    s[key] = value;
    s
}
