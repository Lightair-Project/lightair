//! The Inspector for a widget (docs/plans/live-integrations.md, 2.5): its settings, made from its
//! fields' descriptions (StreamElements' format: text, numbers, colours, choices, switches,
//! pictures and sounds, buttons), test events to try it with, and its address for OBS. Every
//! change is a source settings change: undoable, and the open page gets it at once.

use gpui_kit::component::button::Button;
use gpui_kit::component::input::{Editor, EditorState, Input as TextInput, InputEvent, InputState};
use gpui_kit::component::menu::{DropdownMenu, PopupMenuItem};
use gpui_kit::component::Sizable;
use gpui_kit::*;
use lightair_core::{Command, Id, Source};
use lightair_widgets::fields::{self, Field};
use lightair_widgets::{Instance, catalog, test_events};
use serde_json::{Value, json};

use super::StudioWindow;
use crate::ui::{self, tok};

/// The text-like fields' inputs for the selected widget.
pub(super) struct WidgetEdit {
    source: Id,
    inputs: Vec<(String, Entity<InputState>)>,
    /// Multi-line fields ("textarea"): set when the widget is selected, saved on leaving them.
    areas: Vec<(String, Entity<EditorState>)>,
    _events: Vec<Subscription>,
}

fn typed(kind: &str) -> bool {
    !matches!(kind, "checkbox" | "dropdown" | "button" | "hidden" | "textarea")
}

fn numeric(kind: &str) -> bool {
    matches!(kind, "number" | "slider")
}

fn shown(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

pub(super) fn widget_of(source: &Source) -> Option<Instance> {
    Instance::from_settings(&source.settings)
}

impl StudioWindow {
    /// Keeps the selected widget's inputs in step (called from render, with the selection).
    pub(super) fn sync_widget_props(&mut self, source: Option<&Source>, window: &mut Window, cx: &mut Context<Self>) {
        let Some((source, instance)) = source.and_then(|s| widget_of(s).map(|i| (s, i))) else {
            self.props.widget = None;
            return;
        };
        let list = fields::parse(instance.schema());
        let values = fields::values(&list, &instance.fields);
        if !self.props.widget.as_ref().is_some_and(|w| w.source == source.id) {
            let mut inputs = Vec::new();
            let mut events = Vec::new();
            for f in list.iter().filter(|f| typed(&f.kind)) {
                let state = cx.new(|cx| InputState::new(window, cx));
                let (id, key, number) = (source.id, f.key.clone(), numeric(&f.kind));
                events.push(cx.subscribe_in(&state, window, move |this, state, ev: &InputEvent, _, cx| {
                    if matches!(ev, InputEvent::PressEnter { .. } | InputEvent::Blur) {
                        let text = state.read(cx).value().to_string();
                        let value = if number { text.trim().parse::<f64>().map(|n| json!(n)).unwrap_or(json!(0)) } else { json!(text) };
                        this.set_widget_field(id, &key, value, cx);
                    }
                }));
                inputs.push((f.key.clone(), state));
            }
            let mut areas = Vec::new();
            for f in list.iter().filter(|f| f.kind == "textarea") {
                let text = values.get(&f.key).map(shown).unwrap_or_default();
                let state = cx.new(|cx| EditorState::new(window, cx).line_number(false).default_value(text));
                let (id, key) = (source.id, f.key.clone());
                events.push(cx.subscribe_in(&state, window, move |this, state, ev: &InputEvent, _, cx| {
                    if matches!(ev, InputEvent::Blur) {
                        let text = state.read(cx).value().to_string();
                        this.set_widget_field(id, &key, json!(text), cx);
                    }
                }));
                areas.push((f.key.clone(), state));
            }
            self.props.widget = Some(WidgetEdit { source: source.id, inputs, areas, _events: events });
        }
        // colour fields: a full picker each
        for f in list.iter().filter(|f| f.kind == "colorpicker") {
            let current = values.get(&f.key).and_then(|v| v.as_str()).and_then(super::colour_field::parse).unwrap_or_else(|| gpui_kit::rgb(0xFFFFFF).into());
            let (id, key) = (source.id, f.key.clone());
            let apply: super::colour_field::Apply = std::rc::Rc::new(move |this: &mut StudioWindow, c, cx| {
                this.set_widget_field(id, &key, json!(super::colour_field::css(c)), cx);
            });
            self.sync_colour(&format!("w-{}-{}", source.id, f.key), current, apply, window, cx);
        }
        let w = self.props.widget.as_ref().unwrap();
        for (key, state) in &w.inputs {
            let value = values.get(key).map(shown).unwrap_or_default();
            let focused = state.read(cx).focus_handle(cx).is_focused(window);
            if !focused && state.read(cx).value() != value.as_str() {
                state.update(cx, |s, cx| s.set_value(value, window, cx));
            }
        }
    }

    pub(super) fn set_widget_field(&mut self, source: Id, key: &str, value: Value, cx: &mut Context<Self>) {
        let workspace = self.studio.read(cx).workspace().clone();
        let Some(s) = workspace.sources.get(&source) else { return };
        let Some(mut instance) = widget_of(s) else { return };
        let list = fields::parse(instance.schema());
        if fields::values(&list, &instance.fields).get(key) == Some(&value) {
            return;
        }
        if !instance.fields.is_object() {
            instance.fields = json!({});
        }
        instance.fields[key] = value;
        let mut settings = s.settings.clone();
        settings["widget"] = json!(instance);
        self.exec(Command::SetSourceSettings { source, settings }, cx);
    }

    /// The platform test events say they're from: the first destination's, else Twitch.
    fn test_platform(&self, cx: &App) -> String {
        self.studio.read(cx).workspace().outputs.destinations.iter().find_map(|d| d.platform.clone()).unwrap_or_else(|| "twitch".into())
    }

    pub(super) fn widget_section(&self, source: &Source, cx: &mut Context<Self>) -> Option<AnyElement> {
        let instance = widget_of(source)?;
        let widgets = self.studio.read(cx).widgets.clone()?;
        let list = fields::parse(instance.schema());
        let values = fields::values(&list, &instance.fields);
        let title: &'static str = catalog::by_kind(&instance.kind).map_or("Widget", |b| b.name);
        let label = |text: String| div().text_size(px(11.)).text_color(tok::text_tertiary()).child(text);

        let url = widgets.url(&instance.id, &instance.key);
        let copy = ui::icon_button("widget-copy-url", "link", tok::text_secondary())
            .tooltip(ui::tip("Copy its address (for OBS or a browser on this computer)"))
            .on_click(cx.listener(move |_, _, _, cx| cx.write_to_clipboard(ClipboardItem::new_string(url.clone()))));
        let mut body = self.section(title, Some(copy.into_any_element()), cx);
        // an imported widget can go out again as a package
        if let Some(custom) = instance.custom.clone() {
            let size = (source.settings["width"].as_u64().unwrap_or(800) as u32, source.settings["height"].as_u64().unwrap_or(600) as u32);
            let name = source.name.clone();
            body = body.child(Button::new("widget-export").label("Export as .lwidget…").xsmall().outline().on_click(cx.listener(move |_, _, window, cx| {
                let path = cx.prompt_for_new_path(&std::env::home_dir().unwrap_or_default().join("Desktop"), Some(&format!("{name}.lwidget")));
                let (custom, name) = (custom.clone(), name.clone());
                cx.spawn_in(window, async move |_, _| {
                    if let Ok(Ok(Some(path))) = path.await
                        && let Err(e) = lightair_widgets::export(&custom, &name, size, &path)
                    {
                        tracing::warn!(%e, "widget export");
                    }
                })
                .detach();
            })));
        }

        // test events
        let platform = self.test_platform(cx);
        let mut tests = div().flex().flex_wrap().gap_1();
        for (i, (kind, name)) in test_events::KINDS.iter().enumerate() {
            let (w, platform) = (widgets.clone(), platform.clone());
            tests = tests.child(
                Button::new(("widget-test", i))
                    .label(*name)
                    .xsmall()
                    .outline()
                    .on_click(move |_, _, _| w.test(kind, &platform)),
            );
        }
        body = body.child(label("Try it".into())).child(tests);

        // the fields, in their groups
        let mut group = String::new();
        for f in list.iter().filter(|f| f.is_shown()) {
            if !f.group.is_empty() && f.group != group {
                group = f.group.clone();
                body = body.child(div().pt_2().text_size(px(11.)).font_weight(FontWeight::SEMIBOLD).text_color(tok::text_secondary()).child(group.clone()));
            }
            body = body.child(self.widget_field(source.id, &instance, f, values.get(&f.key).cloned().unwrap_or(Value::Null), cx));
        }
        Some(body.into_any_element())
    }

    fn widget_field(&self, source: Id, instance: &Instance, f: &Field, value: Value, cx: &mut Context<Self>) -> AnyElement {
        let label = div().text_size(px(12.)).text_color(tok::text_secondary()).child(f.label.clone());
        let key = f.key.clone();
        match f.kind.as_str() {
            "checkbox" => {
                let on = value.as_bool().unwrap_or(false);
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap_2()
                    .child(label)
                    .child(super::settings::switch(SharedString::from(format!("wf-{key}")), on).on_click(cx.listener(move |this, _, _, cx| {
                        this.set_widget_field(source, &key, json!(!on), cx);
                    })))
                    .into_any_element()
            }
            "dropdown" => {
                let current = shown(&value);
                let shown_label = f.options.iter().find(|(v, _)| *v == current).map_or(current.clone(), |(_, l)| l.clone());
                let options = f.options.clone();
                let me = cx.entity().downgrade();
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(label)
                    .child(Button::new(SharedString::from(format!("wf-{key}"))).label(shown_label).small().outline().w_full().dropdown_caret(true).dropdown_menu(
                        move |mut menu, _, _| {
                            for (v, l) in options.iter().cloned() {
                                let (me, key) = (me.clone(), key.clone());
                                menu = menu.item(PopupMenuItem::new(l).on_click(move |_, _, cx| {
                                    if let Some(me) = me.upgrade() {
                                        me.update(cx, |this, cx| this.set_widget_field(source, &key, json!(v.clone()), cx));
                                    }
                                }));
                            }
                            menu
                        },
                    ))
                    .into_any_element()
            }
            "button" => {
                let (widgets, id) = (self.studio.read(cx).widgets.clone(), instance.id.clone());
                Button::new(SharedString::from(format!("wf-{key}")))
                    .label(f.label.clone())
                    .small()
                    .outline()
                    .w_full()
                    .on_click(move |_, _, _| {
                        if let Some(w) = &widgets {
                            w.button(&id, &key);
                        }
                    })
                    .into_any_element()
            }
            "textarea" => match self.props.widget.as_ref().filter(|w| w.source == source).and_then(|w| w.areas.iter().find(|(k, _)| *k == f.key)) {
                Some((_, state)) => div()
                    .rounded_md()
                    .border_1()
                    .border_color(tok::border())
                    .child(Editor::new(state).h(px(96.)).bordered(false))
                    .into_any_element(),
                None => div().into_any_element(),
            },
            kind => {
                let Some(state) = self.props.widget.as_ref().filter(|w| w.source == source).and_then(|w| w.inputs.iter().find(|(k, _)| *k == f.key)).map(|(_, s)| s.clone())
                else {
                    return div().into_any_element();
                };
                let file = matches!(kind, "image-input" | "sound-input" | "video-input");
                let mut row = div().flex().items_center().gap_1();
                if kind == "colorpicker" {
                    row = row.child(div().flex_none().child(self.colour_field(&format!("w-{source}-{}", f.key), cx)));
                }
                row = row.child(div().flex_1().min_w_0().child(TextInput::new(&state).small()));
                if file {
                    let key = key.clone();
                    row = row.child(
                        Button::new(SharedString::from(format!("wf-pick-{key}"))).label("Choose…").xsmall().outline().on_click(cx.listener(move |_, _, window, cx| {
                            let paths = cx.prompt_for_paths(PathPromptOptions { files: true, directories: false, multiple: false, prompt: None });
                            let key = key.clone();
                            cx.spawn_in(window, async move |this, cx| {
                                let Ok(Ok(Some(paths))) = paths.await else { return };
                                let Some(path) = paths.into_iter().next() else { return };
                                let _ = this.update(cx, |this, cx| this.set_widget_field(source, &key, json!(path.to_string_lossy()), cx));
                            })
                            .detach();
                        })),
                    );
                }
                div().flex().flex_col().gap_1().child(label).child(row).into_any_element()
            }
        }
    }
}
