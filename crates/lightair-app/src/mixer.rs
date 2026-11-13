//! The audio mixer (docs/10-audio.md): one channel strip per track, with effect list, meter,
//! fader, Cue and Mute. Meters are read from the engine's lock-free cells every frame.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::time::Instant;

use gpui_kit::component::menu::ContextMenuExt;
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;
use lightair_core::{AudioMix, Bus, Command, Effect, Id, Input, Master, MixerStrip, Project, Track};
use lightair_engine::meters::meters;
use serde_json::json;

use crate::studio::Studio;
use crate::ui::{self, tok};

const STRIP_W: f32 = 92.0;
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
            let track = project.track(view.id).cloned();
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
                        let studio = self.studio.downgrade();
                        move |m, _, _| match &track {
                            Some(t) => strip_menu(studio.clone(), t)(m),
                            None => m,
                        }
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
                    .overflow_x_scroll()
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
        let (muted, cue, volume) = (view.muted, view.cue, view.volume_db);
        let selected = self.studio.read(cx).selected_track == Some(id);
        let active_fx = view.fx;
        let project = self.studio.read(cx).project().clone();
        let unity = volume_cmd(&project, view.kind, 0.0);
        let mute = mute_cmd(&project, view.kind, !muted);

        // Meters and fader stretch to the strip's height: everything is in fractions of it.
        let meter_bar = |db: f32| {
            let frac = meter_frac(db);
            // Zones: green below −20, yellow −20…−9, red above −9 dBFS.
            let (g_end, y_end) = (meter_frac(-20.0), meter_frac(-9.0));
            let seg = |from: f32, to: f32, color: Hsla| {
                let h = (frac.min(to) - from).max(0.0);
                div().absolute().left_0().right_0().bottom(relative(from)).h(relative(h)).bg(color)
            };
            div()
                .relative()
                .w(px(5.))
                .h_full()
                .rounded(px(2.))
                .bg(tok::bg_app())
                .child(seg(0.0, g_end, tok::ok()))
                .child(seg(g_end, y_end, tok::warn()))
                .child(seg(y_end, 1.0, tok::error()))
        };
        let faders = self.faders.clone();
        let fader = div()
            .id(("fader", i))
            .relative()
            .w(px(26.))
            .h_full()
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
            .child(div().absolute().left(px(12.)).top_0().bottom_0().w(px(2.)).rounded_full().bg(tok::border_strong()))
            // unity mark
            .child(
                div()
                    .absolute()
                    .left(px(5.))
                    .w(px(16.))
                    .h(px(1.))
                    .top(relative(1.0 - db_to_pos(0.0)))
                    .bg(tok::text_tertiary()),
            )
            .child(
                div()
                    .absolute()
                    .left(px(1.))
                    .top(relative(1.0 - db_to_pos(volume)))
                    .mt(px(-6.))
                    .w(px(24.))
                    .h(px(12.))
                    .rounded(px(3.))
                    .bg(tok::text())
                    .border_1()
                    .border_color(tok::bg_app()),
            )
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, ev: &MouseDownEvent, _, cx| {
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

        let toggle = |key: &'static str, icon: &'static str, tip: &'static str, on: bool, on_color: Hsla| {
            div()
                .id((key, i))
                .flex()
                .items_center()
                .justify_center()
                .size(px(26.))
                .rounded(px(5.))
                .cursor_pointer()
                .bg(if on { on_color } else { tok::bg_app() })
                .hover(|d| d.opacity(0.85))
                .child(ui::icon(icon).size(px(14.)).text_color(if on {
                    tok::text_inverse()
                } else {
                    tok::text_secondary()
                }))
                .tooltip(move |w, cx| gpui_kit::component::tooltip::Tooltip::new(tip).build(w, cx))
        };

        div()
            .id(("strip", i))
            .flex()
            .flex_col()
            .flex_none()
            .w(px(STRIP_W))
            .h_full()
            .p_2()
            .gap_2()
            .rounded(px(8.))
            .bg(tok::bg_raised())
            .border_1()
            .border_color(if selected { tok::accent_primary() } else { tok::bg_raised() })
            .cursor_pointer()
            .on_click(cx.listener(move |this, _, _, cx| {
                this.studio.update(cx, |s, cx| {
                    s.select_track(Some(id));
                    cx.notify();
                })
            }))
            // Name, and how many effects are on (the Inspector edits them).
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_1()
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .text_size(px(12.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .overflow_hidden()
                            .text_ellipsis()
                            .child(view.name.clone()),
                    )
                    .when_some(view.badge, |d, b| {
                        d.child(
                            div()
                                .px_1()
                                .rounded(px(3.))
                                .text_size(px(9.))
                                .font_weight(FontWeight::SEMIBOLD)
                                .bg(tok::bg_app())
                                .text_color(tok::text_tertiary())
                                .child(b),
                        )
                    })
                    .when(active_fx > 0, |d| {
                        d.child(
                            div()
                                .px_1()
                                .rounded(px(3.))
                                .text_size(px(9.))
                                .font_weight(FontWeight::SEMIBOLD)
                                .bg(tok::bg_selected())
                                .text_color(tok::text_secondary())
                                .child(format!("FX {active_fx}")),
                        )
                    }),
            )
            .child(
                div()
                    .flex()
                    .flex_1()
                    .min_h_0()
                    .justify_center()
                    .gap_1()
                    .child(meter_bar(shown[0]))
                    .child(meter_bar(shown[1]))
                    .child(div().w(px(6.)))
                    .child(fader),
            )
            .child(
                div()
                    .text_center()
                    .text_size(px(10.))
                    .font_family("Menlo")
                    .text_color(tok::text_secondary())
                    .child(if volume.is_finite() { format!("{volume:+.1} dB") } else { "−∞".into() }),
            )
            .child(
                div()
                    .flex()
                    .justify_center()
                    .gap_1()
                    .when_some(cue, |d, cue| {
                        d.child(toggle("cue", "headphones", "Listen in headphones", cue, tok::text()).on_click(
                            cx.listener(move |this, _, _, cx| {
                                cx.stop_propagation();
                                this.exec(Command::SetTrackCue { track: id, cue: !cue }, cx)
                            }),
                        ))
                    })
                    .child(
                        toggle("mute", if muted { "volume-x" } else { "volume-2" }, "Mute", muted, tok::live())
                            .on_click(cx.listener(move |this, _, _, cx| {
                                cx.stop_propagation();
                                if let Some(cmd) = mute.clone() {
                                    this.exec(cmd, cx);
                                }
                            })),
                    ),
            )
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

/// A strip's right-click menu: mute, listen, back to 0 dB, remove.
fn strip_menu(
    studio: WeakEntity<Studio>,
    track: &Track,
) -> impl Fn(gpui_kit::component::menu::PopupMenu) -> gpui_kit::component::menu::PopupMenu + 'static {
    use gpui_kit::component::menu::PopupMenuItem;
    let (id, muted, cue, at_zero) = (track.id, track.muted, track.cue, track.volume_db.abs() < 0.05);
    move |menu| {
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
        menu.item(
            PopupMenuItem::new(if muted { "Unmute" } else { "Mute" })
                .icon(crate::ui::icon(if muted { "volume-2" } else { "volume-x" }))
                .on_click(run(Command::SetTrackMuted { track: id, muted: !muted })),
        )
        .item(
            PopupMenuItem::new("Listen in headphones")
                .icon(crate::ui::icon("headphones"))
                .checked(cue)
                .on_click(run(Command::SetTrackCue { track: id, cue: !cue })),
        )
        .item(
            PopupMenuItem::new("Reset to 0 dB")
                .disabled(at_zero)
                .on_click(run(Command::SetTrackVolume { track: id, volume_db: 0.0 })),
        )
        .separator()
        .item(
            PopupMenuItem::new("Remove track")
                .icon(crate::ui::icon("trash"))
                .on_click(run(Command::RemoveTrack { track: id })),
        )
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
    fx: usize,
    badge: Option<&'static str>,
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
    let on = |e: &[Effect]| e.iter().filter(|e| e.enabled).count();
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
            fx: on(&t.effects),
            badge: None,
        })
        .collect();
    v.extend(p.audio.buses.iter().map(|b| StripView {
        id: b.id,
        kind: MixerStrip::Bus(b.id),
        name: b.name.clone(),
        volume_db: b.volume_db,
        muted: b.muted,
        cue: None,
        fx: on(&b.effects),
        badge: Some("BUS"),
    }));
    let m = &p.audio.master;
    v.push(StripView {
        id: lightair_core::MASTER_ID,
        kind: MixerStrip::Master,
        name: "Program".into(),
        volume_db: m.volume_db,
        muted: m.muted,
        cue: None,
        fx: on(&m.effects),
        badge: None, // "Program" says it
    });
    v.extend(p.audio.mixes.iter().map(|x| StripView {
        id: x.id,
        kind: MixerStrip::Mix(x.id),
        name: x.name.clone(),
        volume_db: x.volume_db,
        muted: x.muted,
        cue: None,
        fx: on(&x.effects),
        badge: Some("MIX"),
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
