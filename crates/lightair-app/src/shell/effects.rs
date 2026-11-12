//! Effects (docs/plans/effects-revamp.md): the source's effect stack in the Inspector, each
//! effect a card with its own switch, summary and controls, and the Effect catalog sheet that
//! adds them (looks, face, background, colour, stylize, blur, distort, keying).

use std::collections::HashMap;

use gpui_kit::component::Sizable;
use gpui_kit::component::Disableable;
use gpui_kit::component::button::{Button, ButtonVariants};
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

/// A page of the catalog sheet.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Page {
    All,
    Looks,
    Of(Category),
}

pub(super) struct EffectSheet {
    input: Id,
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

/// Colours offered for effect colour controls: neutrals, then makeup-friendly and bright ones.
const SWATCHES: &[u32] = &[
    0xFFFFFF, 0x1A1A1A, 0x2F6B3A, 0x00B140, 0x0047BB, 0xC0405A, 0xD98080, 0xE8A0A0, 0xB5654A, 0x8E3B5A, 0xF5A5A5,
    0xC89080, 0xFFB020, 0x8B5CF6, 0x1AFFE6, 0xFF4A3B,
];

impl StudioWindow {
    // ------------------------------------------------------------ the stack

    pub(super) fn effects_section(&self, input: &Input, cx: &mut Context<Self>) -> AnyElement {
        let iid = input.id;
        let add = Button::new("add-effect")
            .icon(ui::icon("plus"))
            .label("Add")
            .xsmall()
            .ghost()
            .on_click(cx.listener(move |this, _, window, cx| this.open_effect_catalog(iid, Page::All, window, cx)));
        let mut col = self.section("Effects", Some(add.into_any_element()), cx);

        // Cameras get a look row: the whole face look in one click.
        if camera_studio::is_camera(&input.kind) {
            let look = camera_studio::current_look(input);
            let has_face = input.effects.iter().any(|e| e.kind.starts_with("studio."));
            col = col.child(
                div()
                    .id("fx-look")
                    .flex()
                    .items_center()
                    .gap_2()
                    .h(px(36.))
                    .px_2()
                    .rounded(px(8.))
                    .bg(tok::bg_raised())
                    .cursor_pointer()
                    .hover(|d| d.bg(tok::bg_selected()))
                    .child(ui::icon("wand-sparkles").size(px(15.)).text_color(tok::accent_primary()))
                    .child(div().flex_1().text_size(px(12.)).child(match look {
                        Some(l) => SharedString::from(l),
                        None if has_face => "Custom look".into(),
                        None => "Choose a look".into(),
                    }))
                    .child(div().text_size(px(11.)).text_color(tok::text_tertiary()).child("Looks"))
                    .child(ui::icon("chevron-right").size(px(12.)).text_color(tok::text_tertiary()))
                    .on_click(
                        cx.listener(move |this, _, window, cx| this.open_effect_catalog(iid, Page::Looks, window, cx)),
                    ),
            );
        }

        if input.effects.is_empty() {
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
                    .child(
                        div()
                            .text_size(px(11.))
                            .text_color(tok::text_tertiary())
                            .child("Face, background, colour, stylize and more"),
                    )
                    .on_click(
                        cx.listener(move |this, _, window, cx| this.open_effect_catalog(iid, Page::All, window, cx)),
                    ),
            );
        }
        let presence = self.presence.get(&iid).copied().unwrap_or_default();
        for (i, e) in input.effects.iter().enumerate() {
            col = col.child(self.effect_card(input, i, e, presence, cx));
        }
        col.into_any_element()
    }

    fn effect_card(&self, input: &Input, i: usize, e: &Effect, presence: Presence, cx: &mut Context<Self>) -> AnyElement {
        let def = catalog::def(&e.kind);
        let (iid, eid, on) = (input.id, e.id, e.enabled);
        let open = self.props.open_effect == Some(eid);
        let system = e.settings.get("bg_model").and_then(Value::as_i64) == Some(catalog::BG_MODEL_SYSTEM);
        let summary = match def {
            Some(_) if system => "macOS camera effect".into(),
            Some(d) => d.summary(&e.settings),
            None => "Plugin effect".into(),
        };
        let warning = def.filter(|_| on && !system).and_then(|d| match d.needs {
            Needs::Face if presence.face == Some(false) => Some("No face in view"),
            Needs::Person if presence.person == Some(false) => Some("No person in view"),
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
                this.exec(Command::SetInputEffectEnabled { input: iid, effect: eid, enabled: !on }, cx)
            })))
            .child(ui::icon(if open { "chevron-down" } else { "chevron-right" }).size(px(12.)).text_color(tok::text_tertiary()));

        let shell = cx.entity().downgrade();
        let count = input.effects.len();
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
        iid: Id,
        e: &Effect,
        def: Option<&'static EffectDef>,
        i: usize,
        count: usize,
        cx: &mut Context<Self>,
    ) -> Div {
        let eid = e.id;
        let mut row = div().flex().items_center().gap_1().px_2().pb_2();
        if let Some(d) = def {
            let defaults = Value::Object(d.defaults());
            let is_default = defaults == e.settings;
            row = row.child(
                Button::new(("fx-reset", i)).label("Reset").xsmall().ghost().disabled(is_default).on_click(cx.listener(
                    move |this, _, _, cx| {
                        let settings = defaults.clone();
                        this.exec(Command::SetInputEffectSettings { input: iid, effect: eid, settings }, cx)
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
                    move |this, _, _, cx| this.exec(Command::RemoveInputEffect { input: iid, effect: eid }, cx),
                )),
            )
    }

    fn effect_controls(&self, iid: Id, e: &Effect, d: &'static EffectDef, cx: &mut Context<Self>) -> Div {
        let eid = e.id;
        let set = move |settings: Value| Command::SetInputEffectSettings { input: iid, effect: eid, settings };
        let mut col = div().flex().flex_col().gap_1p5().px_2().pb_2().pt_0p5();
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
                    let opts = options
                        .iter()
                        .enumerate()
                        .map(|(n, (l, p))| (*l, set(with(&e.settings, key, p.json())), chosen == Some(n)))
                        .collect();
                    col = col.child(control_row(label, self.segmented(key, opts, cx).flex_1()));
                    if let Some(hint) = chosen.and_then(|n| choice_hint(key, n)) {
                        col = col.child(
                            div().pl(px(96.)).text_size(px(11.)).text_color(tok::text_tertiary()).child(hint),
                        );
                    }
                }
                Control::Colour { key, label, default } => {
                    let current = e.settings.get(key).and_then(Value::as_u64).map_or(default, |v| v as u32);
                    let mut row = div().flex().flex_wrap().gap_1().flex_1();
                    let mut swatches: Vec<u32> = SWATCHES.to_vec();
                    let cur_rgb = super::layer_props::rgb_of(current);
                    if !swatches.contains(&cur_rgb) {
                        swatches.insert(0, cur_rgb);
                    }
                    for (n, rgb) in swatches.into_iter().enumerate() {
                        let settings = with(&e.settings, key, json!(super::layer_props::abgr(rgb)));
                        row = row.child(
                            div()
                                .id(SharedString::from(format!("fx-sw-{key}-{n}")))
                                .size(px(18.))
                                .rounded_full()
                                .cursor_pointer()
                                .bg(super::layer_props::rgb_to_hsla(rgb))
                                .border_2()
                                .border_color(if rgb == cur_rgb { tok::text() } else { tok::border() })
                                .on_click(cx.listener(move |this, _, _, cx| this.exec(set(settings.clone()), cx))),
                        );
                    }
                    col = col.child(control_row(label, row));
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

    fn fx_slider(&self, iid: Id, e: &Effect, s: SliderDef, cx: &mut Context<Self>) -> Div {
        let eid = e.id;
        let value = s.value(&e.settings);
        let settings = e.settings.clone();
        let slider = self.slider(
            format!("fx:{eid}:{}", s.key),
            (s.min as f32, s.max as f32, s.step as f32),
            value as f32,
            move |v| Command::SetInputEffectSettings {
                input: iid,
                effect: eid,
                settings: with(&settings, s.key, s.json(v as f64)),
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
                            this.exec(Command::SetInputEffectSettings { input: iid, effect: eid, settings: reset.clone() }, cx)
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
        iid: Id,
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
                            this.exec(Command::SetInputEffectSettings { input: iid, effect: eid, settings }, cx)
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
        iid: Id,
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
                this.exec(Command::SetInputEffectSettings { input: iid, effect: eid, settings }, cx)
            });
        })
        .detach();
    }

    pub(super) fn move_effect(&mut self, iid: Id, eid: Id, by: i32, cx: &mut Context<Self>) {
        let project = self.studio.read(cx).project().clone();
        let Some(input) = project.inputs.get(&iid) else { return };
        let Some(at) = input.effects.iter().position(|e| e.id == eid) else { return };
        let to = at as i32 + by;
        if to < 0 || to as usize >= input.effects.len() {
            return;
        }
        let effect = input.effects[at].clone();
        let label = format!("Move {}", effect.name);
        let commands = vec![
            Command::RemoveInputEffect { input: iid, effect: eid },
            Command::AddInputEffect { input: iid, effect, index: Some(to as usize) },
        ];
        self.exec(Command::Batch { label, commands }, cx);
    }

    pub(super) fn duplicate_effect(&mut self, iid: Id, eid: Id, cx: &mut Context<Self>) {
        let project = self.studio.read(cx).project().clone();
        let Some(input) = project.inputs.get(&iid) else { return };
        let Some(at) = input.effects.iter().position(|e| e.id == eid) else { return };
        let mut copy = input.effects[at].clone();
        copy.id = Id::new();
        self.props.open_effect = Some(copy.id);
        self.exec(Command::AddInputEffect { input: iid, effect: copy, index: Some(at + 1) }, cx);
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

    pub(super) fn open_effect_catalog(&mut self, input: Id, page: Page, window: &mut Window, cx: &mut Context<Self>) {
        let search = cx.new(|cx| InputState::new(window, cx).placeholder("Search effects"));
        search.update(cx, |s, cx| s.focus(window, cx));
        let events = cx.subscribe_in(&search, window, |_, _, ev: &InputEvent, _, cx| {
            if matches!(ev, InputEvent::Change) {
                cx.notify();
            }
        });
        self.effect_sheet = Some(EffectSheet { input, page, search, _events: events });
        cx.notify();
    }

    fn close_effect_catalog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.effect_sheet = None;
        window.focus(&self.focus, cx);
        cx.notify();
    }

    /// Adds an effect from the catalog (a face effect already in the stack opens instead).
    fn add_from_catalog(&mut self, input: Id, def: &'static EffectDef, window: &mut Window, cx: &mut Context<Self>) {
        let project = self.studio.read(cx).project().clone();
        let Some(i) = project.inputs.get(&input) else { return };
        if def.is_studio()
            && let Some(e) = i.effects.iter().find(|e| e.kind == def.kind)
        {
            self.props.open_effect = Some(e.id);
            if !e.enabled {
                self.exec(Command::SetInputEffectEnabled { input, effect: e.id, enabled: true }, cx);
            }
        } else {
            let effect = def.create();
            self.props.open_effect = Some(effect.id);
            self.exec(Command::AddInputEffect { input, effect, index: None }, cx);
        }
        self.close_effect_catalog(window, cx);
    }

    pub(super) fn effect_catalog_overlay(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let sheet = self.effect_sheet.as_ref()?;
        let (iid, page) = (sheet.input, sheet.page);
        let query = sheet.search.read(cx).value().to_string();
        let project = self.studio.read(cx).project().clone();
        let input = project.inputs.get(&iid)?.clone();
        let searching = !query.trim().is_empty();

        // Categories down the side.
        let mut side = div().flex().flex_col().gap_0p5().w(px(168.)).flex_none().pr_2();
        let mut pages = vec![(Page::All, "All effects", "layout-grid")];
        if !camera_studio::looks().is_empty() {
            pages.push((Page::Looks, "Looks", "wand-sparkles"));
        }
        for c in Category::ALL {
            pages.push((Page::Of(c), c.name(), category_icon(c)));
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
        let in_stack: HashMap<&str, bool> = input.effects.iter().map(|e| (e.kind.as_str(), e.enabled)).collect();
        let mut body = div().id("fx-sheet-body").flex().flex_col().gap_3().flex_1().min_h_0().overflow_y_scroll().pr_1();
        if page == Page::Looks && !searching {
            let current = camera_studio::current_look(&input);
            for (ci, (category, list)) in camera_studio::looks().iter().enumerate() {
                let mut grid = div().grid().grid_cols(3).gap_2();
                for (li, look) in list.iter().enumerate() {
                    let selected = current == Some(look.name.as_str());
                    let look = look.clone();
                    let input = input.clone();
                    grid = grid.child(
                        tile(("fx-look", ci * 100 + li).into(), "wand-sparkles", &look.name, "A whole face look", None, selected)
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.exec(camera_studio::apply_look(&input, &look), cx);
                                this.close_effect_catalog(window, cx);
                            })),
                    );
                }
                body = body.child(group_heading(category)).child(grid);
            }
        } else {
            let defs: Vec<&'static EffectDef> = if searching {
                catalog::search(&query).collect()
            } else {
                match page {
                    Page::Of(c) => catalog::in_category(c).collect(),
                    _ => catalog::CATALOG.iter().collect(),
                }
            };
            if defs.is_empty() {
                body = body.child(div().p_4().text_color(tok::text_tertiary()).child("No effects match"));
            }
            for c in Category::ALL {
                let list: Vec<_> = defs.iter().copied().filter(|d| d.category == c).collect();
                if list.is_empty() {
                    continue;
                }
                let mut grid = div().grid().grid_cols(3).gap_2();
                for d in list {
                    let badge = match d.needs {
                        Needs::Face => Some("Needs a face"),
                        Needs::Person => Some("Needs a person"),
                        Needs::Nothing => None,
                    };
                    let added = in_stack.contains_key(d.kind);
                    let n = catalog::CATALOG.iter().position(|x| x.kind == d.kind).unwrap_or(0);
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
                                                .child(format!("for {}", input.name)),
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
    iid: Id,
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
            s.update(cx, |w, cx| w.exec(Command::RemoveInputEffect { input: iid, effect: eid }, cx));
        }
    }))
}
