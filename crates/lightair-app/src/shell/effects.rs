//! Effects (docs/plans/effects-revamp.md): the source's effect stack in the Inspector, each
//! effect a card with its own switch, summary and controls, and the Effect catalog sheet that
//! adds them (face, background, colour, stylize, blur, distort, keying).

use std::collections::HashMap;

use gpui_kit::component::Sizable;
use gpui_kit::component::Disableable;
use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::color_picker::{ColorPicker, ColorPickerEvent, ColorPickerState};
use gpui_kit::component::input::{Input as TextInput, InputEvent, InputState};
use gpui_kit::component::menu::{ContextMenuExt, PopupMenu, PopupMenuItem};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;
use lightair_core::catalog::{self, Category, Control, EffectDef, Needs, Presets, Slider as SliderDef};
use lightair_core::{Command, Effect, Id, Input};
use serde_json::{Value, json};

use super::StudioWindow;
use crate::camera_studio::{self, with};
use crate::ui::{self, tok};

/// Whose effects these are: a source's (video) or a mixer track's (audio).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Owner {
    Input(Id),
    Track(Id),
    /// A bus, mix or the master.
    Strip(lightair_core::MixerStrip),
}

impl Owner {
    fn add(self, effect: Effect, index: Option<usize>) -> Command {
        match self {
            Owner::Input(input) => Command::AddInputEffect { input, effect, index },
            Owner::Track(track) => Command::AddEffect { track, effect, index },
            Owner::Strip(strip) => Command::AddStripEffect { strip, effect, index },
        }
    }
    fn remove(self, effect: Id) -> Command {
        match self {
            Owner::Input(input) => Command::RemoveInputEffect { input, effect },
            Owner::Track(track) => Command::RemoveEffect { track, effect },
            Owner::Strip(strip) => Command::RemoveStripEffect { strip, effect },
        }
    }
    fn enable(self, effect: Id, enabled: bool) -> Command {
        match self {
            Owner::Input(input) => Command::SetInputEffectEnabled { input, effect, enabled },
            Owner::Track(track) => Command::SetEffectEnabled { track, effect, enabled },
            Owner::Strip(strip) => Command::SetStripEffectEnabled { strip, effect, enabled },
        }
    }
    fn settings(self, effect: Id, settings: Value) -> Command {
        match self {
            Owner::Input(input) => Command::SetInputEffectSettings { input, effect, settings },
            Owner::Track(track) => Command::SetEffectSettings { track, effect, settings },
            Owner::Strip(strip) => Command::SetStripEffectSettings { strip, effect, settings },
        }
    }
    /// Its effects and name.
    fn find(self, project: &lightair_core::Project) -> Option<(String, Vec<Effect>)> {
        match self {
            Owner::Input(id) => project.inputs.get(&id).map(|i| (i.name.clone(), i.effects.clone())),
            Owner::Track(id) => project.track(id).map(|t| (t.name.clone(), t.effects.clone())),
            Owner::Strip(lightair_core::MixerStrip::Bus(id)) => {
                project.audio.bus(id).map(|b| (b.name.clone(), b.effects.clone()))
            }
            Owner::Strip(lightair_core::MixerStrip::Mix(id)) => {
                project.audio.mix(id).map(|m| (m.name.clone(), m.effects.clone()))
            }
            Owner::Strip(lightair_core::MixerStrip::Master) => Some(("Program".into(), project.audio.master.effects.clone())),
            Owner::Strip(lightair_core::MixerStrip::Track(id)) => project.track(id).map(|t| (t.name.clone(), t.effects.clone())),
        }
    }
    fn audio(self) -> bool {
        !matches!(self, Owner::Input(_))
    }

    /// Whose effect `effect` is (a mixer strip's: the audio inserts).
    fn of_audio_effect(project: &lightair_core::Project, effect: Id) -> Option<(Owner, Effect)> {
        let has = |effects: &[Effect]| effects.iter().find(|e| e.id == effect).cloned();
        let a = &project.audio;
        project
            .tracks
            .iter()
            .find_map(|t| has(&t.effects).map(|e| (Owner::Track(t.id), e)))
            .or_else(|| {
                a.buses.iter().find_map(|b| has(&b.effects).map(|e| (Owner::Strip(lightair_core::MixerStrip::Bus(b.id)), e)))
            })
            .or_else(|| {
                a.mixes.iter().find_map(|m| has(&m.effects).map(|e| (Owner::Strip(lightair_core::MixerStrip::Mix(m.id)), e)))
            })
            .or_else(|| has(&a.master.effects).map(|e| (Owner::Strip(lightair_core::MixerStrip::Master), e)))
    }
}

/// A page of the catalog sheet.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Page {
    All,
    Of(Category),
    /// Installed plugins (Audio Units), for the audio catalog.
    Plugins,
}

pub(super) struct EffectSheet {
    owner: Owner,
    page: Page,
    search: Entity<InputState>,
    _events: Subscription,
}

/// What a source's face effects can see (from Camera Studio), polled while it's selected.
#[derive(Clone, Copy, Default, PartialEq)]
pub(super) struct Presence {
    pub face: Option<bool>,
    pub person: Option<bool>,
}

/// A colour control's picker, kept per effect and key (it needs the window to be made).
pub(super) struct ColourPicker {
    state: Entity<ColorPickerState>,
    /// The value it last showed (0xAABBGGRR), to follow undo and other changes.
    shown: u32,
    _events: Subscription,
}

fn hsla_of_abgr(abgr: u32) -> Hsla {
    super::layer_props::rgb_to_hsla(super::layer_props::rgb_of(abgr))
}

fn abgr_of_hsla(c: Hsla) -> u32 {
    let rgba = Rgba::from(c);
    let ch = |v: f32| (v.clamp(0.0, 1.0) * 255.0).round() as u32;
    super::layer_props::abgr((ch(rgba.r) << 16) | (ch(rgba.g) << 8) | ch(rgba.b))
}

/// Colours offered for effect colour controls: neutrals, then makeup-friendly and bright ones.
const SWATCHES: &[u32] = &[
    0xFFFFFF, 0x1A1A1A, 0x2F6B3A, 0x00B140, 0x0047BB, 0xC0405A, 0xD98080, 0xE8A0A0, 0xB5654A, 0x8E3B5A, 0xF5A5A5,
    0xC89080, 0xFFB020, 0x8B5CF6, 0x1AFFE6, 0xFF4A3B,
];

impl StudioWindow {
    // ------------------------------------------------------------ the stack

    pub(super) fn effects_section(&self, input: &Input, cx: &mut Context<Self>) -> AnyElement {
        self.effect_stack(Owner::Input(input.id), &input.effects, cx)
    }

    /// A bus's, mix's or the master's effects.
    pub(super) fn strip_effects_section(
        &self,
        strip: lightair_core::MixerStrip,
        effects: &[Effect],
        cx: &mut Context<Self>,
    ) -> AnyElement {
        self.effect_stack(Owner::Strip(strip), effects, cx)
    }

    /// A mixer track's effects: the same cards, from the audio catalog.
    pub(super) fn track_effects_section(&self, track: &lightair_core::Track, cx: &mut Context<Self>) -> AnyElement {
        self.effect_stack(Owner::Track(track.id), &track.effects, cx)
    }

    fn effect_stack(&self, iid: Owner, effects: &[Effect], cx: &mut Context<Self>) -> AnyElement {
        let add = Button::new("add-effect")
            .icon(ui::icon("plus"))
            .label("Add")
            .xsmall()
            .ghost()
            .on_click(cx.listener(move |this, _, window, cx| this.open_effect_catalog(iid, Page::All, window, cx)));
        let mut col = self.section("Effects", Some(add.into_any_element()), cx);

        if effects.is_empty() {
            col = col.child(
                div()
                    .id("fx-empty")
                    .flex()
                    .flex_col()
                    .items_center()
                    .gap_1()
                    .py_3()
                    .rounded(px(8.))
                    .border_1()
                    .border_dashed()
                    .border_color(tok::border_strong())
                    .cursor_pointer()
                    .hover(|d| d.bg(tok::bg_raised()))
                    .child(ui::icon("sparkles").size(px(16.)).text_color(tok::text_tertiary()))
                    .child(div().text_size(px(12.)).text_color(tok::text_secondary()).child("Add effect"))
                    .child(div().text_size(px(11.)).text_color(tok::text_tertiary()).child(if iid.audio() {
                        "Voice, dynamics, EQ, space and more"
                    } else {
                        "Face, background, colour, stylize and more"
                    }))
                    .on_click(
                        cx.listener(move |this, _, window, cx| this.open_effect_catalog(iid, Page::All, window, cx)),
                    ),
            );
        }
        let presence = match iid {
            Owner::Input(id) => self.presence.get(&id).copied().unwrap_or_default(),
            Owner::Track(_) | Owner::Strip(_) => Presence::default(),
        };
        for (i, e) in effects.iter().enumerate() {
            col = col.child(self.effect_card(iid, effects.len(), i, e, presence, cx));
        }
        col.into_any_element()
    }

    fn effect_card(
        &self,
        iid: Owner,
        count: usize,
        i: usize,
        e: &Effect,
        presence: Presence,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let def = catalog::def(&e.kind);
        let (eid, on) = (e.id, e.enabled);
        let open = self.props.open_effect == Some(eid);
        let system = e.settings.get("bg_model").and_then(Value::as_i64) == Some(catalog::BG_MODEL_SYSTEM);
        let summary = match def {
            Some(_) if system => "macOS camera effect".into(),
            Some(d) => d.summary(&e.settings),
            None => "Plugin effect".into(),
        };
        let warning: Option<SharedString> = def.filter(|_| on && !system).and_then(|d| match d.needs {
            _ if d.kind == catalog::PLUGIN_KIND => match lightair_engine::plugins::status(eid) {
                lightair_engine::plugins::Status::Failed(why) => Some(format!("Plugin unavailable: {why}").into()),
                lightair_engine::plugins::Status::Loading => Some("Loading the plugin…".into()),
                _ => None,
            },
            Needs::Face if presence.face == Some(false) => Some("No face in view".into()),
            Needs::Person if presence.person == Some(false) => Some("No person in view".into()),
            _ => None,
        });
        let header = div()
            .id(("fx-head", i))
            .flex()
            .items_center()
            .gap_2()
            .px_2()
            .py_1p5()
            .cursor_pointer()
            .on_click(cx.listener(move |this, _, _, cx| {
                this.props.open_effect = if open { None } else { Some(eid) };
                cx.notify();
            }))
            .child(
                div()
                    .flex()
                    .flex_none()
                    .items_center()
                    .justify_center()
                    .size(px(28.))
                    .rounded(px(7.))
                    .bg(if on { tok::bg_selected() } else { tok::bg_raised() })
                    .child(ui::icon(def.map_or("puzzle", |d| d.icon)).size(px(15.)).text_color(if on {
                        tok::accent_primary()
                    } else {
                        tok::text_tertiary()
                    })),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_w_0()
                    .child(
                        div()
                            .text_size(px(12.))
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(if on { tok::text() } else { tok::text_tertiary() })
                            .truncate()
                            .child(e.name.clone()),
                    )
                    .when(!summary.is_empty(), |d| {
                        d.child(div().text_size(px(11.)).text_color(tok::text_tertiary()).truncate().child(summary))
                    }),
            )
            .child(super::settings::switch(("fx-on", i), on).on_click(cx.listener(move |this, _, _, cx| {
                cx.stop_propagation();
                this.exec(iid.enable(eid, !on), cx)
            })))
            .child(ui::icon(if open { "chevron-down" } else { "chevron-right" }).size(px(12.)).text_color(tok::text_tertiary()));

        let shell = cx.entity().downgrade();
        let mut card = div()
            .id(("fx-card", i))
            .flex()
            .flex_col()
            .rounded(px(9.))
            .border_1()
            .border_color(if open { tok::border_strong() } else { tok::border() })
            .bg(if open { tok::bg_raised() } else { tok::bg_panel() })
            .hover(|d| d.border_color(tok::border_strong()))
            .child(header)
            .context_menu(move |m, _, _| effect_menu(m, &shell, iid, eid, i, count));
        if let Some(w) = warning {
            card = card.child(
                div()
                    .flex()
                    .items_center()
                    .gap_1()
                    .px_2()
                    .pb_1p5()
                    .text_size(px(11.))
                    .text_color(tok::warn())
                    .child(ui::icon("triangle-alert").size(px(12.)))
                    .child(w),
            );
        }
        if open {
            match def {
                Some(d) => card = card.child(self.effect_controls(iid, e, d, cx)),
                None => {
                    card = card.child(
                        div()
                            .px_2()
                            .pb_2()
                            .text_size(px(11.))
                            .text_color(tok::text_tertiary())
                            .child("A plugin's effect: its settings are its own."),
                    )
                }
            }
            card = card.child(self.effect_footer(iid, e, def, i, count, cx));
        }
        card.into_any_element()
    }

    fn effect_footer(
        &self,
        iid: Owner,
        e: &Effect,
        def: Option<&'static EffectDef>,
        i: usize,
        count: usize,
        cx: &mut Context<Self>,
    ) -> Div {
        let eid = e.id;
        let mut row = div().flex().items_center().gap_1().px_2().pb_2();
        if let Some(d) = def.filter(|d| d.kind != catalog::PLUGIN_KIND) {
            let defaults = Value::Object(d.defaults());
            let is_default = defaults == e.settings;
            row = row.child(
                Button::new(("fx-reset", i)).label("Reset").xsmall().ghost().disabled(is_default).on_click(cx.listener(
                    move |this, _, _, cx| {
                        let settings = defaults.clone();
                        this.exec(iid.settings(eid, settings), cx)
                    },
                )),
            );
        }
        row.child(div().flex_1())
            .child(
                ui::icon_button(("fx-up", i), "chevron-up", if i > 0 { tok::text_secondary() } else { tok::border_strong() })
                    .on_click(cx.listener(move |this, _, _, cx| this.move_effect(iid, eid, -1, cx))),
            )
            .child(
                ui::icon_button(
                    ("fx-down", i),
                    "chevron-down",
                    if i + 1 < count { tok::text_secondary() } else { tok::border_strong() },
                )
                .on_click(cx.listener(move |this, _, _, cx| this.move_effect(iid, eid, 1, cx))),
            )
            .child(
                ui::icon_button(("fx-dup", i), "copy-plus", tok::text_secondary())
                    .on_click(cx.listener(move |this, _, _, cx| this.duplicate_effect(iid, eid, cx))),
            )
            .child(
                ui::icon_button(("fx-remove", i), "trash", tok::text_secondary()).on_click(cx.listener(
                    move |this, _, _, cx| this.exec(iid.remove(eid), cx),
                )),
            )
    }

    fn effect_controls(&self, iid: Owner, e: &Effect, d: &'static EffectDef, cx: &mut Context<Self>) -> Div {
        let eid = e.id;
        let set = move |settings: Value| iid.settings(eid, settings);
        let mut col = div().flex().flex_col().gap_1p5().px_2().pb_2().pt_0p5();
        if d.kind == "audio.eq" {
            col = col.child(eq_curve(e));
        }
        if d.kind == catalog::PLUGIN_KIND {
            return col.child(self.plugin_panel(e, cx));
        }
        let mut group_open = true;
        let mut first_heading = true;
        let system = e.settings.get("bg_model").and_then(Value::as_i64) == Some(catalog::BG_MODEL_SYSTEM);
        if system {
            col = col.child(system_effect_panel(d.kind, cx));
        }
        for (j, c) in d.controls.iter().enumerate() {
            let key = c.key();
            // Background replace shows the colour or the image it uses, not both.
            if d.kind == "studio.bg_replace" {
                let image = e.settings["bg_mode"].as_i64() == Some(4);
                if (key == Some("bg_color") && image) || (key == Some("bg_image") && !image) {
                    continue;
                }
            }
            // An EQ band's slope is for its passes; its gain and Q for the rest (no gain on a notch).
            if d.kind == "audio.eq"
                && let Some(k) = key
                && let Some((band, field)) = k.split_once('_')
                && matches!(field, "slope" | "gain" | "q")
            {
                let kind_key = format!("{band}_type");
                let kind = e.settings.get(&kind_key).and_then(Value::as_str).map(String::from).or_else(|| {
                    d.controls.iter().find(|c| c.key() == Some(kind_key.as_str())).and_then(|c| c.default_json()).and_then(|v| v.as_str().map(String::from))
                });
                let pass = matches!(kind.as_deref(), Some("hp" | "lp"));
                let hide = match field {
                    "slope" => !pass,
                    "q" => pass, // a pass's shape comes from its slope (Butterworth)
                    _ => pass || kind.as_deref() == Some("notch"),
                };
                if hide {
                    continue;
                }
            }
            // Green screen's own colour only when it's Custom.
            if d.kind == "chroma_key_filter_v2"
                && key == Some("key_color")
                && e.settings.get("key_color_type").and_then(Value::as_str) != Some("custom")
            {
                continue;
            }
            // Quality levels are Apple Vision's; macOS's own effect has its settings in Control Center.
            let model = e.settings.get("bg_model").and_then(Value::as_i64).unwrap_or(0);
            if key == Some("bg_quality") && model != 0 {
                continue;
            }
            if model == catalog::BG_MODEL_SYSTEM && key.is_some_and(|k| k != "bg_model") {
                continue;
            }
            match *c {
                Control::Heading(h) => {
                    let gk = format!("{eid}:{h}");
                    group_open = first_heading != self.fx_groups.contains(&gk);
                    first_heading = false;
                    col = col.child(
                        div()
                            .id(("fx-group", j))
                            .flex()
                            .items_center()
                            .gap_1()
                            .pt_1()
                            .cursor_pointer()
                            .text_size(px(10.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(tok::text_tertiary())
                            .hover(|d| d.text_color(tok::text_secondary()))
                            .child(ui::icon(if group_open { "chevron-down" } else { "chevron-right" }).size(px(11.)))
                            .child(h.to_uppercase())
                            .on_click(cx.listener(move |this, _, _, cx| {
                                if !this.fx_groups.remove(&gk) {
                                    this.fx_groups.insert(gk.clone());
                                }
                                cx.notify();
                            })),
                    );
                }
                _ if !group_open => {}
                Control::Slider(s) => col = col.child(self.fx_slider(iid, e, s, cx)),
                Control::Choice { key, label, options, default } => {
                    let current = e.settings.get(key);
                    let chosen = options
                        .iter()
                        .position(|(_, p)| current.is_some_and(|v| p.matches(v)))
                        .or_else(|| options.iter().position(|(_, p)| *p == default));
                    let opts: Vec<(&'static str, Command, bool)> = options
                        .iter()
                        .enumerate()
                        .map(|(n, (l, p))| (*l, set(with(&e.settings, key, p.json())), chosen == Some(n)))
                        .collect();
                    // many options wrap as chips; a few sit in one segmented row
                    let control = if options.len() > 4 {
                        let mut row = div().flex().flex_wrap().gap_1().flex_1();
                        for (n, (l, cmd, on)) in opts.into_iter().enumerate() {
                            row = row.child(
                                div()
                                    .id(SharedString::from(format!("fx-chip-{key}-{n}")))
                                    .px_2()
                                    .h(px(24.))
                                    .flex()
                                    .items_center()
                                    .rounded(px(5.))
                                    .text_size(px(11.))
                                    .cursor_pointer()
                                    .when(on, |d| d.bg(tok::bg_selected()).text_color(tok::text()))
                                    .when(!on, |d| d.text_color(tok::text_secondary()).hover(|d| d.bg(tok::bg_raised())))
                                    .child(l)
                                    .on_click(cx.listener(move |this, _, _, cx| this.exec(cmd.clone(), cx))),
                            );
                        }
                        row
                    } else {
                        self.segmented(key, opts, cx).flex_1()
                    };
                    col = col.child(control_row(label, control));
                    if let Some(hint) = chosen.and_then(|n| choice_hint(key, n)) {
                        col = col.child(
                            div().pl(px(96.)).text_size(px(11.)).text_color(tok::text_tertiary()).child(hint),
                        );
                    }
                }
                Control::Colour { key, label, default } => {
                    let current = e.settings.get(key).and_then(Value::as_u64).map_or(default, |v| v as u32);
                    col = col.child(control_row(label, self.colour_control(iid, eid, key, current, cx)));
                }
                Control::Presets { key, set: which, default, .. } => {
                    col = col.child(self.fx_presets(iid, e, key, which, default, cx));
                }
                Control::File { key, label, extensions } => {
                    let path = e.settings.get(key).and_then(Value::as_str).unwrap_or("").to_string();
                    let name = std::path::Path::new(&path)
                        .file_name()
                        .map(|n| n.to_string_lossy().to_string())
                        .unwrap_or_else(|| "None".into());
                    let settings = e.settings.clone();
                    col = col.child(control_row(
                        label,
                        div()
                            .flex()
                            .flex_1()
                            .min_w_0()
                            .items_center()
                            .gap_2()
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .truncate()
                                    .text_size(px(11.))
                                    .text_color(if path.is_empty() { tok::text_tertiary() } else { tok::text() })
                                    .child(name),
                            )
                            .child(Button::new(SharedString::from(format!("fx-file-{key}"))).label("Choose…").xsmall().outline().on_click(
                                cx.listener(move |this, _, window, cx| {
                                    this.choose_effect_file(iid, eid, key, extensions, settings.clone(), window, cx)
                                }),
                            )),
                    ));
                }
                Control::Track { key, label } => {
                    let current = e.settings.get(key).and_then(Value::as_str).unwrap_or("").to_string();
                    let tracks: Vec<(String, String)> = self
                        .studio
                        .read(cx)
                        .project()
                        .tracks
                        .iter()
                        .map(|t| (t.id.0.to_string(), t.name.clone()))
                        .collect();
                    let mut row = div().flex().flex_wrap().gap_1().flex_1();
                    for (n, (id, name)) in tracks.into_iter().enumerate() {
                        let on = id == current;
                        let cmd = set(with(&e.settings, key, json!(id)));
                        row = row.child(
                            div()
                                .id(SharedString::from(format!("fx-track-{key}-{n}")))
                                .px_2()
                                .h(px(24.))
                                .flex()
                                .items_center()
                                .rounded(px(5.))
                                .text_size(px(11.))
                                .cursor_pointer()
                                .when(on, |d| d.bg(tok::bg_selected()).text_color(tok::text()))
                                .when(!on, |d| d.text_color(tok::text_secondary()).hover(|d| d.bg(tok::bg_raised())))
                                .child(name)
                                .on_click(cx.listener(move |this, _, _, cx| this.exec(cmd.clone(), cx))),
                        );
                    }
                    col = col.child(control_row(label, row));
                }
                Control::Switch { key, label, default } => {
                    let on = e.settings.get(key).and_then(Value::as_bool).unwrap_or(default);
                    let settings = with(&e.settings, key, json!(!on));
                    col = col.child(control_row(
                        label,
                        div().flex().flex_1().justify_end().child(
                            super::settings::switch(SharedString::from(format!("fx-sw-{key}")), on)
                                .on_click(cx.listener(move |this, _, _, cx| this.exec(set(settings.clone()), cx))),
                        ),
                    ));
                }
            }
        }
        col
    }

    fn fx_slider(&self, iid: Owner, e: &Effect, s: SliderDef, cx: &mut Context<Self>) -> Div {
        let eid = e.id;
        let value = s.value(&e.settings);
        let settings = e.settings.clone();
        // frequencies and times move on a log scale: the slider works in log10 of the value
        let log = s.log && s.min > 0.0;
        let (lo, hi, pos) = if log {
            (s.min.log10() as f32, s.max.log10() as f32, value.max(s.min).log10() as f32)
        } else {
            (s.min as f32, s.max as f32, value as f32)
        };
        let step = if log { (hi - lo) / 400.0 } else { s.step as f32 };
        let slider = self.slider(
            format!("fx:{eid}:{}", s.key),
            (lo, hi, step),
            pos,
            move |v| {
                let v = if log { 10f64.powf(v as f64) } else { v as f64 };
                iid.settings(eid, with(&settings, s.key, s.json(v)))
            },
            cx,
        );
        let reset = with(&e.settings, s.key, s.json(s.default));
        let changed = (value - s.default).abs() > 1e-9;
        div()
            .flex()
            .items_center()
            .gap_2()
            .child(
                div()
                    .id(SharedString::from(format!("fx-l-{eid}-{}", s.key)))
                    .w(px(88.))
                    .flex_none()
                    .truncate()
                    .text_size(px(12.))
                    .text_color(tok::text_secondary())
                    .cursor_pointer()
                    .child(s.label)
                    .tooltip(|window, cx| {
                        gpui_kit::component::tooltip::Tooltip::new("Double-click to reset").build(window, cx)
                    })
                    .on_click(cx.listener(move |this, ev: &ClickEvent, _, cx| {
                        if ev.click_count() == 2 {
                            this.exec(iid.settings(eid, reset.clone()), cx)
                        }
                    })),
            )
            .child(div().flex_1().min_w_0().child(slider))
            .child(
                div()
                    .w(px(44.))
                    .flex_none()
                    .text_right()
                    .text_size(px(11.))
                    .font_family("Menlo")
                    .text_color(if changed { tok::text() } else { tok::text_tertiary() })
                    .child(s.display(value)),
            )
    }

    /// A preset grid: tiles by group (makeup looks fill in their values to tweak).
    fn fx_presets(
        &self,
        iid: Owner,
        e: &Effect,
        key: &'static str,
        which: Presets,
        default: &'static str,
        cx: &mut Context<Self>,
    ) -> Div {
        let eid = e.id;
        let current = e.settings.get(key).and_then(Value::as_str).unwrap_or(default).to_string();
        let mut col = div().flex().flex_col().gap_1();
        let groups = camera_studio::presets(which);
        if groups.is_empty() {
            return col.child(div().text_size(px(11.)).text_color(tok::text_tertiary()).child("Camera Studio isn't installed"));
        }
        let mut n = 0;
        // Makeup can be none at all.
        let none = [(String::new(), vec!["None".to_string()])];
        let groups: Vec<&(String, Vec<String>)> =
            if which == Presets::Makeup { none.iter().chain(groups).collect() } else { groups.iter().collect() };
        let many = groups.iter().filter(|(g, _)| !g.is_empty()).count() > 1;
        for (group, names) in groups {
            if !group.is_empty() && many {
                col = col.child(div().pt_1().text_size(px(10.)).text_color(tok::text_tertiary()).child(group.to_uppercase()));
            }
            let mut grid = div().grid().grid_cols(3).gap_1();
            for name in names {
                let selected = *name == current;
                let mut settings = with(&e.settings, key, json!(name));
                if which == Presets::Makeup
                    && let Value::Object(s) = &mut settings
                {
                    let p = camera_studio::makeup_preset(name).unwrap_or_else(|| json!({}));
                    s.extend(catalog::makeup_values(&p));
                }
                n += 1;
                grid = grid.child(
                    div()
                        .id(SharedString::from(format!("fx-p-{key}-{n}")))
                        .flex()
                        .items_center()
                        .justify_center()
                        .h(px(30.))
                        .px_1()
                        .rounded(px(6.))
                        .border_1()
                        .border_color(if selected { tok::accent_primary() } else { tok::border() })
                        .bg(if selected { tok::bg_selected() } else { tok::bg_app() })
                        .cursor_pointer()
                        .hover(|d| d.border_color(tok::border_strong()))
                        .text_size(px(11.))
                        .text_color(if selected { tok::text() } else { tok::text_secondary() })
                        .child(div().truncate().child(name.clone()))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            let settings = settings.clone();
                            this.exec(iid.settings(eid, settings), cx)
                        })),
                );
            }
            col = col.child(grid);
        }
        col
    }

    #[allow(clippy::too_many_arguments)]
    fn choose_effect_file(
        &mut self,
        iid: Owner,
        eid: Id,
        key: &'static str,
        extensions: &'static [&'static str],
        settings: Value,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let paths =
            cx.prompt_for_paths(PathPromptOptions { files: true, directories: false, multiple: false, prompt: None });
        cx.spawn_in(window, async move |this, cx| {
            let Ok(Ok(Some(paths))) = paths.await else { return };
            let Some(path) = paths.into_iter().next() else { return };
            let ext = path.extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default();
            if !extensions.contains(&ext.as_str()) {
                return;
            }
            let settings = with(&settings, key, json!(path.to_string_lossy()));
            let _ = this.update(cx, |this, cx| {
                this.exec(iid.settings(eid, settings), cx)
            });
        })
        .detach();
    }

    /// A colour: the swatch opens the picker (palette, HSLA, hex); the dropper picks from the
    /// screen; the hex says what it is.
    fn colour_control(&self, iid: Owner, eid: Id, key: &'static str, current: u32, cx: &mut Context<Self>) -> Div {
        let hex = format!("#{:06X}", super::layer_props::rgb_of(current));
        let picker = match self.colour_pickers.get(&format!("{eid}:{key}")) {
            Some(p) => ColorPicker::new(&p.state)
                .featured_colors(SWATCHES.iter().map(|rgb| super::layer_props::rgb_to_hsla(*rgb)).collect())
                .anchor(Anchor::TopRight) // opens towards the canvas: the Inspector sits at the window's edge
                .small()
                .into_any_element(),
            None => div().size(px(22.)).rounded(px(5.)).bg(hsla_of_abgr(current)).into_any_element(),
        };
        div()
            .flex()
            .flex_1()
            .items_center()
            .gap_2()
            .child(picker)
            .child(div().flex_1().text_size(px(11.)).font_family("Menlo").text_color(tok::text_secondary()).child(hex))
            .child(
                ui::icon_button(SharedString::from(format!("fx-drop-{eid}-{key}")), "pipette", tok::text_secondary())
                    .tooltip(|window, cx| {
                        gpui_kit::component::tooltip::Tooltip::new("Pick a colour from the screen").build(window, cx)
                    })
                    .on_click(cx.listener(move |this, _, _, cx| this.pick_colour(iid, eid, key, cx))),
            )
    }

    /// Makes (or brings up to date) the colour pickers of the selected object's effects. Called
    /// from the Inspector's render, which has the window the pickers need.
    pub(super) fn ensure_colour_pickers(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let studio = self.studio.read(cx);
        let input = studio
            .selected_layer
            .and_then(|l| studio.selected_scene.and_then(|s| studio.project().scene(s)).and_then(|s| s.layer(l)))
            .and_then(|l| l.input)
            .and_then(|i| studio.project().inputs.get(&i).cloned());
        let Some(input) = input else {
            self.colour_pickers.clear();
            return;
        };
        let mut wanted = Vec::new();
        for e in &input.effects {
            let Some(d) = catalog::def(&e.kind) else { continue };
            for c in d.controls {
                if let Control::Colour { key, default, .. } = *c {
                    let v = e.settings.get(key).and_then(Value::as_u64).map_or(default, |v| v as u32);
                    wanted.push((input.id, e.id, key, v));
                }
            }
        }
        self.colour_pickers.retain(|k, _| wanted.iter().any(|(_, e, key, _)| *k == format!("{e}:{key}")));
        for (iid, eid, key, v) in wanted {
            let k = format!("{eid}:{key}");
            match self.colour_pickers.get_mut(&k) {
                Some(p) if p.shown != v => {
                    p.shown = v;
                    p.state.update(cx, |s, cx| s.set_value(hsla_of_abgr(v), window, cx));
                }
                Some(_) => {}
                None => {
                    let state = cx.new(|cx| ColorPickerState::new(window, cx).default_value(hsla_of_abgr(v)));
                    if std::env::var("LIGHTAIR_OPEN").as_deref() == Ok("fx-key-open") && key == "key_color" {
                        state.update(cx, |s, cx| s.set_open(true, cx)); // screenshot hook
                    }
                    let events = cx.subscribe_in(&state, window, move |this, _, ev: &ColorPickerEvent, _, cx| {
                        if let ColorPickerEvent::Change(Some(c)) = ev {
                            this.set_effect_colour(Owner::Input(iid), eid, key, abgr_of_hsla(*c), cx);
                        }
                    });
                    self.colour_pickers.insert(k, ColourPicker { state, shown: v, _events: events });
                }
            }
        }
    }

    fn set_effect_colour(&mut self, iid: Owner, eid: Id, key: &'static str, abgr: u32, cx: &mut Context<Self>) {
        let project = self.studio.read(cx).project().clone();
        let Some((_, effects)) = iid.find(&project) else { return };
        let Some(e) = effects.iter().find(|e| e.id == eid) else { return };
        if e.settings.get(key).and_then(Value::as_u64) == Some(abgr as u64) {
            return;
        }
        if let Some(p) = self.colour_pickers.get_mut(&format!("{eid}:{key}")) {
            p.shown = abgr;
        }
        let settings = with(&e.settings, key, json!(abgr));
        self.exec(iid.settings(eid, settings), cx);
    }

    /// The eyedropper: macOS's loupe picks a colour from anywhere on screen. Off air, the effect
    /// being picked for is switched off meanwhile, so a key colour is picked from the picture
    /// before the key (on air, the picture stays as viewers see it).
    fn pick_colour(&mut self, iid: Owner, eid: Id, key: &'static str, cx: &mut Context<Self>) {
        use lightair_engine::outputs::runtime;
        let project = self.studio.read(cx).project().clone();
        let Some((_, effects)) = iid.find(&project) else { return };
        let Some(effect) = effects.iter().find(|e| e.id == eid) else { return };
        let filter = if effect.kind.starts_with("studio.") { catalog::STUDIO_ID } else { eid };
        let restore = catalog::lower(&effects).iter().find(|f| f.id == filter).is_some_and(|f| f.enabled);
        // only a source's effect can be switched off live (a track's colour has no picture)
        let Owner::Input(input) = iid else { return };
        let on_air = runtime().any_stream_running() || runtime().any_recording_running();
        let engine = self.studio.read(cx).engine().clone();
        if !on_air {
            engine.call(move |s| s.reconciler.set_filter_live(input, filter, false));
        }
        let picked = crate::eyedropper::pick();
        cx.spawn(async move |this, cx| {
            let result = loop {
                match picked.try_recv() {
                    Ok(r) => break r,
                    Err(std::sync::mpsc::TryRecvError::Empty) => {
                        cx.background_executor().timer(std::time::Duration::from_millis(30)).await
                    }
                    Err(_) => break None,
                }
            };
            if !on_air {
                engine.call(move |s| s.reconciler.set_filter_live(input, filter, restore));
            }
            if let Some(rgb) = result {
                let _ = this.update(cx, |this, cx| {
                    this.set_effect_colour(iid, eid, key, super::layer_props::abgr(rgb), cx)
                });
            }
        })
        .detach();
    }

    pub(super) fn move_effect(&mut self, iid: Owner, eid: Id, by: i32, cx: &mut Context<Self>) {
        let project = self.studio.read(cx).project().clone();
        let Some((_, effects)) = iid.find(&project) else { return };
        let Some(at) = effects.iter().position(|e| e.id == eid) else { return };
        let to = at as i32 + by;
        if to < 0 || to as usize >= effects.len() {
            return;
        }
        let effect = effects[at].clone();
        let label = format!("Move {}", effect.name);
        let commands = vec![
            iid.remove(eid),
            iid.add(effect, Some(to as usize)),
        ];
        self.exec(Command::Batch { label, commands }, cx);
    }

    pub(super) fn duplicate_effect(&mut self, iid: Owner, eid: Id, cx: &mut Context<Self>) {
        let project = self.studio.read(cx).project().clone();
        let Some((_, effects)) = iid.find(&project) else { return };
        let Some(at) = effects.iter().position(|e| e.id == eid) else { return };
        let mut copy = effects[at].clone();
        copy.id = Id::new();
        self.props.open_effect = Some(copy.id);
        self.exec(iid.add(copy, Some(at + 1)), cx);
    }

    // ------------------------------------------------------------ presence

    /// Asks Camera Studio whether the selected source shows a face and a person (for the
    /// cards' hints). Cheap; called on a timer while the Inspector shows face effects.
    pub(super) fn refresh_presence(&mut self, cx: &mut Context<Self>) {
        let studio = self.studio.read(cx);
        let Some(input) = studio
            .selected_layer
            .and_then(|l| studio.selected_scene.and_then(|s| studio.project().scene(s)).and_then(|s| s.layer(l)))
            .and_then(|l| l.input)
        else {
            return;
        };
        let wants = studio
            .project()
            .inputs
            .get(&input)
            .is_some_and(|i| i.effects.iter().any(|e| e.enabled && e.kind.starts_with("studio.")));
        if !wants {
            return;
        }
        let p = studio.engine().call(move |s| Presence {
            face: s.reconciler.face_present(input),
            person: s.reconciler.person_present(input),
        });
        if self.presence.get(&input) != Some(&p) {
            self.presence.insert(input, p);
            cx.notify();
        }
    }

    // ------------------------------------------------------------ the catalog sheet

    pub(super) fn open_effect_catalog(&mut self, owner: Owner, page: Page, window: &mut Window, cx: &mut Context<Self>) {
        if owner.audio() {
            self.scan_plugins(cx);
        }
        let search = cx.new(|cx| InputState::new(window, cx).placeholder("Search effects"));
        search.update(cx, |s, cx| s.focus(window, cx));
        let events = cx.subscribe_in(&search, window, |_, _, ev: &InputEvent, _, cx| {
            if matches!(ev, InputEvent::Change) {
                cx.notify();
            }
        });
        self.effect_sheet = Some(EffectSheet { owner, page, search, _events: events });
        cx.notify();
    }

    fn close_effect_catalog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.effect_sheet = None;
        window.focus(&self.focus, cx);
        cx.notify();
    }

    /// Adds an effect from the catalog (a face effect already in the stack opens instead).
    fn add_from_catalog(&mut self, input: Owner, def: &'static EffectDef, window: &mut Window, cx: &mut Context<Self>) {
        let project = self.studio.read(cx).project().clone();
        let Some((_, effects)) = input.find(&project) else { return };
        if def.is_studio()
            && let Some(e) = effects.iter().find(|e| e.kind == def.kind)
        {
            self.props.open_effect = Some(e.id);
            if !e.enabled {
                self.exec(input.enable(e.id, true), cx);
            }
        } else {
            let effect = def.create();
            self.props.open_effect = Some(effect.id);
            self.exec(input.add(effect, None), cx);
        }
        self.close_effect_catalog(window, cx);
    }

    /// Adds an installed plugin as an insert; its editor opens once it has loaded.
    pub(super) fn add_plugin(
        &mut self,
        owner: Owner,
        info: &lightair_engine::plugins::PluginInfo,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let mut effect = catalog::plugin_effect(&info.format, &info.id, &info.name, &info.vendor);
        if !info.path.is_empty() {
            effect.settings["path"] = Value::String(info.path.clone());
        }
        let eid = effect.id;
        self.props.open_effect = Some(eid);
        self.exec(owner.add(effect, None), cx);
        self.close_effect_catalog(window, cx);
        // wait for it to load (a few seconds at most), then show its editor
        cx.spawn(async move |this, cx| {
            for _ in 0..100 {
                cx.background_executor().timer(std::time::Duration::from_millis(100)).await;
                match lightair_engine::plugins::status(eid) {
                    lightair_engine::plugins::Status::Ready { .. } => {
                        let _ = this.update(cx, |this, cx| this.open_plugin_editor(eid, cx));
                        return;
                    }
                    lightair_engine::plugins::Status::Failed(_) | lightair_engine::plugins::Status::None => {
                        let _ = this.update(cx, |_, cx| cx.notify());
                        return;
                    }
                    lightair_engine::plugins::Status::Loading => {}
                }
            }
        })
        .detach();
    }

    /// A plugin insert's card: its editor, latency and where it stands.
    fn plugin_panel(&self, e: &Effect, cx: &mut Context<Self>) -> Div {
        use lightair_engine::plugins::Status;
        let eid = e.id;
        let status = lightair_engine::plugins::status(eid);
        let note: SharedString = match &status {
            Status::Ready { latency } if *latency > 0 => {
                format!("Adds {:.1} ms of delay, compensated in recordings", *latency as f32 / 48.0).into()
            }
            Status::Ready { .. } => "Runs inside Lightair's mixer, in real time".into(),
            Status::Loading => "Loading…".into(),
            Status::Failed(why) => format!("Couldn't run it: {why}").into(),
            Status::None => "Starts when the mixer plays".into(),
        };
        let open = self.plugin_editors.get(&eid).is_some_and(|ed| ed.is_open());
        div()
            .flex()
            .items_center()
            .gap_2()
            .child(
                Button::new(("plugin-editor", eid.0.as_u128() as u64))
                    .icon(ui::icon("app-window"))
                    .label(if open { "Show editor" } else { "Open editor" })
                    .small()
                    .disabled(!matches!(status, Status::Ready { .. }))
                    .on_click(cx.listener(move |this, _, _, cx| this.open_plugin_editor(eid, cx))),
            )
            .child(div().flex_1().min_w_0().text_size(px(11.)).text_color(tok::text_tertiary()).child(note))
    }

    /// The installed plugins, fresh each time the audio catalog opens (one installed meanwhile
    /// shows up): Audio Units at once, VST3 when its scan is done (new bundles are probed in a
    /// helper process, then remembered).
    fn scan_plugins(&mut self, cx: &mut Context<Self>) {
        use lightair_engine::plugins::PluginInfo;
        fn merged(au: Vec<PluginInfo>, vst3: Vec<PluginInfo>) -> std::sync::Arc<Vec<PluginInfo>> {
            let mut all: Vec<PluginInfo> = au.into_iter().chain(vst3).collect();
            all.sort_by_key(|p| (p.vendor.to_lowercase(), p.name.to_lowercase(), p.format.clone()));
            std::sync::Arc::new(all)
        }
        let vst3_known: Vec<PluginInfo> =
            self.plugin_list.iter().flat_map(|l| l.iter()).filter(|p| p.format == "vst3").cloned().collect();
        self.plugin_list = Some(merged(lightair_engine::plugins::scan_audio_units(), vst3_known));
        cx.spawn(async move |this, cx| {
            let vst3 = cx.background_executor().spawn(async { crate::plugin_scan::vst3() }).await;
            let _ = this.update(cx, |this, cx| {
                let au: Vec<PluginInfo> =
                    this.plugin_list.iter().flat_map(|l| l.iter()).filter(|p| p.format != "vst3").cloned().collect();
                this.plugin_list = Some(merged(au, vst3));
                cx.notify();
            });
        })
        .detach();
    }

    /// Shows a plugin insert's own editor (in front, if it's open already). Closing it saves the
    /// plugin's state in the project (one undo step).
    pub(super) fn open_plugin_editor(&mut self, effect: Id, cx: &mut Context<Self>) {
        use lightair_engine::plugins::Hosted;
        if let Some(ed) = self.plugin_editors.get(&effect).filter(|ed| ed.is_open()) {
            ed.front();
            return;
        }
        let Some(hosted) = lightair_engine::plugins::hosted(effect) else { return };
        let project = self.studio.read(cx).project().clone();
        let Some((owner, e)) = Owner::of_audio_effect(&project, effect) else { return };
        let strip = owner.find(&project).map(|(n, _)| n).unwrap_or_default();
        let title = format!("{} — {}", e.name, strip);
        match hosted {
            Hosted::Vst3(p) => {
                if let Some(ed) = crate::plugin_editor::Editor::open_vst3(p, &title) {
                    self.plugin_editors.insert(effect, ed);
                    self.watch_plugin_editor(effect, cx);
                }
            }
            Hosted::Au(unit) => {
                let made = crate::plugin_editor::request(&unit);
                cx.spawn(async move |this, cx| {
                    // an AUv3 answers on its own time
                    let mut got = None;
                    for _ in 0..100 {
                        if let Ok(m) = made.try_recv() {
                            got = Some(m);
                            break;
                        }
                        cx.background_executor().timer(std::time::Duration::from_millis(30)).await;
                    }
                    let made = got.unwrap_or(crate::plugin_editor::Made::Generic);
                    let _ = this.update(cx, |this, cx| {
                        if let Some(ed) = crate::plugin_editor::Editor::open_au(unit, made, &title) {
                            this.plugin_editors.insert(effect, ed);
                            this.watch_plugin_editor(effect, cx);
                        }
                    });
                })
                .detach();
            }
        }
        cx.notify();
    }

    /// Waits for an editor to close, then saves its plugin's state.
    fn watch_plugin_editor(&mut self, effect: Id, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(std::time::Duration::from_millis(400)).await;
                let done = this.update(cx, |this, cx| {
                    let open = this.plugin_editors.get(&effect).is_some_and(|ed| ed.is_open());
                    if !open {
                        this.plugin_editors.remove(&effect);
                        this.save_plugin_state(effect, cx);
                        cx.notify();
                    }
                    !open
                });
                if !matches!(done, Ok(false)) {
                    break;
                }
            }
        })
        .detach();
    }

    /// Writes a plugin insert's current state into the project, if it changed.
    pub(super) fn save_plugin_state(&mut self, effect: Id, cx: &mut Context<Self>) {
        let Some(state) = lightair_engine::plugins::take_state(effect) else { return };
        let project = self.studio.read(cx).project().clone();
        let Some((owner, e)) = Owner::of_audio_effect(&project, effect) else { return };
        if e.settings.get("state").and_then(Value::as_str) == Some(state.as_str()) {
            return;
        }
        let mut settings = e.settings.clone();
        settings["state"] = Value::String(state);
        self.exec(owner.settings(effect, settings), cx);
    }

    /// Closes editors whose inserts are gone (removed, undone).
    pub(super) fn prune_plugin_editors(&mut self, cx: &mut Context<Self>) {
        if self.plugin_editors.is_empty() {
            return;
        }
        let project = self.studio.read(cx).project().clone();
        let present = lightair_engine::plugins::inserts(&project);
        self.plugin_editors.retain(|id, ed| {
            let keep = present.contains(id);
            if !keep {
                ed.close();
            }
            keep
        });
    }

    pub(super) fn effect_catalog_overlay(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let sheet = self.effect_sheet.as_ref()?;
        let (iid, page) = (sheet.owner, sheet.page);
        let query = sheet.search.read(cx).value().to_string();
        let project = self.studio.read(cx).project().clone();
        let (owner_name, owner_effects) = iid.find(&project)?;
        let searching = !query.trim().is_empty();
        // a track's sheet is the audio catalog; a source's, the video one
        let (list, categories): (&'static [EffectDef], &[Category]) =
            if iid.audio() { (catalog::AUDIO, &Category::AUDIO) } else { (catalog::CATALOG, &Category::ALL) };

        // Categories down the side.
        let mut side = div().flex().flex_col().gap_0p5().w(px(168.)).flex_none().pr_2();
        let mut pages = vec![(Page::All, "All effects", "layout-grid")];
        for &c in categories {
            pages.push((Page::Of(c), c.name(), category_icon(c)));
        }
        let plugins = self.plugin_list.clone().filter(|_| iid.audio()).unwrap_or_default();
        if iid.audio() {
            pages.push((Page::Plugins, "Plugins", "plug"));
        }
        for (n, (p, label, icon)) in pages.into_iter().enumerate() {
            let on = p == page && !searching;
            side = side.child(
                div()
                    .id(("fx-page", n))
                    .flex()
                    .items_center()
                    .gap_2()
                    .h(px(30.))
                    .px_2()
                    .rounded(px(6.))
                    .cursor_pointer()
                    .text_size(px(12.))
                    .when(on, |d| d.bg(tok::bg_selected()).text_color(tok::text()))
                    .when(!on, |d| d.text_color(tok::text_secondary()).hover(|d| d.bg(tok::bg_raised())))
                    .child(ui::icon(icon).size(px(14.)))
                    .child(label)
                    .on_click(cx.listener(move |this, _, window, cx| {
                        if let Some(s) = &mut this.effect_sheet {
                            s.page = p;
                            s.search.update(cx, |st, cx| st.set_value("", window, cx));
                        }
                        cx.notify();
                    })),
            );
        }

        // Tiles.
        let in_stack: HashMap<&str, bool> = owner_effects.iter().map(|e| (e.kind.as_str(), e.enabled)).collect();
        let mut body = div().id("fx-sheet-body").flex().flex_col().gap_3().flex_1().min_h_0().overflow_y_scroll().pr_1();
        let q = query.trim().to_lowercase();
        let defs: Vec<&'static EffectDef> = list
            .iter()
            .filter(|d| match page {
                _ if searching => {
                    d.name.to_lowercase().contains(&q)
                        || d.description.to_lowercase().contains(&q)
                        || d.category.name().to_lowercase().contains(&q)
                }
                Page::Of(c) => d.category == c,
                Page::All => true,
                Page::Plugins => false,
            })
            .collect();
        let shown_plugins: Vec<&lightair_engine::plugins::PluginInfo> = plugins
            .iter()
            .filter(|p| match page {
                _ if searching => p.name.to_lowercase().contains(&q) || p.vendor.to_lowercase().contains(&q),
                Page::Plugins => true,
                _ => false,
            })
            .collect();
        if defs.is_empty() && shown_plugins.is_empty() {
            body = body.child(div().p_4().text_color(tok::text_tertiary()).child("No effects match"));
        }
        for &c in categories {
            let in_cat: Vec<_> = defs.iter().copied().filter(|d| d.category == c).collect();
            if in_cat.is_empty() {
                continue;
            }
            let mut grid = div().grid().grid_cols(3).gap_2();
            for d in in_cat {
                let badge = match d.needs {
                    Needs::Face => Some("Needs a face"),
                    Needs::Person => Some("Needs a person"),
                    Needs::Nothing => None,
                };
                let added = in_stack.contains_key(d.kind);
                let n = list.iter().position(|x| x.kind == d.kind).unwrap_or(0);
                grid = grid.child(
                    tile(("fx-tile", n).into(), d.icon, d.name, d.description, badge, added)
                        .on_click(cx.listener(move |this, _, window, cx| this.add_from_catalog(iid, d, window, cx))),
                );
            }
            if page == Page::All || searching {
                body = body.child(group_heading(c.name()));
            }
            body = body.child(grid);
        }
        if !shown_plugins.is_empty() {
            let in_stack: Vec<&str> = owner_effects
                .iter()
                .filter(|e| e.kind == catalog::PLUGIN_KIND)
                .filter_map(|e| e.settings.get("id").and_then(Value::as_str))
                .collect();
            // by vendor, three across
            let mut vendor = None;
            let mut grid = div().grid().grid_cols(3).gap_2();
            for (n, p) in shown_plugins.into_iter().enumerate() {
                if vendor != Some(p.vendor.as_str()) {
                    if vendor.is_some() {
                        body = body.child(grid);
                        grid = div().grid().grid_cols(3).gap_2();
                    }
                    vendor = Some(p.vendor.as_str());
                    body = body.child(group_heading(if p.vendor.is_empty() { "Other" } else { &p.vendor }));
                }
                let added = in_stack.contains(&p.id.as_str());
                let what = match (p.format.as_str(), p.v3) {
                    ("vst3", _) => "VST3",
                    (_, true) => "Audio Unit v3",
                    _ => "Audio Unit",
                };
                let info = p.clone();
                grid = grid.child(
                    tile(("fx-plugin", n).into(), "plug", &p.name, what, None, added)
                        .on_click(cx.listener(move |this, _, window, cx| this.add_plugin(iid, &info, window, cx))),
                );
            }
            body = body.child(grid);
        } else if page == Page::Plugins {
            body = body.child(
                div()
                    .p_4()
                    .text_size(px(12.))
                    .text_color(tok::text_tertiary())
                    .child("No Audio Unit or VST3 effects are installed. Install one and it shows here."),
            );
        }

        Some(
            div()
                .absolute()
                .inset_0()
                .flex()
                .justify_center()
                .items_start()
                .pt(px(64.))
                .bg(tok::scrim())
                .on_mouse_down(MouseButton::Left, cx.listener(|this, _, window, cx| this.close_effect_catalog(window, cx)))
                .child(
                    div()
                        .id("fx-sheet")
                        .flex()
                        .flex_col()
                        .gap_3()
                        .w(px(820.))
                        .h(px(600.))
                        .p_4()
                        .rounded(px(14.))
                        .bg(tok::bg_panel())
                        .border_1()
                        .border_color(tok::border_strong())
                        .shadow_lg()
                        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                        .capture_key_down(cx.listener(|this, ev: &KeyDownEvent, window, cx| {
                            if ev.keystroke.key == "escape" {
                                this.close_effect_catalog(window, cx);
                                cx.stop_propagation();
                            }
                        }))
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .gap_3()
                                .child(
                                    div()
                                        .flex()
                                        .flex_col()
                                        .w(px(168.))
                                        .flex_none()
                                        .child(div().text_size(px(15.)).font_weight(FontWeight::SEMIBOLD).child("Effects"))
                                        .child(
                                            div()
                                                .text_size(px(11.))
                                                .text_color(tok::text_tertiary())
                                                .truncate()
                                                .child(format!("for {owner_name}")),
                                        ),
                                )
                                .child(div().flex_1().child(TextInput::new(&sheet.search)))
                                .child(Button::new("fx-sheet-done").label("Done").small().on_click(
                                    cx.listener(|this, _, window, cx| this.close_effect_catalog(window, cx)),
                                )),
                        )
                        .child(div().flex().flex_1().min_h_0().child(side).child(body)),
                )
                .into_any_element(),
        )
    }
}

/// What a choice means, for the ones whose trade-off isn't obvious.
fn choice_hint(key: &str, chosen: usize) -> Option<&'static str> {
    let hints: &[&str] = match key {
        "bg_model" => &[
            "Apple's person segmentation: finer, steadier edges",
            "Lighter on the GPU; rougher edges and hair",
            "macOS's own camera effect: the best edges, no cost to Lightair. Cameras only",
        ],
        "bg_quality" => &[
            "Lowest cost; coarse edges",
            "Made for video: fine edges that stay steady",
            "Finest edges, most GPU; may trail fast movement",
        ],
        _ => return None,
    };
    hints.get(chosen).copied()
}

/// macOS's own effect: whether it's on for Lightair, and the way to turn it on.
fn system_effect_panel(kind: &str, cx: &mut Context<StudioWindow>) -> Div {
    use crate::system_effects as sys;
    let (name, state) = if kind == "studio.bg_blur" {
        ("Portrait", sys::portrait_on())
    } else {
        ("Backgrounds", sys::backgrounds_on())
    };
    let (dot, text) = match state {
        Some(true) => (tok::ok(), format!("{name} is on in macOS")),
        Some(false) => (tok::warn(), format!("{name} is off: turn it on in Video Effects")),
        None => (tok::warn(), format!("{name} needs a newer macOS")),
    };
    div()
        .flex()
        .flex_col()
        .gap_1p5()
        .p_2()
        .rounded(px(8.))
        .bg(tok::bg_app())
        .child(
            div()
                .flex()
                .items_center()
                .gap_2()
                .child(div().flex_none().size(px(8.)).rounded_full().bg(dot))
                .child(div().flex_1().text_size(px(12.)).child(text)),
        )
        .child(
            div()
                .text_size(px(11.))
                .text_color(tok::text_tertiary())
                .child("macOS does this inside the camera, before Lightair gets the picture. Turn it on for Lightair and choose the blur or picture in Video Effects."),
        )
        .child(
            Button::new("open-video-effects")
                .label("Open Video Effects…")
                .icon(ui::icon("camera"))
                .xsmall()
                .outline()
                .on_click(cx.listener(|_, _, _, cx| {
                    sys::open_video_effects();
                    cx.notify();
                })),
        )
}

/// The EQ's response, drawn: 20 Hz to 20 kHz across, ±18 dB up and down, with the engine's
/// own filters (what's drawn is what's heard).
fn eq_curve(e: &Effect) -> impl IntoElement {
    let Some(lightair_audio::dsp::Params::Eq(p)) = lightair_engine::audio::params(e) else {
        return div().into_any_element();
    };
    const N: usize = 120;
    let (lo, hi) = (20f32.log10(), 20_000f32.log10());
    let x_of = move |f: f32| (f.log10() - lo) / (hi - lo);
    let curve: Vec<(f32, f32)> = (0..N)
        .map(|i| {
            let t = i as f32 / (N - 1) as f32;
            let f = 10f32.powf(lo + t * (hi - lo));
            let db = p.response_db(f, 48_000.0).clamp(-18.0, 18.0);
            (t, 0.5 - db / 36.0)
        })
        .collect();
    let (line, grid, accent) = (tok::border(), tok::border_strong(), tok::accent_primary());
    div()
        .h(px(84.))
        .w_full()
        .rounded(px(6.))
        .bg(tok::bg_app())
        .child(
            canvas(
                |_, _, _| {},
                move |bounds, _, window, _| {
                    let (o, w, h) = (bounds.origin, f32::from(bounds.size.width), f32::from(bounds.size.height));
                    let at = |x: f32, y: f32| point(o.x + px(x * w), o.y + px(y * h));
                    let mut stroke = |pts: &[(f32, f32)], width: f32, colour: Hsla| {
                        let mut path = PathBuilder::stroke(px(width));
                        path.move_to(at(pts[0].0, pts[0].1));
                        for p in &pts[1..] {
                            path.line_to(at(p.0, p.1));
                        }
                        if let Ok(path) = path.build() {
                            window.paint_path(path, colour);
                        }
                    };
                    for db in [-12.0f32, 12.0] {
                        let y = 0.5 - db / 36.0;
                        stroke(&[(0.0, y), (1.0, y)], 1.0, line);
                    }
                    stroke(&[(0.0, 0.5), (1.0, 0.5)], 1.0, grid);
                    for f in [100.0, 1000.0, 10_000.0] {
                        let x = x_of(f);
                        stroke(&[(x, 0.0), (x, 1.0)], 1.0, line);
                    }
                    stroke(&curve, 2.0, accent);
                },
            )
            .size_full(),
        )
        .into_any_element()
}

fn control_row(label: &'static str, control: impl IntoElement) -> Div {
    div()
        .flex()
        .items_center()
        .gap_2()
        .child(div().w(px(88.)).flex_none().truncate().text_size(px(12.)).text_color(tok::text_secondary()).child(label))
        .child(control)
}

fn group_heading(label: &str) -> Div {
    div()
        .text_size(px(11.))
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(tok::text_tertiary())
        .child(label.to_uppercase())
}

fn category_icon(c: Category) -> &'static str {
    match c {
        Category::Face => "scan-face",
        Category::Background => "user-round",
        Category::Colour => "palette",
        Category::Stylize => "sparkles",
        Category::Blur => "droplet",
        Category::Distort => "waves-horizontal",
        Category::Keying => "square-dashed",
        Category::Voice => "mic-vocal",
        Category::Dynamics => "chevrons-down-up",
        Category::Tone => "sliders-vertical",
        Category::Space => "waves-horizontal",
        Category::Utility => "wrench",
    }
}

/// A catalog tile: icon (with what it needs beside it), name, and a line on what it does.
fn tile(
    id: ElementId,
    icon: &str,
    name: &str,
    description: &str,
    badge: Option<&'static str>,
    selected: bool,
) -> Stateful<Div> {
    div()
        .id(id)
        .flex()
        .flex_col()
        .gap_1p5()
        .p_2p5()
        .rounded(px(10.))
        .border_1()
        .border_color(if selected { tok::accent_primary() } else { tok::border() })
        .bg(if selected { tok::bg_selected() } else { tok::bg_raised() })
        .cursor_pointer()
        .hover(|d| d.border_color(tok::border_strong()).bg(tok::bg_selected()))
        .child(
            div()
                .flex()
                .flex_none()
                .items_center()
                .gap_2()
                .child(
                    div()
                        .flex()
                        .flex_none()
                        .items_center()
                        .justify_center()
                        .size(px(28.))
                        .rounded(px(7.))
                        .bg(tok::bg_app())
                        .child(ui::icon(icon).size(px(15.)).text_color(tok::accent_primary())),
                )
                .child(div().flex_1())
                .when_some(badge, |d, b| {
                    d.child(
                        div()
                            .flex()
                            .items_center()
                            .gap_1()
                            .px_1p5()
                            .h(px(18.))
                            .rounded(px(9.))
                            .bg(tok::bg_app())
                            .text_size(px(10.))
                            .text_color(tok::text_tertiary())
                            .child(ui::icon(if b.contains("face") { "scan-face" } else { "user-round" }).size(px(11.)))
                            .child(if b.contains("face") { "Face" } else { "Person" }),
                    )
                })
                .when(selected, |d| d.child(ui::icon("check").size(px(14.)).text_color(tok::accent_primary()))),
        )
        .child(
            div()
                .flex_none()
                .text_size(px(12.))
                .font_weight(FontWeight::MEDIUM)
                .text_color(tok::text())
                .truncate()
                .child(name.to_string()),
        )
        .child(
            div()
                .flex_none()
                .h(px(28.))
                .overflow_hidden()
                .text_size(px(11.))
                .line_height(px(14.))
                .text_color(tok::text_tertiary())
                .child(description.to_string()),
        )
}

/// Right-click on an effect card.
fn effect_menu(
    menu: PopupMenu,
    shell: &WeakEntity<StudioWindow>,
    iid: Owner,
    eid: Id,
    at: usize,
    count: usize,
) -> PopupMenu {
    let (s1, s2, s3, s4) = (shell.clone(), shell.clone(), shell.clone(), shell.clone());
    let mut menu = menu;
    if at > 0 {
        menu = menu.item(PopupMenuItem::new("Move up").icon(ui::icon("chevron-up")).on_click(move |_, _, cx| {
            if let Some(s) = s1.upgrade() {
                s.update(cx, |w, cx| w.move_effect(iid, eid, -1, cx));
            }
        }));
    }
    if at + 1 < count {
        menu = menu.item(PopupMenuItem::new("Move down").icon(ui::icon("chevron-down")).on_click(move |_, _, cx| {
            if let Some(s) = s2.upgrade() {
                s.update(cx, |w, cx| w.move_effect(iid, eid, 1, cx));
            }
        }));
    }
    menu.item(PopupMenuItem::new("Duplicate").icon(ui::icon("copy-plus")).on_click(move |_, _, cx| {
        if let Some(s) = s3.upgrade() {
            s.update(cx, |w, cx| w.duplicate_effect(iid, eid, cx));
        }
    }))
    .separator()
    .item(PopupMenuItem::new("Remove").icon(ui::icon("trash")).on_click(move |_, _, cx| {
        if let Some(s) = s4.upgrade() {
            s.update(cx, |w, cx| w.exec(iid.remove(eid), cx));
        }
    }))
}
