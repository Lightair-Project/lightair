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
use lightair_core::{Command, Destination, Encoding, Canvas, Id, Workspace, VideoEncoder};
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
    canvas: Option<Id>,
    /// `None`: Automatic.
    encoding: Option<Encoding>,
    /// Its sound (`None`: the master), and Twitch's VOD track (`None`: none).
    audio_mix: Option<Id>,
    vod_mix: Option<Id>,
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
    canvas: Option<Id>,
    error: Option<String>,
    /// A vertical canvas was asked for: pick it once it exists.
    want_vertical: bool,
}

/// Asks the window to add a vertical canvas (reframing every scene into it).
pub struct AddVerticalCanvas;

pub struct OutputsPanel {
    studio: Entity<Studio>,
    setup: Option<Setup>,
    focus: FocusHandle,
    /// The recording drive's free space.
    disk: crate::storage::DiskWatch,
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

/// "16:9", "9:16", "1:1" for a canvas.
fn aspect_label(f: &Canvas) -> String {
    fn gcd(a: u32, b: u32) -> u32 {
        if b == 0 { a } else { gcd(b, a % b) }
    }
    let g = gcd(f.width, f.height).max(1);
    format!("{}:{}", f.width / g, f.height / g)
}

/// The canvas after `current` in the workspace's list (wrapping), for the badge's click.
fn next_canvas(workspace: &Workspace, current: Option<Id>) -> Option<Id> {
    let cur = current.unwrap_or(workspace.primary_canvas().id);
    let i = workspace.canvases.iter().position(|f| f.id == cur).unwrap_or(0);
    let next = &workspace.canvases[(i + 1) % workspace.canvases.len()];
    (next.id != workspace.primary_canvas().id).then_some(next.id)
}

fn canvas_badge(id: impl Into<ElementId>, workspace: &Workspace, canvas: Option<Id>) -> Stateful<Div> {
    let f = canvas.and_then(|id| workspace.canvas(id)).unwrap_or(workspace.primary_canvas());
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
        .when(workspace.canvases.len() > 1, |d| d.cursor_pointer().hover(|d| d.bg(tok::bg_selected())))
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

/// A form row: its name on the left (an ⓘ with its meaning, when it has one), the control on
/// the right (segmented controls hug their choices; inputs and dropdowns fill the row).
fn form_row(name: &'static str, tip: Option<SharedString>, control: impl IntoElement) -> Div {
    div()
        .flex()
        .items_center()
        .gap_3()
        .min_h(px(28.))
        .child(
            div()
                .w(px(108.))
                .flex_none()
                .flex()
                .items_center()
                .gap_1()
                .child(div().text_size(px(12.)).text_color(tok::text_secondary()).child(name))
                .children(tip.map(|t| ui::info(name, t))),
        )
        .child(div().flex_1().min_w_0().flex().items_center().child(control))
}

/// A card of form rows under a small title.
fn form_card(title: &'static str) -> Div {
    div()
        .flex()
        .flex_col()
        .gap_2p5()
        .p_3p5()
        .rounded(px(10.))
        .bg(tok::bg_raised())
        .border_1()
        .border_color(tok::border())
        .child(div().text_size(px(10.)).font_weight(FontWeight::SEMIBOLD).text_color(tok::text_tertiary()).child(title.to_uppercase()))
}

/// Choices side by side in one well (a segmented control)...
fn segments() -> Div {
    div().flex().flex_wrap().gap(px(2.)).p(px(2.)).rounded(px(7.)).bg(tok::bg_app())
}

/// ...and one of them.
fn segment(id: impl Into<ElementId>, text: impl Into<SharedString>, on: bool, enabled: bool) -> Stateful<Div> {
    div()
        .id(id)
        .px_2p5()
        .h(px(24.))
        .flex()
        .items_center()
        .gap_1p5()
        .rounded(px(5.))
        .text_size(px(12.))
        .when(on, |d| d.bg(tok::bg_selected()).text_color(tok::text()).font_weight(FontWeight::MEDIUM))
        .when(!on && enabled, |d| d.cursor_pointer().text_color(tok::text_secondary()).hover(|d| d.text_color(tok::text())))
        .when(!enabled, |d| d.opacity(0.4).text_color(tok::text_tertiary()))
        .child(text.into())
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
        Self { studio, setup: None, focus: cx.focus_handle(), disk: Default::default() }
    }

    /// Asks for a vertical canvas, to send once it's there.
    pub fn request_vertical(&mut self, cx: &mut Context<Self>) {
        if let Some(Setup::Configure(c)) = &mut self.setup {
            c.want_vertical = true;
        }
        cx.emit(AddVerticalCanvas);
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
        // The shape it wants: a vertical canvas for vertical platforms, when there is one.
        let workspace = self.studio.read(cx).workspace().clone();
        let canvas = match platform.shape {
            Shape::Vertical => workspace.canvases.iter().find(|f| f.height > f.width).map(|f| f.id),
            Shape::Landscape => None,
        };
        let first = if platform.servers.is_empty() { server_input.clone() } else { key.clone() };
        first.update(cx, |i, cx| i.focus(window, cx));
        self.setup = Some(Setup::Configure(Configure {
            platform,
            server: platforms::nearest_server(platform),
            server_input,
            key,
            canvas,
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
        let workspace = self.studio.read(cx).workspace().clone();
        let base = if c.platform.id == "custom" { host_of(&server) } else { c.platform.name.to_string() };
        let taken = |n: &str| workspace.outputs.destinations.iter().any(|d| d.name == n);
        let name =
            if taken(&base) { (2..).map(|n| format!("{base} {n}")).find(|n| !taken(n)).unwrap_or(base) } else { base };
        let destination = Destination {
            id: Id::new(),
            name,
            server,
            enabled: true,
            canvas: c.canvas,
            platform: Some(c.platform.id.to_string()),
            encoding: None,
            audio_mix: None,
            vod_mix: None,
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
        let workspace = self.studio.read(cx).workspace().clone();
        let Some(d) = workspace.outputs.destinations.iter().find(|d| d.id == id).cloned() else { return };
        let platform = d
            .platform
            .as_deref()
            .and_then(platforms::by_id)
            .or_else(|| platforms::guess(&d.server))
            .unwrap_or(platforms::custom());
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
            canvas: d.canvas,
            encoding: d.encoding,
            audio_mix: d.audio_mix,
            vod_mix: d.vod_mix,
            error: None,
        }));
        cx.notify();
    }

    fn save_edit(&mut self, cx: &mut Context<Self>) {
        let Some(Setup::Edit(e)) = &mut self.setup else { return };
        let workspace = self.studio.read(cx).workspace().clone();
        let Some(old) = workspace.outputs.destinations.iter().find(|d| d.id == e.id).cloned() else { return };
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
            canvas: e.canvas,
            encoding: e.encoding,
            audio_mix: e.audio_mix,
            vod_mix: e.vod_mix,
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

    fn edit_view(&self, e: &Edit, workspace: &Workspace, cx: &mut Context<Self>) -> Div {
        let p = e.platform;
        let caps = self.studio.read(cx).engine().capabilities.encoders.clone();
        let row = |name: &'static str, tip: Option<&'static str>, control: AnyElement| form_row(name, tip.map(SharedString::from), control);
        let card = form_card;
        let seg = segments;
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
        let chip = |id: (&'static str, usize), text: String, on: bool, enabled: bool| segment(id, text, on, enabled);

        // Server: the platform's list, or the typed address.
        let server = if p.servers.len() > 1 {
            let current = e.server.and_then(|i| p.servers.get(i)).map_or("Custom address", |(l, _)| *l);
            let selected = e.server;
            let me = cx.entity().downgrade();
            row("Server", None, div().flex_1().child(Button::new("edit-server").label(current).small().outline().w_full().dropdown_caret(true).dropdown_menu(
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
            )).into_any_element())
        } else if p.servers.len() == 1 && e.server.is_some() {
            row("Server", None, div().text_size(px(12.)).text_color(tok::text_tertiary()).child(host_of(p.servers[0].1)).into_any_element())
        } else {
            row("Server URL", None, div().flex_1().child(Input::new(&e.server_input).small()).into_any_element())
        };

        // Shape.
        let mut shapes = seg();
        for (i, f) in workspace.canvases.iter().enumerate() {
            let id = (f.id != workspace.primary_canvas().id).then_some(f.id);
            shapes = shapes.child(
                chip(("edit-shape", i), format!("{} {}", f.name, aspect_label(f)), e.canvas == id, true).on_click(
                    cx.listener(move |this, _, _, cx| {
                        if let Some(Setup::Edit(e)) = &mut this.setup {
                            e.canvas = id;
                        }
                        cx.notify();
                    }),
                ),
            );
        }

        // Sound: the master or a mix; Twitch also takes a VOD track (a second mix kept in VODs).
        // (the audio engine plays the first five mixes out: the rest can't be picked)
        let mixes: Vec<(Id, String, bool)> = workspace
            .audio
            .mixes
            .iter()
            .enumerate()
            .map(|(k, m)| (m.id, m.name.clone(), k < 5))
            .collect();
        // a mix by id: its name (the master / off when none)
        let mix_name = |id: Option<Id>, none: &str| {
            id.and_then(|id| mixes.iter().find(|m| m.0 == id)).map_or(none.to_string(), |m| m.1.clone())
        };
        let me = cx.entity().downgrade();
        // a dropdown that sets e.audio_mix or e.vod_mix (`vod`)
        let mix_menu = |id: &'static str, current: String, none: &'static str, vod: bool| {
            let (me, mixes) = (me.clone(), mixes.clone());
            Button::new(id).label(current).small().outline().w_full().dropdown_caret(true).dropdown_menu(move |mut menu, _, _| {
                let pick = |mix: Option<Id>| {
                    let me = me.clone();
                    move |_: &ClickEvent, _: &mut Window, cx: &mut App| {
                        if let Some(me) = me.upgrade() {
                            me.update(cx, |this, cx| {
                                if let Some(Setup::Edit(e)) = &mut this.setup {
                                    if vod {
                                        e.vod_mix = mix;
                                    } else {
                                        e.audio_mix = mix;
                                    }
                                }
                                cx.notify();
                            });
                        }
                    }
                };
                menu = menu.item(PopupMenuItem::new(none).on_click(pick(None)));
                if !mixes.is_empty() {
                    menu = menu.separator().item(PopupMenuItem::label("Mixes"));
                }
                for (mid, name, usable) in mixes.iter().cloned() {
                    menu = menu.item(PopupMenuItem::new(name).disabled(!usable).on_click(pick(Some(mid))));
                }
                menu
            })
        };
        let audio_row = mix_menu("edit-audio-mix", mix_name(e.audio_mix, "Master"), "Master", false);
        // (the editor's platform: the saved one, or the one its server address belongs to)
        let twitch = p.id == "twitch";
        let vod_row = twitch.then(|| mix_menu("edit-vod-mix", mix_name(e.vod_mix, "Off"), "Off", true));

        // Quality: Automatic, or its own.
        let destination = workspace.outputs.destinations.iter().find(|d| d.id == e.id).cloned();
        let auto = destination.as_ref().map(|d| crate::encoding::automatic(workspace, d)).unwrap_or_default();
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
        let mut quality = card("Quality").child(
            div()
                .flex()
                .items_center()
                .justify_between()
                .gap_3()
                .child(div().flex_1().min_w_0().flex().flex_col().child(div().text_size(px(13.)).child("Automatic")).child(
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
            let mut encoders = seg();
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
                    d.child(div().flex_1().min_w_0().text_size(px(11.)).text_color(tok::warn()).child(format!(
                        "{} takes up to {} Mbps",
                        p.name,
                        m / 1000
                    )))
                });
            let mut heights = seg();
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
            let mut keys = seg();
            for (i, k) in [1, 2, 4].into_iter().enumerate() {
                keys = keys.child(
                    chip(("edit-keyframe", i), format!("{k} s"), enc.keyframe_s == k, true)
                        .on_click(set(Box::new(move |e| e.keyframe_s = k))),
                );
            }
            let mut audio = seg();
            for (i, k) in [96, 128, 160, 320].into_iter().enumerate() {
                audio = audio.child(
                    chip(("edit-audio", i), format!("{k} kbps"), enc.audio_kbps == k, true)
                        .on_click(set(Box::new(move |e| e.audio_kbps = k))),
                );
            }
            let hevc = enc.encoder == VideoEncoder::Hevc;
            quality = quality
                .child(div().h(px(1.)).bg(tok::border()))
                .child(row(
                    "Encoder",
                    Some("H.264 plays everywhere; HEVC streams to YouTube (most other platforms need H.264); x264 encodes on the CPU."),
                    div().flex().items_center().gap_2().child(encoders).when(hevc, |d| {
                        d.child(div().text_size(px(11.)).text_color(tok::warn()).child("YouTube only"))
                    }).into_any_element(),
                ))
                .child(row("Bitrate", None, bitrate.into_any_element()))
                .child(row("Resolution", None, heights.into_any_element()))
                .child(row("Keyframes", Some("How often a full picture is sent: 2 s is what most platforms ask for."), keys.into_any_element()))
                .child(row("Audio bitrate", None, audio.into_any_element()));
        }
        let id = e.id;
        let connection = card("Connection")
            .child(server)
            .child(row("Stream key", None, div().flex_1().child(Input::new(&e.key).small().mask_toggle()).into_any_element()));
        let picture_sound = card("Picture & sound")
            .child(row("Canvas", None, shapes.into_any_element()))
            .child(row(
                "Audio",
                Some("What it hears: the master (everything on air), or a mix built from sends, such as one without the music."),
                div().flex_1().child(audio_row).into_any_element(),
            ))
            .when_some(vod_row, |d, vod| {
                d.child(row(
                    "VOD track",
                    Some("A second track Twitch keeps in your VODs and clips instead of the stream's sound: live viewers hear the stream, the VOD gets, say, a mix without copyrighted music."),
                    div().flex_1().child(vod).into_any_element(),
                ))
            });
        div()
            .flex()
            .flex_col()
            .gap_3()
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_3()
                    .pb_1()
                    .child(badge(p, 36.))
                    .child(div().flex_1().child(Input::new(&e.name).small()))
                    .child(
                        ui::icon_button("edit-remove", "trash", tok::text_secondary())
                            .tooltip(ui::tip("Remove this destination"))
                            .on_click(cx.listener(move |this, _, _, cx| this.remove_destination(id, cx))),
                    ),
            )
            .child(connection)
            .child(picture_sound)
            .child(quality)
            .when_some(e.error.clone(), |d, err| d.child(div().text_size(px(12.)).text_color(tok::error()).child(err)))
            .child(
                div()
                    .flex()
                    .justify_end()
                    .gap_2()
                    .pt_1()
                    .child(Button::new("edit-cancel").label("Cancel").small().ghost().on_click(cx.listener(
                        |this, _, _, cx| {
                            this.setup = None;
                            cx.notify();
                        },
                    )))
                    .child(
                        Button::new("edit-save")
                            .label("Save")
                            .small()
                            .primary()
                            .on_click(cx.listener(|this, _, _, cx| this.save_edit(cx))),
                    ),
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

    /// Whether the destination dialog (adding or editing) is open.
    pub fn dialog_open(&self) -> bool {
        self.setup.is_some()
    }

    /// The Add destination dialog, drawn over the window while open.
    pub fn dialog(&mut self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let workspace = self.studio.read(cx).workspace().clone();
        // The vertical canvas asked for has arrived: send it.
        if let Some(Setup::Configure(c)) = &mut self.setup
            && c.want_vertical
            && let Some(f) = workspace.canvases.iter().find(|f| f.height > f.width)
        {
            c.canvas = Some(f.id);
            c.want_vertical = false;
        }
        let body = match self.setup.as_ref()? {
            Setup::Pick => self.picker(cx),
            Setup::Configure(c) => self.configure(c, &workspace, cx),
            Setup::Edit(e) => self.edit_view(e, &workspace, cx),
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
                // a sheet: nothing behind it takes the mouse (scrolling here never pans the canvas)
                .occlude()
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

    /// A platform's details: server, key, the canvas it's sent.
    fn configure(&self, c: &Configure, workspace: &Workspace, cx: &mut Context<Self>) -> Div {
        let p = c.platform;
        let me = cx.entity().downgrade();
        // Server: a choice where the platform has a list, otherwise pasted.
        let server = match p.servers.len() {
            0 => form_row("Server URL", None, div().flex_1().child(Input::new(&c.server_input).small())),
            1 => form_row("Server", None, div().text_size(px(12.)).text_color(tok::text_tertiary()).child(host_of(p.servers[0].1))),
            _ => {
                let current = p.servers[c.server.min(p.servers.len() - 1)].0;
                let selected = c.server;
                form_row(
                    "Server",
                    None,
                    div().flex_1().child(Button::new("server-choice").label(current).small().outline().w_full().dropdown_caret(true).dropdown_menu(
                        move |mut menu, _, _| {
                            for (i, (label, _)) in p.servers.iter().enumerate() {
                                let me = me.clone();
                                menu = menu.item(PopupMenuItem::new(*label).checked(i == selected).on_click(move |_, _, cx| {
                                    if let Some(me) = me.upgrade() {
                                        me.update(cx, |this, cx| {
                                            if let Some(Setup::Configure(c)) = &mut this.setup {
                                                c.server = i;
                                            }
                                            cx.notify();
                                        });
                                    }
                                }));
                            }
                            menu.max_h(px(320.)).scrollable(true)
                        },
                    )),
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
        let key = form_row(
            if p.id == "custom" { "Key or token" } else { "Stream key" },
            Some("Kept in your Keychain, never in the workspace's files.".into()),
            div().flex_1().child(Input::new(&c.key).small().mask_toggle()),
        );
        // Canvas: the workspace's canvases, the ones the platform takes marked with a dot.
        let mut shapes = segments();
        for (i, f) in workspace.canvases.iter().enumerate() {
            let id = (f.id != workspace.primary_canvas().id).then_some(f.id);
            let fits = match p.shape {
                Shape::Vertical => f.height > f.width,
                Shape::Landscape => f.width >= f.height,
            };
            shapes = shapes.child(
                segment(("dest-shape", i), format!("{} {}", f.name, aspect_label(f)), c.canvas == id, true)
                    .when(fits && p.id != "custom", |d| d.child(div().size(px(5.)).rounded_full().bg(tok::ok())))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if let Some(Setup::Configure(c)) = &mut this.setup {
                            c.canvas = id;
                        }
                        cx.notify();
                    })),
            );
        }
        let shape_tip: Option<SharedString> = (p.id != "custom").then(|| {
            let shape = if p.shape == Shape::Vertical { "vertical" } else { "landscape" };
            let rate = p.max_kbps.map_or(String::new(), |k| format!(" It takes up to {} Mbps.", k / 1000));
            format!("{} wants {shape} video: the canvases that fit have a green dot.{rate}", p.name).into()
        });
        let no_vertical = p.shape == Shape::Vertical && !workspace.canvases.iter().any(|f| f.height > f.width);
        let picture = form_card("Picture").child(form_row("Canvas", shape_tip, shapes)).when(no_vertical, |d| {
            d.child(
                div()
                    .flex()
                    .items_center()
                    .gap_3()
                    .p_2p5()
                    .rounded(px(8.))
                    .bg(tok::bg_app())
                    .child(ui::icon("smartphone").size(px(14.)).text_color(tok::warn()))
                    .child(div().flex_1().min_w_0().text_size(px(12.)).text_color(tok::text_secondary()).child(format!(
                        "{} wants vertical video, and this workspace has no vertical canvas yet.",
                        p.name
                    )))
                    .child(
                        Button::new("add-vertical")
                            .flex_none()
                            .label("Add vertical canvas")
                            .xsmall()
                            .outline()
                            .on_click(cx.listener(|this, _, _, cx| this.request_vertical(cx))),
                    ),
            )
        });
        div()
            .flex()
            .flex_col()
            .gap_3()
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_3()
                    .pb_1()
                    .child(badge(p, 40.))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .gap_0p5()
                            .child(div().text_size(px(16.)).font_weight(FontWeight::SEMIBOLD).child(p.name))
                            .child(div().text_size(px(12.)).text_color(tok::text_tertiary()).child(p.help)),
                    )
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
            .child(
                form_card("Connection")
                    .child(server)
                    .child(key)
                    .children(key_link.map(|l| div().flex().pl(px(120.)).child(l))),
            )
            .child(picture)
            .when_some(c.error.clone(), |d, e| d.child(div().text_size(px(12.)).text_color(tok::error()).child(e)))
            .child(
                div()
                    .flex()
                    .justify_end()
                    .gap_2()
                    .pt_1()
                    .child(Button::new("dest-cancel").label("Cancel").small().ghost().on_click(cx.listener(
                        |this, _, _, cx| {
                            this.setup = None;
                            cx.notify();
                        },
                    )))
                    .child(
                        Button::new("dest-save")
                            .label(format!("Add {}", p.name))
                            .small()
                            .primary()
                            .on_click(cx.listener(|this, _, _, cx| this.save(cx))),
                    ),
            )
    }
}

impl Render for OutputsPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        window.request_animation_frame();
        let studio = self.studio.read(cx);
        let workspace = studio.workspace().clone();
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
        let settings = workspace.outputs.recording.clone();
        let targets = settings.targets(&workspace);
        let several = workspace.canvases.len() > 1;
        let container = settings.container.to_uppercase();
        let folder_name = folder.file_name().map(|f| f.to_string_lossy().into_owned()).unwrap_or_default();
        let mut recordings = div().flex().flex_col().gap_0p5();
        for (i, f) in workspace.canvases.iter().enumerate() {
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
                        // while recording: the file's size so far, growing
                        if running && st.bytes > 0 {
                            format!("{container} · {}", crate::storage::bytes(st.bytes, true))
                        } else {
                            format!("{container} · {folder_name}")
                        },
                    ))
                    .child(status(state))
                    .when(several, |d| {
                        let s2 = settings.clone();
                        let p2 = workspace.clone();
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
                                    let settings = s2.with_canvas(&p2, fid, !on);
                                    this.studio.update(cx, |s, cx| {
                                        let _ = s.execute(Command::SetRecordingSettings { settings }, cx);
                                    })
                                })),
                        )
                    }),
            );
        }
        // The recording drive: free space (moving live while recording) and, once it can tell,
        // how long it lasts at the rate it's going down.
        let recording = workspace.canvases.iter().any(|f| runtime().get(OutputKey::Recording(f.id)).state.is_running());
        self.disk.poll(&folder, recording, Instant::now(), lightair_platform::files::space);
        let storage = self.disk.space.map(|(free, total)| {
            let left = self.disk.time_left().filter(|_| recording);
            let low = |warn_secs: u64, warn_bytes: u64| {
                left.is_some_and(|d| d.as_secs() < warn_secs) || (recording && free < warn_bytes)
            };
            let colour = if low(5 * 60, 2_000_000_000) {
                tok::error()
            } else if low(30 * 60, 10_000_000_000) {
                tok::warn()
            } else if recording {
                tok::accent_primary()
            } else {
                tok::text_tertiary()
            };
            let used = if total > 0 { 1.0 - free as f32 / total as f32 } else { 0.0 };
            let detail = match left {
                Some(d) => crate::storage::left(d),
                None => format!("of {}", crate::storage::bytes(total, false)),
            };
            let tip = format!("Recordings are saved to {}", folder.display());
            div()
                .id("recording-storage")
                .mx_2()
                .px_2()
                .py_1p5()
                .flex()
                .flex_col()
                .gap_1()
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap_1p5()
                        .text_size(px(11.))
                        .child(ui::icon("hard-drive").size(px(12.)).text_color(tok::text_tertiary()))
                        .child(
                            div()
                                .when(recording, |d| d.font_family("Menlo"))
                                .text_color(if recording { colour } else { tok::text_secondary() })
                                .child(format!("{} free", crate::storage::bytes(free, recording))),
                        )
                        .child(div().flex_1())
                        .child(div().text_color(if left.is_some() { colour } else { tok::text_tertiary() }).child(detail)),
                )
                .child(
                    div()
                        .h(px(3.))
                        .w_full()
                        .rounded_full()
                        .bg(tok::border())
                        .child(div().h_full().rounded_full().bg(colour).w(relative(used.clamp(0.0, 1.0)))),
                )
                .tooltip(ui::tip(tip))
        });
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
            let live = workspace
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
        for (i, d) in workspace.outputs.destinations.iter().enumerate() {
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
                .unwrap_or(platforms::custom());
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
                    .child(canvas_badge(("dest-canvas", i), &workspace, d.canvas).on_click(cx.listener({
                        let workspace = workspace.clone();
                        let current = d.canvas;
                        move |this, _, _, cx| {
                            cx.stop_propagation();
                            let canvas = next_canvas(&workspace, current);
                            this.studio.update(cx, |s, cx| {
                                let _ = s.execute(Command::SetDestinationCanvas { destination: id, canvas }, cx);
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
            .children(storage)
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
impl EventEmitter<AddVerticalCanvas> for OutputsPanel {}
impl EventEmitter<OpenRecordingSettings> for OutputsPanel {}

impl Focusable for OutputsPanel {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}
