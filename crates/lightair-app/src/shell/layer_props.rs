//! Inspector sections for what a layer shows (docs/07-scenes-layers-canvas.md): media
//! transport, text content and style, colour. Content edits are input settings changes, so
//! they're undoable and apply wherever the input appears.

use std::time::{Duration, Instant};

use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::input::{Input as TextInput, InputEvent, InputState};
use gpui_kit::component::{Disableable, Sizable};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;
use lightair_core::{Command, Id, Input};
use lightair_engine::media::{MediaAction, MediaState, MediaStatus};
use lightair_engine::timers::{TIMER_KIND, TimerAction, TimerStatus};
use serde_json::{Value, json};

use super::StudioWindow;
use crate::ui::tok;

/// The text field for the selected text layer.
pub(super) struct TextEdit {
    input: Id,
    state: Entity<InputState>,
    _events: Subscription,
}

/// Inspector state for content sections.
#[derive(Default)]
pub(super) struct LayerProps {
    text: Option<TextEdit>,
    media: Option<(Id, MediaStatus, Instant)>,
    /// Effects section: the add menu is open; the effect whose controls are shown.
    effect_menu: bool,
    open_effect: Option<Id>,
    timer: Option<(Id, TimerStatus, Instant)>,
}

/// The shape mask filter in Lightair's libobs module.
pub(super) const SHAPE_MASK: &str = "lightair_shape_mask";

fn is_audio_only(kind: &str) -> bool {
    kind.contains("audio") || kind.contains("input_capture")
}

pub(super) fn is_text(kind: &str) -> bool {
    kind.starts_with("text_ft2_source") || kind.starts_with("text_gdiplus")
}
pub(super) fn is_colour(kind: &str) -> bool {
    kind.starts_with("color_source")
}
pub(super) fn is_media(kind: &str) -> bool {
    kind == "ffmpeg_source" || kind == "vlc_source"
}

/// libobs colours are 0xAABBGGRR.
fn abgr(rgb: u32) -> u32 {
    0xFF00_0000 | ((rgb & 0xFF) << 16) | (rgb & 0xFF00) | ((rgb >> 16) & 0xFF)
}
fn rgb_of(abgr: u32) -> u32 {
    ((abgr & 0xFF) << 16) | (abgr & 0xFF00) | ((abgr >> 16) & 0xFF)
}

/// Swatches offered for text and colour layers (RGB).
const PALETTE: &[u32] = &[0xFFFFFF, 0x0B0B0F, 0xFF4A3B, 0xFFB020, 0x2BD67B, 0x3B82F6, 0x8B5CF6, 0xF472B6];

/// `settings` with `key` set (a merged copy: the reconciler updates libobs from the whole object).
fn with(settings: &Value, key: &str, value: Value) -> Value {
    let mut s = settings.clone();
    if !s.is_object() {
        s = json!({});
    }
    s[key] = value;
    s
}

fn clock(ms: i64) -> String {
    let s = ms.max(0) / 1000;
    format!("{}:{:02}", s / 60, s % 60)
}

impl StudioWindow {
    /// Keeps the text field and the media status in step with the selection. Called from render.
    pub(super) fn sync_layer_props(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let studio = self.studio.read(cx);
        let project = studio.project().clone();
        let input = studio
            .selected_scene
            .and_then(|s| project.scene(s))
            .and_then(|s| studio.selected_layer.and_then(|l| s.layer(l)))
            .and_then(|l| project.inputs.get(&l.input))
            .cloned();
        let engine = studio.engine().clone();

        // Text field: one per selected text input; refreshed when the text changes elsewhere
        // (undo) and the field isn't being typed in.
        match input.as_ref().filter(|i| is_text(&i.kind)) {
            Some(i) => {
                let text = i.settings["text"].as_str().unwrap_or("").to_string();
                let same = self.props.text.as_ref().is_some_and(|t| t.input == i.id);
                if !same {
                    let state = cx.new(|cx| InputState::new(window, cx));
                    let id = i.id;
                    let events = cx.subscribe_in(&state, window, move |this, state, ev: &InputEvent, _, cx| {
                        if matches!(ev, InputEvent::PressEnter { .. } | InputEvent::Blur) {
                            let value = state.read(cx).value().to_string();
                            this.set_text(id, value, cx);
                        }
                    });
                    self.props.text = Some(TextEdit { input: i.id, state, _events: events });
                }
                let t = self.props.text.as_ref().unwrap();
                let focused = t.state.read(cx).focus_handle(cx).is_focused(window);
                if !focused && t.state.read(cx).value() != text.as_str() {
                    t.state.update(cx, |s, cx| s.set_value(text, window, cx));
                }
            }
            None => self.props.text = None,
        }

        // Timer status, polled the same way.
        match input.as_ref().filter(|i| i.kind == TIMER_KIND) {
            Some(i) => {
                let stale = self
                    .props
                    .timer
                    .as_ref()
                    .is_none_or(|(id, _, at)| *id != i.id || at.elapsed() > Duration::from_millis(200));
                if stale {
                    let id = i.id;
                    self.props.timer =
                        engine.call(move |s| s.reconciler.timer_status(id)).map(|status| (id, status, Instant::now()));
                }
            }
            None => self.props.timer = None,
        }

        // Media status, polled a few times a second while a media layer is selected.
        match input.as_ref().filter(|i| is_media(&i.kind)) {
            Some(i) => {
                let stale = self
                    .props
                    .media
                    .as_ref()
                    .is_none_or(|(id, _, at)| *id != i.id || at.elapsed() > Duration::from_millis(200));
                if stale {
                    let id = i.id;
                    self.props.media =
                        engine.call(move |s| s.reconciler.media_status(id)).map(|status| (id, status, Instant::now()));
                }
            }
            None => self.props.media = None,
        }
    }

    fn set_text(&mut self, input: Id, text: String, cx: &mut Context<Self>) {
        let project = self.studio.read(cx).project().clone();
        let Some(i) = project.inputs.get(&input) else { return };
        if i.settings["text"].as_str() == Some(text.as_str()) {
            return;
        }
        self.exec(Command::SetInputSettings { input, settings: with(&i.settings, "text", json!(text)) }, cx);
    }

    fn media(&self, input: Id, action: MediaAction, cx: &mut Context<Self>) {
        self.studio.read(cx).engine().post(move |s| s.reconciler.media_action(input, action));
    }

    /// The content section for a layer's input, if its kind has one.
    pub(super) fn content_section(&self, input: &Input, cx: &mut Context<Self>) -> Option<AnyElement> {
        if input.kind == TIMER_KIND {
            Some(div().child(self.timer_section(input, cx)).child(self.text_section(input, cx)).into_any_element())
        } else if is_text(&input.kind) {
            Some(self.text_section(input, cx).into_any_element())
        } else if is_colour(&input.kind) {
            Some(self.colour_section(input, cx).into_any_element())
        } else if is_media(&input.kind) {
            Some(self.media_section(input, cx).into_any_element())
        } else {
            None
        }
    }

    /// Shape: cut the layer into a rounded box, circle or pill, with a soft edge and a border.
    /// (A mask on the input, so it shows wherever the input does.)
    pub(super) fn shape_section(&self, input: &Input, cx: &mut Context<Self>) -> Option<AnyElement> {
        if is_audio_only(&input.kind) || input.kind == lightair_engine::timers::TIMER_KIND || is_text(&input.kind) {
            return None;
        }
        let iid = input.id;
        let mask = input.effects.iter().find(|e| e.kind == SHAPE_MASK).cloned();
        let shape = mask.as_ref().map(|m| m.settings["shape"].as_i64().unwrap_or(0));
        let set_shape = |value: Option<i64>| -> Command {
            match (&mask, value) {
                (None, None) => Command::Batch { label: "No shape".into(), commands: vec![] },
                (Some(m), None) => Command::RemoveInputEffect { input: iid, effect: m.id },
                (Some(m), Some(v)) => Command::SetInputEffectSettings {
                    input: iid,
                    effect: m.id,
                    settings: with(&m.settings, "shape", json!(v)),
                },
                (None, Some(v)) => Command::AddInputEffect {
                    input: iid,
                    effect: lightair_core::Effect::new(
                        "Shape",
                        SHAPE_MASK,
                        json!({"shape": v, "radius": 0.25, "feather": 1.0, "border": 0.0, "border_color": 0xFFFFFFFFu32}),
                    ),
                    index: None,
                },
            }
        };
        let options = vec![
            ("None", set_shape(None), shape.is_none()),
            ("Rounded", set_shape(Some(0)), shape == Some(0)),
            ("Circle", set_shape(Some(1)), shape == Some(1)),
            ("Pill", set_shape(Some(2)), shape == Some(2)),
        ];
        let mut col = self.section("Shape", None, cx).child(self.segmented("shape", options, cx));
        if let Some(m) = mask {
            let (eid, settings) = (m.id, m.settings.clone());
            let row = |label: &'static str, slider: gpui_kit::component::slider::Slider, value: String| {
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(div().w(px(64.)).text_size(px(12.)).text_color(tok::text_secondary()).child(label))
                    .child(div().flex_1().child(slider))
                    .child(
                        div()
                            .w(px(40.))
                            .text_right()
                            .text_size(px(11.))
                            .font_family("Menlo")
                            .text_color(tok::text_tertiary())
                            .child(value),
                    )
            };
            let setter = |key: &'static str| {
                let settings = settings.clone();
                move |v: f32| Command::SetInputEffectSettings {
                    input: iid,
                    effect: eid,
                    settings: with(&settings, key, json!(v)),
                }
            };
            if shape == Some(0) {
                let r = settings["radius"].as_f64().unwrap_or(0.25) as f32;
                let s = self.slider(format!("mask:{eid}:radius"), (0.0, 1.0, 0.01), r, setter("radius"), cx);
                col = col.child(row("Corners", s, format!("{:.0}%", r * 100.0)));
            }
            let feather = settings["feather"].as_f64().unwrap_or(1.0) as f32;
            let s = self.slider(format!("mask:{eid}:feather"), (0.0, 40.0, 1.0), feather, setter("feather"), cx);
            col = col.child(row("Softness", s, format!("{feather:.0} px")));
            let border = settings["border"].as_f64().unwrap_or(0.0) as f32;
            let s = self.slider(format!("mask:{eid}:border"), (0.0, 40.0, 1.0), border, setter("border"), cx);
            col = col.child(row("Border", s, format!("{border:.0} px")));
            if border > 0.0 {
                let current = settings["border_color"].as_u64().map(|c| rgb_of(c as u32));
                let mut swatches = div().flex().flex_wrap().gap_1p5();
                for (i, &rgb) in PALETTE.iter().enumerate() {
                    let cmd = Command::SetInputEffectSettings {
                        input: iid,
                        effect: eid,
                        settings: with(&settings, "border_color", json!(abgr(rgb))),
                    };
                    swatches = swatches.child(
                        div()
                            .id(("border-swatch", i))
                            .size(px(20.))
                            .rounded_full()
                            .cursor_pointer()
                            .bg(rgb_to_hsla(rgb))
                            .border_2()
                            .border_color(if current == Some(rgb) { tok::text() } else { tok::border() })
                            .on_click(cx.listener(move |this, _, _, cx| this.exec(cmd.clone(), cx))),
                    );
                }
                col = col.child(swatches);
            }
        }
        Some(col.into_any_element())
    }

    /// The layer's video effects (except Camera Studio, which has its own section).
    pub(super) fn effects_section(&self, input: &Input, cx: &mut Context<Self>) -> AnyElement {
        use crate::video_effects as fx;
        let iid = input.id;
        let add = Button::new("add-effect")
            .label(if self.props.effect_menu { "Close" } else { "Add" })
            .xsmall()
            .ghost()
            .on_click(cx.listener(|this, _, _, cx| {
                this.props.effect_menu = !this.props.effect_menu;
                cx.notify();
            }));
        let mut col = self.section("Effects", Some(add.into_any_element()), cx);
        if self.props.effect_menu {
            let mut menu = div().flex().flex_wrap().gap_1();
            for (i, d) in fx::CATALOG.iter().enumerate() {
                menu = menu.child(
                    Button::new(("effect-def", i))
                        .label(d.name)
                        .icon(crate::ui::icon(d.icon))
                        .xsmall()
                        .outline()
                        .on_click(cx.listener(move |this, _, _, cx| {
                            let effect = fx::create(&fx::CATALOG[i]);
                            this.props.open_effect = Some(effect.id);
                            this.props.effect_menu = false;
                            this.exec(Command::AddInputEffect { input: iid, effect, index: None }, cx);
                        })),
                );
            }
            col = col.child(menu);
        }
        let shown: Vec<_> = input
            .effects
            .iter()
            .filter(|e| e.kind != crate::camera_studio::KIND && e.kind != SHAPE_MASK)
            .cloned()
            .collect();
        if shown.is_empty() && !self.props.effect_menu {
            col = col.child(div().text_color(tok::text_tertiary()).child("No effects"));
        }
        for (i, e) in shown.into_iter().enumerate() {
            let (eid, enabled) = (e.id, e.enabled);
            let open = self.props.open_effect == Some(eid);
            let def = fx::def(&e.kind);
            col = col.child(
                div()
                    .id(("effect-row", i))
                    .flex()
                    .items_center()
                    .gap_2()
                    .h(px(28.))
                    .px_1()
                    .rounded(px(5.))
                    .cursor_pointer()
                    .hover(|d| d.bg(tok::bg_raised()))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.props.open_effect = if open { None } else { Some(eid) };
                        cx.notify();
                    }))
                    .child(
                        div()
                            .id(("effect-on", i))
                            .size(px(10.))
                            .rounded_full()
                            .bg(if enabled { tok::accent_primary() } else { tok::border_strong() })
                            .on_click(cx.listener(move |this, _, _, cx| {
                                cx.stop_propagation();
                                this.exec(
                                    Command::SetInputEffectEnabled { input: iid, effect: eid, enabled: !enabled },
                                    cx,
                                )
                            })),
                    )
                    .child(
                        div()
                            .flex_1()
                            .text_color(if enabled { tok::text() } else { tok::text_tertiary() })
                            .child(e.name.clone()),
                    )
                    .when(def.is_none(), |d| {
                        d.child(div().text_size(px(10.)).text_color(tok::text_tertiary()).child("plugin"))
                    })
                    .child(crate::ui::icon_button(("effect-remove", i), "trash", tok::text_tertiary()).on_click(
                        cx.listener(move |this, _, _, cx| {
                            cx.stop_propagation();
                            this.exec(Command::RemoveInputEffect { input: iid, effect: eid }, cx)
                        }),
                    )),
            );
            if let (true, Some(def)) = (open, def) {
                for (j, p) in def.params.iter().enumerate() {
                    let (minus, plus) = (fx::stepped(&e.settings, p, -1.0), fx::stepped(&e.settings, p, 1.0));
                    let id = i * 16 + j;
                    col = col.child(
                        div()
                            .flex()
                            .items_center()
                            .gap_1()
                            .pl_4()
                            .child(div().w(px(84.)).text_color(tok::text_secondary()).child(p.label))
                            .child(Button::new(("fx-minus", id)).label("−").xsmall().ghost().on_click(cx.listener(
                                move |this, _, _, cx| {
                                    let settings = minus.clone();
                                    this.exec(Command::SetInputEffectSettings { input: iid, effect: eid, settings }, cx)
                                },
                            )))
                            .child(
                                div().w(px(56.)).text_center().font_family("Menlo").child(fx::display(&e.settings, p)),
                            )
                            .child(Button::new(("fx-plus", id)).label("+").xsmall().ghost().on_click(cx.listener(
                                move |this, _, _, cx| {
                                    let settings = plus.clone();
                                    this.exec(Command::SetInputEffectSettings { input: iid, effect: eid, settings }, cx)
                                },
                            ))),
                    );
                }
            }
        }
        col.into_any_element()
    }

    fn swatches(&self, input: &Input, key: &'static str, keys: &'static [&'static str], cx: &mut Context<Self>) -> Div {
        let current = input.settings[key].as_u64().map(|c| rgb_of(c as u32));
        let mut row = div().flex().flex_wrap().gap_1p5();
        for (i, &rgb) in PALETTE.iter().enumerate() {
            let (id, settings) = (input.id, input.settings.clone());
            row = row.child(
                div()
                    .id(("swatch", i))
                    .size(px(22.))
                    .rounded(px(5.))
                    .cursor_pointer()
                    .bg(rgb_to_hsla(rgb))
                    .border_2()
                    .border_color(if current == Some(rgb) { tok::text() } else { tok::border() })
                    .on_click(cx.listener(move |this, _, _, cx| {
                        let mut s = settings.clone();
                        for k in keys {
                            s = with(&s, k, json!(abgr(rgb)));
                        }
                        this.exec(Command::SetInputSettings { input: id, settings: s }, cx);
                    })),
            );
        }
        row
    }

    fn text_section(&self, input: &Input, cx: &mut Context<Self>) -> impl IntoElement {
        const FONTS: &[&str] = &[
            "Helvetica Neue",
            "Avenir Next",
            "Futura",
            "Gill Sans",
            "Georgia",
            "Didot",
            "Menlo",
            "Impact",
            "Marker Felt",
        ];
        let (id, settings) = (input.id, input.settings.clone());
        let font = if settings["font"].is_object() {
            settings["font"].clone()
        } else {
            json!({"face": "Helvetica Neue", "size": 128, "flags": 0})
        };
        let size = font["size"].as_i64().unwrap_or(128);
        let face = font["face"].as_str().unwrap_or("Helvetica Neue").to_string();
        let flags = font["flags"].as_i64().unwrap_or(0);
        let set_font = {
            let (settings, font) = (settings.clone(), font.clone());
            move |key: &str, value: Value| {
                let mut f = font.clone();
                f[key] = value;
                Command::SetInputSettings { input: id, settings: with(&settings, "font", f) }
            }
        };
        let next_face = FONTS[(FONTS.iter().position(|f| *f == face).map_or(0, |i| i + 1)) % FONTS.len()];
        let cycle_face = set_font("face", json!(next_face));
        let (smaller, bigger) =
            (set_font("size", json!((size - 8).clamp(8, 1024))), set_font("size", json!((size + 8).clamp(8, 1024))));

        // Style toggles: bold and italic are font flags (1, 2); outline and shadow are settings.
        let mut toggles = div().flex().gap_0p5().p_0p5().rounded(px(6.)).bg(tok::bg_raised());
        let toggle = |i: usize, label: &'static str, on: bool, cmd: Command, cx: &mut Context<Self>| {
            div()
                .id(("text-toggle", i))
                .flex_1()
                .text_center()
                .py_1()
                .rounded(px(4.))
                .text_size(px(11.))
                .cursor_pointer()
                .when(on, |d| d.bg(tok::bg_selected()).text_color(tok::text()))
                .when(!on, |d| d.text_color(tok::text_tertiary()))
                .child(label)
                .on_click(cx.listener(move |this, _, _, cx| this.exec(cmd.clone(), cx)))
        };
        for (i, (label, bit)) in [("Bold", 1i64), ("Italic", 2)].into_iter().enumerate() {
            toggles = toggles.child(toggle(i, label, flags & bit != 0, set_font("flags", json!(flags ^ bit)), cx));
        }
        for (i, (label, key)) in [("Outline", "outline"), ("Shadow", "drop_shadow")].into_iter().enumerate() {
            let on = settings[key].as_bool().unwrap_or(false);
            let cmd = Command::SetInputSettings { input: id, settings: with(&settings, key, json!(!on)) };
            toggles = toggles.child(toggle(2 + i, label, on, cmd, cx));
        }

        let font_chip = div()
            .id("text-font")
            .flex()
            .items_center()
            .gap_1()
            .h(px(30.))
            .px_2()
            .rounded(px(6.))
            .bg(tok::bg_raised())
            .cursor_pointer()
            .hover(|d| d.bg(tok::bg_selected()))
            .child(div().flex_1().min_w_0().overflow_hidden().text_ellipsis().text_size(px(12.)).child(face))
            .child(crate::ui::icon("chevrons-up-down").size(px(12.)).text_color(tok::text_tertiary()))
            .on_click(cx.listener(move |this, _, _, cx| this.exec(cycle_face.clone(), cx)));
        self.section("Text", None, cx)
            .when_some(self.props.text.as_ref().filter(|t| t.input == input.id), |d, t| {
                d.child(TextInput::new(&t.state).small())
            })
            .child(div().grid().grid_cols(2).gap_1().child(font_chip).child(self.field(
                "text-size",
                "Aa",
                size.to_string(),
                smaller,
                bigger,
                cx,
            )))
            .child(toggles)
            .child(self.swatches(input, "color1", &["color1", "color2"], cx))
    }

    fn timer_section(&self, input: &Input, cx: &mut Context<Self>) -> impl IntoElement {
        let id = input.id;
        let settings = input.settings.clone();
        let mode = settings["mode"].as_str().unwrap_or("countdown").to_string();
        let seconds = settings["seconds"].as_i64().unwrap_or(300);
        let status = self.props.timer.as_ref().filter(|(i, ..)| *i == id).map(|(_, s, _)| *s);
        let running = status.is_some_and(|s| s.running);
        let set = move |key: &str, value: Value| Command::SetInputSettings {
            input: id,
            settings: with(&settings, key, value),
        };
        let modes = self.segmented(
            "timer-mode",
            [("countdown", "Countdown"), ("stopwatch", "Stopwatch"), ("clock", "Clock")]
                .into_iter()
                .map(|(value, label)| (label, set("mode", json!(value)), mode == value))
                .collect(),
            cx,
        );
        let action = move |a: TimerAction| {
            move |this: &mut Self, _: &ClickEvent, _: &mut Window, cx: &mut Context<Self>| {
                this.studio.read(cx).engine().post(move |s| s.reconciler.timer_action(id, a));
                this.props.timer = None;
                cx.notify();
            }
        };
        let mut col = self.section("Timer", None, cx).child(modes);
        if mode == "countdown" {
            let (shorter, longer) =
                (set("seconds", json!((seconds - 60).max(10))), set("seconds", json!(seconds + 60)));
            // Then: which scene to go to when it ends (click to cycle).
            let project = self.studio.read(cx).project().clone();
            let end = input.settings["end_scene"].as_str().map(str::to_string);
            let current = project.scenes.iter().position(|sc| Some(sc.id.to_string()) == end);
            let next = match current {
                None => project.scenes.first().map(|sc| json!(sc.id.to_string())),
                Some(i) => project.scenes.get(i + 1).map(|sc| json!(sc.id.to_string())),
            }
            .unwrap_or(Value::Null);
            let then = current.map_or("Stay".to_string(), |i| format!("→ {}", project.scenes[i].name));
            let cycle = set("end_scene", next);
            col = col.child(
                div()
                    .grid()
                    .grid_cols(2)
                    .gap_1()
                    .child(self.field(
                        "timer-length",
                        "⏱",
                        format!("{}:{:02}", seconds / 60, seconds % 60),
                        shorter,
                        longer,
                        cx,
                    ))
                    .child(
                        div()
                            .id("timer-end")
                            .flex()
                            .items_center()
                            .gap_1()
                            .h(px(30.))
                            .px_2()
                            .rounded(px(6.))
                            .bg(tok::bg_raised())
                            .cursor_pointer()
                            .hover(|d| d.bg(tok::bg_selected()))
                            .tooltip(|w, cx| {
                                gpui_kit::component::tooltip::Tooltip::new("When it ends (click to change)")
                                    .build(w, cx)
                            })
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .overflow_hidden()
                                    .text_ellipsis()
                                    .text_size(px(12.))
                                    .child(then),
                            )
                            .on_click(cx.listener(move |this, _, _, cx| this.exec(cycle.clone(), cx))),
                    ),
            );
        }
        if mode == "clock" {
            let secs = input.settings["show_seconds"].as_bool().unwrap_or(false);
            col = col.child(self.segmented(
                "clock-seconds",
                vec![
                    ("12:30", set("show_seconds", json!(false)), !secs),
                    ("12:30:45", set("show_seconds", json!(true)), secs),
                ],
                cx,
            ));
        } else {
            let icon_button = |id: &'static str, icon: &'static str, tip: &'static str| {
                crate::ui::icon_button(id, icon, tok::text_secondary())
                    .tooltip(move |w, cx| gpui_kit::component::tooltip::Tooltip::new(tip).build(w, cx))
            };
            col = col.child(
                div()
                    .flex()
                    .gap_1()
                    .child(
                        icon_button(
                            "timer-run",
                            if running { "pause" } else { "play" },
                            if running { "Pause" } else { "Start" },
                        )
                        .on_click(cx.listener(action(if running {
                            TimerAction::Pause
                        } else {
                            TimerAction::Start
                        }))),
                    )
                    .child(
                        icon_button("timer-reset", "rotate-ccw", "Reset")
                            .on_click(cx.listener(action(TimerAction::Reset))),
                    )
                    .child(
                        icon_button("timer-add", "plus", "Add a minute")
                            .on_click(cx.listener(action(TimerAction::Add(60)))),
                    ),
            );
        }
        col
    }

    /// A mixer track: level, mute, and its effect chain (the mixer strip stays compact).
    pub(super) fn track_inspector(
        &self,
        project: &lightair_core::Project,
        track: &lightair_core::Track,
        body: Div,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let tid = track.id;
        let mut effects = div().flex().flex_col().gap_0p5();
        for (j, e) in track.effects.iter().enumerate() {
            let (eid, enabled) = (e.id, e.enabled);
            effects = effects.child(
                div()
                    .id(("track-fx", j))
                    .flex()
                    .items_center()
                    .gap_2()
                    .h(px(30.))
                    .px_2()
                    .rounded(px(5.))
                    .hover(|d| d.bg(tok::bg_raised()))
                    .child(
                        crate::ui::icon_button(
                            ("track-fx-on", j),
                            if enabled { "circle-check" } else { "circle" },
                            if enabled { tok::ok() } else { tok::text_tertiary() },
                        )
                        .tooltip(move |w, cx| {
                            gpui_kit::component::tooltip::Tooltip::new(if enabled { "Bypass" } else { "Turn on" })
                                .build(w, cx)
                        })
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.exec(Command::SetEffectEnabled { track: tid, effect: eid, enabled: !enabled }, cx)
                        })),
                    )
                    .child(
                        div()
                            .flex_1()
                            .text_color(if enabled { tok::text() } else { tok::text_tertiary() })
                            .child(e.name.clone()),
                    )
                    .child(crate::ui::icon_button(("track-fx-del", j), "trash", tok::text_tertiary()).on_click(
                        cx.listener(move |this, _, _, cx| {
                            this.exec(Command::RemoveEffect { track: tid, effect: eid }, cx)
                        }),
                    )),
            );
        }
        let mut presets = div().flex().flex_wrap().gap_1();
        for (k, (label, list)) in crate::mixer::effect_presets().into_iter().enumerate() {
            presets = presets.child(Button::new(("track-preset", k)).label(label).xsmall().outline().on_click(
                cx.listener(move |this, _, _, cx| {
                    let commands = list
                        .iter()
                        .cloned()
                        .map(|effect| Command::AddEffect { track: tid, effect, index: None })
                        .collect();
                    this.exec(Command::Batch { label: format!("Add {label}"), commands }, cx);
                }),
            ));
        }
        body.child(
            div()
                .flex()
                .items_center()
                .gap_2()
                .child(crate::ui::icon("audio-lines").size(px(16.)).text_color(tok::text_secondary()))
                .child(div().font_weight(FontWeight::SEMIBOLD).child(track.name.clone())),
        )
        .children(project.inputs.get(&track.input).and_then(|i| self.source_section(&i.clone(), cx)))
        .child(self.section("Effects", None, cx).child(if track.effects.is_empty() {
            div().text_color(tok::text_tertiary()).child("None yet").into_any_element()
        } else {
            effects.into_any_element()
        }))
        .child(self.section("Add effect", None, cx).child(presets))
        .child(
            div().pt_2().child(
                Button::new("remove-track")
                    .label("Remove track")
                    .small()
                    .danger()
                    .on_click(cx.listener(move |this, _, _, cx| this.exec(Command::RemoveTrack { track: tid }, cx))),
            ),
        )
        .into_any_element()
    }

    fn colour_section(&self, input: &Input, cx: &mut Context<Self>) -> impl IntoElement {
        self.section("Colour", None, cx).child(self.swatches(input, "color", &["color"], cx))
    }

    fn media_section(&self, input: &Input, cx: &mut Context<Self>) -> impl IntoElement {
        let id = input.id;
        let status = self.props.media.as_ref().filter(|(i, ..)| *i == id).map(|(_, s, _)| *s);
        let playing = status
            .is_some_and(|s| matches!(s.state, MediaState::Playing | MediaState::Opening | MediaState::Buffering));
        let looping = input.settings["looping"].as_bool().unwrap_or(false);
        let file = input.settings["local_file"]
            .as_str()
            .and_then(|p| std::path::Path::new(p).file_name())
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "No file".into());
        let (time, duration) = status.map_or((0, 0), |s| (s.time_ms, s.duration_ms));
        let progress = if duration > 0 { (time as f32 / duration as f32).clamp(0.0, 1.0) } else { 0.0 };
        let state_label = match status.map(|s| s.state) {
            Some(MediaState::Error) => "Couldn't play this file",
            Some(MediaState::Ended) => "Ended",
            Some(MediaState::Stopped) => "Stopped",
            _ => "",
        };
        let loop_settings = with(&input.settings, "looping", json!(!looping));
        self.section("Media", None, cx)
            .child(div().text_color(tok::text_secondary()).overflow_hidden().text_ellipsis().child(file))
            .child(
                div()
                    .id("media-progress")
                    .h(px(4.))
                    .w_full()
                    .rounded_full()
                    .bg(tok::border())
                    .child(div().h_full().rounded_full().bg(tok::accent_primary()).w(relative(progress))),
            )
            .child(
                div()
                    .flex()
                    .justify_between()
                    .text_size(px(11.))
                    .font_family("Menlo")
                    .text_color(tok::text_tertiary())
                    .child(clock(time))
                    .child(state_label)
                    .child(clock(duration)),
            )
            .child(
                div()
                    .flex()
                    .gap_1()
                    .child(
                        Button::new("media-play")
                            .label(if playing { "Pause" } else { "Play" })
                            .small()
                            .outline()
                            .disabled(status.is_none())
                            .on_click(cx.listener(move |this, _, _, cx| {
                                let action = if playing { MediaAction::Pause } else { MediaAction::Play };
                                this.media(id, action, cx)
                            })),
                    )
                    .child(
                        Button::new("media-restart")
                            .label("Restart")
                            .small()
                            .ghost()
                            .disabled(status.is_none())
                            .on_click(cx.listener(move |this, _, _, cx| this.media(id, MediaAction::Restart, cx))),
                    )
                    .child({
                        let b = Button::new("media-loop").label("Loop").small().on_click(cx.listener(
                            move |this, _, _, cx| {
                                this.exec(Command::SetInputSettings { input: id, settings: loop_settings.clone() }, cx)
                            },
                        ));
                        if looping { b.primary() } else { b.ghost() }
                    }),
            )
    }
}

fn rgb_to_hsla(rgb_value: u32) -> Hsla {
    rgb(rgb_value).into()
}

#[cfg(test)]
mod tests {
    use super::{abgr, clock, rgb_of, with};
    use serde_json::json;

    #[test]
    fn colour_order() {
        assert_eq!(abgr(0xFF4A3B), 0xFF3B4AFF);
        assert_eq!(rgb_of(abgr(0x123456)), 0x123456);
        assert_eq!(clock(83_500), "1:23");
        let s = with(&json!({"text": "a", "font": {"size": 10}}), "text", json!("b"));
        assert_eq!(s, json!({"text": "b", "font": {"size": 10}}));
    }
}
