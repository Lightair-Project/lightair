//! Camera Studio's data for the effect stack (docs/plans/effects-revamp.md): the module bundle,
//! its preset collections (makeup looks, stickers, lights, grades, ambience, motion) and the
//! looks, which set a source's face effects as a whole. The face effects themselves are in the
//! catalog (`lightair_core::catalog`); the engine merges them into one `lightair_camera_studio`
//! filter.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::OnceLock;

use lightair_core::catalog::{self, Presets};
use lightair_core::{Command, Effect, Input};
use serde_json::{Value, json};

/// The libobs filter all face effects run on.
pub const KIND: &str = catalog::STUDIO_FILTER;

/// Where the Camera Studio module bundle lives: `target/modules/lightair-camera-fx` in
/// development (found from the executable, also inside the development `Lightair.app`).
pub fn plugin_dir() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    exe.ancestors()
        .map(|a| a.join("modules/lightair-camera-fx"))
        .find(|dir| dir.join("lightair-camera-fx.plugin").exists())
}

fn resource(name: &str) -> Option<Value> {
    let path = plugin_dir()?.join("lightair-camera-fx.plugin/Contents/Resources").join(name);
    serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()
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
        let Some(v) = resource("looks.json") else { return vec![] };
        let names = |v: &Value| -> Vec<Look> {
            v.as_array()
                .map(|ls| {
                    ls.iter()
                        .map(|l| Look { name: l["name"].as_str().unwrap_or("").into(), settings: l["settings"].clone() })
                        .collect()
                })
                .unwrap_or_default()
        };
        v["categories"]
            .as_array()
            .map(|cats| cats.iter().map(|c| (c["name"].as_str().unwrap_or("").to_string(), names(&c["looks"]))).collect())
            .unwrap_or_default()
    })
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

/// The face effects a look is made of.
pub fn look_effects(look: &Look) -> Vec<Effect> {
    let as_studio = Effect::new(look.name.clone(), KIND, look.settings.clone());
    let mut parts = catalog::split_camera_studio(&as_studio).unwrap_or_default();
    // A look's makeup is the look's own values, tweakable.
    for e in parts.iter_mut().filter(|e| e.kind == "studio.beauty") {
        if let Some(p) = e.settings["makeup_preset"].as_str().and_then(makeup_preset)
            && let Value::Object(s) = &mut e.settings
        {
            s.extend(catalog::makeup_values(&p));
        }
    }
    parts
}

/// The face effects a look replaces (it is a whole face look; background, motion and ambience
/// it doesn't set stay).
const LOOK_KINDS: &[&str] = &["studio.beauty", "studio.light", "studio.grade"];

/// Applies a look to an input: its face effects in place of the ones it covers.
pub fn apply_look(input: &Input, look: &Look) -> Command {
    let parts = look_effects(look);
    let covers = |kind: &str| LOOK_KINDS.contains(&kind) || parts.iter().any(|p| p.kind == kind);
    let at = input.effects.iter().position(|e| covers(&e.kind)).unwrap_or(0);
    let mut commands: Vec<Command> = input
        .effects
        .iter()
        .filter(|e| covers(&e.kind))
        .map(|e| Command::RemoveInputEffect { input: input.id, effect: e.id })
        .collect();
    let removed_before = input.effects.iter().take(at).filter(|e| covers(&e.kind)).count();
    for (n, effect) in parts.into_iter().enumerate() {
        commands.push(Command::AddInputEffect { input: input.id, effect, index: Some(at - removed_before + n) });
    }
    Command::Batch { label: format!("Apply look {}", look.name), commands }
}

/// The look a stack matches exactly, if any (for showing which look is on).
pub fn current_look(input: &Input) -> Option<&'static str> {
    let studio = catalog::studio_settings(&input.effects);
    let face = |e: &&Effect| LOOK_KINDS.contains(&e.kind.as_str());
    looks().iter().flat_map(|(_, ls)| ls).find_map(|l| {
        let parts = look_effects(l);
        let mine: Vec<_> = input.effects.iter().filter(face).filter(|e| e.enabled).collect();
        let same_kinds = mine.len() == parts.iter().filter(|p| LOOK_KINDS.contains(&p.kind.as_str())).count();
        let same = same_kinds && catalog::studio_settings(&parts).iter().all(|(k, v)| studio.get(k) == Some(v));
        same.then_some(l.name.as_str())
    })
}

pub fn is_camera(kind: &str) -> bool {
    matches!(kind, "macos-avcapture" | "macos-avcapture-fast" | "av_capture_input_v2" | "av_capture_input")
}

/// The settings with one key set.
pub fn with(settings: &Value, key: &str, value: Value) -> Value {
    let mut s = if settings.is_object() { settings.clone() } else { json!({}) };
    s[key] = value;
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_look_replaces_the_face_effects_it_covers() {
        let mut input = Input::new("Cam", "av_capture_input_v2", json!({}));
        let blur = catalog::def("studio.bg_blur").unwrap().create();
        let skin = catalog::def("studio.beauty").unwrap().create();
        input.effects = vec![blur.clone(), skin.clone()];
        let look = Look {
            name: "Test".into(),
            settings: json!({"smooth": 40, "light_preset": "Soft Box", "light_amt": 30}),
        };
        let Command::Batch { commands, .. } = apply_look(&input, &look) else { panic!() };
        let removed: Vec<_> = commands
            .iter()
            .filter_map(|c| if let Command::RemoveInputEffect { effect, .. } = c { Some(*effect) } else { None })
            .collect();
        assert_eq!(removed, [skin.id], "background blur stays");
        let added: Vec<_> = commands
            .iter()
            .filter_map(|c| match c {
                Command::AddInputEffect { effect, index, .. } => Some((effect.kind.as_str(), *index)),
                _ => None,
            })
            .collect();
        assert_eq!(added, [("studio.beauty", Some(1)), ("studio.light", Some(2))]);
    }
}
