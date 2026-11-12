//! The Inspector's building blocks (docs/05-interface-shell.md, "Inspector"): collapsible
//! sections, compact numeric fields, sliders, and the layer view (Transform, content, Effects,
//! Camera Studio, Formats).

use std::rc::Rc;

use gpui_kit::component::Sizable;
use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::slider::{Slider, SliderEvent, SliderState};
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;
use lightair_core::{Command, Fit, Format, Input, Layer, Layout, Project, Scene};

use super::StudioWindow;
use crate::camera_studio::{KIND as CAMERA_STUDIO, looks};
use crate::ui::{self, tok};

const HEADER_H: f32 = 34.0;

/// A slider bound to a document value: recreated when the value changes elsewhere.
pub(super) struct BoundSlider {
    state: Entity<SliderState>,
    value: f32,
    dragging: bool,
    on_change: Rc<dyn Fn(f32) -> Command>,
    _events: Subscription,
}

/// Skin controls: (setting key, label, default, signed).
const SKIN: &[(&str, &str, f32, bool)] = &[
    ("smooth", "Smooth", 50.0, false),
    ("foundation", "Foundation", 20.0, false),
    ("skin_tone", "Skin tone", 0.0, true),
    ("redness", "Redness", 0.0, false),
    ("shadows", "Dark circles", 0.0, false),
    ("eyelight", "Bright eyes", 30.0, false),
];

impl StudioWindow {
    /// A section with a header that collapses it (remembered per title). `action` sits at the
    /// header's right.
    pub(super) fn section(&self, title: &'static str, action: Option<AnyElement>, cx: &mut Context<Self>) -> Div {
        let open = !self.collapsed.borrow().contains(title);
        div()
            .flex()
            .flex_col()
            .gap_2()
            .pb_3()
            .border_b_1()
            .border_color(tok::border())
            .when(!open, |d| d.h(px(HEADER_H)).pb_0().overflow_hidden())
            .child(
                div()
                    .flex()
                    .items_center()
                    .h(px(HEADER_H))
                    .flex_none()
                    .child(
                        div()
                            .id(SharedString::from(format!("section-{title}")))
                            .flex()
                            .flex_1()
                            .items_center()
                            .gap_1()
                            .h_full()
                            .cursor_pointer()
                            .text_size(px(11.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(tok::text_secondary())
                            .hover(|d| d.text_color(tok::text()))
                            .child(ui::icon(if open { "chevron-down" } else { "chevron-right" }).size(px(12.)))
                            .child(title.to_uppercase())
                            .on_click(cx.listener(move |this, _, _, cx| {
                                let mut c = this.collapsed.borrow_mut();
                                if !c.remove(title) {
                                    c.insert(title);
                                }
                                drop(c);
                                cx.notify();
                            })),
                    )
                    .children(action),
            )
    }

    /// A labelled number with small −/+ (shift-free steps of `step`).
    pub(super) fn field(
        &self,
        id: &'static str,
        label: &'static str,
        value: String,
        minus: Command,
        plus: Command,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let small = |i: usize, icon: &'static str, cmd: Command| {
            div()
                .id((id, i))
                .flex()
                .items_center()
                .justify_center()
                .size(px(20.))
                .rounded(px(4.))
                .cursor_pointer()
                .text_color(tok::text_tertiary())
                .hover(|d| d.bg(tok::bg_selected()).text_color(tok::text()))
                .child(ui::icon(icon).size(px(12.)))
                .on_click(cx.listener(move |this, _, _, cx| this.exec(cmd.clone(), cx)))
        };
        div()
            .flex()
            .items_center()
            .gap_1()
            .h(px(30.))
            .px_2()
            .rounded(px(6.))
            .bg(tok::bg_raised())
            .child(div().w(px(16.)).text_size(px(11.)).text_color(tok::text_tertiary()).child(label))
            .child(div().flex_1().text_size(px(12.)).font_family("Menlo").child(value))
            .child(small(0, "minus", minus))
            .child(small(1, "plus", plus))
    }

    /// A row of mutually exclusive options; `options` are (label, command, selected).
    pub(super) fn segmented(
        &self,
        id: &'static str,
        options: Vec<(&'static str, Command, bool)>,
        cx: &mut Context<Self>,
    ) -> Div {
        let mut row = div().flex().gap_0p5().p_0p5().rounded(px(6.)).bg(tok::bg_raised());
        for (i, (label, cmd, on)) in options.into_iter().enumerate() {
            row = row.child(
                div()
                    .id((id, i))
                    .flex_1()
                    .text_center()
                    .py_1()
                    .rounded(px(4.))
                    .text_size(px(11.))
                    .cursor_pointer()
                    .when(on, |d| d.bg(tok::bg_selected()).text_color(tok::text()))
                    .when(!on, |d| d.text_color(tok::text_tertiary()).hover(|d| d.text_color(tok::text_secondary())))
                    .child(label)
                    .on_click(cx.listener(move |this, _, _, cx| this.exec(cmd.clone(), cx))),
            );
        }
        row
    }

    /// A slider whose changes are one undo step per drag.
    fn slider(
        &self,
        key: String,
        range: (f32, f32, f32),
        value: f32,
        on_change: impl Fn(f32) -> Command + 'static,
        cx: &mut Context<Self>,
    ) -> Slider {
        let on_change: Rc<dyn Fn(f32) -> Command> = Rc::new(on_change);
        let mut sliders = self.sliders.borrow_mut();
        let stale = sliders.get(&key).is_none_or(|s| !s.dragging && (s.value - value).abs() > f32::EPSILON);
        if stale {
            let (min, max, step) = range;
            let state = cx.new(|_| SliderState::new().min(min).max(max).step(step).default_value(value));
            let k = key.clone();
            let events = cx.subscribe(&state, move |this, _, ev: &SliderEvent, cx| {
                let (v, release) = match ev {
                    SliderEvent::Change(v) => (v.end(), false),
                    SliderEvent::Release(v) => (v.end(), true),
                };
                let cmd = {
                    let mut sliders = this.sliders.borrow_mut();
                    let Some(s) = sliders.get_mut(&k) else { return };
                    if !s.dragging {
                        s.dragging = true;
                        this.studio.update(cx, |st, _| st.begin_gesture());
                    }
                    s.value = v;
                    if release {
                        s.dragging = false;
                    }
                    (s.on_change)(v)
                };
                this.exec(cmd, cx);
                if release {
                    this.studio.update(cx, |st, _| st.end_gesture());
                }
            });
            sliders.insert(
                key.clone(),
                BoundSlider { state, value, dragging: false, on_change: on_change.clone(), _events: events },
            );
        } else if let Some(s) = sliders.get_mut(&key) {
            s.on_change = on_change;
        }
        Slider::new(&sliders[&key].state)
    }

    /// The Inspector for one layer.
    pub(super) fn layer_inspector(
        &self,
        project: &Project,
        scene: &Scene,
        layer: &Layer,
        format: &Format,
        body: Div,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let lo = layer.layouts.get(&format.id).copied().unwrap_or(Layout::boxed(0., 0., 100., 100.));
        let (sid, lid, fid) = (scene.id, layer.id, format.id);
        let input = project.inputs.get(&layer.input).cloned();
        let kind = input.as_ref().map(|i| i.kind.clone()).unwrap_or_default();
        let set = move |f: &dyn Fn(&mut Layout)| {
            let mut l = lo;
            f(&mut l);
            Command::SetLayout { scene: sid, layer: lid, format: fid, layout: l }
        };

        // Header: what it is, and the two actions people reach for.
        let full = set(&|l| {
            *l =
                Layout { x: 0.0, y: 0.0, width: format.width as f32, height: format.height as f32, rotation: 0.0, ..*l }
        });
        let header = div()
            .flex()
            .items_center()
            .gap_2()
            .py_2()
            .child(ui::icon(ui::input_icon(&kind)).size(px(16.)).text_color(tok::text_secondary()))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .overflow_hidden()
                    .text_ellipsis()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(layer.name.clone()),
            )
            .child(
                ui::icon_button("fill-canvas", "maximize", tok::text_secondary())
                    .tooltip(|w, cx| Tooltip::new("Fill the canvas").build(w, cx))
                    .on_click(cx.listener(move |this, _, _, cx| this.exec(full.clone(), cx))),
            )
            .child(
                ui::icon_button("delete-layer", "trash", tok::text_secondary())
                    .tooltip(|w, cx| Tooltip::new("Delete layer").build(w, cx))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.exec(Command::RemoveLayer { scene: sid, layer: lid }, cx)
                    })),
            );

        // Transform.
        let grid = div()
            .grid()
            .grid_cols(2)
            .gap_1()
            .child(self.field("tx", "X", format!("{:.0}", lo.x), set(&|l| l.x -= 10.0), set(&|l| l.x += 10.0), cx))
            .child(self.field("ty", "Y", format!("{:.0}", lo.y), set(&|l| l.y -= 10.0), set(&|l| l.y += 10.0), cx))
            .child(self.field(
                "tw",
                "W",
                format!("{:.0}", lo.width),
                set(&|l| l.width = (l.width - 10.0).max(8.0)),
                set(&|l| l.width += 10.0),
                cx,
            ))
            .child(self.field(
                "th",
                "H",
                format!("{:.0}", lo.height),
                set(&|l| l.height = (l.height - 10.0).max(8.0)),
                set(&|l| l.height += 10.0),
                cx,
            ))
            .child(self.field(
                "tr",
                "↻",
                format!("{:.0}°", lo.rotation),
                set(&|l| l.rotation -= 15.0),
                set(&|l| l.rotation += 15.0),
                cx,
            ));
        let mut fits = div().flex().gap_0p5().p_0p5().rounded(px(6.)).bg(tok::bg_raised());
        for (i, (fit, label, tip)) in [
            (Fit::Fit, "Fit", "Whole picture, inside the box"),
            (Fit::Fill, "Fill", "Cover the box, crop the overflow"),
            (Fit::Stretch, "Stretch", "Stretch to the box"),
            (Fit::Original, "1:1", "Original size"),
        ]
        .into_iter()
        .enumerate()
        {
            let cmd = Command::SetLayout { scene: sid, layer: lid, format: fid, layout: Layout { fit, ..lo } };
            let on = lo.fit == fit;
            fits = fits.child(
                div()
                    .id(("fit", i))
                    .flex_1()
                    .text_center()
                    .py_1()
                    .rounded(px(4.))
                    .text_size(px(11.))
                    .cursor_pointer()
                    .when(on, |d| d.bg(tok::bg_selected()).text_color(tok::text()))
                    .when(!on, |d| d.text_color(tok::text_tertiary()))
                    .tooltip(move |w, cx| Tooltip::new(tip).build(w, cx))
                    .child(label)
                    .on_click(cx.listener(move |this, _, _, cx| this.exec(cmd.clone(), cx))),
            );
        }
        let reset_crop = (lo.crop != lightair_core::Crop::default()).then(|| {
            let cmd = set(&|l| l.crop = lightair_core::Crop::default());
            Button::new("reset-crop")
                .label("Reset crop")
                .xsmall()
                .ghost()
                .on_click(cx.listener(move |this, _, _, cx| this.exec(cmd.clone(), cx)))
                .into_any_element()
        });
        let transform = self.section("Transform", reset_crop, cx).child(grid).child(fits);

        let mut col = body.gap_0().child(header);
        if let Some(source) = input.as_ref().and_then(|i| self.source_section(i, cx)) {
            col = col.child(source);
        }
        col = col.child(transform);
        if let Some(input) = &input {
            if let Some(content) = self.content_section(input, cx) {
                col = col.child(content);
            }
            col = col.child(self.effects_section(input, cx));
            if let Some(cs) = self.camera_studio_compact(input, cx) {
                col = col.child(cs);
            }
        }
        if project.formats.len() > 1 {
            col = col.child(self.formats_section(project, layer, sid, cx));
        }
        col.into_any_element()
    }

    /// Camera Studio for a camera input: on/off and the look in one row, skin sliders below,
    /// every look one click away in a grid.
    fn camera_studio_compact(&self, input: &Input, cx: &mut Context<Self>) -> Option<AnyElement> {
        let Some(effect) = input.effects.iter().find(|e| e.kind == CAMERA_STUDIO).cloned() else {
            let cmd = crate::camera_studio::is_camera(&input.kind)
                .then(|| crate::camera_studio::ensure_effect(input))
                .flatten()?;
            return Some(
                self.section("Camera Studio", None, cx)
                    .child(
                        Button::new("add-camera-studio")
                            .label("Turn on Camera Studio")
                            .icon(ui::icon("sparkles"))
                            .small()
                            .outline()
                            .on_click(cx.listener(move |this, _, _, cx| this.exec(cmd.clone(), cx))),
                    )
                    .into_any_element(),
            );
        };
        let (iid, eid, on) = (input.id, effect.id, effect.enabled);
        let look = effect.settings.get("lightair_look").and_then(|v| v.as_str()).map(str::to_string);
        let switch = div()
            .id("cs-toggle")
            .w(px(30.))
            .h(px(18.))
            .rounded_full()
            .cursor_pointer()
            .bg(if on { tok::interactive() } else { tok::border_strong() })
            .child(div().size(px(14.)).m(px(2.)).rounded_full().bg(tok::bg_app()).when(on, |d| d.ml(px(14.))))
            .on_click(cx.listener(move |this, _, _, cx| {
                cx.stop_propagation();
                this.exec(Command::SetInputEffectEnabled { input: iid, effect: eid, enabled: !on }, cx)
            }));
        let mut col = self.section("Camera Studio", Some(switch.into_any_element()), cx).child(
            div()
                .id("cs-look")
                .flex()
                .items_center()
                .gap_2()
                .h(px(34.))
                .px_2()
                .rounded(px(6.))
                .bg(tok::bg_raised())
                .cursor_pointer()
                .hover(|d| d.bg(tok::bg_selected()))
                .child(ui::icon("sparkles").size(px(14.)).text_color(tok::text_secondary()))
                .child(div().flex_1().child(look.unwrap_or_else(|| "Custom".into())))
                .child(div().text_size(px(11.)).text_color(tok::text_tertiary()).child("Change look"))
                .child(ui::icon("chevron-right").size(px(12.)).text_color(tok::text_tertiary()))
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.looks_for = Some((iid, eid));
                    cx.notify();
                })),
        );
        for (key, label, default, signed) in SKIN {
            let value = effect.settings.get(*key).and_then(|v| v.as_f64()).map_or(*default, |v| v as f32);
            let settings = effect.settings.clone();
            let key_s = *key;
            let slider = self.slider(
                format!("cs:{eid}:{key}"),
                (if *signed { -100.0 } else { 0.0 }, 100.0, 1.0),
                value,
                move |v| {
                    let mut s = settings.clone();
                    s[key_s] = (v.round() as i64).into();
                    if let Some(obj) = s.as_object_mut() {
                        obj.remove("lightair_look");
                    }
                    Command::SetInputEffectSettings { input: iid, effect: eid, settings: s }
                },
                cx,
            );
            col = col.child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(div().w(px(84.)).text_size(px(12.)).text_color(tok::text_secondary()).child(*label))
                    .child(div().flex_1().child(slider))
                    .child(
                        div()
                            .w(px(28.))
                            .text_right()
                            .text_size(px(11.))
                            .font_family("Menlo")
                            .text_color(tok::text_tertiary())
                            .child(format!("{value:.0}")),
                    ),
            );
        }
        Some(col.into_any_element())
    }

    /// The look picker: every look, grouped, as a grid.
    pub(super) fn looks_overlay(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let (iid, eid) = self.looks_for?;
        let project = self.studio.read(cx).project().clone();
        let current = project
            .inputs
            .get(&iid)
            .and_then(|i| i.effects.iter().find(|e| e.id == eid))
            .and_then(|e| e.settings.get("lightair_look").and_then(|v| v.as_str()).map(str::to_string));
        let mut body = div().id("looks-body").flex().flex_col().gap_3().overflow_y_scroll().flex_1().min_h_0();
        for (ci, (category, list)) in looks().iter().enumerate() {
            let mut grid = div().grid().grid_cols(3).gap_1p5();
            for (li, look) in list.iter().enumerate() {
                let mut settings = look.settings.clone();
                settings["lightair_look"] = look.name.clone().into();
                let label = look.name.clone();
                let selected = current.as_deref() == Some(look.name.as_str());
                grid = grid.child(
                    div()
                        .id(("look", ci * 100 + li))
                        .flex()
                        .items_center()
                        .justify_center()
                        .h(px(40.))
                        .rounded(px(8.))
                        .border_1()
                        .border_color(if selected { tok::accent_primary() } else { tok::border() })
                        .bg(if selected { tok::bg_selected() } else { tok::bg_raised() })
                        .cursor_pointer()
                        .hover(|d| d.bg(tok::bg_selected()))
                        .text_size(px(12.))
                        .child(label.clone())
                        .on_click(cx.listener(move |this, _, _, cx| {
                            let commands = vec![
                                Command::SetInputEffectSettings { input: iid, effect: eid, settings: settings.clone() },
                                Command::SetInputEffectEnabled { input: iid, effect: eid, enabled: true },
                            ];
                            this.exec(Command::Batch { label: format!("Apply look {label}"), commands }, cx);
                        })),
                );
            }
            body = body.child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1p5()
                    .child(
                        div()
                            .text_size(px(11.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(tok::text_tertiary())
                            .child(category.to_uppercase()),
                    )
                    .child(grid),
            );
        }
        Some(
            div()
                .absolute()
                .inset_0()
                .flex()
                .justify_center()
                .items_center()
                .bg(tok::scrim())
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _, _, cx| {
                        this.looks_for = None;
                        cx.notify();
                    }),
                )
                .child(
                    div()
                        .id("looks")
                        .flex()
                        .flex_col()
                        .gap_3()
                        .w(px(520.))
                        .h(px(600.))
                        .p_4()
                        .rounded(px(14.))
                        .bg(tok::bg_panel())
                        .border_1()
                        .border_color(tok::border_strong())
                        .shadow_lg()
                        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .justify_between()
                                .child(div().text_size(px(15.)).font_weight(FontWeight::SEMIBOLD).child("Looks"))
                                .child(Button::new("looks-done").label("Done").small().primary().on_click(
                                    cx.listener(|this, _, _, cx| {
                                        this.looks_for = None;
                                        cx.notify();
                                    }),
                                )),
                        )
                        .child(body),
                )
                .into_any_element(),
        )
    }
}
