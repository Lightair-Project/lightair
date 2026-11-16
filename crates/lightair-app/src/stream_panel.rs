//! The Stream manager panel: every destination with a channel, as its platform shows it now
//! (live or not, title, category, viewers, how long), looked up every 30 s while the panel is
//! on screen, and the platform's own stream dashboard opened signed in, in a Lightair window
//! (to change the title and category there).

use std::cell::Cell;
use std::collections::HashMap;
use std::rc::Rc;
use std::time::{Duration, Instant};

use gpui_kit::component::Sizable;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input as TextInput, InputState};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;
use lightair_live::stream_info::{self, StreamInfo};

use crate::outputs_panel::badge;
use crate::platforms;
use crate::studio::Studio;
use crate::ui::{self, tok};

type Key = (String, String);
/// A channel being edited: title, category, and description (YouTube).
type Editing = (Key, Entity<InputState>, Entity<InputState>, Option<Entity<InputState>>);

pub struct StreamPanel {
    studio: Entity<Studio>,
    focus: FocusHandle,
    info: HashMap<Key, Result<StreamInfo, String>>,
    /// Drawn lately (lookups only while it's on screen).
    drawn: Rc<Cell<Instant>>,
    /// Dashboards open (closed with their windows).
    windows: Vec<Box<dyn lightair_live::pages::Page>>,
    /// The channel being edited: its title and category fields, and description (YouTube).
    editing: Option<Editing>,
    /// What the last change said, per channel.
    notes: HashMap<Key, String>,
}

fn since(ms: i64) -> String {
    let s = ((lightair_live::event::now_ms() - ms) / 1000).max(0);
    format!("{}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60)
}

impl StreamPanel {
    pub fn new(studio: Entity<Studio>, cx: &mut Context<Self>) -> Self {
        let drawn = Rc::new(Cell::new(Instant::now() - Duration::from_secs(3600)));
        let seen = drawn.clone();
        cx.spawn(async move |this, cx| {
            let mut last_lookup: Option<Instant> = None;
            loop {
                cx.background_executor().timer(Duration::from_secs(1)).await;
                if this.upgrade().is_none() {
                    break;
                }
                let on_screen = seen.get().elapsed() < Duration::from_secs(5);
                if !on_screen || last_lookup.is_some_and(|t| t.elapsed() < Duration::from_secs(30)) {
                    // the "live for" clock still ticks while shown
                    if on_screen {
                        let _ = this.update(cx, |_, cx| cx.notify());
                    }
                    continue;
                }
                last_lookup = Some(Instant::now());
                let channels: Vec<Key> = this
                    .update(cx, |this, cx| {
                        let mut out: Vec<Key> = this
                            .studio
                            .read(cx)
                            .workspace()
                            .outputs
                            .destinations
                            .iter()
                            .filter_map(|d| Some((d.platform.clone()?, d.chat.clone().filter(|c| !c.trim().is_empty())?.trim().to_string())))
                            .collect();
                        out.dedup();
                        out
                    })
                    .unwrap_or_default();
                for key in channels {
                    let k = key.clone();
                    let result = cx.background_executor().spawn(async move { stream_info::info(&k.0, &k.1) }).await;
                    let _ = this.update(cx, |this, cx| {
                        this.info.insert(key, result);
                        cx.notify();
                    });
                }
            }
        })
        .detach();
        StreamPanel { studio, focus: cx.focus_handle(), info: HashMap::new(), drawn, windows: Vec::new(), editing: None, notes: HashMap::new() }
    }

    fn open_dashboard(&mut self, url: String, name: String) {
        let Ok(host) = lightair_live::pages::host() else { return };
        self.windows.push(host.open_window(&url, &format!("{name} · Lightair"), std::sync::Arc::new(|| {})));
    }

    fn start_edit(&mut self, key: Key, window: &mut Window, cx: &mut Context<Self>) {
        let now = self.info.get(&key).and_then(|r| r.as_ref().ok()).cloned().unwrap_or_default();
        let title = cx.new(|cx| InputState::new(window, cx).placeholder("Title").default_value(now.title));
        let category = cx.new(|cx| InputState::new(window, cx).placeholder("Category or game").default_value(now.category));
        let description = stream_info::can_describe(&key.0).then(|| {
            let text = now.description.clone().unwrap_or_default();
            cx.new(|cx| InputState::new(window, cx).placeholder("Description").default_value(text))
        });
        title.update(cx, |t, cx| t.focus(window, cx));
        self.editing = Some((key, title, category, description));
        cx.notify();
    }

    fn save_edit(&mut self, cx: &mut Context<Self>) {
        let Some((key, title, category, description)) = self.editing.take() else { return };
        let (t, c) = (title.read(cx).value().to_string(), category.read(cx).value().to_string());
        let d = description.map(|d| d.read(cx).value().to_string());
        let before = self.info.get(&key).and_then(|r| r.as_ref().ok()).cloned().unwrap_or_default();
        let t = (t != before.title).then_some(t);
        let c = (c != before.category).then_some(c);
        let d = d.filter(|d| Some(d) != before.description.as_ref());
        self.notes.insert(key.clone(), "Saving…".into());
        cx.spawn(async move |this, cx| {
            let k = key.clone();
            let result = cx
                .background_executor()
                .spawn(async move {
                    let r = stream_info::edit(&k.0, &k.1, t.as_deref(), d.as_deref(), c.as_deref());
                    (r, stream_info::info(&k.0, &k.1))
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                let (r, fresh) = result;
                this.notes.insert(key.clone(), r.unwrap_or_else(|e| e));
                this.info.insert(key, fresh);
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn row(&self, platform: &str, channel: &str, i: usize, cx: &mut Context<Self>) -> Option<AnyElement> {
        let p = platforms::by_id(platform)?;
        let info = self.info.get(&(platform.to_string(), channel.to_string()));
        let (state, colour) = match info {
            Some(Ok(s)) if s.live => ("Live", tok::live()),
            Some(Ok(_)) => ("Not live", tok::text_tertiary()),
            Some(Err(_)) => ("No details", tok::text_tertiary()),
            None => ("Looking…", tok::text_tertiary()),
        };
        let s = info.and_then(|r| r.as_ref().ok());
        let mut facts = Vec::new();
        if let Some(v) = s.and_then(|s| s.viewers) {
            facts.push(format!("{v} watching"));
        }
        if let Some(t) = s.filter(|s| s.live).and_then(|s| s.started_ms) {
            facts.push(format!("live {}", since(t)));
        }
        let title = s.map(|s| s.title.clone()).filter(|t| !t.is_empty()).unwrap_or_else(|| channel.to_string());
        let category = s.map(|s| s.category.clone()).unwrap_or_default();
        let dashboard = stream_info::dashboard(platform, channel);
        let name = p.name.to_string();
        Some(
            div()
                .flex()
                .flex_col()
                .gap_1()
                .px_3()
                .py_2()
                .border_b_1()
                .border_color(tok::border())
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap_2()
                        .child(badge(p, 16.))
                        .child(div().text_size(px(12.)).font_weight(FontWeight::SEMIBOLD).text_color(tok::text()).child(p.name))
                        .child(div().flex().items_center().gap_1().child(div().size(px(7.)).rounded_full().bg(colour)).child(div().text_size(px(11.)).text_color(colour).child(state)))
                        .child(div().flex_1())
                        .when(stream_info::can_edit(platform) && self.editing.is_none(), |d| {
                            let key = (platform.to_string(), channel.to_string());
                            d.child(Button::new(("stream-edit", i)).label("Edit").xsmall().outline().on_click(cx.listener(move |this, _, window, cx| this.start_edit(key.clone(), window, cx))))
                        })
                        .when_some(dashboard, |d, url| {
                            d.child(Button::new(("stream-dashboard", i)).label("Open dashboard").xsmall().outline().on_click(cx.listener(move |this, _, _, _| {
                                this.open_dashboard(url.clone(), name.clone());
                            })))
                        }),
                )
                .child(div().text_size(px(13.)).text_color(tok::text()).truncate().child(title))
                .when(!category.is_empty() || !facts.is_empty(), |d| {
                    let line = std::iter::once(category).filter(|c| !c.is_empty()).chain(facts).collect::<Vec<_>>().join(" · ");
                    d.child(div().text_size(px(11.)).text_color(tok::text_tertiary()).child(line))
                })
                .when_some(info.and_then(|r| r.as_ref().err()).cloned(), |d, e| d.child(div().text_size(px(11.)).text_color(tok::text_tertiary()).child(e)))
                .when_some(self.editing.as_ref().filter(|(k, _, _, _)| k.0 == platform && k.1 == channel), |d, (_, t, c, desc)| {
                    d.child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_1()
                            .pt_1()
                            .child(TextInput::new(t).small())
                            .child(TextInput::new(c).small())
                            .when_some(desc.as_ref(), |d, desc| d.child(TextInput::new(desc).small()))
                            .child(
                                div()
                                    .flex()
                                    .gap_1()
                                    .child(Button::new(("stream-save", i)).label("Save").xsmall().on_click(cx.listener(|this, _, _, cx| this.save_edit(cx))))
                                    .child(Button::new(("stream-cancel", i)).label("Cancel").xsmall().ghost().on_click(cx.listener(|this, _, _, cx| {
                                        this.editing = None;
                                        cx.notify();
                                    }))),
                            ),
                    )
                })
                .when_some(self.notes.get(&(platform.to_string(), channel.to_string())).cloned(), |d, n| d.child(div().text_size(px(11.)).text_color(tok::text_secondary()).child(n)))
                .into_any_element(),
        )
    }
}

impl Render for StreamPanel {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.drawn.set(Instant::now());
        let mut channels: Vec<Key> = self
            .studio
            .read(cx)
            .workspace()
            .outputs
            .destinations
            .iter()
            .filter_map(|d| Some((d.platform.clone()?, d.chat.clone().filter(|c| !c.trim().is_empty())?.trim().to_string())))
            .collect();
        channels.dedup();
        let rows: Vec<AnyElement> = channels.iter().enumerate().filter_map(|(i, (p, c))| self.row(p, c, i, cx)).collect();
        let body = if rows.is_empty() {
            div()
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .gap_2()
                .flex_1()
                .p_6()
                .child(ui::icon("radio").size(px(28.)).text_color(tok::text_tertiary()))
                .child(div().text_size(px(13.)).text_color(tok::text_secondary()).child("No channels yet"))
                .child(div().text_size(px(12.)).text_color(tok::text_tertiary()).text_center().child("Add your channel to a destination in Outputs (its Chat field) to see how your stream looks on each platform."))
                .into_any_element()
        } else {
            div().id("stream-rows").flex_1().min_h_0().overflow_y_scroll().children(rows).into_any_element()
        };
        div().flex().flex_col().size_full().bg(tok::bg_panel()).child(body)
    }
}

impl gpui_kit::component::dock::BasePanel for StreamPanel {
    fn panel_name(&self) -> &'static str {
        "stream"
    }
}

impl gpui_kit::component::dock::Panel for StreamPanel {
    fn tab_name(&self, _: &App) -> Option<SharedString> {
        None
    }
    fn title(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        ui::panel_title("radio", "Stream")
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

impl EventEmitter<gpui_kit::component::dock::PanelEvent> for StreamPanel {}

impl Focusable for StreamPanel {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}
