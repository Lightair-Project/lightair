//! Camera Studio in the Inspector (docs/09-camera-studio.md): on/off, Looks, and the main skin
//! controls for the selected camera. The pipeline itself is the `lightair_camera_studio` libobs
//! filter (modules/lightair-camera-fx), attached as an input effect.

use std::path::PathBuf;
use std::sync::OnceLock;

use gpui_kit::component::Sizable;
use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;
use lightair_core::{Command, Effect, Input};
use serde_json::{Value, json};

use crate::shell::StudioWindow;
use crate::ui::{self, tok};

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
const SKIN: &[(&str, &str, i64, bool)] = &[
    ("smooth", "Smooth", 50, false),
    ("foundation", "Foundation", 20, false),
    ("skin_tone", "Skin tone", 0, true),
    ("redness", "Redness", 0, false),
    ("shadows", "Dark circles", 0, false),
    ("eyelight", "Bright eyes", 30, false),
];

impl StudioWindow {
    /// The Camera Studio section for `input`, if it has the effect.
    pub(crate) fn camera_studio_section(&self, input: &Input, cx: &mut Context<Self>) -> Option<AnyElement> {
        let effect = input.effects.iter().find(|e| e.kind == KIND)?.clone();
        let (iid, eid, on) = (input.id, effect.id, effect.enabled);
        let current_look = effect.settings.get("lightair_look").and_then(|v| v.as_str()).map(str::to_string);

        let toggle = div()
            .id("cs-toggle")
            .w(px(30.))
            .h(px(18.))
            .rounded_full()
            .cursor_pointer()
            .bg(if on { tok::interactive() } else { tok::border_strong() })
            .child(div().size(px(14.)).m(px(2.)).rounded_full().bg(tok::bg_app()).when(on, |d| d.ml(px(14.))))
            .on_click(cx.listener(move |this, _, _, cx| {
                this.exec(Command::SetInputEffectEnabled { input: iid, effect: eid, enabled: !on }, cx)
            }));

        let mut body = div().flex().flex_col().gap_2().p_2().rounded(px(10.)).bg(tok::bg_raised()).child(
            div()
                .flex()
                .items_center()
                .gap_2()
                .child(ui::icon("sparkles").size(px(15.)).text_color(tok::text()))
                .child(div().flex_1().font_weight(FontWeight::SEMIBOLD).child("Camera Studio"))
                .child(toggle),
        );

        // Looks: one click sets every control (and turns Camera Studio on).
        for (ci, (category, list)) in looks().iter().enumerate() {
            let mut row = div().flex().flex_wrap().gap_1();
            for (li, look) in list.iter().enumerate() {
                let mut settings = look.settings.clone();
                settings["lightair_look"] = look.name.clone().into();
                let label = look.name.clone();
                let selected = current_look.as_deref() == Some(look.name.as_str());
                let b = Button::new(("cs-look", ci * 100 + li)).label(label.clone()).xsmall().on_click(cx.listener(
                    move |this, _, _, cx| {
                        let commands = vec![
                            Command::SetInputEffectSettings { input: iid, effect: eid, settings: settings.clone() },
                            Command::SetInputEffectEnabled { input: iid, effect: eid, enabled: true },
                        ];
                        this.exec(Command::Batch { label: format!("Apply look {label}"), commands }, cx);
                    },
                ));
                row = row.child(if selected { b.primary() } else { b.ghost() });
            }
            body = body
                .child(div().text_size(px(11.)).text_color(tok::text_tertiary()).child(category.clone()))
                .child(row);
        }

        // Skin controls.
        body = body.child(div().text_size(px(11.)).text_color(tok::text_tertiary()).pt_1().child("Skin"));
        for (si, (key, label, default, signed)) in SKIN.iter().enumerate() {
            let value = effect.settings.get(*key).and_then(|v| v.as_i64()).unwrap_or(*default);
            let step = |delta: i64| {
                let mut settings = effect.settings.clone();
                let min = if *signed { -100 } else { 0 };
                settings[*key] = (value + delta).clamp(min, 100).into();
                if let Some(obj) = settings.as_object_mut() {
                    obj.remove("lightair_look");
                }
                Command::SetInputEffectSettings { input: iid, effect: eid, settings }
            };
            let (minus, plus) = (step(-10), step(10));
            body = body.child(
                div()
                    .flex()
                    .items_center()
                    .gap_1()
                    .child(div().w(px(96.)).text_color(tok::text_secondary()).child(*label))
                    .child(
                        Button::new(("cs-minus", si))
                            .label("−")
                            .xsmall()
                            .ghost()
                            .on_click(cx.listener(move |this, _, _, cx| this.exec(minus.clone(), cx))),
                    )
                    .child(div().w(px(40.)).text_center().font_family("Menlo").child(value.to_string()))
                    .child(
                        Button::new(("cs-plus", si))
                            .label("+")
                            .xsmall()
                            .ghost()
                            .on_click(cx.listener(move |this, _, _, cx| this.exec(plus.clone(), cx))),
                    ),
            );
        }
        Some(body.into_any_element())
    }
}

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
