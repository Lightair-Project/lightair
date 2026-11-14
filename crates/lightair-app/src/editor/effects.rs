//! The inspector's Effects tab: the effect catalog's shader effects (the Live filter's) on a
//! clip, in order, each switched on or off, with the catalog's own controls.

use super::*;
use lightair_core::catalog::{self, Control, Pick};
use lightair_render::sequence::{Clip, ClipEffect};

/// OBS colours (0xAABBGGRR) offered for effect colours.
const COLOURS: [u32; 8] = [0xFFFFFFFF, 0xFF111111, 0xFF15CCFA, 0xFF1673F9, 0xFF5E3FF4, 0xFFF755A8, 0xFFF6823B, 0xFFEED367];

impl Editor {
    fn set_effects(&mut self, clip: Id, f: impl FnOnce(&mut Vec<ClipEffect>), cx: &mut Context<Self>) {
        self.change(|s| if let Some(c) = s.clip_mut(clip) { f(&mut c.effects) }, cx);
    }

    pub(super) fn effects_section(&self, c: &Clip, mut body: Div, cx: &mut Context<Self>) -> Div {
        let cid = c.id;
        let me = cx.entity().downgrade();
        let add = Button::new("ed-fx-add").label("Add effect").icon(ui::icon("plus")).xsmall().outline().dropdown_menu(move |mut menu, _, _| {
            for cat in catalog::Category::ALL {
                let defs: Vec<_> = catalog::CATALOG.iter().filter(|d| d.kind.starts_with("fx.") && d.category == cat).collect();
                if defs.is_empty() {
                    continue;
                }
                menu = menu.label(cat.name());
                for d in defs {
                    let me = me.clone();
                    let (kind, settings) = (d.kind, d.defaults());
                    menu = menu.item(PopupMenuItem::new(d.name).icon(ui::icon(d.icon)).on_click(move |_, _, cx| {
                        let settings = settings.clone();
                        if let Some(me) = me.upgrade() {
                            me.update(cx, |this, cx| this.set_effects(cid, |e| e.push(ClipEffect { kind: kind.into(), settings, on: true }), cx));
                        }
                    }));
                }
            }
            menu
        });
        body = body.child(label("EFFECTS")).child(div().flex().child(add));
        if c.effects.is_empty() {
            return body.child(hint("The Live effects (VHS, glow, blur, swirl, duotone…) on this clip's picture, run in order."));
        }
        for (n, e) in c.effects.iter().enumerate() {
            let Some(def) = catalog::def(&e.kind) else { continue };
            let on = e.on;
            let head = div()
                .flex()
                .items_center()
                .gap_1()
                .pt_2()
                .child(ui::icon(def.icon).size(px(14.)).text_color(tok::text_secondary()))
                .child(div().flex_1().text_size(px(13.)).font_weight(FontWeight::SEMIBOLD).when(!on, |d| d.text_color(tok::text_tertiary())).child(def.name))
                .child(ui::icon_button(("ed-fx-on", n), if on { "eye" } else { "eye-off" }, tok::text_secondary()).on_click(cx.listener(move |this, _, _, cx| {
                    this.set_effects(cid, |e| if let Some(x) = e.get_mut(n) { x.on = !x.on }, cx)
                })))
                .child(ui::icon_button(("ed-fx-up", n), "arrow-up", if n > 0 { tok::text_secondary() } else { tok::text_tertiary() }).on_click(cx.listener(move |this, _, _, cx| {
                    if n > 0 {
                        this.set_effects(cid, |e| e.swap(n, n - 1), cx)
                    }
                })))
                .child(ui::icon_button(("ed-fx-del", n), "x", tok::text_secondary()).on_click(cx.listener(move |this, _, _, cx| {
                    this.set_effects(cid, |e| {
                        if n < e.len() {
                            e.remove(n);
                        }
                    }, cx)
                })));
            body = body.child(head);
            let settings = serde_json::Value::Object(e.settings.clone());
            for (k, control) in def.controls.iter().enumerate() {
                let id = n * 16 + k;
                match *control {
                    Control::Slider(sl) => {
                        let v = sl.value(&settings);
                        let step = (sl.max - sl.min) / 20.0;
                        body = body.child(self.stepper_dyn(("ed-fx-s", id), sl.label, sl.display(v), move |this, d, cx| {
                            this.set_effects(cid, |e| {
                                if let Some(x) = e.get_mut(n) {
                                    let cur = x.settings.get(sl.key).and_then(serde_json::Value::as_f64).unwrap_or(sl.default);
                                    x.settings.insert(sl.key.into(), sl.json(cur + d as f64 * step));
                                }
                            }, cx)
                        }, cx));
                    }
                    Control::Choice { key, label: name, options, default } => {
                        let cur = settings.get(key).cloned().unwrap_or(default.json());
                        let mut row = div().flex().flex_wrap().gap_1().child(div().w_full().text_size(px(12.)).text_color(tok::text_secondary()).child(name));
                        for (o, (text, pick)) in options.iter().enumerate() {
                            let (pick, sel) = (*pick, pick.json() == cur);
                            row = row.child(
                                div()
                                    .id(("ed-fx-c", id * 16 + o))
                                    .px_2()
                                    .py_0p5()
                                    .rounded(px(6.))
                                    .text_size(px(12.))
                                    .cursor_pointer()
                                    .when(sel, |d| d.bg(tok::bg_selected()))
                                    .hover(|d| d.bg(tok::bg_selected()))
                                    .child(*text)
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.set_effects(cid, |e| if let Some(x) = e.get_mut(n) { x.settings.insert(key.into(), pick_json(pick)); }, cx)
                                    })),
                            );
                        }
                        body = body.child(row);
                    }
                    Control::Colour { key, label: name, default } => {
                        let cur = settings.get(key).and_then(serde_json::Value::as_u64).unwrap_or(default as u64) as u32;
                        let mut row = div().flex().flex_wrap().gap_1p5().items_center().child(div().w(px(76.)).text_size(px(12.)).text_color(tok::text_secondary()).child(name));
                        for (o, c) in COLOURS.iter().enumerate() {
                            let c = *c;
                            // 0xAABBGGRR as 0xRRGGBB
                            let shown = ((c & 0xFF) << 16) | (c & 0xFF00) | ((c >> 16) & 0xFF);
                            row = row.child(
                                div()
                                    .id(("ed-fx-k", id * 16 + o))
                                    .size(px(20.))
                                    .rounded(px(5.))
                                    .border_2()
                                    .border_color(if c == cur { tok::accent_primary() } else { tok::border() })
                                    .bg(rgb(shown))
                                    .cursor_pointer()
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.set_effects(cid, |e| if let Some(x) = e.get_mut(n) { x.settings.insert(key.into(), serde_json::json!(c)); }, cx)
                                    })),
                            );
                        }
                        body = body.child(row);
                    }
                    Control::Switch { key, label: name, default } => {
                        let cur = settings.get(key).and_then(serde_json::Value::as_bool).unwrap_or(default);
                        body = body.child(div().flex().items_center().gap_2().child(div().w(px(76.)).text_size(px(12.)).text_color(tok::text_secondary()).child(name)).child(
                            self.chips_dyn(("ed-fx-w", id), &[("Off", false), ("On", true)], cur, move |this, v, cx| {
                                this.set_effects(cid, |e| if let Some(x) = e.get_mut(n) { x.settings.insert(key.into(), serde_json::json!(v)); }, cx)
                            }, cx),
                        ));
                    }
                    _ => {}
                }
            }
        }
        body
    }

    /// A stepper whose id is made at run time (one per effect control).
    fn stepper_dyn(&self, id: (&'static str, usize), name: &'static str, value: String, step: impl Fn(&mut Self, f32, &mut Context<Self>) + Copy + 'static, cx: &mut Context<Self>) -> Div {
        div()
            .flex()
            .items_center()
            .gap_2()
            .child(div().w(px(76.)).text_size(px(12.)).text_color(tok::text_secondary()).child(name))
            .child(div().flex_1().px_2().py_0p5().rounded(px(6.)).bg(tok::bg_app()).text_size(px(12.)).font_family("Menlo").child(value))
            .child(ui::icon_button((id.0, id.1 * 2), "minus", tok::text_secondary()).on_click(cx.listener(move |this, _, _, cx| step(this, -1.0, cx))))
            .child(ui::icon_button((id.0, id.1 * 2 + 1), "plus", tok::text_secondary()).on_click(cx.listener(move |this, _, _, cx| step(this, 1.0, cx))))
    }

    fn chips_dyn<T: Copy + PartialEq + 'static>(&self, id: (&'static str, usize), options: &[(&'static str, T)], current: T, on: impl Fn(&mut Self, T, &mut Context<Self>) + Copy + 'static, cx: &mut Context<Self>) -> Div {
        let mut row = div().flex().gap_1();
        for (k, (text, v)) in options.iter().enumerate() {
            let v = *v;
            row = row.child(
                div()
                    .id((id.0, id.1 * 8 + k))
                    .px_2()
                    .py_0p5()
                    .rounded(px(6.))
                    .text_size(px(12.))
                    .cursor_pointer()
                    .when(v == current, |d| d.bg(tok::bg_selected()))
                    .hover(|d| d.bg(tok::bg_selected()))
                    .child(*text)
                    .on_click(cx.listener(move |this, _, _, cx| on(this, v, cx))),
            );
        }
        row
    }
}

fn pick_json(p: Pick) -> serde_json::Value {
    p.json()
}

impl Editor {
    /// For the screenshot hook: the first clip through two effects with a hidden area, a
    /// sticker over it, the clip selected on the Effects tab.
    pub fn hook_more(&mut self, cx: &mut Context<Self>) {
        let Some(first) = self.seq().and_then(|s| s.tracks.iter().find(|t| t.kind == TrackKind::Video)).and_then(|t| t.clips.first()).map(|c| c.id) else { return };
        let k = self.seq().map_or(1.0, |s| s.size.1 as f32 / 1080.0);
        self.change(
            |s| {
                if let Some(c) = s.clip_mut(first) {
                    for kind in ["fx.vhs", "fx.vignette"] {
                        if let Some(d) = catalog::def(kind) {
                            c.effects.push(ClipEffect { kind: kind.into(), settings: d.defaults(), on: true });
                        }
                    }
                    c.hidden = vec![(0.62, 0.62, 0.3, 0.3)];
                    c.fade_in_ms = 800;
                }
                let style = lightair_render::sequence::TextStyle {
                    text: "🚀".into(),
                    size: 220.0 * k,
                    color: 0xFFFFFF,
                    bold: false,
                    align: 1,
                    background: None,
                    outline: Some(0xFFFFFF),
                    width: 1920.0 * k,
                };
                let id = s.place_content(lightair_render::sequence::Content::Text(style), 1_000, 4_000);
                if let Some(c) = s.clip_mut(id) {
                    c.transform = Transform { x: -560.0 * k, y: -260.0 * k, ..Transform::default() };
                }
            },
            cx,
        );
        self.selected = Some(first);
        self.tab = InspectorTab::Effects;
        self.left = LeftTab::Shapes;
        self.seek(2_000, cx);
    }
}
