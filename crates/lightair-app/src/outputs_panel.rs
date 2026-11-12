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
use lightair_core::{Command, Destination, Format, Id, Project};
use lightair_engine::outputs::{OutputKey, OutputState, StreamProtocol, runtime};

use crate::platforms::{self, PLATFORMS, Platform, Shape};
use crate::studio::Studio;
use crate::ui::{self, tok};

/// Adding a destination: first a platform, then its details.
enum Setup {
    Pick,
    Configure(Configure),
}

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
                .pt(px(72.))
                .bg(tok::scrim())
                .on_mouse_down(MouseButton::Left, close)
                .child(
                    div()
                        .id("destination-dialog")
                        .w(px(520.))
                        .max_h(px(640.))
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
        let rec = runtime().get(OutputKey::Recording);
        let (rec_text, rec_color) = status_text(&rec.state);
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

        // ---- Recording
        let rec_settings = project.outputs.recording.clone();
        let rec_badge = format_badge("rec-format", &project, rec_settings.format).on_click(cx.listener({
            let project = project.clone();
            move |this, _, _, cx| {
                let settings = lightair_core::RecordingSettings {
                    format: next_format(&project, rec_settings.format),
                    ..rec_settings.clone()
                };
                this.studio.update(cx, |s, cx| {
                    let _ = s.execute(Command::SetRecordingSettings { settings }, cx);
                })
            }
        }));
        let recording = rec.state.is_running();
        let open_folder = folder.clone();
        let recording_card = card("out-rec".into())
            .child(
                div()
                    .size(px(28.))
                    .flex_none()
                    .rounded(px(7.))
                    .bg(tok::bg_selected())
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(div().size(px(10.)).rounded_full().bg(if recording {
                        tok::live()
                    } else {
                        tok::text_tertiary()
                    })),
            )
            .child(two_lines(
                "Recording".into(),
                format!(
                    "{} · {}",
                    project.outputs.recording.container.to_uppercase(),
                    folder.file_name().map(|f| f.to_string_lossy().into_owned()).unwrap_or_default()
                ),
            ))
            .child(rec_badge)
            .child(status((rec_text, rec_color)))
            .child(
                ui::icon_button("open-recordings", "folder-open", tok::text_secondary())
                    .tooltip(|w, cx| gpui_kit::component::tooltip::Tooltip::new("Show recordings").build(w, cx))
                    .on_click(move |_, _, cx| {
                        let _ = std::fs::create_dir_all(&open_folder);
                        cx.open_url(&format!("file://{}", open_folder.to_string_lossy()))
                    }),
            );

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
                    .when(!enabled, |d| d.opacity(0.6))
                    .child(badge(platform, 28.))
                    .child(two_lines(d.name.clone(), where_))
                    .child(format_badge(("dest-format", i), &project, d.format).on_click(cx.listener({
                        let project = project.clone();
                        let current = d.format;
                        move |this, _, _, cx| {
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
        let empty = project.outputs.destinations.is_empty();
        // Nothing yet: the main platforms, one click from their setup.
        let quick = empty.then(|| {
            let mut row = div().flex().flex_wrap().gap_1p5();
            for (i, p) in PLATFORMS.iter().take(6).enumerate() {
                row = row.child(
                    div()
                        .id(("quick-platform", i))
                        .cursor_pointer()
                        .rounded(px(8.))
                        .hover(|d| d.opacity(0.85))
                        .child(badge(p, 30.))
                        .tooltip(move |w, cx| gpui_kit::component::tooltip::Tooltip::new(p.name).build(w, cx))
                        .on_click(cx.listener(move |this, _, window, cx| this.choose(p, window, cx))),
                );
            }
            row = row.child(
                div()
                    .id("quick-more")
                    .size(px(30.))
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(px(7.))
                    .cursor_pointer()
                    .border_1()
                    .border_color(tok::border_strong())
                    .text_color(tok::text_secondary())
                    .hover(|d| d.bg(tok::bg_raised()))
                    .child(ui::icon("plus").size(px(14.)))
                    .tooltip(|w, cx| gpui_kit::component::tooltip::Tooltip::new("More platforms").build(w, cx))
                    .on_click(cx.listener(|this, _, _, cx| this.open_picker(cx))),
            );
            div()
                .flex()
                .flex_col()
                .gap_2()
                .mx_4()
                .mt_1()
                .child(div().text_size(px(12.)).text_color(tok::text_secondary()).child("Go live on"))
                .child(row)
        });

        div()
            .id("outputs")
            .flex()
            .flex_col()
            .size_full()
            .overflow_y_scroll()
            .pb_3()
            .child(section_label("RECORDING"))
            .child(recording_card)
            .child(div().flex().items_center().justify_between().pr_3().child(section_label("STREAMING")).when(
                !empty,
                |d| {
                    d.child(
                        Button::new("add-destination")
                            .label("Add")
                            .icon(ui::icon("plus"))
                            .xsmall()
                            .ghost()
                            .on_click(cx.listener(|this, _, _, cx| this.open_picker(cx))),
                    )
                },
            ))
            .child(streams)
            .children(quick)
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

impl Focusable for OutputsPanel {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}
