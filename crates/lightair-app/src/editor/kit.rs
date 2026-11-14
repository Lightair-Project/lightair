//! The inspector's controls: number fields dragged (or stepped) to change, segmented choices,
//! underlined tabs, section titles. A drag on a field is one undo step.

use std::rc::Rc;

use super::*;

/// What a number field does with a step (−1 or +1).
pub(super) type Step = Rc<dyn Fn(&mut Editor, f32, &mut Context<Editor>)>;

/// A field being dragged: where the pointer was, the part of a step not taken yet, the step.
pub(super) struct FieldDrag {
    x: f32,
    carry: f32,
    step: Step,
}

/// Pixels of drag per step.
const PER_STEP: f32 = 6.0;

impl Editor {
    /// A labelled number: its value in a field dragged left or right to change it, − and +
    /// at its ends.
    pub(super) fn field(&self, id: SharedString, name: &'static str, value: String, step: Step, cx: &mut Context<Self>) -> Div {
        let (down, minus, plus) = (step.clone(), step.clone(), step);
        div()
            .flex()
            .items_center()
            .gap_2()
            .h(px(28.))
            .child(div().w(px(84.)).flex_none().text_size(px(12.)).text_color(tok::text_secondary()).overflow_hidden().text_ellipsis().whitespace_nowrap().child(name))
            .child(
                div()
                    .id(id.clone())
                    .group("field")
                    .flex_1()
                    .min_w_0()
                    .h(px(26.))
                    .flex()
                    .items_center()
                    .rounded(px(6.))
                    .bg(tok::bg_app())
                    .border_1()
                    .border_color(tok::border())
                    .hover(|d| d.border_color(tok::border_strong()))
                    .child(
                        div()
                            .id(SharedString::from(format!("{id}-")))
                            .w(px(22.))
                            .h_full()
                            .flex()
                            .items_center()
                            .justify_center()
                            .text_color(tok::text_tertiary())
                            .invisible()
                            .group_hover("field", |d| d.visible())
                            .hover(|d| d.text_color(tok::text()))
                            .child(ui::icon("minus").size(px(11.)))
                            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                            .on_click(cx.listener(move |this, _, _, cx| minus(this, -1.0, cx))),
                    )
                    .child(
                        div()
                            .flex_1()
                            .h_full()
                            .flex()
                            .items_center()
                            .justify_center()
                            .cursor(CursorStyle::ResizeLeftRight)
                            .text_size(px(12.))
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(tok::text())
                            .child(value)
                            .on_mouse_down(MouseButton::Left, cx.listener(move |this, ev: &MouseDownEvent, _, cx| {
                                cx.stop_propagation();
                                this.field_before = this.project.as_ref().map(|p| p.sequence.clone());
                                this.field_drag = Some(FieldDrag { x: f32::from(ev.position.x), carry: 0.0, step: down.clone() });
                            })),
                    )
                    .child(
                        div()
                            .id(SharedString::from(format!("{id}+")))
                            .w(px(22.))
                            .h_full()
                            .flex()
                            .items_center()
                            .justify_center()
                            .text_color(tok::text_tertiary())
                            .invisible()
                            .group_hover("field", |d| d.visible())
                            .hover(|d| d.text_color(tok::text()))
                            .child(ui::icon("plus").size(px(11.)))
                            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                            .on_click(cx.listener(move |this, _, _, cx| plus(this, 1.0, cx))),
                    ),
            )
    }

    /// A field being dragged follows the pointer.
    pub(super) fn field_move(&mut self, x: Pixels, cx: &mut Context<Self>) {
        let Some(d) = &mut self.field_drag else { return };
        d.carry += f32::from(x) - d.x;
        d.x = f32::from(x);
        let steps = (d.carry / PER_STEP).trunc();
        if steps == 0.0 {
            return;
        }
        d.carry -= steps * PER_STEP;
        let step = d.step.clone();
        for _ in 0..steps.abs() as usize {
            step(self, steps.signum(), cx);
        }
    }

    /// The drag ended: what it changed is one undo step.
    pub(super) fn field_end(&mut self) {
        if self.field_drag.take().is_none() {
            return;
        }
        if let (Some(before), Some(p)) = (self.field_before.take(), &self.project)
            && before != p.sequence
        {
            // the steps the drag made were kept out of undo; the drag is one
            self.undo.push(before);
            self.redo.clear();
        }
    }

    /// One of several, as a segmented control.
    pub(super) fn segmented<T: Copy + PartialEq + 'static>(
        &self,
        id: SharedString,
        options: &[(&'static str, T)],
        current: T,
        on: impl Fn(&mut Self, T, &mut Context<Self>) + Copy + 'static,
        cx: &mut Context<Self>,
    ) -> Div {
        let mut row = div().flex().flex_wrap().gap_0p5().p_0p5().rounded(px(7.)).bg(tok::bg_app()).border_1().border_color(tok::border());
        for (n, (text, v)) in options.iter().enumerate() {
            let (v, sel) = (*v, *v == current);
            row = row.child(
                div()
                    .id(SharedString::from(format!("{id}-{n}")))
                    .flex_grow(1.0)
                    .px_2()
                    .h(px(22.))
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(px(5.))
                    .text_size(px(12.))
                    .cursor_pointer()
                    .when(sel, |d| d.bg(tok::bg_raised()).text_color(tok::text()).font_weight(FontWeight::MEDIUM).shadow_sm())
                    .when(!sel, |d| d.text_color(tok::text_secondary()).hover(|d| d.text_color(tok::text())))
                    .child(*text)
                    .on_click(cx.listener(move |this, _, _, cx| on(this, v, cx))),
            );
        }
        row
    }

    /// Tabs: their names in a row, the one shown underlined.
    pub(super) fn tab_bar(&self, options: &[(&'static str, InspectorTab)], current: InspectorTab, cx: &mut Context<Self>) -> AnyElement {
        let mut row = div().id("ed-tabs").flex().items_end().gap_3().px_3().h(px(34.)).overflow_x_scroll();
        for (n, (text, t)) in options.iter().enumerate() {
            let (t, sel) = (*t, *t == current);
            row = row.child(
                div()
                    .id(("ed-tab", n))
                    .flex_none()
                    .pb(px(7.))
                    .border_b_2()
                    .border_color(if sel { tok::accent_primary() } else { rgba(0x00000000).into() })
                    .text_size(px(12.))
                    .cursor_pointer()
                    .when(sel, |d| d.text_color(tok::text()).font_weight(FontWeight::SEMIBOLD))
                    .when(!sel, |d| d.text_color(tok::text_secondary()).hover(|d| d.text_color(tok::text())))
                    .child(*text)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.tab = t;
                        cx.notify();
                    })),
            );
        }
        row.into_any_element()
    }

    /// The inspector's head: what's selected, and what it is.
    pub(super) fn inspector_head(&self, icon: &'static str, name: String, detail: String) -> AnyElement {
        div()
            .flex()
            .items_center()
            .gap_2()
            .px_3()
            .pt_3()
            .pb_2()
            .child(div().size(px(28.)).flex_none().rounded(px(7.)).bg(tok::bg_raised()).flex().items_center().justify_center().child(ui::icon(icon).size(px(14.)).text_color(tok::text_secondary())))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .min_w_0()
                    .child(div().text_size(px(13.)).font_weight(FontWeight::SEMIBOLD).overflow_hidden().text_ellipsis().whitespace_nowrap().child(name))
                    .child(div().text_size(px(11.)).text_color(tok::text_tertiary()).child(detail)),
            )
            .into_any_element()
    }
}
