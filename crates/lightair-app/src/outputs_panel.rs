//! The Outputs panel (docs/11-outputs-streaming-recording.md): recording, and the platforms to
//! go live on. Adding a destination starts from a platform (YouTube, Twitch, TikTok…), which
//! fills in its server (the nearest region where there are several), points at the page with
//! the stream key and picks the shape it wants.

use std::time::Instant;

use gpui_kit::component::Sizable;
use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::menu::{ContextMenuExt, DropdownMenu, PopupMenuItem};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;
use lightair_core::{Command, Destination, Encoding, Format, Id, Project, VideoEncoder};
use lightair_engine::outputs::{OutputKey, OutputState, StreamProtocol, runtime};

use crate::platforms::{self, PLATFORMS, Platform, Shape};
use crate::studio::Studio;
use crate::ui::{self, tok};

/// Adding a destination: first a platform, then its details.
enum Setup {
    Pick,
    Configure(Configure),
    Edit(Edit),
}

/// A destination's own settings.
struct Edit {
    id: Id,
    platform: &'static Platform,
    name: Entity<InputState>,
    /// Index into the platform's servers, or `None` for the typed address.
    server: Option<usize>,
    server_input: Entity<InputState>,
    /// A new key; empty keeps the saved one.
    key: Entity<InputState>,
    format: Option<Id>,
    /// `None`: Automatic.
    encoding: Option<Encoding>,
    error: Option<String>,
}

/// Asks the window to open Settings at its Recording page.
pub struct OpenRecordingSettings;

struct Configure {
    platform: &'static Platform,
    /// Index into the platform's servers (when it has a list).
    server: usize,
    /// The pasted server address (when it hasn't).
    server_input: Entity<InputState>,
    key: Entity<InputState>,
    format: Option<Id>,
    error: Option<String>,
    /// A vertical format was asked for: pick it once it exists.
    want_vertical: bool,
}

/// Asks the window to add a vertical format (reframing every scene into it).
pub struct AddVerticalFormat;

pub struct OutputsPanel {
    studio: Entity<Studio>,
    setup: Option<Setup>,
    focus: FocusHandle,
}

pub fn elapsed(since: Instant) -> String {
    let s = since.elapsed().as_secs();
    format!("{:02}:{:02}:{:02}", s / 3600, (s / 60) % 60, s % 60)
}

fn status_text(state: &OutputState) -> (String, Hsla) {
    match state {
        OutputState::Idle => ("Off".into(), tok::text_tertiary()),
        OutputState::Starting => ("Starting…".into(), tok::warn()),
        OutputState::Active { since } => (elapsed(*since), tok::live()),
        OutputState::Reconnecting { .. } => ("Reconnecting…".into(), tok::warn()),
        OutputState::Stopping => ("Stopping…".into(), tok::text_secondary()),
        OutputState::Failed(_) => ("Failed".into(), tok::error()),
    }
}

/// "rtmp://live.twitch.tv/app" → "live.twitch.tv"
fn host_of(server: &str) -> String {
    server.split("://").nth(1).unwrap_or(server).split(['/', ':']).next().unwrap_or(server).to_string()
}

/// "16:9", "9:16", "1:1" for a format.
fn aspect_label(f: &Format) -> String {
    fn gcd(a: u32, b: u32) -> u32 {
        if b == 0 { a } else { gcd(b, a % b) }
    }
    let g = gcd(f.width, f.height).max(1);
    format!("{}:{}", f.width / g, f.height / g)
}

/// The format after `current` in the project's list (wrapping), for the badge's click.
fn next_format(project: &Project, current: Option<Id>) -> Option<Id> {
    let cur = current.unwrap_or(project.primary_format().id);
    let i = project.formats.iter().position(|f| f.id == cur).unwrap_or(0);
    let next = &project.formats[(i + 1) % project.formats.len()];
    (next.id != project.primary_format().id).then_some(next.id)
}

fn format_badge(id: impl Into<ElementId>, project: &Project, format: Option<Id>) -> Stateful<Div> {
    let f = format.and_then(|id| project.format(id)).unwrap_or(project.primary_format());
    div()
        .id(id)
        .px_1p5()
        .h(px(18.))
        .flex()
        .items_center()
        .rounded(px(4.))
        .border_1()
        .border_color(tok::border_strong())
        .text_size(px(10.))
        .font_family("Menlo")
        .text_color(tok::text_secondary())
        .when(project.formats.len() > 1, |d| d.cursor_pointer().hover(|d| d.bg(tok::bg_selected())))
        .child(aspect_label(f))
}

/// A platform's badge: its colour with its logo (or a short mark).
pub fn badge(p: &Platform, size: f32) -> Div {
    let light = matches!(p.id, "kick");
    let ink: Hsla = if light { rgb(0x0B0B0C).into() } else { rgb(0xFFFFFF).into() };
    div()
        .size(px(size))
        .flex_none()
        .rounded(px(size * 0.24))
        .bg(rgb(p.color))
        .border_1()
        .border_color(rgba(0xFFFFFF22))
        .flex()
        .items_center()
        .justify_center()
        .text_size(px(size * 0.4))
        .font_weight(FontWeight::BOLD)
        .text_color(ink)
        .map(|d| match (p.logo, p.id) {
            (Some(logo), _) => d.child(ui::icon(logo).size(px(size * 0.58)).text_color(ink)),
            (None, "custom") => d.child(ui::icon("server").size(px(size * 0.5)).text_color(ink)),
            (None, _) => d.child(p.mark),
        })
}

fn section_label(text: &'static str) -> Div {
    div()
        .px_3()
        .pt_3()
        .pb_1()
        .text_size(px(10.))
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(tok::text_tertiary())
        .child(text)
}

fn toggle(id: impl Into<ElementId>, on: bool) -> Stateful<Div> {
    div()
        .id(id)
        .flex_none()
        .w(px(30.))
        .h(px(18.))
        .rounded_full()
        .cursor_pointer()
        .bg(if on { tok::ok() } else { tok::border_strong() })
        .child(div().size(px(14.)).m(px(2.)).rounded_full().bg(rgb(0xFFFFFF)).when(on, |d| d.ml(px(14.))))
}

impl OutputsPanel {
    pub fn new(studio: Entity<Studio>, cx: &mut Context<Self>) -> Self {
        cx.observe(&studio, |_, _, cx| cx.notify()).detach();
        Self { studio, setup: None, focus: cx.focus_handle() }
    }

    /// Asks for a vertical format, to send once it's there.
    pub fn request_vertical(&mut self, cx: &mut Context<Self>) {
        if let Some(Setup::Configure(c)) = &mut self.setup {
            c.want_vertical = true;
        }
        cx.emit(AddVerticalFormat);
    }

    pub fn open_picker(&mut self, cx: &mut Context<Self>) {
        self.setup = Some(Setup::Pick);
        cx.notify();
    }

    pub fn choose(&mut self, platform: &'static Platform, window: &mut Window, cx: &mut Context<Self>) {
        let server_input = cx.new(|cx| {
            InputState::new(window, cx).placeholder(if platform.id == "custom" {
                "rtmp://…, srt://… or https://… (WHIP)"
            } else {
                "Paste the server URL"
            })
        });
        let key = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(if platform.id == "custom" { "Stream key, token or SRT stream ID" } else { "Stream key" })
                .masked(true)
        });
        // The shape it wants: a vertical format for vertical platforms, when there is one.
        let project = self.studio.read(cx).project().clone();
        let format = match platform.shape {
            Shape::Vertical => project.formats.iter().find(|f| f.height > f.width).map(|f| f.id),
            Shape::Landscape => None,
        };
        let first = if platform.servers.is_empty() { server_input.clone() } else { key.clone() };
        first.update(cx, |i, cx| i.focus(window, cx));
        self.setup = Some(Setup::Configure(Configure {
            platform,
            server: platforms::nearest_server(platform),
            server_input,
            key,
            format,
            error: None,
            want_vertical: false,
        }));
        cx.notify();
    }

    fn save(&mut self, cx: &mut Context<Self>) {
        let Some(Setup::Configure(c)) = &mut self.setup else { return };
        let server = match c.platform.servers.get(c.server) {
            Some((_, url)) => url.to_string(),
            None => c.server_input.read(cx).value().trim().to_string(),
        };
        let key = c.key.read(cx).value().trim().to_string();
        let protocol = StreamProtocol::of(&server);
        // RTMP and WHIP need a key (stream key, bearer token); for SRT it's an optional stream ID.
        let needs_key = matches!(protocol, Some(StreamProtocol::Rtmp | StreamProtocol::Whip));
        c.error = if protocol.is_none() {
            Some(if server.is_empty() {
                "Paste the server URL.".into()
            } else {
                "That doesn't look like a server URL (rtmp://, rtmps://, srt:// or https://).".into()
            })
        } else if needs_key && key.is_empty() {
            Some("Paste the stream key.".into())
        } else {
            None
        };
        if c.error.is_some() {
            cx.notify();
            return;
        }
        let project = self.studio.read(cx).project().clone();
        let base = if c.platform.id == "custom" { host_of(&server) } else { c.platform.name.to_string() };
        let taken = |n: &str| project.outputs.destinations.iter().any(|d| d.name == n);
        let name =
            if taken(&base) { (2..).map(|n| format!("{base} {n}")).find(|n| !taken(n)).unwrap_or(base) } else { base };
        let destination = Destination {
            id: Id::new(),
            name,
            server,
            enabled: true,
            format: c.format,
            platform: Some(c.platform.id.to_string()),
            encoding: None,
        };
        if let Err(e) = crate::secrets::set_stream_key(destination.id, &key) {
            c.error = Some(format!("Couldn't save the stream key: {e}"));
            cx.notify();
            return;
        }
        self.studio.update(cx, |s, cx| {
            let _ = s.execute(Command::AddDestination { destination }, cx);
        });
        self.setup = None;
        cx.notify();
    }

    /// Opens a destination's settings.
    pub fn open_edit(&mut self, id: Id, window: &mut Window, cx: &mut Context<Self>) {
        let project = self.studio.read(cx).project().clone();
        let Some(d) = project.outputs.destinations.iter().find(|d| d.id == id).cloned() else { return };
        let platform = d
            .platform
            .as_deref()
            .and_then(platforms::by_id)
            .or_else(|| platforms::guess(&d.server))
            .unwrap_or(&PLATFORMS[PLATFORMS.len() - 1]);
        let server = platform.servers.iter().position(|(_, url)| *url == d.server);
        let name = cx.new(|cx| InputState::new(window, cx));
        name.update(cx, |i, cx| i.set_value(d.name.clone(), window, cx));
        let server_input = cx.new(|cx| InputState::new(window, cx).placeholder("Server URL"));
        if server.is_none() {
            server_input.update(cx, |i, cx| i.set_value(d.server.clone(), window, cx));
        }
        let key = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("Saved in your Keychain · paste a new key to replace it")
                .masked(true)
        });
        self.setup = Some(Setup::Edit(Edit {
            id,
            platform,
            name,
            server,
            server_input,
            key,
            format: d.format,
            encoding: d.encoding,
            error: None,
        }));
        cx.notify();
    }

    fn save_edit(&mut self, cx: &mut Context<Self>) {
        let Some(Setup::Edit(e)) = &mut self.setup else { return };
        let project = self.studio.read(cx).project().clone();
        let Some(old) = project.outputs.destinations.iter().find(|d| d.id == e.id).cloned() else { return };
        let server = match e.server.and_then(|i| e.platform.servers.get(i)) {
            Some((_, url)) => url.to_string(),
            None => e.server_input.read(cx).value().trim().to_string(),
        };
        if StreamProtocol::of(&server).is_none() {
            e.error = Some("That doesn't look like a server URL (rtmp://, rtmps://, srt:// or https://).".into());
            cx.notify();
            return;
        }
        let name = e.name.read(cx).value().trim().to_string();
        let key = e.key.read(cx).value().trim().to_string();
        if !key.is_empty()
            && let Err(err) = crate::secrets::set_stream_key(e.id, &key)
        {
            e.error = Some(format!("Couldn't save the stream key: {err}"));
            cx.notify();
            return;
        }
        let destination = Destination {
            name: if name.is_empty() { old.name.clone() } else { name },
            server,
            format: e.format,
            encoding: e.encoding,
            ..old
        };
        self.studio.update(cx, |s, cx| {
            let _ = s.execute(Command::UpdateDestination { destination }, cx);
        });
        self.setup = None;
        cx.notify();
    }

    fn remove_destination(&mut self, id: Id, cx: &mut Context<Self>) {
        crate::secrets::delete_stream_key(id);
        self.studio.update(cx, |s, cx| {
            let _ = s.execute(Command::RemoveDestination { destination: id }, cx);
        });
        self.setup = None;
        cx.notify();
    }

    fn edit_view(&self, e: &Edit, project: &Project, cx: &mut Context<Self>) -> Div {
        let p = e.platform;
        let caps = self.studio.read(cx).engine().capabilities.encoders.clone();
        let label = |text: &'static str| {
            div().text_size(px(11.)).font_weight(FontWeight::MEDIUM).text_color(tok::text_secondary()).child(text)
        };
        let field = |name: &'static str| div().flex().flex_col().gap_1().child(label(name));
        let me = cx.entity().downgrade();
        // Change the dialog's draft encoding.
        let set = move |f: Box<dyn Fn(&mut Encoding)>| {
            let me = me.clone();
            move |_: &ClickEvent, _: &mut Window, cx: &mut App| {
                if let Some(me) = me.upgrade() {
                    me.update(cx, |this, cx| {
                        if let Some(Setup::Edit(e)) = &mut this.setup
                            && let Some(enc) = &mut e.encoding
                        {
                            f(enc);
                        }
                        cx.notify();
                    });
                }
            }
        };
        let chip = |id: (&'static str, usize), text: String, on: bool, enabled: bool| {
            div()
                .id(id)
                .px_2()
                .h(px(24.))
                .flex()
                .items_center()
                .rounded(px(5.))
                .text_size(px(11.))
                .border_1()
                .border_color(if on { tok::text_secondary() } else { tok::border() })
                .when(on, |d| d.bg(tok::bg_selected()).text_color(tok::text()))
                .when(!on && enabled, |d| {
                    d.cursor_pointer().text_color(tok::text_secondary()).hover(|d| d.bg(tok::bg_raised()))
                })
                .when(!enabled, |d| d.opacity(0.4).text_color(tok::text_tertiary()))
                .child(text)
        };

        // Server: the platform's list, or the typed address.
        let server = if p.servers.len() > 1 {
            let current = e.server.and_then(|i| p.servers.get(i)).map_or("Custom address", |(l, _)| *l);
            let selected = e.server;
            let me = cx.entity().downgrade();
            field("Server").child(Button::new("edit-server").label(current).small().outline().w_full().dropdown_menu(
                move |mut menu, _, _| {
                    for (i, (l, _)) in p.servers.iter().enumerate() {
                        let me = me.clone();
                        menu =
                            menu.item(PopupMenuItem::new(*l).checked(selected == Some(i)).on_click(move |_, _, cx| {
                                if let Some(me) = me.upgrade() {
                                    me.update(cx, |this, cx| {
                                        if let Some(Setup::Edit(e)) = &mut this.setup {
                                            e.server = Some(i);
                                        }
                                        cx.notify();
                                    });
                                }
                            }));
                    }
                    menu.max_h(px(320.)).scrollable(true)
                },
            ))
        } else if p.servers.len() == 1 && e.server.is_some() {
            field("Server")
                .child(div().text_size(px(12.)).text_color(tok::text_tertiary()).child(host_of(p.servers[0].1)))
        } else {
            field("Server URL").child(Input::new(&e.server_input).small())
        };

        // Shape.
        let mut shapes = div().flex().flex_wrap().gap_1();
        for (i, f) in project.formats.iter().enumerate() {
            let id = (f.id != project.primary_format().id).then_some(f.id);
            shapes = shapes.child(
                chip(("edit-shape", i), format!("{} {}", f.name, aspect_label(f)), e.format == id, true).on_click(
                    cx.listener(move |this, _, _, cx| {
                        if let Some(Setup::Edit(e)) = &mut this.setup {
                            e.format = id;
                        }
                        cx.notify();
                    }),
                ),
            );
        }

        // Quality: Automatic, or its own.
        let destination = project.outputs.destinations.iter().find(|d| d.id == e.id).cloned();
        let auto = destination.as_ref().map(|d| crate::encoding::automatic(project, d)).unwrap_or_default();
        let automatic = e.encoding.is_none();
        let summary = |enc: &Encoding| {
            format!(
                "{} · {:.1} Mbps · {} · keyframes every {} s · audio {} kbps",
                enc.encoder.label(),
                enc.bitrate_kbps as f32 / 1000.0,
                enc.height.map_or("canvas size".to_string(), |h| format!("{h}p")),
                enc.keyframe_s,
                enc.audio_kbps
            )
        };
        let mut quality = div().flex().flex_col().gap_2().p_3().rounded(px(8.)).bg(tok::bg_raised()).child(
            div()
                .flex()
                .items_center()
                .justify_between()
                .child(div().flex().flex_col().child(div().text_size(px(13.)).child("Automatic")).child(
                    div().text_size(px(11.)).text_color(tok::text_tertiary()).child(if automatic {
                        summary(&auto)
                    } else {
                        "Off: this destination uses its own settings below.".to_string()
                    }),
                ))
                .child(toggle("edit-auto", automatic).on_click(cx.listener(move |this, _, _, cx| {
                    if let Some(Setup::Edit(e)) = &mut this.setup {
                        e.encoding = if e.encoding.is_some() { None } else { Some(auto) };
                    }
                    cx.notify();
                }))),
        );
        if let Some(enc) = e.encoding {
            let mut encoders = div().flex().flex_wrap().gap_1();
            for (i, choice) in [VideoEncoder::H264, VideoEncoder::Hevc, VideoEncoder::X264].into_iter().enumerate() {
                let ok = crate::encoding::available(&caps, choice);
                let mut c = chip(("edit-encoder", i), choice.label().to_string(), enc.encoder == choice, ok);
                if ok {
                    c = c.on_click(set(Box::new(move |e| e.encoder = choice)));
                }
                encoders = encoders.child(c);
            }
            let max = p.max_kbps;
            let bitrate = div()
                .flex()
                .items_center()
                .gap_1()
                .child(
                    Button::new("edit-bitrate-down")
                        .icon(ui::icon("minus"))
                        .xsmall()
                        .ghost()
                        .on_click(set(Box::new(|e| e.bitrate_kbps = e.bitrate_kbps.saturating_sub(500).max(500)))),
                )
                .child(
                    div()
                        .w(px(84.))
                        .text_center()
                        .text_size(px(12.))
                        .font_family("Menlo")
                        .child(format!("{:.1} Mbps", enc.bitrate_kbps as f32 / 1000.0)),
                )
                .child(
                    Button::new("edit-bitrate-up")
                        .icon(ui::icon("plus"))
                        .xsmall()
                        .ghost()
                        .on_click(set(Box::new(|e| e.bitrate_kbps = (e.bitrate_kbps + 500).min(51_000)))),
                )
                .when_some(max.filter(|m| enc.bitrate_kbps > *m), |d, m| {
                    d.child(div().text_size(px(11.)).text_color(tok::warn()).child(format!(
                        "{} takes up to {} Mbps",
                        p.name,
                        m / 1000
                    )))
                });
            let mut heights = div().flex().flex_wrap().gap_1();
            for (i, h) in [None, Some(1080), Some(720), Some(480), Some(360)].into_iter().enumerate() {
                heights = heights.child(
                    chip(
                        ("edit-height", i),
                        h.map_or("Canvas".to_string(), |h| format!("{h}p")),
                        enc.height == h,
                        true,
                    )
                    .on_click(set(Box::new(move |e| e.height = h))),
                );
            }
            let mut keys = div().flex().flex_wrap().gap_1();
            for (i, k) in [1, 2, 4].into_iter().enumerate() {
                keys = keys.child(
                    chip(("edit-keyframe", i), format!("{k} s"), enc.keyframe_s == k, true)
                        .on_click(set(Box::new(move |e| e.keyframe_s = k))),
                );
            }
            let mut audio = div().flex().flex_wrap().gap_1();
            for (i, k) in [96, 128, 160, 320].into_iter().enumerate() {
                audio = audio.child(
                    chip(("edit-audio", i), format!("{k} kbps"), enc.audio_kbps == k, true)
                        .on_click(set(Box::new(move |e| e.audio_kbps = k))),
                );
            }
            quality = quality
                .child(field("Encoder").child(encoders).when(enc.encoder == VideoEncoder::Hevc, |d| {
                    d.child(
                        div()
                            .text_size(px(11.))
                            .text_color(tok::text_tertiary())
                            .child("HEVC streams to YouTube; most other platforms need H.264."),
                    )
                }))
                .child(field("Bitrate").child(bitrate))
                .child(field("Resolution").child(heights))
                .child(field("Keyframes every").child(keys))
                .child(field("Audio").child(audio));
        }
        let id = e.id;
        div()
            .flex()
            .flex_col()
            .gap_4()
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_3()
                    .child(badge(p, 36.))
                    .child(div().flex_1().child(Input::new(&e.name).small()))
                    .child(
                        Button::new("edit-remove")
                            .label("Remove")
                            .icon(ui::icon("trash"))
                            .xsmall()
                            .ghost()
                            .on_click(cx.listener(move |this, _, _, cx| this.remove_destination(id, cx))),
                    ),
            )
            .child(server)
            .child(field("Stream key").child(Input::new(&e.key).small().mask_toggle()))
            .child(field("Send").child(shapes))
            .child(field("Quality").child(quality))
            .when_some(e.error.clone(), |d, err| d.child(div().text_size(px(11.)).text_color(tok::error()).child(err)))
            .child(
                div()
                    .flex()
                    .gap_2()
                    .child(
                        Button::new("edit-save")
                            .label("Save")
                            .small()
                            .primary()
                            .on_click(cx.listener(|this, _, _, cx| this.save_edit(cx))),
                    )
                    .child(Button::new("edit-cancel").label("Cancel").small().ghost().on_click(cx.listener(
                        |this, _, _, cx| {
                            this.setup = None;
                            cx.notify();
                        },
                    ))),
            )
    }

    /// The grid of platforms to start from.
    fn picker(&self, cx: &mut Context<Self>) -> Div {
        let mut grid = div().grid().grid_cols(4).gap_2();
        for (i, p) in PLATFORMS.iter().enumerate() {
            grid = grid.child(
                div()
                    .id(("platform", i))
                    .flex()
                    .flex_col()
                    .items_center()
                    .gap_2()
                    .py_3()
                    .rounded(px(10.))
                    .cursor_pointer()
                    .bg(tok::bg_raised())
                    .border_1()
                    .border_color(tok::border())
                    .hover(|d| d.bg(tok::bg_selected()).border_color(tok::border_strong()))
                    .child(badge(p, 36.))
                    .child(div().text_size(px(12.)).text_color(tok::text_secondary()).child(p.name))
                    .on_click(cx.listener(move |this, _, window, cx| this.choose(p, window, cx))),
            );
        }
        div()
            .flex()
            .flex_col()
            .gap_3()
            .child(div().text_size(px(16.)).font_weight(FontWeight::SEMIBOLD).child("Add a destination"))
            .child(div().text_size(px(12.)).text_color(tok::text_secondary()).child("Where do you want to go live?"))
            .child(grid)
    }

    /// The Add destination dialog, drawn over the window while open.
    pub fn dialog(&mut self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let project = self.studio.read(cx).project().clone();
        // The vertical format asked for has arrived: send it.
        if let Some(Setup::Configure(c)) = &mut self.setup
            && c.want_vertical
            && let Some(f) = project.formats.iter().find(|f| f.height > f.width)
        {
            c.format = Some(f.id);
            c.want_vertical = false;
        }
        let body = match self.setup.as_ref()? {
            Setup::Pick => self.picker(cx),
            Setup::Configure(c) => self.configure(c, &project, cx),
            Setup::Edit(e) => self.edit_view(e, &project, cx),
        };
        let close = cx.listener(|this, _, _, cx| {
            this.setup = None;
            cx.notify();
        });
        Some(
            div()
                .absolute()
                .inset_0()
                .flex()
                .justify_center()
                .items_start()
                .pt(px(48.))
                .bg(tok::scrim())
                .on_mouse_down(MouseButton::Left, close)
                .child(
                    div()
                        .id("destination-dialog")
                        .w(px(520.))
                        .max_h(relative(0.88))
                        .overflow_y_scroll()
                        .p_4()
                        .rounded(px(14.))
                        .bg(tok::bg_panel())
                        .border_1()
                        .border_color(tok::border_strong())
                        .shadow_lg()
                        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                        .capture_key_down(cx.listener(|this, ev: &KeyDownEvent, _, cx| {
                            if ev.keystroke.key == "escape" {
                                this.setup = None;
                                cx.notify();
                            }
                        }))
                        .child(body),
                )
                .into_any_element(),
        )
    }

    /// A platform's details: server, key, shape.
    fn configure(&self, c: &Configure, project: &Project, cx: &mut Context<Self>) -> Div {
        let p = c.platform;
        let field_label = |text: &'static str| {
            div().text_size(px(11.)).font_weight(FontWeight::MEDIUM).text_color(tok::text_secondary()).child(text)
        };
        let me = cx.entity().downgrade();
        // Server: a choice where the platform has a list, otherwise pasted.
        let server = match p.servers.len() {
            0 => div()
                .flex()
                .flex_col()
                .gap_1()
                .child(field_label("Server URL"))
                .child(Input::new(&c.server_input).small()),
            1 => div()
                .flex()
                .flex_col()
                .gap_1()
                .child(field_label("Server"))
                .child(div().text_size(px(12.)).text_color(tok::text_tertiary()).child(host_of(p.servers[0].1))),
            _ => {
                let current = p.servers[c.server.min(p.servers.len() - 1)].0;
                let selected = c.server;
                div().flex().flex_col().gap_1().child(field_label("Server")).child(
                    Button::new("server-choice").label(current).small().outline().w_full().dropdown_menu(
                        move |mut menu, _, _| {
                            for (i, (label, _)) in p.servers.iter().enumerate() {
                                let me = me.clone();
                                menu = menu.item(PopupMenuItem::new(*label).checked(i == selected).on_click(
                                    move |_, _, cx| {
                                        if let Some(me) = me.upgrade() {
                                            me.update(cx, |this, cx| {
                                                if let Some(Setup::Configure(c)) = &mut this.setup {
                                                    c.server = i;
                                                }
                                                cx.notify();
                                            });
                                        }
                                    },
                                ));
                            }
                            menu.max_h(px(320.)).scrollable(true)
                        },
                    ),
                )
            }
        };
        let key_link = p.key_link.map(|url| {
            Button::new("key-link")
                .label("Get your stream key")
                .icon(ui::icon("square-arrow-out-up-right"))
                .xsmall()
                .ghost()
                .on_click(move |_, _, cx| cx.open_url(url))
        });
        // Shape: the project's formats, the platform's preferred one marked.
        let mut shapes = div().flex().flex_wrap().gap_1();
        for (i, f) in project.formats.iter().enumerate() {
            let id = (f.id != project.primary_format().id).then_some(f.id);
            let on = c.format == id;
            let fits = match p.shape {
                Shape::Vertical => f.height > f.width,
                Shape::Landscape => f.width >= f.height,
            };
            shapes = shapes.child(
                div()
                    .id(("dest-shape", i))
                    .flex()
                    .items_center()
                    .gap_1()
                    .h(px(24.))
                    .px_2()
                    .rounded(px(5.))
                    .cursor_pointer()
                    .text_size(px(11.))
                    .border_1()
                    .border_color(if on { tok::text_secondary() } else { tok::border() })
                    .when(on, |d| d.bg(tok::bg_selected()).text_color(tok::text()))
                    .when(!on, |d| d.text_color(tok::text_secondary()).hover(|d| d.bg(tok::bg_raised())))
                    .child(format!("{} {}", f.name, aspect_label(f)))
                    .when(fits && p.id != "custom", |d| d.child(div().size(px(5.)).rounded_full().bg(tok::ok())))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if let Some(Setup::Configure(c)) = &mut this.setup {
                            c.format = id;
                        }
                        cx.notify();
                    })),
            );
        }
        let no_vertical = p.shape == Shape::Vertical && !project.formats.iter().any(|f| f.height > f.width);
        div()
            .flex()
            .flex_col()
            .gap_4()
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_3()
                    .child(badge(p, 36.))
                    .child(div().flex_1().text_size(px(16.)).font_weight(FontWeight::SEMIBOLD).child(p.name))
                    .child(
                        Button::new("change-platform")
                            .label("All platforms")
                            .icon(ui::icon("arrow-left"))
                            .xsmall()
                            .ghost()
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.setup = Some(Setup::Pick);
                                cx.notify();
                            })),
                    ),
            )
            .child(div().text_size(px(12.)).text_color(tok::text_secondary()).child(p.help))
            .child(server)
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(field_label(if p.id == "custom" { "Key or token" } else { "Stream key" }))
                    .child(Input::new(&c.key).small().mask_toggle())
                    .child(
                        div().flex().items_center().justify_between().children(key_link).child(
                            div()
                                .flex()
                                .items_center()
                                .gap_1()
                                .text_size(px(10.))
                                .text_color(tok::text_tertiary())
                                .child(ui::icon("key-round").size(px(11.)))
                                .child("Kept in your Keychain"),
                        ),
                    ),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(field_label("Send"))
                    .child(shapes)
                    .when(no_vertical, |d| {
                        d.child(
                            div()
                                .flex()
                                .items_center()
                                .justify_between()
                                .gap_2()
                                .child(div().flex_1().min_w_0().text_size(px(11.)).text_color(tok::warn()).child(
                                    format!(
                                        "{} wants vertical video, and this project has no vertical format yet.",
                                        p.name
                                    ),
                                ))
                                .child(
                                    Button::new("add-vertical")
                                        .flex_none()
                                        .label("Add vertical format")
                                        .xsmall()
                                        .outline()
                                        .on_click(cx.listener(|this, _, _, cx| this.request_vertical(cx))),
                                ),
                        )
                    })
                    .when_some(p.max_kbps, |d, kbps| {
                        d.child(
                            div()
                                .text_size(px(10.))
                                .text_color(tok::text_tertiary())
                                .child(format!("Takes up to {} Mbps", kbps / 1000)),
                        )
                    }),
            )
            .when_some(c.error.clone(), |d, e| d.child(div().text_size(px(11.)).text_color(tok::error()).child(e)))
            .child(
                div()
                    .flex()
                    .gap_2()
                    .child(
                        Button::new("dest-save")
                            .label(format!("Add {}", p.name))
                            .small()
                            .primary()
                            .on_click(cx.listener(|this, _, _, cx| this.save(cx))),
                    )
                    .child(Button::new("dest-cancel").label("Cancel").small().ghost().on_click(cx.listener(
                        |this, _, _, cx| {
                            this.setup = None;
                            cx.notify();
                        },
                    ))),
            )
    }
}

impl Render for OutputsPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        window.request_animation_frame();
        let studio = self.studio.read(cx);
        let project = studio.project().clone();
        let folder = studio.recording_folder();
        let card = |id: ElementId| {
            div()
                .id(id)
                .flex()
                .items_center()
                .gap_2p5()
                .mx_2()
                .px_2()
                .py_2()
                .rounded(px(8.))
                .hover(|d| d.bg(tok::bg_raised()))
        };
        let two_lines = |title: String, subtitle: String| {
            div()
                .flex()
                .flex_col()
                .flex_1()
                .min_w_0()
                .child(div().text_size(px(13.)).overflow_hidden().text_ellipsis().whitespace_nowrap().child(title))
                .child(
                    div()
                        .text_size(px(11.))
                        .text_color(tok::text_tertiary())
                        .overflow_hidden()
                        .text_ellipsis()
                        .whitespace_nowrap()
                        .child(subtitle),
                )
        };
        let status = |(text, color): (String, Hsla)| {
            div().flex_none().text_size(px(11.)).font_family("Menlo").text_color(color).child(text)
        };

        // ---- Recording: each canvas, recorded or not (Record records all that are on).
        let settings = project.outputs.recording.clone();
        let targets = settings.targets(&project);
        let several = project.formats.len() > 1;
        let container = settings.container.to_uppercase();
        let folder_name = folder.file_name().map(|f| f.to_string_lossy().into_owned()).unwrap_or_default();
        let mut recordings = div().flex().flex_col().gap_0p5();
        for (i, f) in project.formats.iter().enumerate() {
            let fid = f.id;
            let on = targets.contains(&fid);
            let st = runtime().get(OutputKey::Recording(fid));
            let running = st.state.is_running();
            let state =
                if on || running { status_text(&st.state) } else { ("Not recorded".into(), tok::text_tertiary()) };
            recordings = recordings.child(
                card(("out-rec", i).into())
                    .when(!on && !running, |d| d.opacity(0.6))
                    .child(
                        div()
                            .size(px(28.))
                            .flex_none()
                            .rounded(px(7.))
                            .bg(tok::bg_selected())
                            .flex()
                            .items_center()
                            .justify_center()
                            .child(div().size(px(10.)).rounded_full().bg(if running {
                                tok::live()
                            } else {
                                tok::text_tertiary()
                            })),
                    )
                    .child(two_lines(
                        if several { format!("{} {}", f.name, aspect_label(f)) } else { "Recording".into() },
                        format!("{container} · {folder_name}"),
                    ))
                    .child(status(state))
                    .when(several, |d| {
                        let s2 = settings.clone();
                        let p2 = project.clone();
                        d.child(
                            toggle(("rec-toggle", i), on)
                                .tooltip(move |w, cx| {
                                    gpui_kit::component::tooltip::Tooltip::new(if on {
                                        "Record records this canvas (click to leave it out)"
                                    } else {
                                        "Left out of recording (click to record it too)"
                                    })
                                    .build(w, cx)
                                })
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    let settings = s2.with_format(&p2, fid, !on);
                                    this.studio.update(cx, |s, cx| {
                                        let _ = s.execute(Command::SetRecordingSettings { settings }, cx);
                                    })
                                })),
                        )
                    }),
            );
        }
        let open_folder = folder.clone();
        let show_recordings = ui::icon_button("open-recordings", "folder-open", tok::text_secondary())
            .tooltip(|w, cx| gpui_kit::component::tooltip::Tooltip::new("Show recordings").build(w, cx))
            .on_click(move |_, _, cx| {
                let _ = std::fs::create_dir_all(&open_folder);
                cx.open_url(&format!("file://{}", open_folder.to_string_lossy()))
            });

        // ---- Streaming
        let mut streams = div().flex().flex_col().gap_0p5();
        if runtime().any_stream_running() {
            let live = project
                .outputs
                .destinations
                .iter()
                .filter(|d| runtime().get(OutputKey::Stream(d.id)).state.is_running())
                .count();
            let encoders = studio.engine().call(|s| s.outputs.stream_encoder_count());
            streams = streams.child(
                div()
                    .px_4()
                    .pb_1()
                    .text_size(px(11.))
                    .text_color(tok::text_tertiary())
                    .child(format!("Live on {live} · {encoders} encoder{}", if encoders == 1 { "" } else { "s" })),
            );
        }
        for (i, d) in project.outputs.destinations.iter().enumerate() {
            let id = d.id;
            let enabled = d.enabled;
            let st = runtime().get(OutputKey::Stream(id));
            let state = if enabled || st.state.is_running() {
                status_text(&st.state)
            } else {
                ("Skipped".into(), tok::text_tertiary())
            };
            let platform = d
                .platform
                .as_deref()
                .and_then(platforms::by_id)
                .or_else(|| platforms::guess(&d.server))
                .unwrap_or(&PLATFORMS[PLATFORMS.len() - 1]);
            // Which server, in words where we know it.
            let where_ = platform
                .servers
                .iter()
                .find(|(_, url)| *url == d.server)
                .map(|(label, _)| format!("{label} · {}", StreamProtocol::label(&d.server)))
                .unwrap_or_else(|| format!("{} · {}", host_of(&d.server), StreamProtocol::label(&d.server)));
            streams = streams.child(
                card(("out-dest", i).into())
                    .cursor_pointer()
                    .when(!enabled, |d| d.opacity(0.6))
                    .on_click(cx.listener(move |this, _, window, cx| this.open_edit(id, window, cx)))
                    .child(badge(platform, 28.))
                    .child(two_lines(d.name.clone(), where_))
                    .child(format_badge(("dest-format", i), &project, d.format).on_click(cx.listener({
                        let project = project.clone();
                        let current = d.format;
                        move |this, _, _, cx| {
                            cx.stop_propagation();
                            let format = next_format(&project, current);
                            this.studio.update(cx, |s, cx| {
                                let _ = s.execute(Command::SetDestinationFormat { destination: id, format }, cx);
                            })
                        }
                    })))
                    .child(status(state))
                    .child(
                        toggle(("dest-toggle", i), enabled)
                            .tooltip(move |w, cx| {
                                gpui_kit::component::tooltip::Tooltip::new(if enabled {
                                    "Going live sends here (click to skip it)"
                                } else {
                                    "Skipped when going live (click to use it)"
                                })
                                .build(w, cx)
                            })
                            .on_click(cx.listener(move |this, _, _, cx| {
                                cx.stop_propagation();
                                this.studio.update(cx, |s, cx| {
                                    let _ = s.execute(
                                        Command::SetDestinationEnabled { destination: id, enabled: !enabled },
                                        cx,
                                    );
                                })
                            })),
                    )
                    .context_menu({
                        let studio = self.studio.downgrade();
                        move |menu, _, _| {
                            let run = |cmd: Command, forget_key: bool| {
                                let studio = studio.clone();
                                move |_: &ClickEvent, _: &mut Window, cx: &mut App| {
                                    if forget_key {
                                        crate::secrets::delete_stream_key(id);
                                    }
                                    if let Some(s) = studio.upgrade() {
                                        s.update(cx, |s, cx| {
                                            let _ = s.execute(cmd.clone(), cx);
                                        });
                                    }
                                }
                            };
                            menu.item(
                                PopupMenuItem::new(if enabled {
                                    "Skip when going live"
                                } else {
                                    "Use when going live"
                                })
                                .on_click(run(
                                    Command::SetDestinationEnabled { destination: id, enabled: !enabled },
                                    false,
                                )),
                            )
                            .separator()
                            .item(
                                PopupMenuItem::new("Remove")
                                    .icon(ui::icon("trash"))
                                    .on_click(run(Command::RemoveDestination { destination: id }, true)),
                            )
                        }
                    }),
            );
        }

        div()
            .id("outputs")
            .flex()
            .flex_col()
            .size_full()
            .overflow_y_scroll()
            .pb_3()
            .child(
                div().flex().items_center().justify_between().pr_3().child(section_label("RECORDING")).child(
                    div().pt_2().flex().gap_0p5().child(show_recordings).child(
                        ui::icon_button("recording-settings", "settings", tok::text_secondary())
                            .tooltip(|w, cx| {
                                gpui_kit::component::tooltip::Tooltip::new("Recording settings").build(w, cx)
                            })
                            .on_click(cx.listener(|_, _, _, cx| cx.emit(OpenRecordingSettings))),
                    ),
                ),
            )
            .child(recordings)
            .child(
                div().flex().items_center().justify_between().pr_3().child(section_label("STREAMING")).child(
                    Button::new("add-destination")
                        .label("Add")
                        .icon(ui::icon("plus"))
                        .xsmall()
                        .ghost()
                        .on_click(cx.listener(|this, _, _, cx| this.open_picker(cx))),
                ),
            )
            .child(streams)
    }
}

// ---------------------------------------------------------------- dock panel
impl gpui_kit::component::dock::BasePanel for OutputsPanel {
    fn panel_name(&self) -> &'static str {
        "outputs"
    }
}

impl gpui_kit::component::dock::Panel for OutputsPanel {
    /// None: the tab shows `title` (icon and name).
    fn tab_name(&self, _: &App) -> Option<SharedString> {
        None
    }

    fn title(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        ui::panel_title("radio", "Outputs")
    }

    fn title_style(&self, _: &App) -> Option<gpui_kit::component::dock::TitleStyle> {
        Some(ui::panel_header_style())
    }

    fn toolbar_buttons(
        &mut self,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<Vec<gpui_kit::component::button::Button>> {
        use gpui_kit::component::Sizable;
        use gpui_kit::component::button::ButtonVariants;
        Some(vec![
            gpui_kit::component::button::Button::new("outputs-add")
                .icon(ui::icon("plus"))
                .ghost()
                .xsmall()
                .tooltip("Add destination")
                .on_click(cx.listener(|this, _, _, cx| this.open_picker(cx))),
        ])
    }

    fn zoom_control(&self, _: &App) -> Option<gpui_kit::component::dock::PanelControl> {
        None
    }

    fn inner_padding(&self, _: &App) -> bool {
        false
    }
}

impl EventEmitter<gpui_kit::component::dock::PanelEvent> for OutputsPanel {}
impl EventEmitter<AddVerticalFormat> for OutputsPanel {}
impl EventEmitter<OpenRecordingSettings> for OutputsPanel {}

impl Focusable for OutputsPanel {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}
