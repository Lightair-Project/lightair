//! The Activity panel (docs/plans/live-integrations.md, 2.4): this stream's follows, subs,
//! donations, cheers and raids from every platform, newest first, with this session's totals;
//! any one can be shown again as an alert, the alert showing skipped, and test events sent.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use gpui_kit::component::Sizable;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;
use lightair_live::ChatEvent;

use crate::outputs_panel::badge;
use crate::platforms;
use crate::studio::Studio;
use crate::ui::{self, tok};

/// Events kept on screen.
const SHOWN: usize = 200;

pub struct ActivityPanel {
    studio: Entity<Studio>,
    focus: FocusHandle,
}

fn kind_icon(kind: &str) -> &'static str {
    match kind {
        "follow" => "heart",
        "sub" => "star",
        "paid" => "gift",
        "cheer" => "gem",
        "raid" => "users",
        _ => "sparkles",
    }
}

fn line(e: &ChatEvent, kind: &str) -> String {
    let name = e.name();
    match (kind, &e.kind) {
        ("raid", lightair_live::Kind::Raid { viewers }) => format!("{name} raided with {viewers}"),
        ("follow", _) => format!("{name} followed"),
        ("sub", _) => e.headline.clone().unwrap_or_else(|| format!("{name} subscribed")),
        _ => format!("{name} · {}", e.paid.as_ref().map_or(String::new(), |p| p.display.clone())),
    }
}

fn ago(ts_ms: i64) -> String {
    let s = ((lightair_live::event::now_ms() - ts_ms) / 1000).max(0);
    match s {
        0..60 => "now".into(),
        60..3600 => format!("{}m", s / 60),
        _ => format!("{}h", s / 3600),
    }
}

impl ActivityPanel {
    pub fn new(studio: Entity<Studio>, cx: &mut Context<Self>) -> Self {
        let dirty = Arc::new(AtomicBool::new(true));
        {
            let dirty = dirty.clone();
            studio.read(cx).chat.on_change(move || dirty.store(true, Ordering::Release));
        }
        cx.spawn(async move |this, cx| {
            let mut ticks = 0u32;
            loop {
                cx.background_executor().timer(Duration::from_millis(250)).await;
                ticks += 1;
                // new events, or the "2m ago"s every half minute
                if (dirty.swap(false, Ordering::AcqRel) || ticks.is_multiple_of(120)) && this.update(cx, |_, cx| cx.notify()).is_err() {
                    break;
                }
                if this.upgrade().is_none() {
                    break;
                }
            }
        })
        .detach();
        ActivityPanel { studio, focus: cx.focus_handle() }
    }

    fn totals(&self, cx: &App) -> Option<Div> {
        let widgets = self.studio.read(cx).widgets.clone()?;
        let s = widgets.session();
        let n = |k: &str| s["counts"][k].as_u64().unwrap_or(0);
        let mut parts = Vec::new();
        for (k, one, many) in [("follow", "follow", "follows"), ("sub", "sub", "subs"), ("raid", "raid", "raids")] {
            if n(k) > 0 {
                parts.push(format!("{} {}", n(k), if n(k) == 1 { one } else { many }));
            }
        }
        if let Some(totals) = s["totals"].as_object() {
            for (currency, amount) in totals {
                let a = amount.as_f64().unwrap_or(0.0);
                parts.push(match currency.as_str() {
                    "BITS" => format!("{a} bits"),
                    c if c.len() == 3 => format!("{a:.2} {c}"),
                    c => format!("{a} {}", c.to_lowercase().replace('_', " ")),
                });
            }
        }
        let text = if parts.is_empty() { "Nothing yet this stream".to_string() } else { parts.join(" · ") };
        Some(div().text_size(px(11.)).text_color(tok::text_tertiary()).child(text))
    }

    fn controls(&self, cx: &mut Context<Self>) -> Div {
        let widgets = self.studio.read(cx).widgets.clone();
        let platform = self
            .studio
            .read(cx)
            .workspace()
            .outputs
            .destinations
            .iter()
            .find_map(|d| d.platform.clone())
            .unwrap_or_else(|| "twitch".into());
        let mut tests = div().flex().flex_wrap().gap_1();
        for (i, (kind, name)) in lightair_widgets::test_events::KINDS.iter().filter(|(k, _)| *k != "message").enumerate() {
            let (w, platform) = (widgets.clone(), platform.clone());
            tests = tests.child(Button::new(("activity-test", i)).label(*name).xsmall().outline().on_click(move |_, _, _| {
                if let Some(w) = &w {
                    w.test(kind, &platform);
                }
            }));
        }
        let waiting = widgets.as_ref().map_or(0, |w| w.waiting());
        let skip = widgets.clone();
        div()
            .flex()
            .flex_col()
            .gap_2()
            .px_3()
            .py_2()
            .border_b_1()
            .border_color(tok::border())
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap_2()
                    .children(self.totals(cx))
                    .child(
                        Button::new("activity-skip")
                            .label(if waiting > 0 { format!("Skip alert ({waiting} waiting)") } else { "Skip alert".into() })
                            .xsmall()
                            .ghost()
                            .on_click(move |_, _, _| {
                                if let Some(w) = &skip {
                                    w.skip();
                                }
                            }),
                    ),
            )
            .child(div().flex().items_center().gap_2().child(div().text_size(px(11.)).text_color(tok::text_tertiary()).child("Test")).child(tests))
    }

    fn row(&self, e: &ChatEvent, i: usize) -> Option<AnyElement> {
        let kind = e.alert_kind()?;
        let p = platforms::by_id(&e.platform)?;
        let widgets = self.studio.clone();
        let event = e.clone();
        Some(
            div()
                .id(("activity", i))
                .group("activity-row")
                .flex()
                .items_center()
                .gap_2()
                .px_3()
                .py(px(6.))
                .border_b_1()
                .border_color(tok::border())
                .when(e.test, |d| d.opacity(0.6))
                .child(badge(p, 16.))
                .child(ui::icon(kind_icon(kind)).size(px(14.)).text_color(if kind == "paid" || kind == "cheer" { rgb(0xF5A623).into() } else { tok::text_secondary() }))
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .flex_1()
                        .min_w_0()
                        .child(div().text_size(px(12.)).text_color(tok::text()).truncate().child(line(e, kind)))
                        .when(!e.text.is_empty(), |d| d.child(div().text_size(px(11.)).text_color(tok::text_tertiary()).truncate().child(e.text.clone()))),
                )
                .child(div().text_size(px(10.)).text_color(tok::text_tertiary()).child(if e.test { "test".to_string() } else { ago(e.ts_ms) }))
                .child(
                    ui::icon_button(("activity-replay", i), "rotate-ccw", tok::text_secondary())
                        .tooltip(ui::tip("Show its alert again"))
                        .on_click(move |_, _, cx| {
                            if let Some(w) = widgets.read(cx).widgets.clone() {
                                w.replay(&event);
                            }
                        }),
                )
                .into_any_element(),
        )
    }
}

impl Render for ActivityPanel {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let history = self.studio.read(cx).chat.history();
        let events: Vec<&ChatEvent> = history.iter().rev().filter(|e| e.alert_kind().is_some() && !e.deleted).take(SHOWN).collect();
        let rows: Vec<AnyElement> = events.iter().enumerate().filter_map(|(i, e)| self.row(e, i)).collect();
        let body = if rows.is_empty() {
            div()
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .gap_2()
                .flex_1()
                .p_6()
                .child(ui::icon("activity").size(px(28.)).text_color(tok::text_tertiary()))
                .child(div().text_size(px(13.)).text_color(tok::text_secondary()).child("No activity yet"))
                .child(
                    div()
                        .text_size(px(12.))
                        .text_color(tok::text_tertiary())
                        .text_center()
                        .child("Follows, subs, donations, cheers and raids from your channels show here. Try one with the test buttons."),
                )
                .into_any_element()
        } else {
            div().id("activity-feed").flex_1().min_h_0().overflow_y_scroll().children(rows).into_any_element()
        };
        div().flex().flex_col().size_full().bg(tok::bg_panel()).child(self.controls(cx)).child(body)
    }
}

impl gpui_kit::component::dock::BasePanel for ActivityPanel {
    fn panel_name(&self) -> &'static str {
        "activity"
    }
}

impl gpui_kit::component::dock::Panel for ActivityPanel {
    fn tab_name(&self, _: &App) -> Option<SharedString> {
        None
    }

    fn title(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        ui::panel_title("activity", "Activity")
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

impl EventEmitter<gpui_kit::component::dock::PanelEvent> for ActivityPanel {}

impl Focusable for ActivityPanel {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}
