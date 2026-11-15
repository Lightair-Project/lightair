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

// ------------------------------------------------------------ keys per field

use lightair_render::sequence::Clip;

/// A property's own value (without keys).
pub(super) fn base_of(c: &Clip, p: &str) -> f32 {
    let f = c.filter;
    match p {
        "x" => c.transform.x,
        "y" => c.transform.y,
        "scale" => c.transform.scale,
        "rotation" => c.transform.rotation,
        "opacity" => c.transform.opacity,
        "volume" => c.volume,
        "brightness" => f.brightness,
        "contrast" => f.contrast,
        "saturation" => f.saturation,
        "temperature" => f.temperature,
        "vignette" => f.vignette,
        "blur" => f.blur,
        "crop.l" => c.crop.0,
        "crop.t" => c.crop.1,
        "crop.r" => c.crop.2,
        "crop.b" => c.crop.3,
        _ => effect_setting(c, p).map_or(0.0, |(n, k)| c.effects.get(n).and_then(|e| e.settings.get(k)).and_then(serde_json::Value::as_f64).unwrap_or(0.0) as f32),
    }
}

fn set_base(c: &mut Clip, p: &str, v: f32) {
    match p {
        "x" => c.transform.x = v,
        "y" => c.transform.y = v,
        "scale" => c.transform.scale = v,
        "rotation" => c.transform.rotation = v,
        "opacity" => c.transform.opacity = v,
        "volume" => c.volume = v,
        "brightness" => c.filter.brightness = v,
        "contrast" => c.filter.contrast = v,
        "saturation" => c.filter.saturation = v,
        "temperature" => c.filter.temperature = v,
        "vignette" => c.filter.vignette = v,
        "blur" => c.filter.blur = v,
        "crop.l" => c.crop.0 = v,
        "crop.t" => c.crop.1 = v,
        "crop.r" => c.crop.2 = v,
        "crop.b" => c.crop.3 = v,
        _ => {
            if let Some((n, k)) = effect_setting(c, p).map(|(n, k)| (n, k.to_string()))
                && let Some(e) = c.effects.get_mut(n)
            {
                e.settings.insert(k, serde_json::json!(v));
            }
        }
    }
}

/// "fx.<id>.<setting>": the effect's place in the clip's effects, and the setting.
fn effect_setting<'a>(c: &Clip, p: &'a str) -> Option<(usize, &'a str)> {
    let rest = p.strip_prefix("fx.")?;
    let (id, k) = rest.split_once('.')?;
    Some((c.effects.iter().position(|e| e.id.to_string() == id)?, k))
}

/// A property's value at `t`.
pub(super) fn prop_value(c: &Clip, p: &str, t: u64) -> f32 {
    if p == "volume" { c.volume_at(t) } else { c.prop_at(p, t, base_of(c, p)) }
}

/// Sets a property at `t`: its key there when it's keyed, else its one value.
pub(super) fn set_prop(c: &mut Clip, p: &str, t: u64, v: f32) {
    if p == "volume" {
        c.set_volume_at(t, v);
    } else if !c.set_key(p, t, v) {
        set_base(c, p, v);
    }
}

fn has_key(c: &Clip, p: &str, t: u64) -> bool {
    if p == "volume" {
        let at = t.saturating_sub(c.start_ms);
        c.volume_keys.iter().any(|k| k.at_ms.abs_diff(at) < 40)
    } else {
        c.has_key(p, t)
    }
}

fn key_times(c: &Clip, p: &str) -> Vec<u64> {
    if p == "volume" { c.volume_keys.iter().map(|k| k.at_ms).collect() } else { c.keys_of(p).iter().map(|k| k.at_ms).collect() }
}

/// A key for `p` at `t`, or the one there taken away.
pub(super) fn toggle_prop(c: &mut Clip, p: &str, t: u64) {
    if p == "volume" {
        let rel = t.saturating_sub(c.start_ms).min(c.len_ms());
        if let Some(i) = c.volume_keys.iter().position(|k| k.at_ms.abs_diff(rel) < 40) {
            let gone = c.volume_keys.remove(i);
            if c.volume_keys.is_empty() {
                c.volume = gone.volume;
            }
        } else {
            let volume = c.volume_at(t);
            c.volume_keys.push(lightair_render::sequence::VolumeKey { at_ms: rel, volume });
            c.volume_keys.sort_by_key(|k| k.at_ms);
        }
        return;
    }
    let mut b = base_of(c, p);
    c.toggle_key(p, t, &mut b);
    set_base(c, p, b);
}

impl Editor {
    /// A number field of a clip's property, keyable: its value at the playhead (dragged or
    /// stepped by `delta`, within `range`), and ‹ ◇ › for its keys.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn keyed(&self, c: &Clip, prop: impl Into<String>, name: &'static str, fmt: Rc<dyn Fn(f32) -> String>, delta: f32, range: (f32, f32), cx: &mut Context<Self>) -> Div {
        let prop: String = prop.into();
        let (cid, t) = (c.id, self.playhead);
        let v = prop_value(c, &prop, t);
        let p = prop.clone();
        let step: Step = Rc::new(move |this, d, cx| {
            let at = this.playhead;
            this.change(
                |s| {
                    if let Some(c) = s.clip_mut(cid) {
                        let cur = prop_value(c, &p, at);
                        set_prop(c, &p, at, (cur + d * delta).clamp(range.0, range.1));
                    }
                },
                cx,
            )
        });
        let id = SharedString::from(format!("ed-k-{prop}"));
        self.field(id, name, fmt(v), step, cx).child(self.key_control(c, vec![prop], cx))
    }

    /// ‹ ◇ ›: the keys of `props` (one, or a section's): the previous and next, and one at the
    /// playhead (filled when there's one there).
    pub(super) fn key_control(&self, c: &Clip, props: Vec<String>, cx: &mut Context<Self>) -> Div {
        let (cid, t) = (c.id, self.playhead);
        let inside = c.start_ms <= t && t < c.end_ms();
        let rel = t.saturating_sub(c.start_ms);
        let times: Vec<u64> = props.iter().flat_map(|p| key_times(c, p)).collect();
        let prev = times.iter().filter(|k| **k + 40 < rel).max().map(|k| c.start_ms + k);
        let next = times.iter().filter(|k| **k > rel + 40).min().map(|k| c.start_ms + k);
        let here = props.iter().all(|p| has_key(c, p, t));
        let keyed = !times.is_empty();
        let id = props.join("+");
        let arrow = |icon: &'static str, to: Option<u64>, n: usize, cx: &mut Context<Self>| {
            div()
                .id(SharedString::from(format!("ed-kn-{id}-{n}")))
                .w(px(12.))
                .flex()
                .justify_center()
                .text_color(if to.is_some() { tok::text_secondary() } else { tok::text_tertiary() })
                .when(to.is_some(), |d| d.cursor_pointer().hover(|d| d.text_color(tok::text())))
                .child(ui::icon(icon).size(px(11.)))
                .on_click(cx.listener(move |this, _, _, cx| if let Some(to) = to { this.seek(to, cx) }))
        };
        let toggle_props = props.clone();
        div()
            .flex()
            .flex_none()
            .items_center()
            .child(arrow("chevron-left", prev, 0, cx))
            .child(
                div()
                    .id(SharedString::from(format!("ed-kd-{id}")))
                    .size(px(20.))
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(px(4.))
                    .when(inside, |d| d.cursor_pointer().hover(|d| d.bg(tok::bg_raised())))
                    // filled (in the accent) when there's a key at the playhead
                    .when(here, |d| d.bg(rgba(0x3B82F633)))
                    .child(ui::icon("diamond").size(px(12.)).text_color(if here { tok::accent_primary() } else if keyed { tok::text_secondary() } else { tok::text_tertiary() }))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        let at = this.playhead;
                        if !this.seq().and_then(|s| s.clip(cid)).is_some_and(|c| c.start_ms <= at && at < c.end_ms()) {
                            this.note = Some("Move the playhead over the clip to key it there.".into());
                            cx.notify();
                            return;
                        }
                        let ps = toggle_props.clone();
                        this.change(
                            |s| {
                                if let Some(c) = s.clip_mut(cid) {
                                    // a section's: all keyed there, or all taken away
                                    let all = ps.iter().all(|p| has_key(c, p, at));
                                    for p in &ps {
                                        if has_key(c, p, at) == all {
                                            toggle_prop(c, p, at);
                                        }
                                    }
                                }
                            },
                            cx,
                        )
                    })),
            )
            .child(arrow("chevron-right", next, 1, cx))
    }

    /// A section's title with ‹ ◇ › for all its properties at once.
    pub(super) fn keyed_title(&self, c: &Clip, title: &'static str, props: &[&str], cx: &mut Context<Self>) -> Div {
        div()
            .flex()
            .items_end()
            .child(label(title).flex_1())
            .child(self.key_control(c, props.iter().map(|p| p.to_string()).collect(), cx))
    }
}
