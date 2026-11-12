//! The Outputs panel (docs/11-outputs-streaming-recording.md): recording status and
//! streaming destinations, with a form to add a custom RTMP(S) destination.

use std::time::Instant;

use gpui_kit::component::Sizable;
use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::menu::ContextMenuExt;
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;
use lightair_core::{Command, Destination, Id};
use lightair_engine::outputs::{OutputKey, OutputState, StreamProtocol, runtime};

use crate::studio::Studio;
use crate::ui::{self, tok};

struct NewDestination {
    server: Entity<InputState>,
    key: Entity<InputState>,
}

pub struct OutputsPanel {
    studio: Entity<Studio>,
    form: Option<NewDestination>,
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
fn aspect_label(f: &lightair_core::Format) -> String {
    fn gcd(a: u32, b: u32) -> u32 {
        if b == 0 { a } else { gcd(b, a % b) }
    }
    let g = gcd(f.width, f.height).max(1);
    format!("{}:{}", f.width / g, f.height / g)
}

/// The format after `current` in the project's list (wrapping), for the badge's click.
fn next_format(project: &lightair_core::Project, current: Option<Id>) -> Option<Id> {
    let cur = current.unwrap_or(project.primary_format().id);
    let i = project.formats.iter().position(|f| f.id == cur).unwrap_or(0);
    let next = &project.formats[(i + 1) % project.formats.len()];
    (next.id != project.primary_format().id).then_some(next.id)
}

fn format_badge(id: impl Into<ElementId>, project: &lightair_core::Project, format: Option<Id>) -> Stateful<Div> {
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
        .text_color(tok::text_secondary())
        .when(project.formats.len() > 1, |d| d.cursor_pointer().hover(|d| d.bg(tok::bg_selected())))
        .child(aspect_label(f))
}

impl OutputsPanel {
    pub fn new(studio: Entity<Studio>, cx: &mut Context<Self>) -> Self {
        cx.observe(&studio, |_, _, cx| cx.notify()).detach();
        Self { studio, form: None, focus: cx.focus_handle() }
    }

    fn open_form(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let server = cx.new(|cx| InputState::new(window, cx).placeholder("rtmp://…, srt://… or https://… (WHIP)"));
        let key =
            cx.new(|cx| InputState::new(window, cx).placeholder("Stream key, token or SRT stream ID").masked(true));
        self.form = Some(NewDestination { server, key });
        cx.notify();
    }

    fn save_form(&mut self, cx: &mut Context<Self>) {
        let Some(form) = &self.form else { return };
        let server = form.server.read(cx).value().trim().to_string();
        let key = form.key.read(cx).value().trim().to_string();
        // RTMP and WHIP need a key (stream key, bearer token); for SRT it's an optional stream ID.
        let protocol = StreamProtocol::of(&server);
        let needs_key = matches!(protocol, Some(StreamProtocol::Rtmp | StreamProtocol::Whip));
        if protocol.is_none() || (needs_key && key.is_empty()) {
            self.studio.update(cx, |s, cx| {
                s.issue = Some(
                    "Enter an rtmp(s)://, srt://, rist:// or https:// (WHIP) address, and its stream key or token."
                        .into(),
                );
                cx.notify();
            });
            return;
        }
        let destination = Destination { id: Id::new(), name: host_of(&server), server, enabled: true, format: None };
        if let Err(e) = crate::secrets::set_stream_key(destination.id, &key) {
            self.studio.update(cx, |s, cx| {
                s.issue = Some(format!("Couldn't save the stream key: {e}").into());
                cx.notify();
            });
            return;
        }
        self.studio.update(cx, |s, cx| {
            let _ = s.execute(Command::AddDestination { destination }, cx);
        });
        self.form = None;
        cx.notify();
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

        let row = |id: ElementId, icon: &'static str, title: String, subtitle: String, status: (String, Hsla)| {
            div()
                .id(id)
                .flex()
                .items_center()
                .gap_2()
                .min_h(px(40.))
                .mx_2()
                .px_2()
                .rounded(px(6.))
                .hover(|d| d.bg(tok::bg_raised()))
                .child(ui::icon(icon).size(px(15.)).text_color(tok::text_secondary()))
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .flex_1()
                        .min_w_0()
                        .child(div().overflow_hidden().text_ellipsis().child(title))
                        .child(
                            div()
                                .text_size(px(11.))
                                .text_color(tok::text_tertiary())
                                .overflow_hidden()
                                .text_ellipsis()
                                .child(subtitle),
                        ),
                )
                .child(div().text_size(px(11.)).font_family("Menlo").text_color(status.1).child(status.0))
        };

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
        let mut list = div().flex().flex_col().gap_0p5().child(
            row(
                "out-rec".into(),
                "circle",
                "Recording".into(),
                format!(
                    "{} · {}",
                    project.outputs.recording.container.to_uppercase(),
                    folder.file_name().map(|f| f.to_string_lossy().into_owned()).unwrap_or_default()
                ),
                (rec_text, rec_color),
            )
            .child(rec_badge),
        );
        // While live: how many encoders the destinations need (identical ones share).
        if runtime().any_stream_running() {
            let live = project
                .outputs
                .destinations
                .iter()
                .filter(|d| runtime().get(OutputKey::Stream(d.id)).state.is_running())
                .count();
            let encoders = studio.engine().call(|s| s.outputs.stream_encoder_count());
            list = list.child(div().mx_4().text_size(px(11.)).text_color(tok::text_tertiary()).child(format!(
                "{live} destination{} · {encoders} encoder{}",
                if live == 1 { "" } else { "s" },
                if encoders == 1 { "" } else { "s" }
            )));
        }
        for (i, d) in project.outputs.destinations.iter().enumerate() {
            let id = d.id;
            let enabled = d.enabled;
            let st = runtime().get(OutputKey::Stream(id));
            let (text, color) = if enabled || st.state.is_running() {
                status_text(&st.state)
            } else {
                ("Skipped".into(), tok::text_tertiary())
            };
            list = list.child(
                row(
                    ("out-dest", i).into(),
                    "radio",
                    d.name.clone(),
                    format!("{} · {}", StreamProtocol::label(&d.server), d.server),
                    (text, color),
                )
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
                .child(
                    div()
                        .id(("dest-toggle", i))
                        .w(px(30.))
                        .h(px(18.))
                        .rounded_full()
                        .cursor_pointer()
                        .bg(if enabled { tok::interactive() } else { tok::border_strong() })
                        .child(
                            div()
                                .size(px(14.))
                                .m(px(2.))
                                .rounded_full()
                                .bg(tok::bg_app())
                                .when(enabled, |d| d.ml(px(14.))),
                        )
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.studio.update(cx, |s, cx| {
                                let _ = s
                                    .execute(Command::SetDestinationEnabled { destination: id, enabled: !enabled }, cx);
                            })
                        })),
                )
                .child(ui::icon_button(("dest-remove", i), "trash", tok::text_tertiary()).on_click(cx.listener(
                    move |this, _, _, cx| {
                        crate::secrets::delete_stream_key(id);
                        this.studio.update(cx, |s, cx| {
                            let _ = s.execute(Command::RemoveDestination { destination: id }, cx);
                        })
                    },
                )))
                .context_menu({
                    let studio = self.studio.downgrade();
                    move |menu, _, _| {
                        use gpui_kit::component::menu::PopupMenuItem;
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
                            PopupMenuItem::new(if enabled { "Skip when going live" } else { "Use when going live" })
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

        let form = self.form.as_ref().map(|f| {
            div()
                .flex()
                .flex_col()
                .gap_2()
                .m_2()
                .p_2()
                .rounded(px(10.))
                .bg(tok::bg_raised())
                .child(div().font_weight(FontWeight::SEMIBOLD).child("Custom RTMP destination"))
                .child(Input::new(&f.server).small())
                .child(Input::new(&f.key).small().mask_toggle())
                .child(div().text_size(px(11.)).text_color(tok::text_tertiary()).child("Saved in your Keychain"))
                .child(
                    div()
                        .flex()
                        .gap_2()
                        .child(
                            Button::new("dest-save")
                                .label("Add")
                                .small()
                                .primary()
                                .on_click(cx.listener(|this, _, _, cx| this.save_form(cx))),
                        )
                        .child(Button::new("dest-cancel").label("Cancel").small().ghost().on_click(cx.listener(
                            |this, _, _, cx| {
                                this.form = None;
                                cx.notify();
                            },
                        ))),
                )
        });

        div()
            .flex()
            .flex_col()
            .pb_2()
            .pt_1()
            .child(list)
            .children(form)
            .when(project.outputs.destinations.is_empty() && self.form.is_none(), |d| {
                d.child(
                    div().px_4().pt_1().text_size(px(11.)).text_color(tok::text_tertiary()).child("No destinations"),
                )
            })
            .child(div().px_4().pt_2().flex().gap_2().child(
                Button::new("open-folder").label("Show recordings").xsmall().ghost().on_click(move |_, _, cx| {
                    let _ = std::fs::create_dir_all(&folder);
                    cx.open_url(&format!("file://{}", folder.to_string_lossy()))
                }),
            ))
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
                .on_click(cx.listener(|this, _, window, cx| this.open_form(window, cx))),
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

impl Focusable for OutputsPanel {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}
