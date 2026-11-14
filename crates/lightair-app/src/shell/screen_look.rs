//! The scene Inspector's "Screen zoom & pointer" (docs/plans/screen-studio.md, live): how the
//! scene's screen captures zoom (in to a click, or always following the pointer), and how the
//! pointer and clicks are drawn on them. The engine puts it on every screen capture in the
//! scene (`lightair_engine::cursor`).

use gpui_kit::component::Sizable;
use gpui_kit::component::button::Button;
use gpui_kit::component::slider::Slider;
use gpui_kit::*;
use lightair_core::{Command, Project, Scene, ScreenLook};

use super::StudioWindow;
use crate::ui::{self, tok};

fn label(text: &'static str) -> Div {
    div().w(px(76.)).flex_none().text_size(px(12.)).text_color(tok::text_secondary()).child(text)
}

fn row(name: &'static str, control: impl IntoElement) -> Div {
    div().flex().items_center().gap_2().child(label(name)).child(div().flex_1().min_w_0().child(control))
}

fn slider_row(name: &'static str, slider: Slider, value: String) -> Div {
    div()
        .flex()
        .items_center()
        .gap_2()
        .child(label(name))
        .child(div().flex_1().child(slider))
        .child(div().w(px(40.)).text_right().text_size(px(11.)).font_family("Menlo").text_color(tok::text_tertiary()).child(value))
}

fn note(text: &'static str) -> Div {
    // (min_w_0: in a row, it wraps instead of running past the edge)
    div().min_w_0().text_size(px(11.)).text_color(tok::text_tertiary()).child(text)
}

impl StudioWindow {
    /// The section, for `scene`.
    pub(super) fn screen_look_section(&self, project: &Project, scene: &Scene, cx: &mut Context<Self>) -> Div {
        let sid = scene.id;
        let on = scene.screen.is_some();
        let look = scene.screen.unwrap_or_default();
        let set = move |l: ScreenLook| Command::SetSceneScreen { scene: sid, screen: Some(l) };
        let toggle = super::settings::switch("screen-look-on", on).on_click(cx.listener(move |this, _, _, cx| {
            this.exec(Command::SetSceneScreen { scene: sid, screen: if on { None } else { Some(ScreenLook::default()) } }, cx)
        }));
        let mut body = self.section("Screen zoom & pointer", Some(toggle.into_any_element()), cx);
        let screens = scene
            .layers
            .iter()
            .filter_map(|l| l.input.and_then(|i| project.inputs.get(&i)))
            .filter(|i| i.kind == lightair_core::kinds::SCREEN)
            .count();
        if !on {
            return body.child(note("Zoom in where you click and draw a bigger, smoother pointer with click effects on this scene's screen, live, as Screen Studio does."));
        }
        if screens == 0 {
            body = body.child(note("Add a screen to this scene: it zooms and draws the pointer there."));
        }
        // clicks (for the zoom and their effects) need the OS to let Lightair see them
        let wants_clicks = look.zoom == ScreenLook::ZOOM_CLICKS || (look.cursor && look.clicks > 0);
        if wants_clicks && !lightair_platform::input::can_listen() {
            body = body.child(
                div()
                    .flex()
                    .items_start()
                    .gap_2()
                    .p_2()
                    .rounded(px(6.))
                    .bg(tok::bg_raised())
                    .child(div().flex_none().pt(px(1.)).child(ui::icon("info").size(px(14.)).text_color(tok::text_secondary())))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .items_start()
                            .gap_2()
                            .child(note("Clicks need Input Monitoring for Lightair. Until it's on, zooms don't follow clicks and clicks aren't drawn."))
                            .child(
                                // asks macOS (which lists Lightair in the pane, off), then shows
                                // the pane to turn it on there
                                Button::new("screen-input-monitoring")
                                    .label("Allow in Settings…")
                                    .icon(ui::icon("external-link"))
                                    .xsmall()
                                    .outline()
                                    .on_click(|_, _, _| {
                                        if !lightair_platform::input::request_listen() {
                                            lightair_platform::input::open_input_monitoring_settings();
                                        }
                                    }),
                            )
                            .child(note("Switch Lightair on in the list. macOS may ask you to quit and reopen it.")),
                    ),
            );
        }
        // ---- zoom
        body = body
            .child(div().pt_1().text_size(px(11.)).font_weight(FontWeight::SEMIBOLD).text_color(tok::text_tertiary()).child("ZOOM"))
            .child(self.segmented(
                "screen-zoom",
                vec![
                    ("Off", set(ScreenLook { zoom: ScreenLook::ZOOM_OFF, ..look }), look.zoom == ScreenLook::ZOOM_OFF),
                    ("On clicks", set(ScreenLook { zoom: ScreenLook::ZOOM_CLICKS, ..look }), look.zoom == ScreenLook::ZOOM_CLICKS),
                    ("Always", set(ScreenLook { zoom: ScreenLook::ZOOM_ALWAYS, ..look }), look.zoom == ScreenLook::ZOOM_ALWAYS),
                ],
                cx,
            ));
        if look.zoom != ScreenLook::ZOOM_OFF {
            let scales: Vec<(&'static str, Command, bool)> = [("1.5×", 1.5f32), ("2×", 2.0), ("2.5×", 2.5), ("3×", 3.0)]
                .into_iter()
                .map(|(n, v)| (n, set(ScreenLook { zoom_scale: v, ..look }), (look.zoom_scale - v).abs() < 0.01))
                .collect();
            body = body.child(row("How far", self.segmented("screen-zoom-scale", scales, cx)));
            if look.zoom == ScreenLook::ZOOM_CLICKS {
                let s = self.slider(format!("screen-hold:{sid}"), (0.5, 8.0, 0.25), look.zoom_hold_s, move |v| set(ScreenLook { zoom_hold_s: v, ..look }), cx);
                body = body.child(slider_row("Stays in", s, format!("{:.1} s", look.zoom_hold_s)));
            }
            body = body.child(row(
                "Motion",
                self.segmented(
                    "screen-zoom-motion",
                    vec![
                        ("Focused", set(ScreenLook { zoom_motion: 0.15, ..look }), look.zoom_motion < 0.34),
                        ("Balanced", set(ScreenLook { zoom_motion: 0.5, ..look }), (0.34..0.67).contains(&look.zoom_motion)),
                        ("Smooth", set(ScreenLook { zoom_motion: 0.85, ..look }), look.zoom_motion >= 0.67),
                    ],
                    cx,
                ),
            ));
            body = body.child(note(if look.zoom == ScreenLook::ZOOM_CLICKS {
                "Zooms in where you click and follows the pointer, then back out once you stop clicking."
            } else {
                "Always zoomed in, the view following the pointer."
            }));
        }
        // ---- pointer
        body = body
            .child(div().pt_2().text_size(px(11.)).font_weight(FontWeight::SEMIBOLD).text_color(tok::text_tertiary()).child("POINTER"))
            .child(self.segmented(
                "screen-cursor",
                vec![
                    ("As captured", set(ScreenLook { cursor: false, ..look }), !look.cursor),
                    ("Drawn by Lightair", set(ScreenLook { cursor: true, ..look }), look.cursor),
                ],
                cx,
            ));
        if look.cursor {
            let style = self.segmented(
                "screen-cursor-style",
                vec![
                    ("Pointer", set(ScreenLook { cursor_style: 0, ..look }), look.cursor_style == 0),
                    ("Dot", set(ScreenLook { cursor_style: 1, ..look }), look.cursor_style == 1),
                    ("Ring", set(ScreenLook { cursor_style: 2, ..look }), look.cursor_style == 2),
                ],
                cx,
            );
            let size = self.slider(format!("screen-size:{sid}"), (0.5, 4.0, 0.1), look.cursor_size, move |v| set(ScreenLook { cursor_size: v, ..look }), cx);
            let smooth = self.slider(format!("screen-smooth:{sid}"), (0.0, 1.0, 0.05), look.cursor_smoothing, move |v| set(ScreenLook { cursor_smoothing: v, ..look }), cx);
            let idle = self.slider(format!("screen-idle:{sid}"), (0.0, 10.0, 0.5), look.cursor_idle_s, move |v| set(ScreenLook { cursor_idle_s: v, ..look }), cx);
            let tilt = super::settings::switch("screen-tilt", look.cursor_tilt).on_click(cx.listener(move |this, _, _, cx| this.exec(set(ScreenLook { cursor_tilt: !look.cursor_tilt, ..look }), cx)));
            let current = super::layer_props::rgb_of(look.cursor_color);
            let mut swatches = div().flex().flex_wrap().gap_1p5();
            for (i, &rgb) in super::layer_props::PALETTE.iter().enumerate() {
                let cmd = set(ScreenLook { cursor_color: super::layer_props::abgr(rgb), ..look });
                swatches = swatches.child(
                    div()
                        .id(("screen-colour", i))
                        .size(px(20.))
                        .rounded(px(5.))
                        .border_2()
                        .border_color(if current == rgb { tok::accent_primary() } else { tok::border() })
                        .bg(rgb_colour(rgb))
                        .cursor_pointer()
                        .on_click(cx.listener(move |this, _, _, cx| this.exec(cmd.clone(), cx))),
                );
            }
            body = body
                .child(row("Look", style))
                .child(slider_row("Size", size, format!("{:.1}×", look.cursor_size)))
                .child(slider_row("Smoothing", smooth, format!("{:.0}%", look.cursor_smoothing * 100.0)))
                .child(slider_row("Hide when still", idle, if look.cursor_idle_s <= 0.0 { "never".into() } else { format!("{:.1} s", look.cursor_idle_s) }))
                .child(row("Tilt moving", div().flex().child(tilt)))
                .child(row("Colour", swatches))
                // ---- clicks
                .child(div().pt_2().text_size(px(11.)).font_weight(FontWeight::SEMIBOLD).text_color(tok::text_tertiary()).child("CLICKS"))
                .child(self.segmented(
                    "screen-clicks",
                    vec![
                        ("None", set(ScreenLook { clicks: 0, ..look }), look.clicks == 0),
                        ("Circle", set(ScreenLook { clicks: 1, ..look }), look.clicks == 1),
                        ("Ripple", set(ScreenLook { clicks: 2, ..look }), look.clicks == 2),
                    ],
                    cx,
                ));
        }
        body
    }
}

fn rgb_colour(rgb: u32) -> Rgba {
    gpui_kit::rgb(rgb)
}
