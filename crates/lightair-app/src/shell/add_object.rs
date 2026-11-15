//! The Add Object picker (docs/05-interface-shell.md, "Layers"): searchable object types as
//! tiles in groups (what each is, on hover), with a second step where a type needs one: the
//! camera, or a web page's address and size.

use gpui_kit::component::input::{Input as TextInput, InputEvent, InputState};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;
use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::Sizable;

use super::StudioWindow;
use crate::ui::{self, tok};

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Step {
    Types,
    Cameras,
    Web,
}

pub(super) struct AddObject {
    step: Step,
    search: Entity<InputState>,
    /// A web page's address, and its size (CSS pixels).
    web_url: Entity<InputState>,
    web_size: (u32, u32),
    _events: Subscription,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Camera,
    Display,
    Image,
    Video,
    Text,
    Colour,
    Timer,
    Web,
}

/// (kind, group, icon, name, what it is)
const KINDS: &[(Kind, &str, &str, &str, &str)] = &[
    (Kind::Camera, "Capture", "camera", "Camera", "A webcam, capture card or phone"),
    (Kind::Display, "Capture", "monitor", "Display", "Everything on a screen"),
    (Kind::Image, "Media", "image", "Image", "A picture or logo"),
    (Kind::Video, "Media", "clapperboard", "Video", "A video file, looped"),
    (Kind::Web, "Media", "globe", "Web page", "A website, an overlay or an alert widget, live"),
    (Kind::Text, "Graphics", "type", "Text", "A title or caption"),
    (Kind::Colour, "Graphics", "square", "Colour", "A solid colour block"),
    (Kind::Timer, "Graphics", "timer", "Timer", "Countdown, stopwatch or clock"),
];

fn matches(query: &str, words: &[&str]) -> bool {
    let q = query.trim().to_lowercase();
    q.is_empty() || words.iter().any(|w| w.to_lowercase().contains(&q))
}

impl StudioWindow {
    pub(super) fn open_add_object(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.add_object.take().is_some() {
            cx.notify();
            return;
        }
        let search = cx.new(|cx| InputState::new(window, cx).placeholder("Search"));
        let web_url = cx.new(|cx| InputState::new(window, cx).placeholder("https://…"));
        search.update(cx, |s, cx| s.focus(window, cx));
        let events = cx.subscribe_in(&search, window, |_, _, ev: &InputEvent, _, cx| {
            if matches!(ev, InputEvent::Change) {
                cx.notify();
            }
        });
        self.add_object = Some(AddObject { step: Step::Types, search, web_url, web_size: (1920, 1080), _events: events });
        cx.notify();
    }

    pub(super) fn close_add_object(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.add_object = None;
        window.focus(&self.focus, cx);
        cx.notify();
    }

    fn pick(&mut self, kind: Kind, window: &mut Window, cx: &mut Context<Self>) {
        match kind {
            Kind::Camera => {
                self.camera_devices = self.studio.read(cx).engine().call(|_| lightair_engine::kinds::cameras());
                if let Some(a) = &mut self.add_object {
                    a.step = Step::Cameras;
                }
                cx.notify();
                return;
            }
            Kind::Display => {
                self.add_source_object("Display", lightair_core::kinds::SCREEN, super::display_settings(), (1920.0, 1080.0), cx)
            }
            Kind::Image => self.add_image(window, cx),
            Kind::Video => self.add_video(window, cx),
            Kind::Text => self.add_text(cx),
            Kind::Colour => self.add_colour(cx),
            Kind::Timer => self.add_timer(cx),
            Kind::Web => {
                if let Some(a) = &mut self.add_object {
                    a.step = Step::Web;
                    a.web_url.update(cx, |i, cx| i.focus(window, cx));
                }
                cx.notify();
                return;
            }
        }
        self.close_add_object(window, cx);
    }

    /// Adds the web page typed in (an address without a scheme gets https://).
    fn add_web_page(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(a) = &self.add_object else { return };
        let typed = a.web_url.read(cx).value().trim().to_string();
        if typed.is_empty() {
            return;
        }
        let url = if typed.contains("://") || typed.starts_with("data:") { typed } else { format!("https://{typed}") };
        let (w, h) = a.web_size;
        let name = url.split("://").nth(1).and_then(|r| r.split('/').next()).filter(|h| !h.is_empty()).unwrap_or("Web page").to_string();
        let settings = serde_json::json!({"url": url, "width": w, "height": h, "fps": 30});
        self.add_source_object(&name, lightair_engine::web::KIND, settings, (w as f32, h as f32), cx);
        self.close_add_object(window, cx);
    }

    pub(super) fn add_object_overlay(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let state = self.add_object.as_ref()?;
        let query = state.search.read(cx).value().to_string();
        let studio = self.studio.read(cx);
        let workspace = studio.workspace().clone();
        let scene = studio.selected_scene.and_then(|id| workspace.scene(id)).cloned();

        let card = |id: ElementId, icon: &'static str, name: String, hint: String| {
            div()
                .id(id)
                .flex()
                .items_center()
                .gap_3()
                .p_2()
                .rounded(px(8.))
                .cursor_pointer()
                .hover(|d| d.bg(tok::bg_selected()))
                .child(
                    div()
                        .flex()
                        .items_center()
                        .justify_center()
                        .flex_none()
                        .size(px(36.))
                        .rounded(px(8.))
                        .bg(tok::bg_app())
                        .child(ui::icon(icon).size(px(18.)).text_color(tok::text())),
                )
                .child(
                    div().flex().flex_col().min_w_0().child(div().font_weight(FontWeight::MEDIUM).child(name)).child(
                        div()
                            .text_size(px(11.))
                            .text_color(tok::text_tertiary())
                            .overflow_hidden()
                            .text_ellipsis()
                            .child(hint),
                    ),
                )
        };
        // a type: its icon in a well, its name under it
        let tile = |id: ElementId, icon: &'static str, name: &'static str| {
            div()
                .id(id)
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .gap_2()
                .h(px(84.))
                .rounded(px(10.))
                .bg(tok::bg_app())
                .border_1()
                .border_color(tok::border())
                .cursor_pointer()
                .hover(|d| d.bg(tok::bg_selected()).border_color(tok::border_strong()))
                .child(ui::icon(icon).size(px(22.)).text_color(tok::text()))
                .child(div().text_size(px(12.)).text_color(tok::text_secondary()).child(name))
        };
        let heading = |text: &str| {
            div()
                .pt_2()
                .pb_1()
                .px_2()
                .text_size(px(11.))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(tok::text_tertiary())
                .child(text.to_uppercase())
        };

        // back to the types
        let back = |cx: &mut Context<Self>| {
            div()
                .id("add-object-back")
                .flex()
                .items_center()
                .gap_1()
                .px_2()
                .py_1()
                .cursor_pointer()
                .text_size(px(12.))
                .text_color(tok::text_secondary())
                .hover(|d| d.text_color(tok::text()))
                .child(ui::icon("chevron-left").size(px(14.)))
                .child("All types")
                .on_click(cx.listener(|this, _, _, cx| {
                    if let Some(a) = &mut this.add_object {
                        a.step = Step::Types;
                    }
                    cx.notify();
                }))
        };
        let mut body =
            div().id("add-object-body").flex().flex_col().flex_shrink(1.).min_h_0().overflow_y_scroll().px_2();
        match state.step {
            Step::Types => {
                for group in ["Capture", "Media", "Graphics"] {
                    let kinds: Vec<_> =
                        KINDS.iter().filter(|k| k.1 == group && matches(&query, &[k.3, k.4, k.1])).collect();
                    if kinds.is_empty() {
                        continue;
                    }
                    let mut grid = div().grid().grid_cols(3).gap_2();
                    for (i, (kind, _, icon, name, hint)) in kinds.into_iter().enumerate() {
                        let kind = *kind;
                        grid = grid.child(
                            tile((group, i).into(), icon, name)
                                .tooltip(ui::tip(*hint))
                                .on_click(cx.listener(move |this, _, window, cx| this.pick(kind, window, cx))),
                        );
                    }
                    body = body.child(heading(group)).child(grid);
                }
            }
            Step::Web => {
                let size = state.web_size;
                let mut sizes = div().flex().flex_wrap().gap(px(2.)).p(px(2.)).rounded(px(7.)).bg(tok::bg_app());
                for (i, (label, wh)) in [("Landscape 1920×1080", (1920u32, 1080u32)), ("1280×720", (1280, 720)), ("Vertical 1080×1920", (1080, 1920)), ("Square 1080×1080", (1080, 1080))]
                    .into_iter()
                    .enumerate()
                {
                    let on = size == wh;
                    sizes = sizes.child(
                        div()
                            .id(("web-size", i))
                            .px_2p5()
                            .h(px(26.))
                            .flex()
                            .items_center()
                            .rounded(px(5.))
                            .text_size(px(12.))
                            .cursor_pointer()
                            .when(on, |d| d.bg(tok::bg_selected()).text_color(tok::text()).font_weight(FontWeight::MEDIUM))
                            .when(!on, |d| d.text_color(tok::text_secondary()).hover(|d| d.text_color(tok::text())))
                            .child(label)
                            .on_click(cx.listener(move |this, _, _, cx| {
                                if let Some(a) = &mut this.add_object {
                                    a.web_size = wh;
                                }
                                cx.notify();
                            })),
                    );
                }
                body = body
                    .child(back(cx))
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_3()
                            .p_2()
                            .child(
                                div()
                                    .flex()
                                    .flex_col()
                                    .gap_1p5()
                                    .child(div().text_size(px(12.)).text_color(tok::text_secondary()).child("Address"))
                                    .child(
                                        div().on_key_down(cx.listener(|this, ev: &KeyDownEvent, window, cx| {
                                            if ev.keystroke.key == "enter" {
                                                this.add_web_page(window, cx);
                                            }
                                        }))
                                        .child(TextInput::new(&state.web_url)),
                                    ),
                            )
                            .child(
                                div()
                                    .flex()
                                    .flex_col()
                                    .gap_1p5()
                                    .child(div().text_size(px(12.)).text_color(tok::text_secondary()).child("Size"))
                                    .child(sizes),
                            )
                            .child(
                                div().flex().justify_end().child(
                                    Button::new("add-web-page")
                                        .label("Add web page")
                                        .small()
                                        .primary()
                                        .on_click(cx.listener(|this, _, window, cx| this.add_web_page(window, cx))),
                                ),
                            ),
                    );
            }
            Step::Cameras => {
                body = body.child(back(cx));
                let cams: Vec<_> =
                    self.camera_devices.iter().filter(|(label, _)| matches(&query, &[label])).cloned().collect();
                if cams.is_empty() {
                    body = body.child(div().p_3().text_color(tok::text_tertiary()).child("No cameras found"));
                }
                let mut grid = div().grid().grid_cols(2).gap_1();
                for (i, (label, uid)) in cams.into_iter().enumerate() {
                    // (by the device's transport type, not its name)
                    let virtual_cam = lightair_platform::devices::camera_is_virtual(&uid);
                    let hint = if virtual_cam { "Virtual camera" } else { "Camera" };
                    grid = grid.child(card(("camera", i).into(), "camera", label.clone(), hint.into()).on_click(
                        cx.listener(move |this, _, window, cx| {
                            this.add_camera(&label, &uid, cx);
                            this.close_add_object(window, cx);
                        }),
                    ));
                }
                body = body.child(grid);
            }
        }

        Some(
            div()
                .absolute()
                .inset_0()
                .flex()
                .justify_center()
                .items_start()
                .pt(px(80.))
                .bg(tok::scrim())
                // a sheet: nothing behind it takes the mouse (scrolling here never pans the canvas)
                .occlude()
                .on_mouse_down(MouseButton::Left, cx.listener(|this, _, window, cx| this.close_add_object(window, cx)))
                .child(
                    div()
                        .id("add-object")
                        .flex()
                        .flex_col()
                        .gap_2()
                        .w(px(560.))
                        .max_h(px(560.))
                        .p_3()
                        .rounded(px(14.))
                        .bg(tok::bg_panel())
                        .border_1()
                        .border_color(tok::border_strong())
                        .shadow_lg()
                        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                        .capture_key_down(cx.listener(|this, ev: &KeyDownEvent, window, cx| {
                            if ev.keystroke.key == "escape" {
                                this.close_add_object(window, cx);
                                cx.stop_propagation();
                            }
                        }))
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .justify_between()
                                .gap_3()
                                .child(div().flex_none().text_size(px(15.)).font_weight(FontWeight::SEMIBOLD).child(
                                    match state.step {
                                        Step::Types => "Add object",
                                        Step::Cameras => "Choose a camera",
                                        Step::Web => "Web page",
                                    },
                                ))
                                .when_some(scene.as_ref(), |d, s| {
                                    d.child(
                                        div()
                                            .min_w_0()
                                            .overflow_hidden()
                                            .text_ellipsis()
                                            .whitespace_nowrap()
                                            .text_size(px(11.))
                                            .text_color(tok::text_tertiary())
                                            .child(format!("to {}", s.name)),
                                    )
                                }),
                        )
                        .when(state.step != Step::Web, |d| d.child(TextInput::new(&state.search)))
                        .child(body),
                )
                .into_any_element(),
        )
    }
}
