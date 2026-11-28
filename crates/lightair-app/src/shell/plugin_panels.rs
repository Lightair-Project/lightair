//! The Plugin panels panel: every running script plugin's panels (docs/13 tier 3,
//! script-api.md "Panels"), drawn natively from the elements each script describes — labels,
//! buttons, toggles, sliders, text fields and lists — each card with its package's badge. Using
//! an element sends it to the script (`scripts_host::ui`); the script answers by updating its
//! panel.

use gpui_kit::component::Sizable;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::dock::PanelEvent;
use gpui_kit::component::input::{Input as TextInput, InputState};
use gpui_kit::component::switch::Switch;
use gpui_kit::*;
use serde_json::{Value, json};

use crate::scripts_host as sh;
use crate::ui::{self, tok};

pub struct PluginPanels {
    focus: FocusHandle,
    seen: u64,
    edit: Entity<InputState>,
    /// The text element being edited: (package, panel, element).
    editing: Option<(String, String, String)>,
}

impl PluginPanels {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        // redraw when a script starts, stops or changes a panel
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(std::time::Duration::from_millis(300)).await;
                let ok = this.update(cx, |this, cx| {
                    let g = sh::generation();
                    if g != this.seen {
                        this.seen = g;
                        cx.notify();
                    }
                });
                if ok.is_err() {
                    break;
                }
            }
        })
        .detach();
        PluginPanels {
            focus: cx.focus_handle(),
            seen: 0,
            edit: cx.new(|cx| InputState::new(window, cx)),
            editing: None,
        }
    }

    fn element(&self, pkg: &str, panel: &str, el: &Value, n: usize, cx: &mut Context<Self>) -> Option<AnyElement> {
        let id = el["id"].as_str().unwrap_or_default().to_string();
        let text = el["text"].as_str().or(el["label"].as_str()).unwrap_or_default().to_string();
        let (p, pl, i) = (pkg.to_string(), panel.to_string(), id.clone());
        let send = move |value: Value| sh::ui(&p, &pl, &i, value);
        Some(match el["type"].as_str()? {
            "label" => div().text_size(px(12.)).text_color(tok::text()).child(text).into_any_element(),
            "button" => Button::new(("plugin-ui-button", n))
                .label(text)
                .small()
                .outline()
                .on_click(move |_, _, _| send(Value::Null))
                .into_any_element(),
            "toggle" => {
                let on = el["value"].as_bool().unwrap_or(false);
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(div().flex_1().text_size(px(12.)).text_color(tok::text()).child(text))
                    .child(
                        Switch::new(("plugin-ui-toggle", n))
                            .checked(on)
                            .small()
                            .on_click(move |c: &bool, _, _| send(json!(*c))),
                    )
                    .into_any_element()
            }
            "slider" => {
                let (min, max) = (el["min"].as_f64().unwrap_or(0.0), el["max"].as_f64().unwrap_or(100.0));
                let step = el["step"].as_f64().unwrap_or(((max - min) / 20.0).max(f64::EPSILON));
                let v = el["value"].as_f64().unwrap_or(min);
                let (down, up) = (send.clone(), send);
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(div().flex_1().text_size(px(12.)).text_color(tok::text()).child(text))
                    .child(
                        Button::new(("plugin-ui-down", n))
                            .label("−")
                            .xsmall()
                            .ghost()
                            .on_click(move |_, _, _| down(json!((v - step).max(min)))),
                    )
                    .child(
                        div()
                            .w(px(44.))
                            .text_center()
                            .text_size(px(12.))
                            .text_color(tok::text_secondary())
                            .child(trim(v)),
                    )
                    .child(
                        Button::new(("plugin-ui-up", n))
                            .label("+")
                            .xsmall()
                            .ghost()
                            .on_click(move |_, _, _| up(json!((v + step).min(max)))),
                    )
                    .into_any_element()
            }
            "text" => {
                let key = (pkg.to_string(), panel.to_string(), id.clone());
                let value = el["value"].as_str().unwrap_or_default().to_string();
                if self.editing.as_ref() == Some(&key) {
                    div()
                        .flex()
                        .items_center()
                        .gap_1()
                        .child(div().flex_1().child(TextInput::new(&self.edit).small()))
                        .child(Button::new(("plugin-ui-send", n)).label("Send").xsmall().outline().on_click(
                            cx.listener(move |this, _, _, cx| {
                                let text = this.edit.read(cx).value().to_string();
                                send(json!(text));
                                this.editing = None;
                                cx.notify();
                            }),
                        ))
                        .into_any_element()
                } else {
                    let shown = if value.is_empty() { text.clone() } else { value.clone() };
                    div()
                        .flex()
                        .items_center()
                        .gap_2()
                        .child(div().flex_1().text_size(px(12.)).text_color(tok::text()).child(text))
                        .child(
                            Button::new(("plugin-ui-edit", n))
                                .label(if shown.is_empty() { "—".to_string() } else { shown })
                                .xsmall()
                                .ghost()
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    this.edit.update(cx, |s, cx| s.set_value(value.clone(), window, cx));
                                    this.editing = Some(key.clone());
                                    cx.notify();
                                })),
                        )
                        .into_any_element()
                }
            }
            "list" => {
                let mut list = div().flex().flex_col().gap_0p5();
                if !text.is_empty() {
                    list = list.child(div().text_size(px(11.)).text_color(tok::text_secondary()).child(text));
                }
                for (k, item) in el["items"].as_array().into_iter().flatten().enumerate().take(200) {
                    let label = item
                        .as_str()
                        .map(str::to_string)
                        .unwrap_or_else(|| item["text"].as_str().unwrap_or_default().to_string());
                    let (s, item) = (send.clone(), item.clone());
                    list = list.child(
                        div()
                            .id(("plugin-ui-item", n * 1000 + k))
                            .px_2()
                            .py_0p5()
                            .rounded(px(4.))
                            .text_size(px(12.))
                            .text_color(tok::text())
                            .cursor_pointer()
                            .hover(|d| d.bg(tok::bg_raised()))
                            .on_click(move |_, _, _| s(item.clone()))
                            .child(label),
                    );
                }
                list.into_any_element()
            }
            _ => return None,
        })
    }
}

fn trim(v: f64) -> String {
    if v.fract() == 0.0 { format!("{v:.0}") } else { format!("{v:.2}") }
}

impl Render for PluginPanels {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let mut body = div().flex().flex_col().gap_3().p_3();
        let mut any = false;
        let mut n = 0;
        for (pkg, name, declared) in sh::everything() {
            for panel in &declared.panels {
                any = true;
                let mut card = div()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .p_3()
                    .rounded(px(8.))
                    .border_1()
                    .border_color(tok::border())
                    .bg(tok::bg_raised())
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(
                                div()
                                    .flex_1()
                                    .text_size(px(12.))
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .text_color(tok::text())
                                    .child(panel.title.clone()),
                            )
                            // the package badge: which plugin draws this
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap_1()
                                    .px_1p5()
                                    .rounded(px(4.))
                                    .bg(tok::bg_panel())
                                    .text_size(px(10.5))
                                    .text_color(tok::text_tertiary())
                                    .child(ui::icon("puzzle").size(px(11.)))
                                    .child(name.clone()),
                            ),
                    );
                for el in panel.ui.as_array().into_iter().flatten() {
                    n += 1;
                    if let Some(e) = self.element(&pkg, &panel.id, el, n, cx) {
                        card = card.child(e);
                    }
                }
                body = body.child(card);
            }
        }
        if !any {
            body = body.child(
                div()
                    .py_6()
                    .text_center()
                    .text_size(px(12.))
                    .text_color(tok::text_tertiary())
                    .child("Panels from script plugins show here."),
            );
        }
        div().id("plugin-panels").size_full().overflow_y_scroll().bg(tok::bg_panel()).child(body)
    }
}

impl gpui_kit::component::dock::BasePanel for PluginPanels {
    fn panel_name(&self) -> &'static str {
        "plugin-panels"
    }
}

impl gpui_kit::component::dock::Panel for PluginPanels {
    fn tab_name(&self, _: &App) -> Option<SharedString> {
        None
    }

    fn title(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        ui::panel_title("puzzle", "Plugin panels")
    }

    fn title_style(&self, _: &App) -> Option<gpui_kit::component::dock::TitleStyle> {
        Some(ui::panel_header_style())
    }

    fn zoom_control(&self, _: &App) -> Option<gpui_kit::component::dock::PanelControl> {
        None
    }

    fn inner_padding(&self, _: &App) -> bool {
        false
    }
}

impl EventEmitter<PanelEvent> for PluginPanels {}

impl Focusable for PluginPanels {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}
