//! The Outputs panel (docs/11-outputs-streaming-recording.md): recording status and
//! streaming destinations, with a form to add a custom RTMP(S) destination.

use std::time::Instant;

use gpui_kit::component::Sizable;
use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;
use lightair_core::{Command, Destination, Id};
use lightair_engine::outputs::{OutputKey, OutputState, runtime};

use crate::studio::Studio;
use crate::ui::{self, tok};

struct NewDestination {
    server: Entity<InputState>,
    key: Entity<InputState>,
}

pub struct OutputsPanel {
    studio: Entity<Studio>,
    form: Option<NewDestination>,
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

impl OutputsPanel {
    pub fn new(studio: Entity<Studio>, cx: &mut Context<Self>) -> Self {
        cx.observe(&studio, |_, _, cx| cx.notify()).detach();
        Self { studio, form: None }
    }

    fn open_form(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let server = cx.new(|cx| InputState::new(window, cx).placeholder("rtmp://server/app"));
        let key = cx.new(|cx| InputState::new(window, cx).placeholder("Stream key").masked(true));
        self.form = Some(NewDestination { server, key });
        cx.notify();
    }

    fn save_form(&mut self, cx: &mut Context<Self>) {
        let Some(form) = &self.form else { return };
        let server = form.server.read(cx).value().trim().to_string();
        let key = form.key.read(cx).value().trim().to_string();
        if !(server.starts_with("rtmp://") || server.starts_with("rtmps://")) || key.is_empty() {
            self.studio.update(cx, |s, cx| {
                s.issue = Some("Enter an rtmp:// or rtmps:// server and a stream key.".into());
                cx.notify();
            });
            return;
        }
        let destination = Destination { id: Id::new(), name: host_of(&server), server, enabled: true };
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

        let mut list = div().flex().flex_col().gap_0p5().child(row(
            "out-rec".into(),
            "circle",
            "Recording".into(),
            format!(
                "{} · {}",
                project.outputs.recording.container.to_uppercase(),
                folder.file_name().map(|f| f.to_string_lossy().into_owned()).unwrap_or_default()
            ),
            (rec_text, rec_color),
        ));
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
                row(("out-dest", i).into(), "radio", d.name.clone(), d.server.clone(), (text, color))
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
                                    let _ = s.execute(
                                        Command::SetDestinationEnabled { destination: id, enabled: !enabled },
                                        cx,
                                    );
                                })
                            })),
                    )
                    .child(ui::icon_button(("dest-remove", i), "trash-2", tok::text_tertiary()).on_click(cx.listener(
                        move |this, _, _, cx| {
                            crate::secrets::delete_stream_key(id);
                            this.studio.update(cx, |s, cx| {
                                let _ = s.execute(Command::RemoveDestination { destination: id }, cx);
                            })
                        },
                    ))),
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
                .child(
                    div()
                        .text_size(px(11.))
                        .text_color(tok::text_tertiary())
                        .child("The stream key is stored in your Keychain."),
                )
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

        let add = ui::icon_button("add-destination", "plus", tok::text_secondary())
            .on_click(cx.listener(|this, _, window, cx| this.open_form(window, cx)));
        div()
            .flex()
            .flex_col()
            .pb_2()
            .child(ui::panel_header("Outputs", Some(add.into_any_element())))
            .child(list)
            .children(form)
            .when(project.outputs.destinations.is_empty() && self.form.is_none(), |d| {
                d.child(
                    div()
                        .px_4()
                        .pt_1()
                        .text_size(px(11.))
                        .text_color(tok::text_tertiary())
                        .child("Add a streaming destination with +. Platform sign-in arrives in a later milestone."),
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
