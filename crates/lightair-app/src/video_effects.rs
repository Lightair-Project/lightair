//! The video effect catalog (docs/08-video-effects.md): libobs core filters with friendly names
//! and the handful of controls that matter. An effect lives on the layer's input, so it applies
//! wherever that input appears.

use lightair_core::Effect;
use serde_json::{Value, json};

/// A numeric control on an effect.
#[derive(Debug, Clone, Copy)]
pub struct Param {
    pub key: &'static str,
    pub label: &'static str,
    pub min: f64,
    pub max: f64,
    pub step: f64,
    pub default: f64,
    /// Shown as a percentage of `max` instead of the raw value.
    pub percent: bool,
}

const fn p(key: &'static str, label: &'static str, min: f64, max: f64, step: f64, default: f64) -> Param {
    Param { key, label, min, max, step, default, percent: false }
}

#[derive(Debug, Clone, Copy)]
pub struct EffectDef {
    pub kind: &'static str,
    pub name: &'static str,
    pub icon: &'static str,
    pub params: &'static [Param],
    /// Fixed settings the controls don't cover.
    pub fixed: fn() -> Value,
}

fn none() -> Value {
    json!({})
}

pub const CATALOG: &[EffectDef] = &[
    EffectDef {
        kind: "color_filter_v2",
        name: "Colour correction",
        icon: "sun",
        params: &[
            p("brightness", "Brightness", -1.0, 1.0, 0.05, 0.0),
            p("contrast", "Contrast", -4.0, 4.0, 0.1, 0.0),
            p("saturation", "Saturation", -1.0, 5.0, 0.1, 0.0),
            p("gamma", "Gamma", -3.0, 3.0, 0.1, 0.0),
            p("hue_shift", "Hue", -180.0, 180.0, 5.0, 0.0),
            Param { percent: true, ..p("opacity", "Opacity", 0.0, 1.0, 0.05, 1.0) },
        ],
        fixed: none,
    },
    EffectDef {
        kind: "chroma_key_filter_v2",
        name: "Green screen",
        icon: "scan-face",
        params: &[
            p("similarity", "Similarity", 1.0, 1000.0, 20.0, 400.0),
            p("smoothness", "Smoothness", 1.0, 1000.0, 10.0, 80.0),
            p("spill", "Spill", 1.0, 1000.0, 10.0, 100.0),
        ],
        fixed: || json!({"key_color_type": "green"}),
    },
    EffectDef {
        kind: "luma_key_filter_v2",
        name: "Luma key",
        icon: "contrast",
        params: &[
            Param { percent: true, ..p("luma_min", "Remove darks", 0.0, 1.0, 0.05, 0.0) },
            Param { percent: true, ..p("luma_max", "Keep up to", 0.0, 1.0, 0.05, 1.0) },
        ],
        fixed: none,
    },
    EffectDef {
        kind: "sharpness_filter_v2",
        name: "Sharpen",
        icon: "sparkles",
        params: &[Param { percent: true, ..p("sharpness", "Amount", 0.0, 1.0, 0.02, 0.08) }],
        fixed: none,
    },
    EffectDef {
        kind: "scroll_filter",
        name: "Scroll",
        icon: "move-horizontal",
        params: &[
            p("speed_x", "Horizontal", -500.0, 500.0, 10.0, 50.0),
            p("speed_y", "Vertical", -500.0, 500.0, 10.0, 0.0),
        ],
        fixed: || json!({"loop": true}),
    },
    EffectDef {
        kind: "gpu_delay",
        name: "Delay",
        icon: "timer",
        params: &[p("delay_ms", "Milliseconds", 0.0, 500.0, 10.0, 0.0)],
        fixed: none,
    },
];

pub fn def(kind: &str) -> Option<&'static EffectDef> {
    CATALOG.iter().find(|d| d.kind == kind)
}

/// A new effect with the catalog's defaults.
pub fn create(def: &EffectDef) -> Effect {
    let mut settings = (def.fixed)();
    for p in def.params {
        settings[p.key] = json!(p.default);
    }
    Effect::new(def.name, def.kind, settings)
}

/// A param's current value in `settings`.
pub fn value(settings: &Value, p: &Param) -> f64 {
    settings[p.key].as_f64().unwrap_or(p.default)
}

/// `settings` with a param stepped by `steps`, clamped to its range.
pub fn stepped(settings: &Value, p: &Param, steps: f64) -> Value {
    let mut s = if settings.is_object() { settings.clone() } else { json!({}) };
    let v = ((value(settings, p) + p.step * steps).clamp(p.min, p.max) / p.step).round() * p.step;
    s[p.key] = json!(v.clamp(p.min, p.max));
    s
}

pub fn display(settings: &Value, p: &Param) -> String {
    let v = value(settings, p);
    if p.percent {
        format!("{:.0}%", v / p.max * 100.0)
    } else if p.step >= 1.0 {
        format!("{v:.0}")
    } else {
        format!("{v:.2}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_and_stepping() {
        let cc = def("color_filter_v2").unwrap();
        let e = create(cc);
        assert_eq!(e.settings["saturation"], 0.0);
        let sat = &cc.params[2];
        let s = stepped(&e.settings, sat, -100.0);
        assert_eq!(s["saturation"], -1.0, "clamped to the minimum");
        let s = stepped(&s, sat, 3.0);
        assert!((s["saturation"].as_f64().unwrap() + 0.7).abs() < 1e-9);
        assert_eq!(display(&e.settings, &cc.params[5]), "100%");
        let key = create(def("chroma_key_filter_v2").unwrap());
        assert_eq!(key.settings["key_color_type"], "green");
    }
}
