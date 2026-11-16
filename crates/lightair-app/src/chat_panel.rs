//! The Chat panel: every destination's chat in one feed (docs/plans/live-integrations.md, 2.4).
//! Messages with their emotes and roles; paid messages, memberships and raids stand out;
//! deleted ones are greyed. New messages come in batches (at most every 120 ms), so a busy chat
//! doesn't redraw the window for every line.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

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
}

impl ChatPanel {
    pub fn new(studio: Entity<Studio>, cx: &mut Context<Self>) -> Self {
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
        ChatPanel { studio, focus: cx.focus_handle(), scroll: ScrollHandle::new(), images, last: 0 }
    }

    fn header(&self, cx: &App) -> Option<Div> {
        let hub = &self.studio.read(cx).chat;
        let mut status: Vec<_> = hub.status().into_iter().collect();
        if status.is_empty() {
            return None;
        }
        status.sort_by(|a, b| (&a.0.platform, &a.0.channel).cmp(&(&b.0.platform, &b.0.channel)));
        let mut row = div().flex().flex_wrap().gap_2().px_3().py_2().border_b_1().border_color(tok::border());
        for (channel, s) in status {
            let (text, colour) = match &s {
                Status::Connected => ("Live chat", tok::ok()),
                Status::Offline => ("Not live", tok::text_tertiary()),
                Status::Connecting => ("Connecting…", tok::warn()),
                Status::Error { .. } => ("Can't read", tok::error()),
            };
            let tip: SharedString = match &s {
                Status::Error { message } => format!("{}: {message}", channel.channel).into(),
                _ => channel.channel.clone().into(),
            };
            let Some(p) = platforms::by_id(&channel.platform) else { continue };
            row = row.child(
                div()
                    .id(SharedString::from(format!("chat-status-{}-{}", channel.platform, channel.channel)))
                    .flex()
                    .items_center()
                    .gap_1()
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
    fn row(&self, e: &ChatEvent) -> Option<AnyElement> {
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
            Kind::Message => div()
                .px_3()
                .py(px(3.))
                .flex()
                .flex_col()
                .gap(px(2.))
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
        let rows: Vec<AnyElement> = events[start..].iter().filter_map(|e| self.row(e)).collect();
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
        div().flex().flex_col().size_full().bg(tok::bg_panel()).children(header).child(feed)
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
