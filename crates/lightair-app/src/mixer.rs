//! The audio mixer (docs/10-audio.md): one channel strip per track, with effect list, meter,
//! fader, Cue and Mute. Meters are read from the engine's lock-free cells every frame.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::time::Instant;

use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;
use lightair_core::{Command, Effect, Id, Input, Track};
use lightair_engine::meters::meters;
use serde_json::json;

use crate::studio::Studio;
use crate::ui::{self, tok};

const STRIP_W: f32 = 132.0;
const METER_H: f32 = 150.0;
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

/// Built-in effect presets: (menu label, effects added in one undo step).
fn effect_presets() -> Vec<(&'static str, Vec<Effect>)> {
    vec![
        ("Voice Clarity", voice_clarity()),
        (
            "Noise suppression",
            vec![Effect::new(
                "Noise suppression",
                "noise_suppress_filter_v2",
                json!({"method": "rnnoise", "suppress_level": -30}),
            )],
        ),
        (
            "Noise gate",
            vec![Effect::new(
                "Noise gate",
                "noise_gate_filter",
                json!({"open_threshold": -26.0, "close_threshold": -32.0}),
            )],
        ),
        (
            "Compressor",
            vec![Effect::new(
                "Compressor",
                "compressor_filter",
                json!({"ratio": 3.0, "threshold": -18.0, "attack_time": 6, "release_time": 60, "output_gain": 0.0}),
            )],
        ),
        ("Limiter", vec![Effect::new("Limiter", "limiter_filter", json!({"threshold": -1.0, "release_time": 60}))]),
        ("Gain", vec![Effect::new("Gain", "gain_filter", json!({"db": 0.0}))]),
    ]
}

/// The vocal chain new microphone tracks get (docs/10-audio.md, Voice Clarity).
pub fn voice_clarity() -> Vec<Effect> {
    vec![
        Effect::new(
            "Noise suppression",
            "noise_suppress_filter_v2",
            json!({"method": "rnnoise", "suppress_level": -30}),
        ),
        Effect::new(
            "Compressor",
            "compressor_filter",
            json!({"ratio": 3.0, "threshold": -18.0, "attack_time": 6, "release_time": 60, "output_gain": 2.0}),
        ),
        Effect::new("Limiter", "limiter_filter", json!({"threshold": -1.0, "release_time": 60})),
    ]
}

pub struct MixerPanel {
    studio: Entity<Studio>,
    faders: Rc<RefCell<HashMap<Id, Bounds<Pixels>>>>,
    dragging: Option<Id>,
    /// Displayed (decaying) peak per track and channel, plus the time of the last frame.
    display: HashMap<Id, [f32; 2]>,
    last_frame: Instant,
    add_track_menu: bool,
    effect_menu: Option<Id>,
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
            effect_menu: None,
        }
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
        self.exec(Command::SetTrackVolume { track, volume_db: db }, cx);
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
        for (i, track) in project.tracks.iter().enumerate() {
            // Meter ballistics: rise instantly, fall at 24 dB/s.
            let levels = meters().get(track.id).map(|c| c.levels());
            let shown = self.display.entry(track.id).or_insert([f32::NEG_INFINITY; 2]);
            for (c, value) in shown.iter_mut().enumerate() {
                let target = levels.map_or(f32::NEG_INFINITY, |l| l.peak[c]);
                let fallen = if value.is_finite() { *value - 24.0 * dt } else { f32::NEG_INFINITY };
                *value = if target > fallen { target } else { fallen.max(-90.0) };
            }
            let shown = *shown;
            let input_name = project.inputs.get(&track.input).map(|i| i.name.clone()).unwrap_or_default();
            strips.push(self.strip(i, track, shown, input_name, cx).into_any_element());
        }
        self.display.retain(|id, _| project.track(*id).is_some());

        let add =
            ui::icon_button("add-track", "plus", tok::text_secondary()).on_click(cx.listener(|this, _, _, cx| {
                this.add_track_menu = !this.add_track_menu;
                cx.notify();
            }));
        div()
            .id("mixer")
            .flex()
            .flex_col()
            .h(px(300.))
            .border_t_1()
            .border_color(tok::border())
            .bg(tok::bg_panel())
            .on_mouse_move(cx.listener(|this, ev: &MouseMoveEvent, _, cx| {
                if let (Some(track), Some(MouseButton::Left)) = (this.dragging, ev.pressed_button) {
                    this.fader_to(track, ev.position.y, cx);
                }
            }))
            .on_mouse_up(MouseButton::Left, cx.listener(|this, _, _, cx| this.end_drag(cx)))
            .on_mouse_up_out(MouseButton::Left, cx.listener(|this, _, _, cx| this.end_drag(cx)))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .h(px(40.))
                    .px_3()
                    .child(div().font_weight(FontWeight::SEMIBOLD).child("Audio Mixer"))
                    .child(add)
                    .when(self.add_track_menu, |d| d.child(self.add_track_menu(cx)))
                    .child(div().flex_1())
                    .children(loudness_readout(cx)),
            )
            .child(div().id("strips").flex().flex_1().gap_2().px_3().pb_3().overflow_x_scroll().children(strips).when(
                project.tracks.is_empty(),
                |d| {
                    d.child(
                        div()
                            .text_color(tok::text_tertiary())
                            .child("No audio tracks. Add a microphone or desktop audio with +."),
                    )
                },
            ))
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

    fn add_track_menu(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let item = |id: &'static str, icon: &'static str, label: &'static str| {
            div()
                .id(id)
                .flex()
                .items_center()
                .gap_1()
                .h(px(26.))
                .px_2()
                .rounded(px(6.))
                .cursor_pointer()
                .bg(tok::bg_raised())
                .hover(|d| d.bg(tok::bg_selected()))
                .child(ui::icon(icon).size(px(13.)).text_color(tok::text_secondary()))
                .child(label)
        };
        div()
            .flex()
            .gap_1()
            .child(item("add-mic", "mic", "Microphone").on_click(cx.listener(|this, _, _, cx| {
                this.add_track("Mic", "coreaudio_input_capture", json!({"device_id": "default"}), voice_clarity(), cx)
            })))
            .child(item("add-desktop", "volume-2", "Desktop audio").on_click(cx.listener(|this, _, _, cx| {
                this.add_track("Desktop audio", "sck_audio_capture", json!({"type": 0}), vec![], cx)
            })))
    }

    fn strip(
        &self,
        i: usize,
        track: &Track,
        shown: [f32; 2],
        input_name: String,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let id = track.id;
        let muted = track.muted;
        let cue = track.cue;
        let volume = track.volume_db;

        // ---- effects
        let effects = track.effects.iter().enumerate().map(|(j, e)| {
            let (eid, enabled) = (e.id, e.enabled);
            div()
                .id(("fx", i * 100 + j))
                .flex()
                .items_center()
                .gap_1p5()
                .h(px(22.))
                .px_1()
                .rounded(px(4.))
                .text_size(px(11.))
                .text_color(if enabled { tok::text() } else { tok::text_tertiary() })
                .hover(|d| d.bg(tok::bg_selected()))
                .child(
                    div()
                        .id(("fx-dot", i * 100 + j))
                        .size(px(8.))
                        .rounded_full()
                        .cursor_pointer()
                        .bg(if enabled { tok::ok() } else { tok::border_strong() })
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.exec(Command::SetEffectEnabled { track: id, effect: eid, enabled: !enabled }, cx)
                        })),
                )
                .child(div().flex_1().overflow_hidden().text_ellipsis().child(e.name.clone()))
                .child(
                    div()
                        .id(("fx-del", i * 100 + j))
                        .text_color(tok::text_tertiary())
                        .cursor_pointer()
                        .hover(|d| d.text_color(tok::text()))
                        .child("×")
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.exec(Command::RemoveEffect { track: id, effect: eid }, cx)
                        })),
                )
        });
        let effect_menu = if self.effect_menu == Some(id) {
            let mut items = Vec::new();
            for (k, (label, effects)) in effect_presets().into_iter().enumerate() {
                items.push(
                    div()
                        .id(("fx-add", i * 100 + k))
                        .px_1()
                        .h(px(20.))
                        .text_size(px(11.))
                        .rounded(px(4.))
                        .cursor_pointer()
                        .hover(|d| d.bg(tok::bg_selected()))
                        .child(label)
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.effect_menu = None;
                            let commands = effects
                                .iter()
                                .cloned()
                                .map(|effect| Command::AddEffect { track: id, effect, index: None })
                                .collect();
                            this.exec(Command::Batch { label: format!("Add {label}"), commands }, cx);
                        })),
                );
            }
            Some(div().flex().flex_col().gap_0p5().p_1().rounded(px(6.)).bg(tok::bg_raised()).children(items))
        } else {
            None
        };

        // ---- meter + fader
        let meter_bar = |db: f32| {
            let frac = meter_frac(db);
            // Zones: green below −20, yellow −20…−9, red above −9 dBFS.
            let (g_end, y_end) = (meter_frac(-20.0), meter_frac(-9.0));
            let seg = |from: f32, to: f32, color: Hsla| {
                let h = (frac.min(to) - from).max(0.0) * METER_H;
                div().absolute().left_0().right_0().bottom(px(from * METER_H)).h(px(h)).bg(color)
            };
            div()
                .relative()
                .w(px(6.))
                .h(px(METER_H))
                .rounded(px(2.))
                .bg(tok::bg_app())
                .child(seg(0.0, g_end, tok::ok()))
                .child(seg(g_end, y_end, tok::warn()))
                .child(seg(y_end, 1.0, tok::error()))
        };
        let faders = self.faders.clone();
        let thumb_y = (1.0 - db_to_pos(volume)) * METER_H;
        let fader = div()
            .id(("fader", i))
            .relative()
            .w(px(28.))
            .h(px(METER_H))
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
            .child(div().absolute().left(px(13.)).top_0().bottom_0().w(px(2.)).bg(tok::border_strong()))
            // unity mark
            .child(
                div()
                    .absolute()
                    .left(px(6.))
                    .w(px(16.))
                    .h(px(1.))
                    .top(px((1.0 - db_to_pos(0.0)) * METER_H))
                    .bg(tok::text_tertiary()),
            )
            .child(
                div()
                    .absolute()
                    .left(px(2.))
                    .top(px(thumb_y - 6.))
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
                        this.exec(Command::SetTrackVolume { track: id, volume_db: 0.0 }, cx);
                        return;
                    }
                    this.studio.update(cx, |s, _| s.begin_gesture());
                    this.dragging = Some(id);
                    this.fader_to(id, ev.position.y, cx);
                }),
            );

        let toggle = |key: &'static str, label: &'static str, on: bool, on_color: Hsla| {
            div()
                .id((key, i))
                .flex()
                .items_center()
                .justify_center()
                .h(px(22.))
                .w(px(52.))
                .rounded(px(4.))
                .text_size(px(10.))
                .font_weight(FontWeight::SEMIBOLD)
                .cursor_pointer()
                .border_1()
                .border_color(if on { on_color } else { tok::border_strong() })
                .bg(if on { on_color } else { tok::bg_raised() })
                .text_color(if on { tok::text_inverse() } else { tok::text_secondary() })
                .child(label)
        };

        div()
            .flex()
            .flex_col()
            .flex_none()
            .w(px(STRIP_W))
            .h_full()
            .p_2()
            .gap_1()
            .rounded(px(10.))
            .bg(tok::bg_raised())
            .child(div().font_weight(FontWeight::SEMIBOLD).overflow_hidden().text_ellipsis().child(track.name.clone()))
            .child(div().flex().flex_col().children(effects).children(effect_menu))
            .child(
                div()
                    .id(("fx-plus", i))
                    .text_color(tok::text_tertiary())
                    .text_size(px(11.))
                    .cursor_pointer()
                    .hover(|d| d.text_color(tok::text()))
                    .child("+ effect")
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.effect_menu = if this.effect_menu == Some(id) { None } else { Some(id) };
                        cx.notify();
                    })),
            )
            .child(div().flex_1())
            .child(
                div()
                    .flex()
                    .items_end()
                    .gap_1()
                    .child(meter_bar(shown[0]))
                    .child(meter_bar(shown[1]))
                    .child(fader)
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_1()
                            .child(
                                div()
                                    .text_size(px(10.))
                                    .font_family("Menlo")
                                    .text_color(tok::text_secondary())
                                    .child(if volume.is_finite() { format!("{volume:+.1}") } else { "−∞".into() }),
                            )
                            .child(toggle("cue", "CUE", cue, tok::text()).on_click(cx.listener(
                                move |this, _, _, cx| this.exec(Command::SetTrackCue { track: id, cue: !cue }, cx),
                            )))
                            .child(toggle("mute", "MUTE", muted, tok::live()).on_click(cx.listener(
                                move |this, _, _, cx| {
                                    this.exec(Command::SetTrackMuted { track: id, muted: !muted }, cx)
                                },
                            ))),
                    ),
            )
            .child(
                div()
                    .text_size(px(10.))
                    .text_color(tok::text_tertiary())
                    .overflow_hidden()
                    .text_ellipsis()
                    .child(input_name),
            )
    }
}

/// Stream loudness (EBU R128): short-term and integrated, against the common −14 LUFS target.
/// Click to start integrated loudness over.
fn loudness_readout(cx: &mut Context<MixerPanel>) -> Option<impl IntoElement> {
    let r = lightair_engine::loudness::stream_loudness()?;
    let show = |v: f64| if v.is_finite() { format!("{v:.1}") } else { "—".into() };
    // Most platforms normalise around −14 LUFS: well below sounds quiet, above gets turned down.
    let colour = match r.short_term {
        v if !v.is_finite() || v < -24.0 => tok::text_tertiary(),
        v if v > -9.0 => tok::warn(),
        _ => tok::ok(),
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
                    "Stream loudness: short-term (3 s) and integrated. Platforms aim for about −14. Click to reset integrated.",
                )
                .build(w, cx)
            })
            .on_click(cx.listener(|_, _, _, cx| {
                lightair_engine::loudness::reset_stream_loudness();
                cx.notify();
            })),
    )
}
