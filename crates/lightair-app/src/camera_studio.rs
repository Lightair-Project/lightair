//! The effect stack's preset collections (docs/plans/effects-revamp.md): makeup looks, stickers,
//! lights, grades, ambience and motion, built in from the Effect Stack's data
//! (`modules/lightair-effect-stack/data`). The face effects themselves are in the catalog
//! (`lightair_model::catalog`); the compositor draws them.

use std::collections::HashMap;
use std::sync::OnceLock;

use lightair_model::catalog::Presets;
use serde_json::{Value, json};

/// The Effect Stack's preset files, by their names in its data folder.
const RESOURCES: &[(&str, &str)] = &[
    ("makeup/presets.json", include_str!("../../../modules/lightair-effect-stack/data/makeup/presets.json")),
    ("effects.json", include_str!("../../../modules/lightair-effect-stack/data/effects.json")),
    ("lights.json", include_str!("../../../modules/lightair-effect-stack/data/lights.json")),
    ("filters.json", include_str!("../../../modules/lightair-effect-stack/data/filters.json")),
    ("atmosphere.json", include_str!("../../../modules/lightair-effect-stack/data/atmosphere.json")),
];

fn resource(name: &str) -> Option<Value> {
    let (_, text) = RESOURCES.iter().find(|(n, _)| *n == name)?;
    serde_json::from_str(text).ok()
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
