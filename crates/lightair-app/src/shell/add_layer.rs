//! The Add Layer picker (docs/05-interface-shell.md, "Layers"): searchable, grouped layer types
//! with a line each, a second step for choosing a camera, and sources already in the show to
//! reuse in this scene.

use gpui_kit::component::input::{Input as TextInput, InputEvent, InputState};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;
use lightair_core::{Command, Id, Layer, Layout};

use super::StudioWindow;
use crate::ui::{self, tok};

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Step {
    Types,
    Cameras,
}

pub(super) struct AddLayer {
    step: Step,
    search: Entity<InputState>,
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
}

/// (kind, group, icon, name, what it is)
const KINDS: &[(Kind, &str, &str, &str, &str)] = &[
    (Kind::Camera, "Capture", "camera", "Camera", "A webcam, capture card or phone"),
    (Kind::Display, "Capture", "monitor", "Display", "Everything on a screen"),
    (Kind::Image, "Media", "image", "Image", "A picture or logo"),
    (Kind::Video, "Media", "clapperboard", "Video", "A video file, looped"),
    (Kind::Text, "Graphics", "type", "Text", "A title or caption"),
    (Kind::Colour, "Graphics", "square", "Colour", "A solid colour block"),
    (Kind::Timer, "Graphics", "timer", "Timer", "Countdown, stopwatch or clock"),
];

fn matches(query: &str, words: &[&str]) -> bool {
    let q = query.trim().to_lowercase();
    q.is_empty() || words.iter().any(|w| w.to_lowercase().contains(&q))
}

impl StudioWindow {
    pub(super) fn open_add_layer(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.add_layer.take().is_some() {
            cx.notify();
            return;
        }
        let search = cx.new(|cx| InputState::new(window, cx).placeholder("Search object types and sources"));
        search.update(cx, |s, cx| s.focus(window, cx));
        let events = cx.subscribe_in(&search, window, |_, _, ev: &InputEvent, _, cx| {
            if matches!(ev, InputEvent::Change) {
                cx.notify();
            }
        });
        self.add_layer = Some(AddLayer { step: Step::Types, search, _events: events });
        cx.notify();
    }

    pub(super) fn close_add_layer(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.add_layer = None;
        window.focus(&self.focus, cx);
        cx.notify();
    }

    fn pick(&mut self, kind: Kind, window: &mut Window, cx: &mut Context<Self>) {
        match kind {
            Kind::Camera => {
                self.camera_devices = self.studio.read(cx).engine().call(|_| lightair_engine::kinds::cameras());
                if let Some(a) = &mut self.add_layer {
                    a.step = Step::Cameras;
                }
                cx.notify();
                return;
            }
            Kind::Display => {
                self.add_input_layer("Display", lightair_core::kinds::SCREEN, super::display_settings(), (1920.0, 1080.0), cx)
            }
            Kind::Image => self.add_image(window, cx),
            Kind::Video => self.add_video(window, cx),
            Kind::Text => self.add_text(cx),
            Kind::Colour => self.add_colour(cx),
            Kind::Timer => self.add_timer(cx),
        }
        self.close_add_layer(window, cx);
    }

    /// Reuses an input already in the show as a new layer in this scene.
    fn add_existing(&mut self, input: Id, window: &mut Window, cx: &mut Context<Self>) {
        let studio = self.studio.read(cx);
        let workspace = studio.workspace().clone();
        let Some(scene) = studio.selected_scene else { return };
        let Some(i) = workspace.inputs.get(&input) else { return };
        let size = studio.engine().call(move |s| s.reconciler.input_size(input)).unwrap_or((1280, 720));
        let (w, h) = (size.0 as f32, size.1 as f32);
        let layouts: Vec<(Id, Layout)> = workspace
            .canvases
            .iter()
            .map(|f| {
                let s = (f.width as f32 / w).min(f.height as f32 / h).min(1.0);
                let (lw, lh) = (w * s, h * s);
                (f.id, Layout::boxed((f.width as f32 - lw) / 2.0, (f.height as f32 - lh) / 2.0, lw, lh))
            })
            .collect();
        let layer = Layer::new(i.name.clone(), input, layouts);
        let id = layer.id;
        self.exec(Command::AddLayer { scene, layer, index: None }, cx);
        self.select_layer(Some(id), cx);
        self.close_add_layer(window, cx);
    }

    pub(super) fn add_layer_overlay(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let state = self.add_layer.as_ref()?;
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

        let mut body =
            div().id("add-layer-body").flex().flex_col().flex_shrink(1.).min_h_0().overflow_y_scroll().px_2();
        match state.step {
            Step::Types => {
                for group in ["Capture", "Media", "Graphics"] {
                    let kinds: Vec<_> =
                        KINDS.iter().filter(|k| k.1 == group && matches(&query, &[k.3, k.4, k.1])).collect();
                    if kinds.is_empty() {
                        continue;
                    }
                    let mut grid = div().grid().grid_cols(2).gap_1();
                    for (i, (kind, _, icon, name, hint)) in kinds.into_iter().enumerate() {
                        let kind = *kind;
                        grid = grid.child(
                            card((group, i).into(), icon, name.to_string(), hint.to_string())
                                .on_click(cx.listener(move |this, _, window, cx| this.pick(kind, window, cx))),
                        );
                    }
                    body = body.child(heading(group)).child(grid);
                }
                // Sources already in the show but not in this scene: reuse them.
                let in_scene: Vec<Id> = scene.iter().flat_map(|s| s.layers.iter().filter_map(|l| l.input)).collect();
                let audio_only = lightair_core::kinds::is_audio;
                let existing: Vec<_> = workspace
                    .inputs
                    .values()
                    .filter(|i| !in_scene.contains(&i.id) && !audio_only(&i.kind))
                    .filter(|i| workspace.scenes.iter().any(|s| s.layers.iter().any(|l| l.input == Some(i.id))))
                    .filter(|i| matches(&query, &[&i.name, &i.kind]))
                    .cloned()
                    .collect();
                if !existing.is_empty() {
                    let mut grid = div().grid().grid_cols(2).gap_1();
                    for (i, input) in existing.into_iter().enumerate() {
                        let id = input.id;
                        grid = grid.child(
                            card(
                                ("existing", i).into(),
                                ui::input_icon(&input.kind),
                                input.name.clone(),
                                "Already in the show".into(),
                            )
                            .on_click(cx.listener(move |this, _, window, cx| this.add_existing(id, window, cx))),
                        );
                    }
                    body = body.child(heading("Already in the show")).child(grid);
                }
            }
            Step::Cameras => {
                body = body.child(
                    div()
                        .id("add-layer-back")
                        .flex()
                        .items_center()
                        .gap_1()
                        .px_2()
                        .py_1()
                        .cursor_pointer()
                        .text_color(tok::text_secondary())
                        .hover(|d| d.text_color(tok::text()))
                        .child(ui::icon("chevron-left").size(px(14.)))
                        .child("Object types")
                        .on_click(cx.listener(|this, _, _, cx| {
                            if let Some(a) = &mut this.add_layer {
                                a.step = Step::Types;
                            }
                            cx.notify();
                        })),
                );
                let cams: Vec<_> =
                    self.camera_devices.iter().filter(|(label, _)| matches(&query, &[label])).cloned().collect();
                if cams.is_empty() {
                    body = body.child(div().p_3().text_color(tok::text_tertiary()).child("No cameras found"));
                }
                let mut grid = div().grid().grid_cols(2).gap_1();
                for (i, (label, uid)) in cams.into_iter().enumerate() {
                    let virtual_cam =
                        label.contains("Virtual") || label.contains("VCam") || label.contains("Extension");
                    let hint = if virtual_cam { "Virtual camera" } else { "Camera" };
                    grid = grid.child(card(("camera", i).into(), "camera", label.clone(), hint.into()).on_click(
                        cx.listener(move |this, _, window, cx| {
                            this.add_camera(&label, &uid, cx);
                            this.close_add_layer(window, cx);
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
                .on_mouse_down(MouseButton::Left, cx.listener(|this, _, window, cx| this.close_add_layer(window, cx)))
                .child(
                    div()
                        .id("add-layer")
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
                                this.close_add_layer(window, cx);
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
                        .child(TextInput::new(&state.search))
                        .child(body),
                )
                .into_any_element(),
        )
    }
}
