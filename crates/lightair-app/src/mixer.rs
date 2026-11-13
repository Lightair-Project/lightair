//! The audio mixer (docs/10-audio.md): one channel strip per track, with effect list, meter,
//! fader, Cue and Mute. Meters are read from the engine's lock-free cells every frame.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::time::Instant;

use gpui_kit::component::Sizable;
use gpui_kit::component::button::ButtonVariants;
use gpui_kit::component::menu::{ContextMenuExt, DropdownMenu};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;
use lightair_core::{AudioMix, Bus, Command, Effect, Id, Input, Master, MixerStrip, Project, Track};
use lightair_engine::meters::meters;
use serde_json::json;

use crate::studio::Studio;
use crate::ui::{self, tok};

const STRIP_W: f32 = 166.0;
const DB_MIN: f32 = -60.0;
const DB_MAX: f32 = 12.0;

/// Fader position (0 = bottom, 1 = top) for a dB value: linear in dB, −60…+12.
fn db_to_pos(db: f32) -> f32 {
    if !db.is_finite() { 0.0 } else { ((db - DB_MIN) / (DB_MAX - DB_MIN)).clamp(0.0, 1.0) }
}

fn pos_to_db(pos: f32) -> f32 {
    if pos <= 0.001 { f32::NEG_INFINITY } else { DB_MIN + pos.clamp(0.0, 1.0) * (DB_MAX - DB_MIN) }
}

/// Meter height fraction for a level (scale 0 to −60 dBFS).
fn meter_frac(db: f32) -> f32 {
    if !db.is_finite() { 0.0 } else { ((db + 60.0) / 60.0).clamp(0.0, 1.0) }
}

/// The vocal chain new microphone tracks get (docs/10-audio.md, Voice Clarity): the engine's
/// one-knob chain (noise, expander, de-esser, EQ, compressor, limiter).
/// The commands that set the mixer up as a template. Podcasts take an interface's channels as
/// mics when there's one with more than two (a stereo USB mic isn't two people); music takes a
/// two-channel one too (vocal and instrument). Otherwise the default input.
pub fn template_command(project: &lightair_core::Project, t: lightair_core::templates::Template) -> Command {
    use lightair_core::templates::{Mics, Template};
    let least = match t {
        Template::Podcast => 3,
        Template::Music => 2,
        _ => u16::MAX,
    };
    let devices = lightair_engine::devices::input_devices();
    let mics = devices
        .iter()
        .filter(|d| d.channels >= least)
        .max_by_key(|d| d.channels)
        .map(|d| Mics { kind: lightair_engine::devices::DEVICE_KIND.into(), device: d.name.clone(), channels: d.channels })
        .unwrap_or(Mics { kind: lightair_engine::devices::DEVICE_KIND.into(), device: String::new(), channels: 1 });
    lightair_core::templates::apply(project, t, &mics)
}

pub fn voice_clarity() -> Vec<Effect> {
    lightair_core::catalog::def("audio.voice").map(|d| vec![d.create()]).unwrap_or_default()
}

pub struct MixerPanel {
    studio: Entity<Studio>,
    faders: Rc<RefCell<HashMap<Id, Bounds<Pixels>>>>,
    dragging: Option<Id>,
    /// Displayed (decaying) peak per track and channel, plus the time of the last frame.
    display: HashMap<Id, [f32; 2]>,
    last_frame: Instant,
    add_track_menu: bool,
    /// When a click outside last closed the add menu (that click on + mustn't reopen it).
    menu_closed: Option<Instant>,
    focus: FocusHandle,
}

impl MixerPanel {
    pub fn new(studio: Entity<Studio>, cx: &mut Context<Self>) -> Self {
        cx.observe(&studio, |_, _, cx| cx.notify()).detach();
        Self {
            studio,
            faders: Rc::default(),
            dragging: None,
            display: HashMap::new(),
            last_frame: Instant::now(),
            add_track_menu: false,
            menu_closed: None,
            focus: cx.focus_handle(),
        }
    }

    pub fn toggle_add_track(&mut self, _: &mut Window, cx: &mut Context<Self>) {
        if self.menu_closed.take().is_some_and(|t| t.elapsed().as_millis() < 300) {
            return;
        }
        self.add_track_menu = !self.add_track_menu;
        cx.notify();
    }

    fn exec(&self, cmd: Command, cx: &mut Context<Self>) {
        self.studio.update(cx, |s, cx| {
            let _ = s.execute(cmd, cx);
        });
    }

    fn fader_to(&self, track: Id, y: Pixels, cx: &mut Context<Self>) {
        let Some(b) = self.faders.borrow().get(&track).copied() else { return };
        let top = f32::from(b.origin.y);
        let h = f32::from(b.size.height).max(1.0);
        let pos = 1.0 - ((f32::from(y) - top) / h).clamp(0.0, 1.0);
        let db = (pos_to_db(pos) * 2.0).round() / 2.0;
        let project = self.studio.read(cx).project().clone();
        if let Some(cmd) = MixerStrip::find(&project, track).and_then(|k| volume_cmd(&project, k, db)) {
            self.exec(cmd, cx);
        }
    }

    fn add_track(
        &mut self,
        name: &str,
        kind: &str,
        settings: serde_json::Value,
        effects: Vec<Effect>,
        cx: &mut Context<Self>,
    ) {
        self.add_track_menu = false;
        let input = Input::new(name, kind, settings);
        let mut track = Track::new(name, input.id);
        track.effects = effects;
        let cmd = Command::Batch {
            label: format!("Add {name} track"),
            commands: vec![Command::AddInput { input }, Command::AddTrack { track, index: None }],
        };
        self.exec(cmd, cx);
    }
}

impl Render for MixerPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        window.request_animation_frame();
        let now = Instant::now();
        let dt = now.duration_since(self.last_frame).as_secs_f32().min(0.1);
        self.last_frame = now;

        let project = self.studio.read(cx).project().clone();
        let mut strips = Vec::new();
        let views = strip_views(&project);
        for (i, view) in views.iter().enumerate() {
            // Meter ballistics: rise instantly, fall at 24 dB/s.
            let levels = meters().get(view.id).map(|c| c.levels());
            let shown = self.display.entry(view.id).or_insert([f32::NEG_INFINITY; 2]);
            for (c, value) in shown.iter_mut().enumerate() {
                let target = levels.map_or(f32::NEG_INFINITY, |l| l.peak[c]);
                let fallen = if value.is_finite() { *value - 24.0 * dt } else { f32::NEG_INFINITY };
                *value = if target > fallen { target } else { fallen.max(-90.0) };
            }
            let shown = *shown;
            let strip = self.strip(i, view, shown, cx);
            let menu = StripMenu::of(&project, view);
            if i > 0 && views[i - 1].kind_order() != view.kind_order() {
                // buses, the master and mixes each start a group
                strips.push(div().flex_none().w(px(1.)).h_full().bg(tok::border()).into_any_element());
            }
            strips.push(
                div()
                    .id(("strip-menu", i))
                    .flex_none()
                    .h_full()
                    .child(strip)
                    .on_mouse_down(MouseButton::Right, {
                        let (studio, id) = (self.studio.downgrade(), view.id);
                        move |_, _, cx| {
                            if let Some(s) = studio.upgrade() {
                                s.update(cx, |s, cx| {
                                    s.select_track(Some(id));
                                    cx.notify();
                                });
                            }
                        }
                    })
                    .context_menu({
                        let f = strip_menu(self.studio.downgrade(), menu);
                        move |m, _, _| f(m)
                    })
                    .into_any_element(),
            );
        }
        self.display.retain(|id, _| MixerStrip::find(&project, *id).is_some());

        div()
            .id("mixer")
            .flex()
            .flex_col()
            .size_full()
            .on_mouse_move(cx.listener(|this, ev: &MouseMoveEvent, _, cx| {
                if let (Some(track), Some(MouseButton::Left)) = (this.dragging, ev.pressed_button) {
                    this.fader_to(track, ev.position.y, cx);
                }
            }))
            .on_mouse_up(MouseButton::Left, cx.listener(|this, _, _, cx| this.end_drag(cx)))
            .on_mouse_up_out(MouseButton::Left, cx.listener(|this, _, _, cx| this.end_drag(cx)))
            .relative()
            .child(
                div()
                    .id("strips")
                    .flex()
                    .flex_1()
                    .min_h_0()
                    .gap_2()
                    .px_3()
                    .py_2()
                    .overflow_scroll()
                    .children(strips)
                    .when(project.tracks.is_empty(), |d| {
                        d.child(div().text_color(tok::text_tertiary()).child("No tracks"))
                    }),
            )
            .when(self.add_track_menu, |d| d.child(self.add_menu(cx)))
    }
}

impl MixerPanel {
    fn end_drag(&mut self, cx: &mut Context<Self>) {
        if self.dragging.take().is_some() {
            self.studio.update(cx, |s, cx| {
                s.end_gesture();
                cx.notify();
            });
        }
    }

    /// The add menu: a card under the panel's + button, in two columns (what a track listens to,
    /// and the mixer's own strips and templates). Any click outside closes it.
    fn add_menu(&self, cx: &mut Context<Self>) -> impl IntoElement {
        fn heading(label: &'static str) -> Div {
            div()
                .px_2()
                .pt_1()
                .pb_1()
                .text_size(px(10.5))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(tok::text_tertiary())
                .child(label.to_uppercase())
        }
        fn entry(id: impl Into<ElementId>, icon: &str, title: impl Into<SharedString>, detail: impl Into<SharedString>) -> Stateful<Div> {
            div()
                .id(id)
                .flex()
                .items_center()
                .gap_2p5()
                .px_2()
                .py_1p5()
                .rounded(px(7.))
                .cursor_pointer()
                .hover(|d| d.bg(tok::bg_selected()))
                .child(
                    div()
                        .flex()
                        .flex_none()
                        .items_center()
                        .justify_center()
                        .size(px(28.))
                        .rounded(px(7.))
                        .bg(tok::bg_raised())
                        .child(ui::icon(icon).size(px(14.)).text_color(tok::text_secondary())),
                )
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .flex_1()
                        .min_w_0()
                        .child(div().text_size(px(12.)).text_color(tok::text()).truncate().child(title.into()))
                        .child(div().text_size(px(11.)).text_color(tok::text_tertiary()).truncate().child(detail.into())),
                )
        }
        let column = || div().flex().flex_col().gap_0p5().flex_1().min_w_0();

        let sources = column()
            .child(heading("Sources"))
            .child(entry("add-mic", "mic", "Audio input", "Any mic or audio interface").on_click(cx.listener(
                |this, _, _, cx| {
                    // captured by the engine directly (low latency)
                    this.add_track("Mic", lightair_engine::devices::DEVICE_KIND, json!({"device": ""}), voice_clarity(), cx)
                },
            )))
            .child(entry("add-desktop", "volume-2", "Desktop audio", "Everything the Mac plays").on_click(cx.listener(
                |this, _, _, cx| this.add_track("Desktop audio", "sck_audio_capture", json!({"type": 0}), vec![], cx),
            )))
            .child(entry("add-app", "app-window", "App audio", "One app: a game, music, a call").on_click(cx.listener(
                |this, _, _, cx| this.add_track("App audio", "sck_audio_capture", json!({"type": 1}), vec![], cx),
            )));

        let mixer = column()
            .child(heading("Mixer"))
            .child(entry("add-bus", "group", "Bus", "Group tracks, or share a reverb").on_click(cx.listener(
                |this, _, _, cx| {
                    this.add_track_menu = false;
                    let bus = crate::shell::mixer_props::new_bus(this.studio.read(cx).project());
                    let id = bus.id;
                    this.exec(Command::AddBus { bus, index: None }, cx);
                    this.studio.update(cx, |s, cx| {
                        s.select_track(Some(id));
                        cx.notify();
                    });
                },
            )))
            .child(entry("add-mix", "split", "Mix", "Headphones, VOD, a mix-minus").on_click(cx.listener(
                |this, _, _, cx| {
                    this.add_track_menu = false;
                    let mix = crate::shell::mixer_props::new_mix(this.studio.read(cx).project());
                    let id = mix.id;
                    this.exec(Command::AddMix { mix }, cx);
                    this.studio.update(cx, |s, cx| {
                        s.select_track(Some(id));
                        cx.notify();
                    });
                },
            )))
            .child(div().mx_2().my_1().h(px(1.)).bg(tok::border()))
            .child(entry("add-template", "layout-template", "Templates…", "A mixer set up for your show").on_click(
                cx.listener(|this, _, window, cx| {
                    this.add_track_menu = false;
                    window.dispatch_action(Box::new(crate::shell::OpenAudioSettings), cx);
                    cx.notify();
                }),
            ));

        // pinned under the + button, above everything, kept inside the window
        let card = div()
                    .id("add-menu")
                    .w(px(500.))
                    .flex()
                    .gap_1()
                    .p_1p5()
                    .rounded(px(12.))
                    .bg(tok::bg_panel())
                    .border_1()
                    .border_color(tok::border_strong())
                    .shadow_lg()
                    .occlude()
                    .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                        this.add_track_menu = false;
                        this.menu_closed = Some(Instant::now());
                        cx.notify();
                    }))
                    .child(sources)
                    .child(div().w(px(1.)).my_1().bg(tok::border()))
                    .child(mixer);
        div().absolute().top(px(4.)).right(px(10.)).size_0().child(deferred(
            anchored().anchor(Anchor::TopRight).snap_to_window_with_margin(px(8.)).child(card),
        ))
    }

    fn strip(&self, i: usize, view: &StripView, shown: [f32; 2], cx: &mut Context<Self>) -> impl IntoElement {
        let id = view.id;
        let kind = view.kind;
        let (muted, cue, volume) = (view.muted, view.cue, view.volume_db);
        let selected = self.studio.read(cx).selected_track == Some(id);
        let project = self.studio.read(cx).project().clone();
        let unity = volume_cmd(&project, kind, 0.0);
        let mute = mute_cmd(&project, kind, !muted);
        let studio = self.studio.downgrade();
        let request = move |req: crate::studio::UiRequest, cx: &mut App| {
            if let Some(s) = studio.upgrade() {
                s.update(cx, |s, cx| {
                    s.ui_request = Some(req);
                    cx.notify();
                });
            }
        };

        // ---- header: the name, and its menu
        let menu = strip_menu(self.studio.downgrade(), StripMenu::of(&project, view));
        let header = div()
            .flex()
            .flex_none()
            .items_center()
            .h(px(30.))
            .pl_2()
            .pr_1()
            .gap_1()
            .border_b_1()
            .border_color(tok::border())
            .child(div().w(px(18.)).flex_none().when_some(view.badge, |d, b| {
                d.child(div().text_size(px(8.5)).font_weight(FontWeight::BOLD).text_color(tok::text_tertiary()).child(b))
            }))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .text_center()
                    .text_size(px(12.5))
                    .font_weight(FontWeight::SEMIBOLD)
                    .truncate()
                    .child(view.name.clone()),
            )
            .child(
                gpui_kit::component::button::Button::new(("strip-chevron", i))
                    .icon(ui::icon("chevron-down"))
                    .ghost()
                    .xsmall()
                    .dropdown_menu(move |m, _, _| menu(m)),
            );

        // ---- effects: a dot to switch each on or off, its name, a button to open it; + adds one
        // gives way to the meter in a short panel
        let mut fx = div()
            .id(("strip-fx", i))
            .flex()
            .flex_col()
            .flex_basis(px(92.))
            .flex_shrink(1.0)
            .min_h(px(50.))
            .overflow_y_scroll()
            .border_b_1()
            .border_color(tok::border());
        for (n, (eid, name, on)) in view.effects.iter().cloned().enumerate() {
            let cmd = Command::SetStripEffectEnabled { strip: kind, effect: eid, enabled: !on };
            let open = request.clone();
            fx = fx.child(
                div()
                    .flex()
                    .flex_none()
                    .items_center()
                    .h(px(24.))
                    .px_1()
                    .gap_1()
                    .border_b_1()
                    .border_color(tok::border())
                    .child(
                        div()
                            .id(("fx-dot", i * 64 + n))
                            .flex()
                            .items_center()
                            .justify_center()
                            .size(px(18.))
                            .cursor_pointer()
                            .child(div().size(px(8.)).rounded_full().bg(if on { tok::ok() } else { tok::text_tertiary() }))
                            .tooltip(move |w, cx| {
                                gpui_kit::component::tooltip::Tooltip::new(if on { "Bypass" } else { "Turn on" }).build(w, cx)
                            })
                            .on_click(cx.listener(move |this, _, _, cx| {
                                cx.stop_propagation();
                                this.exec(cmd.clone(), cx);
                            })),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .text_center()
                            .text_size(px(11.5))
                            .text_color(if on { tok::text() } else { tok::text_tertiary() })
                            .truncate()
                            .child(name),
                    )
                    .child(
                        div()
                            .id(("fx-edit", i * 64 + n))
                            .flex()
                            .items_center()
                            .justify_center()
                            .size(px(18.))
                            .rounded(px(4.))
                            .cursor_pointer()
                            .hover(|d| d.bg(tok::bg_selected()))
                            .child(ui::icon("sliders-horizontal").size(px(12.)).text_color(tok::text_secondary()))
                            .on_click(move |_, _, cx| {
                                cx.stop_propagation();
                                open(crate::studio::UiRequest::OpenEffect(kind, eid), cx);
                            }),
                    ),
            );
        }
        let add = request.clone();
        fx = fx.child(
            div()
                .id(("fx-add", i))
                .flex()
                .flex_1()
                .min_h(px(24.))
                .items_center()
                .justify_center()
                .cursor_pointer()
                .hover(|d| d.bg(tok::bg_selected()))
                .child(ui::icon("plus").size(px(14.)).text_color(tok::text_secondary()))
                .on_click(move |_, _, cx| {
                    cx.stop_propagation();
                    add(crate::studio::UiRequest::AddEffect(kind), cx);
                }),
        );

        // ---- the stereo meter: two bars, a dB scale beside them, the peak above
        let bar = |db: f32| {
            let frac = meter_frac(db);
            // green below −20, yellow −20…−9, red above −9 dBFS
            let (g_end, y_end) = (meter_frac(-20.0), meter_frac(-9.0));
            let seg = |from: f32, to: f32, color: Hsla| {
                let h = (frac.min(to) - from).max(0.0);
                div().absolute().left_0().right_0().bottom(relative(from)).h(relative(h)).bg(color)
            };
            div()
                .relative()
                .w(px(6.))
                .h_full()
                .bg(tok::bg_panel())
                .child(seg(0.0, g_end, tok::ok()))
                .child(seg(g_end, y_end, tok::warn()))
                .child(seg(y_end, 1.0, tok::error()))
        };
        let mut scale = div().relative().flex_1().h_full();
        for db in [0, -6, -12, -18, -24, -30, -36, -42, -48, -54] {
            scale = scale.child(
                div()
                    .absolute()
                    .right(px(2.))
                    .bottom(relative(meter_frac(db as f32)))
                    .mb(px(-6.))
                    .text_size(px(8.5))
                    .font_family("Menlo")
                    .whitespace_nowrap()
                    .text_color(tok::text_tertiary())
                    .child(if db == 0 { "0".to_string() } else { format!("{db}") }),
            );
        }
        let peak = shown[0].max(shown[1]);
        let readout = |text: String| {
            div()
                .flex()
                .items_center()
                .justify_center()
                .h(px(18.))
                .rounded(px(3.))
                .text_size(px(10.))
                .font_family("Menlo")
                .child(text)
        };
        let meter = div()
            .flex()
            .flex_col()
            .gap_1()
            .w(px(50.))
            .h_full()
            .child(
                readout(if peak > -90.0 { format!("{peak:.1}") } else { "-inf".into() })
                    .bg(tok::bg_app())
                    .text_color(if peak > -1.0 { tok::error() } else { tok::text_secondary() }),
            )
            .child(
                div()
                    .flex()
                    .flex_1()
                    .min_h_0()
                    .gap(px(2.))
                    .p(px(3.))
                    .rounded(px(3.))
                    .bg(tok::bg_app())
                    .child(bar(shown[0]))
                    .child(bar(shown[1]))
                    .child(scale),
            );

        // ---- the fader: its value above, a long throw, a thumb you can grab
        let faders = self.faders.clone();
        let fader = div()
            .id(("fader", i))
            .relative()
            .flex_1()
            .min_h_0()
            .w(px(32.))
            .cursor_pointer()
            .child(
                canvas(
                    move |b, _, _| {
                        faders.borrow_mut().insert(id, b);
                    },
                    |_, _, _, _| {},
                )
                .absolute()
                .size_full(),
            )
            .child(div().absolute().left(px(14.)).top(px(4.)).bottom(px(4.)).w(px(4.)).rounded_full().bg(tok::bg_app()))
            // unity
            .child(div().absolute().left(px(6.)).w(px(20.)).h(px(1.)).top(relative(1.0 - db_to_pos(0.0))).bg(tok::text_tertiary()))
            .child(
                div()
                    .absolute()
                    .left_0()
                    .top(relative(1.0 - db_to_pos(volume)))
                    .mt(px(-9.))
                    .w(px(32.))
                    .h(px(18.))
                    .rounded(px(3.))
                    .bg(tok::text_secondary())
                    .border_1()
                    .border_color(tok::text())
                    .shadow_sm()
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(div().w(px(18.)).h(px(2.)).rounded_full().bg(tok::bg_app())),
            )
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, ev: &MouseDownEvent, _, cx| {
                    cx.stop_propagation();
                    if ev.click_count >= 2 {
                        if let Some(cmd) = unity.clone() {
                            this.exec(cmd, cx);
                        }
                        return;
                    }
                    this.studio.update(cx, |s, _| s.begin_gesture());
                    this.dragging = Some(id);
                    this.fader_to(id, ev.position.y, cx);
                }),
            );
        let fader_col = div()
            .flex()
            .flex_col()
            .items_center()
            .gap_1()
            .h_full()
            .child(readout(if volume.is_finite() { format!("{volume:.1}") } else { "-inf".into() }).w(px(36.)).text_color(tok::text()))
            .child(fader);

        // ---- the buttons
        let button = |key: &'static str, label: &'static str, on: bool, on_bg: Hsla| {
            div()
                .id((key, i))
                .flex()
                .items_center()
                .justify_center()
                .w(px(44.))
                .h(px(26.))
                .rounded(px(5.))
                .border_1()
                .border_color(if on { on_bg } else { tok::border_strong() })
                .bg(if on { on_bg } else { tok::bg_app() })
                .text_size(px(10.5))
                .font_weight(FontWeight::BOLD)
                .text_color(if on { tok::text_inverse() } else { tok::text_secondary() })
                .cursor_pointer()
                .hover(|d| d.opacity(0.85))
                .child(label)
        };
        let buttons = div()
            .flex()
            .flex_col()
            .gap_1p5()
            .h_full()
            .child(div().flex_1())
            .when_some(cue, |d, cue| {
                d.child(button("cue", "CUE", cue, tok::text()).on_click(cx.listener(move |this, _, _, cx| {
                    cx.stop_propagation();
                    this.exec(Command::SetTrackCue { track: id, cue: !cue }, cx)
                })))
            })
            .child(button("mute", "MUTE", muted, tok::live()).on_click(cx.listener(move |this, _, _, cx| {
                cx.stop_propagation();
                if let Some(cmd) = mute.clone() {
                    this.exec(cmd, cx);
                }
            })));

        let levels = div()
            .flex()
            .flex_1()
            .min_h(px(132.))
            .gap_2()
            .px_2()
            .py_2()
            .child(meter)
            .child(fader_col)
            .child(div().flex_1())
            .child(buttons);

        div()
            .id(("strip", i))
            .flex()
            .flex_col()
            .flex_none()
            .w(px(STRIP_W))
            .h_full()
            .min_h(px(266.))
            .rounded(px(8.))
            .overflow_hidden()
            .bg(tok::bg_raised())
            .border_1()
            .border_color(if selected { tok::accent_primary() } else { tok::border() })
            .on_click(cx.listener(move |this, _, _, cx| {
                this.studio.update(cx, |s, cx| {
                    s.selected_layer = None;
                    s.select_track(Some(id));
                    cx.notify();
                })
            }))
            .child(header)
            .child(fx)
            .child(levels)
            .child(div().flex_none().px_2().pb_2().child(self.strip_source(i, view, cx)))
    }

    /// The strip's foot: a track's device (pick another here), or what the strip is.
    fn strip_source(&self, i: usize, view: &StripView, cx: &mut Context<Self>) -> AnyElement {
        let chip = |icon: &'static str, label: String| {
            div()
                .flex()
                .items_center()
                .gap_1p5()
                .h(px(28.))
                .px_2()
                .rounded(px(6.))
                .border_1()
                .border_color(tok::border_strong())
                .text_size(px(11.5))
                .text_color(tok::text_secondary())
                .child(ui::icon(icon).size(px(13.)).flex_none())
                .child(div().flex_1().min_w_0().truncate().child(label))
        };
        match &view.source {
            Some(src) => {
                let (track, current) = (view.id, src.current.clone());
                let me = cx.entity().downgrade();
                gpui_kit::component::button::Button::new(("strip-source", i))
                    .icon(ui::icon(src.icon))
                    .label(src.label.clone())
                    .small()
                    .outline()
                    .w_full()
                    .dropdown_caret(true)
                    .dropdown_menu(move |menu, window, cx| {
                        use gpui_kit::component::menu::PopupMenuItem;
                        let pick = move |to: Source, app: String, label: String, me: WeakEntity<MixerPanel>, on: bool| {
                            source_item(track, to, app, label, me, on)
                        };
                        let mut menu = menu.item(PopupMenuItem::label("Microphones and interfaces"));
                        let mut devices = vec![(String::new(), "Default mic (as set in macOS)".to_string())];
                        devices.extend(lightair_engine::devices::input_devices().into_iter().map(|d| {
                            let n = if d.channels == 1 { "1 input".to_string() } else { format!("{} inputs", d.channels) };
                            (d.name.clone(), format!("{} · {n}", d.name))
                        }));
                        for (value, label) in devices {
                            let to = Source::Device(value);
                            let on = to == current;
                            menu = menu.item(pick(to, String::new(), label, me.clone(), on));
                        }
                        menu = menu.separator().item(pick(
                            Source::Desktop,
                            String::new(),
                            "Desktop audio".into(),
                            me.clone(),
                            current == Source::Desktop,
                        ));
                        let (me2, current2) = (me.clone(), current.clone());
                        menu.submenu("App", window, cx, move |mut sub, _, _| {
                            let apps = running_apps();
                            if apps.is_empty() {
                                sub = sub.item(PopupMenuItem::new("No apps open").disabled(true));
                            }
                            for (name, bundle) in apps {
                                let to = Source::App(bundle);
                                let on = to == current2;
                                sub = sub.item(source_item(track, to, name.clone(), name, me2.clone(), on));
                            }
                            sub
                        })
                    })
                    .into_any_element()
            }
            None => chip(
                match view.kind {
                    MixerStrip::Bus(_) => "group",
                    MixerStrip::Mix(_) => "split",
                    _ => "radio",
                },
                match view.kind {
                    MixerStrip::Bus(_) => "Bus".into(),
                    MixerStrip::Mix(_) => "Mix".into(),
                    _ => "Stream & recording".into(),
                },
            )
            .into_any_element(),
        }
    }
}

/// Stream loudness (EBU R128): short-term and integrated, against the common −14 LUFS target.
/// Click to start integrated loudness over.
fn loudness_readout(cx: &mut Context<MixerPanel>) -> Option<impl IntoElement> {
    let r = lightair_engine::loudness::stream_loudness()?;
    let show = |v: f64| if v.is_finite() { format!("{v:.1}") } else { "—".into() };
    // Against the loudness target (Settings › Audio): within 2 LU is on target; well below
    // sounds quiet; above gets turned down by the platform.
    let target = crate::prefs::LOUDNESS_TARGET.load(std::sync::atomic::Ordering::Relaxed) as f64;
    let colour = match r.short_term {
        v if !v.is_finite() || v < target - 10.0 => tok::text_tertiary(),
        v if v > target + 4.0 => tok::warn(),
        v if (v - target).abs() <= 2.0 => tok::ok(),
        _ => tok::text_secondary(),
    };
    Some(
        div()
            .id("loudness")
            .flex()
            .items_baseline()
            .gap_2()
            .px_2()
            .py_0p5()
            .rounded(px(5.))
            .cursor_pointer()
            .hover(|d| d.bg(tok::bg_raised()))
            .font_family("Menlo")
            .text_size(px(11.))
            .child(div().text_color(tok::text_tertiary()).child("LUFS"))
            .child(div().text_color(colour).child(format!("S {}", show(r.short_term))))
            .child(div().text_color(tok::text_secondary()).child(format!("I {}", show(r.integrated))))
            .tooltip(|w, cx| {
                gpui_kit::component::tooltip::Tooltip::new(
                    "Stream loudness: short-term (3 s) and integrated, green on your target (Settings › Audio). Click to reset integrated.",
                )
                .build(w, cx)
            })
            .on_click(cx.listener(|_, _, _, cx| {
                lightair_engine::loudness::reset_stream_loudness();
                cx.notify();
            })),
    )
}

// ---------------------------------------------------------------- dock panel
impl gpui_kit::component::dock::BasePanel for MixerPanel {
    fn panel_name(&self) -> &'static str {
        "mixer"
    }
}

impl gpui_kit::component::dock::Panel for MixerPanel {
    /// None: the tab shows `title` (icon and name).
    fn tab_name(&self, _: &App) -> Option<SharedString> {
        None
    }

    fn title(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        ui::panel_title("audio-lines", "Audio Mixer")
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
            gpui_kit::component::button::Button::new("mixer-add")
                .icon(ui::icon("plus"))
                .ghost()
                .xsmall()
                .tooltip("Add a track, bus or mix")
                .on_click(cx.listener(|this, _, window, cx| this.toggle_add_track(window, cx))),
        ])
    }

    fn title_suffix(&mut self, _: &mut Window, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        loudness_readout(cx)
    }

    fn zoom_control(&self, _: &App) -> Option<gpui_kit::component::dock::PanelControl> {
        None
    }

    fn inner_padding(&self, _: &App) -> bool {
        false
    }
}

impl EventEmitter<gpui_kit::component::dock::PanelEvent> for MixerPanel {}

impl Focusable for MixerPanel {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

/// What a strip's menu offers (its commands, worked out from the project).
#[derive(Clone)]
struct StripMenu {
    muted: bool,
    mute: Option<Command>,
    cue: Option<(bool, Command)>,
    reset: Option<Command>,
    remove: Option<(&'static str, Command)>,
}

impl StripMenu {
    fn of(p: &Project, v: &StripView) -> Self {
        Self {
            muted: v.muted,
            mute: mute_cmd(p, v.kind, !v.muted),
            cue: v.cue.map(|c| (c, Command::SetTrackCue { track: v.id, cue: !c })),
            reset: (v.volume_db.abs() >= 0.05).then(|| volume_cmd(p, v.kind, 0.0)).flatten(),
            remove: match v.kind {
                MixerStrip::Track(track) => Some(("Remove track", Command::RemoveTrack { track })),
                MixerStrip::Bus(bus) => Some(("Remove bus", Command::RemoveBus { bus })),
                MixerStrip::Mix(mix) => Some(("Remove mix", Command::RemoveMix { mix })),
                MixerStrip::Master => None,
            },
        }
    }
}

/// A strip's menu (its ⌄ and right-click): mute, listen, back to 0 dB, remove.
fn strip_menu(
    studio: WeakEntity<Studio>,
    m: StripMenu,
) -> impl Fn(gpui_kit::component::menu::PopupMenu) -> gpui_kit::component::menu::PopupMenu + 'static {
    use gpui_kit::component::menu::PopupMenuItem;
    move |mut menu| {
        let run = |cmd: Command| {
            let studio = studio.clone();
            move |_: &ClickEvent, _: &mut Window, cx: &mut App| {
                if let Some(s) = studio.upgrade() {
                    s.update(cx, |s, cx| {
                        let _ = s.execute(cmd.clone(), cx);
                    });
                }
            }
        };
        if let Some(cmd) = m.mute.clone() {
            menu = menu.item(
                PopupMenuItem::new(if m.muted { "Unmute" } else { "Mute" })
                    .icon(crate::ui::icon(if m.muted { "volume-2" } else { "volume-x" }))
                    .on_click(run(cmd)),
            );
        }
        if let Some((on, cmd)) = m.cue.clone() {
            menu = menu.item(
                PopupMenuItem::new("Listen in headphones").icon(crate::ui::icon("headphones")).checked(on).on_click(run(cmd)),
            );
        }
        menu = match m.reset.clone() {
            Some(cmd) => menu.item(PopupMenuItem::new("Reset to 0 dB").on_click(run(cmd))),
            None => menu.item(PopupMenuItem::new("Reset to 0 dB").disabled(true)),
        };
        if let Some((label, cmd)) = m.remove.clone() {
            menu = menu.separator().item(PopupMenuItem::new(label).icon(crate::ui::icon("trash")).on_click(run(cmd)));
        }
        menu
    }
}

/// What a mixer strip shows: a track, a bus, the master or a mix.
struct StripView {
    id: Id,
    kind: MixerStrip,
    name: String,
    volume_db: f32,
    muted: bool,
    /// Tracks have a cue button.
    cue: Option<bool>,
    /// Its effects: id, name, on.
    effects: Vec<(Id, String, bool)>,
    badge: Option<&'static str>,
    /// A track's input (buses, mixes and the master have none).
    source: Option<StripSource>,
}

/// What a track listens to, for the strip's foot.
struct StripSource {
    icon: &'static str,
    label: String,
    current: Source,
}

/// A track's source, as the strip's source menu offers them.
#[derive(Clone, PartialEq)]
enum Source {
    /// An audio interface the engine opens ("" for the system default).
    Device(String),
    /// Everything the Mac plays.
    Desktop,
    /// One app (its bundle id; "" until one is chosen).
    App(String),
    /// Something else (a media layer, an older capture): kept until another is chosen.
    Other,
}

impl Source {
    fn of(i: &Input) -> Self {
        match i.kind.as_str() {
            lightair_engine::devices::DEVICE_KIND => Source::Device(i.settings["device"].as_str().unwrap_or("").into()),
            "sck_audio_capture" if i.settings["type"].as_i64() == Some(1) => {
                Source::App(i.settings["application"].as_str().unwrap_or("").into())
            }
            "sck_audio_capture" => Source::Desktop,
            _ => Source::Other,
        }
    }

    /// The input it needs: name, kind, settings.
    fn input(&self, app_name: &str) -> (String, &'static str, serde_json::Value) {
        match self {
            Source::Device(d) => (
                if d.is_empty() { "Microphone".into() } else { d.clone() },
                lightair_engine::devices::DEVICE_KIND,
                json!({ "device": d }),
            ),
            Source::Desktop => ("Desktop audio".into(), "sck_audio_capture", json!({ "type": 0 })),
            Source::App(b) => (app_name.to_string(), "sck_audio_capture", json!({ "type": 1, "application": b })),
            Source::Other => (String::new(), "", json!({})),
        }
    }
}

/// A source menu entry: picking it points `track` at `to`.
fn source_item(
    track: Id,
    to: Source,
    app: String,
    label: String,
    me: WeakEntity<MixerPanel>,
    on: bool,
) -> gpui_kit::component::menu::PopupMenuItem {
    gpui_kit::component::menu::PopupMenuItem::new(label).checked(on).on_click(move |_, _, cx| {
        if let Some(me) = me.upgrade() {
            me.update(cx, |this, cx| {
                let p = this.studio.read(cx).project().clone();
                if let Some(cmd) = source_cmd(&p, track, &to, &app) {
                    this.exec(cmd, cx);
                }
            });
        }
    })
}

/// The apps that are open (the Dock's), name and bundle id, by name; not Lightair itself.
fn running_apps() -> Vec<(String, String)> {
    use objc2::msg_send;
    use objc2::rc::Retained;
    use objc2::runtime::{AnyClass, AnyObject};
    use objc2_foundation::NSString;
    let mut out = Vec::new();
    let Some(class) = AnyClass::get(c"NSWorkspace") else { return out };
    unsafe {
        let ws: Retained<AnyObject> = msg_send![class, sharedWorkspace];
        let apps: Retained<AnyObject> = msg_send![&*ws, runningApplications];
        let n: usize = msg_send![&*apps, count];
        for k in 0..n {
            let app: Retained<AnyObject> = msg_send![&*apps, objectAtIndex: k];
            let policy: isize = msg_send![&*app, activationPolicy];
            let pid: i32 = msg_send![&*app, processIdentifier];
            if policy != 0 || pid as u32 == std::process::id() {
                continue;
            }
            let name: Option<Retained<NSString>> = msg_send![&*app, localizedName];
            let bundle: Option<Retained<NSString>> = msg_send![&*app, bundleIdentifier];
            if let (Some(name), Some(bundle)) = (name, bundle) {
                out.push((name.to_string(), bundle.to_string()));
            }
        }
    }
    out.sort_by_key(|(n, _)| n.to_lowercase());
    out.dedup_by(|a, b| a.1 == b.1);
    out
}

/// The commands that make `track` listen to `to`: the same kind of input changes its settings;
/// another kind replaces the input (the old one goes if nothing else uses it). A track still
/// named after its old source takes the new one's name.
fn source_cmd(p: &Project, track: Id, to: &Source, app_name: &str) -> Option<Command> {
    let t = p.track(track)?;
    let old = p.inputs.get(&t.input)?;
    let (name, kind, settings) = to.input(app_name);
    if kind.is_empty() {
        return None;
    }
    let renamed = |t: &Track| if t.name == old.name || t.name == "Mic" { name.clone() } else { t.name.clone() };
    let mut commands = Vec::new();
    let mut new_track = Track { channels: lightair_core::TrackChannels::All, ..(**t).clone() };
    new_track.name = renamed(t);
    if old.kind == kind {
        let mut s = old.settings.clone();
        if let (Some(s), Some(n)) = (s.as_object_mut(), settings.as_object()) {
            for (k, v) in n {
                s.insert(k.clone(), v.clone());
            }
        }
        commands.push(Command::SetInputSettings { input: old.id, settings: s });
        if old.name != name {
            commands.push(Command::RenameInput { input: old.id, name: name.clone() });
        }
    } else {
        let input = Input::new(name.clone(), kind, settings);
        new_track.input = input.id;
        commands.push(Command::AddInput { input });
    }
    let switched = new_track.input != t.input;
    commands.push(Command::UpdateTrack { track: new_track });
    let shown = p.scenes.iter().any(|s| s.layers.iter().any(|l| l.input == Some(old.id)));
    let shared = p.tracks.iter().any(|o| o.id != track && o.input == old.id);
    if switched && !shown && !shared {
        commands.push(Command::RemoveInput { input: old.id });
    }
    Some(Command::Batch { label: "Change audio source".into(), commands })
}

impl StripView {
    fn kind_order(&self) -> u8 {
        match self.kind {
            MixerStrip::Track(_) => 0,
            MixerStrip::Bus(_) => 1,
            MixerStrip::Master => 2,
            MixerStrip::Mix(_) => 3,
        }
    }
}

/// Tracks, then buses, the master, and the mixes.
fn strip_views(p: &Project) -> Vec<StripView> {
    let on = |e: &[Effect]| e.iter().map(|e| (e.id, e.name.clone(), e.enabled)).collect::<Vec<_>>();
    let source = |t: &Track| {
        let i = p.inputs.get(&t.input)?;
        let current = Source::of(i);
        let (icon, label) = match &current {
            Source::Device(d) => ("mic", if d.is_empty() { "Default mic".into() } else { d.clone() }),
            Source::Desktop => ("volume-2", "Desktop audio".into()),
            Source::App(b) if b.is_empty() => ("app-window", "Pick an app".into()),
            Source::App(_) => ("app-window", i.name.clone()),
            Source::Other => (ui::input_icon(&i.kind), i.name.clone()),
        };
        Some(StripSource { icon, label, current })
    };
    let mut v: Vec<StripView> = p
        .tracks
        .iter()
        .map(|t| StripView {
            id: t.id,
            kind: MixerStrip::Track(t.id),
            name: t.name.clone(),
            volume_db: t.volume_db,
            muted: t.muted,
            cue: Some(t.cue),
            effects: on(&t.effects),
            badge: None,
            source: source(t),
        })
        .collect();
    v.extend(p.audio.buses.iter().map(|b| StripView {
        id: b.id,
        kind: MixerStrip::Bus(b.id),
        name: b.name.clone(),
        volume_db: b.volume_db,
        muted: b.muted,
        cue: None,
        effects: on(&b.effects),
        badge: Some("BUS"),
        source: None,
    }));
    let m = &p.audio.master;
    v.push(StripView {
        id: lightair_core::MASTER_ID,
        kind: MixerStrip::Master,
        name: "Program".into(),
        volume_db: m.volume_db,
        muted: m.muted,
        cue: None,
        effects: on(&m.effects),
        badge: None, // "Program" says it
        source: None,
    });
    v.extend(p.audio.mixes.iter().map(|x| StripView {
        id: x.id,
        kind: MixerStrip::Mix(x.id),
        name: x.name.clone(),
        volume_db: x.volume_db,
        muted: x.muted,
        cue: None,
        effects: on(&x.effects),
        badge: Some("MIX"),
        source: None,
    }));
    v
}

/// The command that sets a strip's fader.
fn volume_cmd(p: &Project, kind: MixerStrip, db: f32) -> Option<Command> {
    Some(match kind {
        MixerStrip::Track(track) => Command::SetTrackVolume { track, volume_db: db },
        MixerStrip::Bus(id) => Command::UpdateBus { bus: Bus { volume_db: db, ..p.audio.bus(id)?.clone() } },
        MixerStrip::Mix(id) => Command::UpdateMix { mix: AudioMix { volume_db: db, ..p.audio.mix(id)?.clone() } },
        MixerStrip::Master => Command::UpdateMaster { master: Master { volume_db: db, ..p.audio.master.clone() } },
    })
}

/// The command that mutes or unmutes a strip.
fn mute_cmd(p: &Project, kind: MixerStrip, muted: bool) -> Option<Command> {
    Some(match kind {
        MixerStrip::Track(track) => Command::SetTrackMuted { track, muted },
        MixerStrip::Bus(id) => Command::UpdateBus { bus: Bus { muted, ..p.audio.bus(id)?.clone() } },
        MixerStrip::Mix(id) => Command::UpdateMix { mix: AudioMix { muted, ..p.audio.mix(id)?.clone() } },
        MixerStrip::Master => Command::UpdateMaster { master: Master { muted, ..p.audio.master.clone() } },
    })
}
