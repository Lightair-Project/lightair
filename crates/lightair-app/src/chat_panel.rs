//! The Chat panel: every destination's chat in one feed (docs/plans/live-integrations.md, 2.4).
//! Messages with their emotes and roles; paid messages, memberships and raids stand out;
//! deleted ones are greyed. New messages come in batches (at most every 120 ms), so a busy chat
//! doesn't redraw the window for every line.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use gpui_kit::component::Sizable;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input as TextInput, InputEvent, InputState};
use gpui_kit::component::menu::{DropdownMenu, PopupMenuItem};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;
use lightair_live::{ChatEvent, Fragment, Images, Kind, Role, Status};

use crate::outputs_panel::badge;
use crate::platforms;
use crate::studio::Studio;
use crate::ui::{self, tok};

/// Messages kept on screen.
const SHOWN: usize = 300;

pub struct ChatPanel {
    studio: Entity<Studio>,
    focus: FocusHandle,
    scroll: ScrollHandle,
    images: Images,
    /// The last event shown (to scroll down when there are newer ones).
    last: u64,
    /// What to send, and where ("": every connected chat).
    input: Entity<InputState>,
    target: String,
    /// Why the last message didn't go.
    send_error: Option<String>,
    /// Only this platform's chat ("": every platform's).
    filter: String,
    /// The message being replied to.
    reply: Option<ChatEvent>,
    /// The message whose Ban was clicked once (a second click bans).
    confirm_ban: Option<u64>,
    _input_events: Subscription,
}

impl ChatPanel {
    /// Deletes, times out or bans (as the signed-in account), saying so if it didn't work.
    fn moderate(&mut self, e: &ChatEvent, action: lightair_live::moderate::Action, cx: &mut Context<Self>) {
        use lightair_live::moderate::Action;
        let (platform, channel) = (e.platform.clone(), e.channel.clone());
        let name = e.name().to_string();
        // owned copies for the thread
        let owned = match action {
            Action::Delete { message_id } => (0u8, message_id.to_string(), String::new(), 0, String::new()),
            Action::Timeout { user_id, login, seconds, reason } => (1, user_id.to_string(), login.to_string(), seconds, reason.to_string()),
            Action::Ban { user_id, login, reason } => (2, user_id.to_string(), login.to_string(), 0, reason.to_string()),
        };
        self.send_error = None;
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    let (kind, id, login, seconds, reason) = owned;
                    let action = match kind {
                        0 => Action::Delete { message_id: &id },
                        1 => Action::Timeout { user_id: &id, login: &login, seconds, reason: &reason },
                        _ => Action::Ban { user_id: &id, login: &login, reason: &reason },
                    };
                    lightair_live::moderate::act(&platform, &channel, action)
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                if let Err(e) = result {
                    this.send_error = Some(format!("{name}: {e}"));
                }
                cx.notify();
            });
        })
        .detach();
    }

    pub fn new(studio: Entity<Studio>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let dirty = Arc::new(AtomicBool::new(true));
        let hub = studio.read(cx).chat.clone();
        {
            let dirty = dirty.clone();
            hub.on_change(move || dirty.store(true, Ordering::Release));
        }
        let images = Images::new(lightair_platform::paths::data_dir().join("cache").join("emotes"));
        {
            let dirty = dirty.clone();
            images.on_ready(move || dirty.store(true, Ordering::Release));
        }
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(Duration::from_millis(120)).await;
                if dirty.swap(false, Ordering::AcqRel) && this.update(cx, |_, cx| cx.notify()).is_err() {
                    break;
                }
                if this.upgrade().is_none() {
                    break;
                }
            }
        })
        .detach();
        let input = cx.new(|cx| InputState::new(window, cx).placeholder("Send a message…"));
        let _input_events = cx.subscribe_in(&input, window, |this, state, ev: &InputEvent, window, cx| {
            if matches!(ev, InputEvent::PressEnter { .. }) {
                let text = state.read(cx).value().trim().to_string();
                if text.is_empty() {
                    return;
                }
                state.update(cx, |s, cx| s.set_value("", window, cx));
                this.send(text, cx);
            }
        });
        ChatPanel { studio, focus: cx.focus_handle(), scroll: ScrollHandle::new(), images, last: 0, input, target: String::new(), send_error: None, filter: String::new(), reply: None, confirm_ban: None, _input_events }
    }

    /// Sends `text` (as the person signed in on each platform), off the main thread.
    fn send(&mut self, text: String, cx: &mut Context<Self>) {
        let hub = self.studio.read(cx).chat.clone();
        // a reply goes to its message's own chat
        let params = match self.reply.take() {
            Some(r) => serde_json::json!({"text": text, "platform": r.platform, "channel": r.channel, "reply_to": r.native_id}),
            None => serde_json::json!({"text": text, "platform": self.target}),
        };
        self.send_error = None;
        cx.spawn(async move |this, cx| {
            let result = cx.background_executor().spawn(async move { crate::chat_send::send(&hub, &params) }).await;
            let _ = this.update(cx, |this, cx| {
                this.send_error = match result {
                    Ok(v) => v["failed"].as_array().filter(|f| !f.is_empty()).map(|f| f.iter().filter_map(|x| x.as_str()).collect::<Vec<_>>().join("; ")),
                    Err(e) => Some(e),
                };
                cx.notify();
            });
        })
        .detach();
    }

    /// The box to send from, and where it sends.
    fn composer(&self, cx: &mut Context<Self>) -> Div {
        let mut platforms: Vec<String> = self.studio.read(cx).chat.status().into_keys().map(|c| c.platform).filter(|p| lightair_live::accounts::account(p).is_some()).collect();
        platforms.sort();
        platforms.dedup();
        let label = if self.target.is_empty() { "All".to_string() } else { platforms::by_id(&self.target).map_or(self.target.clone(), |p| p.name.to_string()) };
        let me = cx.entity().downgrade();
        let menu = Button::new("chat-target").label(label).xsmall().ghost().dropdown_caret(true).dropdown_menu(move |mut menu, _, _| {
            for (key, name) in std::iter::once((String::new(), "All".to_string())).chain(platforms.iter().map(|p| (p.clone(), platforms::by_id(p).map_or(p.clone(), |x| x.name.to_string())))) {
                let me = me.clone();
                menu = menu.item(PopupMenuItem::new(name).on_click(move |_, _, cx| {
                    if let Some(me) = me.upgrade() {
                        me.update(cx, |this, cx| {
                            this.target = key.clone();
                            cx.notify();
                        });
                    }
                }));
            }
            menu
        });
        div()
            .flex()
            .flex_col()
            .gap_1()
            .p_2()
            .border_t_1()
            .border_color(tok::border())
            .when_some(self.send_error.clone(), |d, e| d.child(div().text_size(px(11.)).text_color(tok::error()).child(e)))
            .when_some(self.reply.as_ref(), |d, r| {
                d.child(
                    div()
                        .flex()
                        .items_center()
                        .gap_1()
                        .text_size(px(11.))
                        .text_color(tok::text_secondary())
                        .child(div().flex_1().truncate().child(format!("Replying to {}: {}", r.name(), r.text)))
                        .child(ui::icon_button("chat-reply-cancel", "x", tok::text_tertiary()).on_click(cx.listener(|this, _, _, cx| {
                            this.reply = None;
                            cx.notify();
                        }))),
                )
            })
            .child(div().flex().items_center().gap_1().child(menu).child(div().flex_1().child(TextInput::new(&self.input).small())))
    }

    fn header(&self, cx: &mut Context<Self>) -> Option<Div> {
        let hub = self.studio.read(cx).chat.clone();
        let mut status: Vec<_> = hub.status().into_iter().collect();
        if status.is_empty() {
            return None;
        }
        status.sort_by(|a, b| (&a.0.platform, &a.0.channel).cmp(&(&b.0.platform, &b.0.channel)));
        let mut row = div().flex().flex_wrap().gap_2().px_3().py_2().border_b_1().border_color(tok::border());
        // tabs: every platform's chat, or one platform's
        let tab = |id: SharedString, on: bool| {
            div().id(id).flex().items_center().gap_1().px_1p5().py(px(2.)).rounded(px(5.)).cursor_pointer().when(on, |d| d.bg(tok::bg_selected())).hover(|d| d.bg(tok::bg_selected()))
        };
        row = row.child(
            tab("chat-tab-all".into(), self.filter.is_empty())
                .child(div().text_size(px(11.)).text_color(tok::text_secondary()).child("All"))
                .on_click(cx.listener(|this, _, _, cx| {
                    this.filter.clear();
                    cx.notify();
                })),
        );
        for (channel, s) in status {
            let (text, colour) = match &s {
                Status::Connected => ("Live chat", tok::ok()),
                Status::Offline => ("Not live", tok::text_tertiary()),
                Status::Connecting => ("Connecting…", tok::warn()),
                Status::Error { .. } => ("Can't read", tok::error()),
            };
            let tip: SharedString = match &s {
                Status::Error { message } => format!("{}: {message}", channel.channel).into(),
                // YouTube only says a channel is live when its stream is public and started
                Status::Offline if channel.platform.starts_with("youtube") && !channel.channel.contains("v=") && !channel.channel.contains("/live/") => format!(
                    "{}: YouTube shows no public live stream there. A Private stream can't be read; an Unlisted one can, from its own link (youtube.com/live/…) in the destination's Chat field. Still in Preview in YouTube Studio?",
                    channel.channel
                )
                .into(),
                _ => channel.channel.clone().into(),
            };
            let Some(p) = platforms::by_id(&channel.platform) else { continue };
            let platform = channel.platform.clone();
            row = row.child(
                tab(SharedString::from(format!("chat-status-{}-{}", channel.platform, channel.channel)), self.filter == channel.platform)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.filter = platform.clone();
                        cx.notify();
                    }))
                    .child(badge(p, 14.))
                    .child(div().size(px(6.)).rounded_full().bg(colour))
                    .child(div().text_size(px(11.)).text_color(tok::text_secondary()).child(text))
                    .tooltip(move |window, cx| gpui_kit::component::tooltip::Tooltip::new(tip.clone()).build(window, cx)),
            );
        }
        Some(row)
    }

    fn empty(&self) -> Div {
        div()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap_2()
            .size_full()
            .p_6()
            .child(ui::icon("message-square").size(px(28.)).text_color(tok::text_tertiary()))
            .child(div().text_size(px(13.)).text_color(tok::text_secondary()).child("No chat yet"))
            .child(
                div()
                    .text_size(px(12.))
                    .text_color(tok::text_tertiary())
                    .text_center()
                    .child("Add your channel to a Twitch, YouTube, TikTok, Kick, CHZZK, SOOP or DouYu destination in Outputs (its Chat field) to read its chat here."),
            )
    }

    /// One event as a row (deletions aren't shown: they grey what they deleted).
    fn row(&self, e: &ChatEvent, cx: &mut Context<Self>) -> Option<AnyElement> {
        let p = platforms::by_id(&e.platform)?;
        let user = e.user.as_ref();
        let name = user.map_or("", |u| u.display_name.as_str()).to_string();
        let name_colour = user
            .and_then(|u| u.color.as_deref())
            .and_then(|c| u32::from_str_radix(c.trim_start_matches('#'), 16).ok())
            .map_or(tok::text(), |c| lighten(c).into());
        let roles = user.map(|u| u.roles.clone()).unwrap_or_default();
        let head = div()
            .flex()
            .items_center()
            .gap_1()
            .child(badge(p, 14.))
            .children(roles.iter().filter_map(|r| role_tag(*r)))
            .child(div().text_size(px(12.)).font_weight(FontWeight::SEMIBOLD).text_color(name_colour).child(name.clone()));
        let body = self.message(e);
        let el = match &e.kind {
            Kind::Deletion { .. } => return None,
            Kind::Message => div().child(
                div()
                .id(SharedString::from(format!("chat-msg-{}", e.id)))
                .group("chat-msg")
                .relative()
                .px_3()
                .py(px(3.))
                .flex()
                .flex_col()
                .gap(px(2.))
                // feature it (the Featured message widget), reply (where Lightair can send, to a
                // message the platform numbered)
                .child({
                    let featured = e.clone();
                    let can_reply = e.native_id.is_some() && lightair_live::accounts::account(&e.platform).is_some();
                    let can_moderate = e.user.is_some() && lightair_live::moderate::can(&e.platform) && lightair_live::accounts::account(&e.platform).is_some();
                    let target = e.clone();
                    div()
                        .absolute()
                        .top(px(2.))
                        .right(px(8.))
                        .flex()
                        .gap_0p5()
                        .invisible()
                        .group_hover("chat-msg", |s| s.visible())
                        .child(
                            ui::icon_button(SharedString::from(format!("chat-feature-{}", e.id)), "message-square-quote", tok::text_secondary())
                                .tooltip(ui::tip("Feature on stream (Featured message widget)"))
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    if let Some(w) = this.studio.read(cx).widgets.as_ref() {
                                        // stamped, so featuring it again shows it again
                                        let mut v = serde_json::to_value(&featured).unwrap_or_default();
                                        v["featured_at"] = lightair_live::event::now_ms().into();
                                        w.set_data("featured", v);
                                    }
                                })),
                        )
                        .when(can_moderate, |d| {
                            let (del, out, ban) = (e.clone(), e.clone(), e.clone());
                            let confirming = self.confirm_ban == Some(e.id);
                            d.when(e.native_id.is_some(), |d| {
                                d.child(
                                    ui::icon_button(SharedString::from(format!("chat-delete-{}", e.id)), "trash", tok::text_secondary())
                                        .tooltip(ui::tip("Delete message"))
                                        .on_click(cx.listener(move |this, _, _, cx| {
                                            let id = del.native_id.clone().unwrap_or_default();
                                            this.moderate(&del, lightair_live::moderate::Action::Delete { message_id: &id }, cx)
                                        })),
                                )
                            })
                            .child(
                                ui::icon_button(SharedString::from(format!("chat-timeout-{}", e.id)), "timer", tok::text_secondary())
                                    .tooltip(ui::tip("Time out for 10 minutes"))
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        let u = out.user.clone().unwrap_or_default();
                                        this.moderate(&out, lightair_live::moderate::Action::Timeout { user_id: &u.id, login: &u.login, seconds: 600, reason: "" }, cx)
                                    })),
                            )
                            .child(
                                ui::icon_button(SharedString::from(format!("chat-ban-{}", e.id)), "ban", if confirming { tok::error() } else { tok::text_secondary() })
                                    .tooltip(ui::tip(if confirming { "Click again to ban" } else { "Ban" }))
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        if this.confirm_ban != Some(ban.id) {
                                            this.confirm_ban = Some(ban.id);
                                            cx.notify();
                                            return;
                                        }
                                        this.confirm_ban = None;
                                        let u = ban.user.clone().unwrap_or_default();
                                        this.moderate(&ban, lightair_live::moderate::Action::Ban { user_id: &u.id, login: &u.login, reason: "" }, cx)
                                    })),
                            )
                        })
                        .when(can_reply, |d| {
                            d.child(
                                ui::icon_button(SharedString::from(format!("chat-reply-{}", e.id)), "reply", tok::text_secondary())
                                    .tooltip(ui::tip("Reply"))
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        this.reply = Some(target.clone());
                                        this.input.update(cx, |i, cx| i.focus(window, cx));
                                        cx.notify();
                                    })),
                            )
                        })
                })
                .when_some(e.reply_to.as_ref(), |d, r| {
                    d.child(
                        div()
                            .text_size(px(11.))
                            .text_color(tok::text_tertiary())
                            .truncate()
                            .child(format!("↳ {}: {}", r.author, r.excerpt)),
                    )
                })
                .child(head)
                .child(body),
            ),
            Kind::Paid => div()
                .mx_2()
                .my_1()
                .p_2()
                .rounded(px(8.))
                .bg(rgba(0xF5A62322))
                .border_1()
                .border_color(rgba(0xF5A62366))
                .flex()
                .flex_col()
                .gap_1()
                .child(
                    div().flex().items_center().justify_between().child(head).child(
                        div()
                            .text_size(px(12.))
                            .font_weight(FontWeight::BOLD)
                            .text_color(rgb(0xF5A623))
                            .child(e.paid.as_ref().map_or(String::new(), |p| p.display.clone())),
                    ),
                )
                .child(body),
            Kind::Membership | Kind::Notice | Kind::Raid { .. } | Kind::Follow => {
                let line = match &e.kind {
                    Kind::Raid { viewers } => format!("{name} is raiding with {viewers} viewers"),
                    Kind::Follow => format!("{name} followed"),
                    _ => e.headline.clone().unwrap_or_else(|| format!("{name} became a member")),
                };
                div()
                    .mx_2()
                    .my_1()
                    .p_2()
                    .rounded(px(8.))
                    .bg(tok::bg_raised())
                    .border_1()
                    .border_color(tok::border_strong())
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(badge(p, 14.))
                            .child(div().text_size(px(12.)).text_color(tok::text()).child(line)),
                    )
                    .when(!e.text.is_empty(), |d| d.child(body))
            }
        };
        Some(el.when(e.deleted, |d| d.opacity(0.4)).into_any_element())
    }

    /// The message: plain text wraps as text; with emotes, word by word with the pictures.
    fn message(&self, e: &ChatEvent) -> AnyElement {
        if e.deleted {
            return div().text_size(px(12.)).italic().text_color(tok::text_tertiary()).child("message deleted").into_any_element();
        }
        let has_emotes = e.fragments.iter().any(|f| matches!(f, Fragment::Emote { .. }));
        if !has_emotes {
            return div().text_size(px(13.)).text_color(tok::text()).child(e.text.clone()).into_any_element();
        }
        let mut row = div().flex().flex_wrap().items_center().text_size(px(13.)).text_color(tok::text());
        for f in &e.fragments {
            match f {
                Fragment::Emote { code, image, .. } => {
                    row = match self.images.get(image) {
                        Some(path) => row.child(img(path).h(px(22.)).max_w(px(88.)).mx(px(1.))),
                        None => row.child(div().child(code.clone())),
                    };
                }
                Fragment::Mention { name } => {
                    row = row.child(div().font_weight(FontWeight::SEMIBOLD).child(name.clone()));
                }
                Fragment::Link { url } => {
                    row = row.child(div().underline().text_color(tok::accent_primary()).child(url.clone()));
                }
                Fragment::Text { text } => {
                    // words as separate pieces so lines wrap between them
                    for (i, word) in text.split(' ').enumerate() {
                        if i > 0 {
                            row = row.child(div().w(px(4.)));
                        }
                        if !word.is_empty() {
                            row = row.child(div().child(word.to_string()));
                        }
                    }
                }
            }
        }
        row.into_any_element()
    }
}

/// Hidden pages for the chat readers, in Lightair's Chromium (its cookies, so a platform signed
/// in to once stays signed in).
pub struct HiddenPages;

struct HiddenPage(lightair_web::Page);

impl lightair_live::pages::Host for HiddenPages {
    fn open(&self, url: &str, script: &str, on_message: lightair_live::pages::OnMessage) -> Box<dyn lightair_live::pages::Page> {
        Box::new(HiddenPage(lightair_web::Page::open_agent(url, script.into(), on_message)))
    }

    fn open_early(&self, url: &str, early: &str, script: &str, on_message: lightair_live::pages::OnMessage) -> Box<dyn lightair_live::pages::Page> {
        Box::new(HiddenPage(lightair_web::Page::open_agent_early(url, early.into(), script.into(), on_message)))
    }

    fn open_window(&self, url: &str, title: &str, on_closed: Arc<dyn Fn() + Send + Sync>) -> Box<dyn lightair_live::pages::Page> {
        Box::new(HiddenPage(lightair_web::Page::open_window(url, title, (1000, 760), on_closed)))
    }

    fn cookies(&self, url: &str, done: lightair_live::pages::CookiesDone) {
        lightair_web::cookies(url, done);
    }
}

impl lightair_live::pages::Page for HiddenPage {
    fn eval(&self, code: &str) {
        self.0.eval(code);
    }

    fn navigate(&self, url: &str) {
        self.0.navigate(url);
    }
}

/// A page in a window of its own (tests).
pub fn window_page(url: &str, closed: Arc<dyn Fn() + Send + Sync>) -> Box<dyn lightair_live::pages::Page> {
    Box::new(HiddenPage(lightair_web::Page::open_window(url, "Lightair", (1000, 760), closed)))
}

/// A name colour made readable on the dark panel.
fn lighten(c: u32) -> Rgba {
    let (r, g, b) = ((c >> 16) & 0xFF, (c >> 8) & 0xFF, c & 0xFF);
    let luma = (r * 299 + g * 587 + b * 114) / 1000;
    if luma >= 90 {
        return rgb(c);
    }
    let lift = |v: u32| v + (255 - v) * 2 / 5;
    rgb((lift(r) << 16) | (lift(g) << 8) | lift(b))
}

fn role_tag(r: Role) -> Option<Div> {
    let (text, colour) = match r {
        Role::Broadcaster => ("HOST", 0xE5484D),
        Role::Moderator => ("MOD", 0x30A46C),
        Role::Vip => ("VIP", 0xD6409F),
        Role::Member | Role::Subscriber => ("SUB", 0x8E4EC6),
        Role::Verified => return None,
    };
    Some(
        div()
            .px(px(3.))
            .rounded(px(3.))
            .bg(rgb(colour))
            .text_size(px(8.))
            .font_weight(FontWeight::BOLD)
            .text_color(rgb(0xFFFFFF))
            .child(text),
    )
}

impl Render for ChatPanel {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let hub = self.studio.read(cx).chat.clone();
        let events = hub.history();
        let header = self.header(cx);
        let start = events.len().saturating_sub(SHOWN);
        let filter = self.filter.clone();
        let rows: Vec<AnyElement> = events[start..].iter().filter(|e| filter.is_empty() || e.platform == filter).filter_map(|e| self.row(e, cx)).collect();
        // new messages: follow them down, unless the reader has scrolled up
        let newest = events.last().map_or(0, |e| e.id);
        if newest != self.last {
            let at_bottom = {
                let max = self.scroll.max_offset().y;
                -self.scroll.offset().y >= max - px(48.)
            };
            if at_bottom || self.last == 0 {
                self.scroll.scroll_to_bottom();
            }
            self.last = newest;
        }
        let feed = if rows.is_empty() && header.is_none() {
            self.empty().into_any_element()
        } else {
            div()
                .id("chat-feed")
                .flex_1()
                .min_h_0()
                .overflow_y_scroll()
                .track_scroll(&self.scroll)
                .py_1()
                .children(rows)
                .into_any_element()
        };
        let composer = (!events.is_empty() || !hub.status().is_empty()).then(|| self.composer(cx));
        div().flex().flex_col().size_full().bg(tok::bg_panel()).children(header).child(feed).children(composer)
    }
}

impl gpui_kit::component::dock::BasePanel for ChatPanel {
    fn panel_name(&self) -> &'static str {
        "chat"
    }
}

impl gpui_kit::component::dock::Panel for ChatPanel {
    fn tab_name(&self, _: &App) -> Option<SharedString> {
        None
    }

    fn title(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        ui::panel_title("message-square", "Chat")
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

impl EventEmitter<gpui_kit::component::dock::PanelEvent> for ChatPanel {}

impl Focusable for ChatPanel {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}
