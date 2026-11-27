//! Settings › Voices (lightair-docs/lightair/docs/plans/voices-and-hearing.md): the voice profiles
//! everything that speaks picks from, the default one, the Voice track's state, and the chat
//! reader.

use std::sync::Mutex;

use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::menu::{DropdownMenu, PopupMenuItem};
use gpui_kit::component::{Disableable, Sizable};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;
use lightair_voice::{Profile, Vendor};

use super::StudioWindow;
use super::settings::{group_tip, row, row_line, switch};
use crate::ui::{self, tok};
use crate::voice::{ReadWhich, Voices};

/// The profile being edited (its id), if any.
static EDITING: Mutex<Option<String>> = Mutex::new(None);

/// A repaint is due (voice lists load and the Voice track's state changes off the main thread).
static REPAINT: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

fn editing() -> Option<String> {
    EDITING.lock().unwrap_or_else(|e| e.into_inner()).clone()
}

fn set_editing(id: Option<String>) {
    *EDITING.lock().unwrap_or_else(|e| e.into_inner()) = id;
}

/// A profile's name, from its vendor and voice ("Cartesia · Ava").
fn name_for(p: &Profile) -> String {
    match (p.vendor, p.voice_name.as_str(), p.voice.as_str()) {
        (Vendor::System, "", "") => "System voice".into(),
        (v, "", "") => format!("{} · default voice", v.name()),
        (v, "", id) => format!("{} · {id}", short(v)),
        (v, name, _) => format!("{} · {name}", short(v)),
    }
}

fn short(v: Vendor) -> &'static str {
    if v == Vendor::System { "System" } else { v.name() }
}

fn chip(id: impl Into<ElementId>, label: &str, on: bool) -> Stateful<Div> {
    div()
        .id(id)
        .px_2()
        .h(px(24.))
        .flex()
        .items_center()
        .rounded(px(5.))
        .text_size(px(11.))
        .cursor_pointer()
        .when(on, |d| d.bg(tok::bg_selected()).text_color(tok::text()))
        .when(!on, |d| d.text_color(tok::text_secondary()).hover(|d| d.bg(tok::bg_raised())))
        .child(label.to_string())
}

fn chip_row() -> Div {
    div().flex().flex_wrap().justify_end().gap_0p5().p_0p5().rounded(px(7.)).bg(tok::bg_panel())
}

fn update(f: impl FnOnce(&mut Voices)) {
    let mut v = crate::voice::settings();
    f(&mut v);
    crate::voice::set_settings(v);
}

fn update_profile(id: &str, f: impl FnOnce(&mut Profile)) {
    update(|v| {
        if let Some(p) = v.profiles.iter_mut().find(|p| p.id == id) {
            f(p);
            p.name = name_for(p);
        }
    });
}

impl StudioWindow {
    pub(super) fn voices_page(&self, cx: &mut Context<Self>) -> Div {
        // drawn again every half second while it's open
        if !REPAINT.swap(true, std::sync::atomic::Ordering::Relaxed) {
            cx.spawn(async move |this, cx| {
                cx.background_executor().timer(std::time::Duration::from_millis(500)).await;
                REPAINT.store(false, std::sync::atomic::Ordering::Relaxed);
                let _ = this.update(cx, |_, cx| cx.notify());
            })
            .detach();
        }
        let settings = crate::voice::settings();
        let profiles = crate::voice::profiles();
        let default = crate::voice::default_profile();
        let me = cx.entity().downgrade();

        // the default voice: a menu of every profile
        let choose = {
            let list: Vec<(String, String)> = profiles.iter().map(|p| (p.id.clone(), p.name.clone())).collect();
            let me = me.clone();
            Button::new("voices-default")
                .label(default.name.clone())
                .xsmall()
                .outline()
                .dropdown_caret(true)
                .dropdown_menu(move |mut menu, _, _| {
                    for (id, name) in &list {
                        let (me, id) = (me.clone(), id.clone());
                        menu = menu.item(PopupMenuItem::new(name.clone()).on_click(move |_, _, cx| {
                            update(|v| v.default = id.clone());
                            if let Some(me) = me.upgrade() {
                                me.update(cx, |_, cx| cx.notify());
                            }
                        }));
                    }
                    menu
                })
        };
        let (waiting, speaking, problem) = crate::voice::status();
        let state = match (&problem, speaking, waiting) {
            (Some(p), _, _) => p.clone(),
            (None, true, 0) => "Saying something".into(),
            (None, true, n) | (None, false, n) if n > 0 => format!("Saying something, {n} waiting"),
            _ => "Quiet".into(),
        };
        let try_it =
            Button::new("voices-try").label("Try it").xsmall().outline().on_click(cx.listener(|this, _, _, cx| {
                this.say_on_voice_track("This is how your voice sounds on stream.", None, cx);
            }));
        let skip = speaking.then(|| {
            Button::new("voices-skip").label("Stop").xsmall().ghost().on_click(cx.listener(|_, _, _, cx| {
                crate::voice::stop(true);
                cx.notify();
            }))
        });
        let top = group_tip(
            "Voice track",
            "What alerts, automations, the chat reader and remote tools say plays on the Voice mixer track, one thing after another, with the default voice unless they name another. Links and long numbers are never read, and personal data or slurs aren't said at all. A shortcut to skip what's being said is in Shortcuts.",
        )
        .child(row("Default voice", None, choose))
        .child(row_line("Voice track", Some(&state), div().flex().items_center().gap_1().children(skip).child(try_it)));

        // the profiles
        let mut list = group_tip(
            "Voices",
            "Set up a voice once and pick it wherever something speaks. Cartesia, ElevenLabs and Gemini need their key in Connections and bill your account; the system voice is free and works offline. ▶ plays a sample on this computer, never on stream.",
        );
        let edit = editing();
        for (i, p) in profiles.iter().enumerate() {
            let is_system = p.id == "system";
            let on = edit.as_deref() == Some(p.id.as_str());
            let detail = match p.vendor {
                Vendor::System if is_system => "Free, offline".to_string(),
                v => format!(
                    "{}{}",
                    v.name(),
                    if (p.speed - 1.0).abs() > 0.01 { format!(" · {:.1}×", p.speed) } else { String::new() }
                ),
            };
            let (id_e, id_d) = (p.id.clone(), p.id.clone());
            let actions = div().flex().items_center().gap_1().when(!is_system, |d| {
                d.child(Button::new(("voices-edit", i)).icon(ui::icon("pencil")).xsmall().ghost().on_click(
                    cx.listener(move |_, _, _, cx| {
                        set_editing(if editing().as_deref() == Some(id_e.as_str()) {
                            None
                        } else {
                            Some(id_e.clone())
                        });
                        cx.notify();
                    }),
                ))
                .child(
                    Button::new(("voices-remove", i)).icon(ui::icon("trash-2")).xsmall().ghost().on_click(cx.listener(
                        move |_, _, _, cx| {
                            update(|v| {
                                v.profiles.retain(|x| x.id != id_d);
                                if v.default == id_d {
                                    v.default = "system".into();
                                }
                            });
                            cx.notify();
                        },
                    )),
                )
            });
            list = list.child(row_line(&p.name, Some(&detail), actions));
            if on {
                list = list.child(self.profile_editor(p, cx));
            }
        }
        list = list.child(div().flex().justify_end().child(
            Button::new("voices-new").label("New voice").xsmall().outline().on_click(cx.listener(|_, _, _, cx| {
                let id = format!(
                    "p{}",
                    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis())
                );
                let mut p = Profile { id: id.clone(), vendor: Vendor::Gemini, ..Profile::default() };
                p.name = name_for(&p);
                update(|v| v.profiles.push(p));
                set_editing(Some(id));
                cx.notify();
            })),
        ));

        // the chat reader
        let r = settings.chat_reader.clone();
        let mut which = chip_row();
        for (i, w) in ReadWhich::ALL.into_iter().enumerate() {
            which = which.child(chip(("voices-which", i), w.label(), r.which == w).on_click(cx.listener(
                move |_, _, _, cx| {
                    update(|v| v.chat_reader.which = w);
                    cx.notify();
                },
            )));
        }
        let max = r.max_chars;
        let length = div()
            .flex()
            .items_center()
            .gap_1()
            .child(Button::new("voices-max-less").icon(ui::icon("minus")).xsmall().ghost().on_click(cx.listener(
                move |_, _, _, cx| {
                    update(|v| v.chat_reader.max_chars = max.saturating_sub(50).max(50));
                    cx.notify();
                },
            )))
            .child(
                div()
                    .w(px(80.))
                    .text_center()
                    .text_size(px(12.))
                    .font_family(crate::ui::MONO)
                    .child(format!("{max} letters")),
            )
            .child(Button::new("voices-max-more").icon(ui::icon("plus")).xsmall().ghost().on_click(cx.listener(
                move |_, _, _, cx| {
                    update(|v| v.chat_reader.max_chars = (max + 50).min(500));
                    cx.notify();
                },
            )));
        let (on, say_name, family) = (r.on, r.say_name, settings.family_friendly);
        let reader = group_tip(
            "Chat reader",
            "Reads chat aloud on the Voice track with the default voice: paid messages only, each viewer's first message too, or every message. Long messages are cut at a word.",
        )
        .child(row(
            "Read chat aloud",
            None,
            switch("voices-reader", on).on_click(cx.listener(move |_, _, _, cx| {
                update(|v| v.chat_reader.on = !on);
                cx.notify();
            })),
        ))
        .when(on, |g| {
            g.child(row("Read", None, which))
                .child(row(
                    "Say who wrote it",
                    None,
                    switch("voices-name", say_name).on_click(cx.listener(move |_, _, _, cx| {
                        update(|v| v.chat_reader.say_name = !say_name);
                        cx.notify();
                    })),
                ))
                .child(row("Longest message", None, length))
        })
        .child(row(
            "Family friendly",
            Some("Swearing isn't said either, by anything on the Voice track."),
            switch("voices-family", family).on_click(cx.listener(move |_, _, _, cx| {
                update(|v| v.family_friendly = !family);
                cx.notify();
            })),
        ));

        div().flex().flex_col().gap_5().child(top).child(list).child(reader)
    }

    /// One profile's provider, voice and speed.
    fn profile_editor(&self, p: &Profile, cx: &mut Context<Self>) -> Div {
        let mut vendors = chip_row();
        for (i, v) in Vendor::ALL.into_iter().enumerate() {
            let id = p.id.clone();
            vendors = vendors.child(chip(("voices-vendor", i), v.name(), p.vendor == v).on_click(cx.listener(
                move |_, _, _, cx| {
                    update_profile(&id, |p| {
                        if p.vendor != v {
                            p.vendor = v;
                            p.voice.clear();
                            p.voice_name.clear();
                        }
                    });
                    cx.notify();
                },
            )));
        }
        let vendor_id = p.vendor.id();
        let voices = self.piair.voices(vendor_id);
        let (loading, playing) = self.piair.preview_state();
        let current = if p.voice_name.is_empty() {
            if p.voice.is_empty() { "Default".to_string() } else { p.voice.clone() }
        } else {
            p.voice_name.clone()
        };
        let picker: AnyElement = match voices {
            crate::piair::Voices::Loading => {
                div().text_size(px(12.)).text_color(tok::text_tertiary()).child("Loading voices…").into_any_element()
            }
            crate::piair::Voices::Failed(why) => {
                div().max_w(px(260.)).text_size(px(11.)).text_color(tok::warn()).child(why).into_any_element()
            }
            crate::piair::Voices::Ready(list) => {
                let me = cx.entity().downgrade();
                let id = p.id.clone();
                let chosen = list.iter().find(|v| v.id == p.voice).cloned();
                let menu_list = list.clone();
                let pick =
                    Button::new("voices-voice").label(current).xsmall().outline().dropdown_caret(true).dropdown_menu(
                        move |mut menu, _, _| {
                            for v in menu_list.iter().take(200) {
                                let (me, id, v) = (me.clone(), id.clone(), v.clone());
                                let label = if v.detail.is_empty() {
                                    v.name.clone()
                                } else {
                                    format!("{} — {}", v.name, v.detail)
                                };
                                menu = menu.item(PopupMenuItem::new(label).on_click(move |_, _, cx| {
                                    update_profile(&id, |p| {
                                        p.voice = v.id.clone();
                                        p.voice_name = v.name.clone();
                                    });
                                    if let Some(me) = me.upgrade() {
                                        me.update(cx, |_, cx| cx.notify());
                                    }
                                }));
                            }
                            menu
                        },
                    );
                let busy = chosen.as_ref().is_some_and(|c| {
                    loading.as_deref() == Some(c.id.as_str()) || playing.as_deref() == Some(c.id.as_str())
                });
                let play = Button::new("voices-preview")
                    .icon(ui::icon("play"))
                    .xsmall()
                    .ghost()
                    .disabled(chosen.is_none() || busy)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if let Some(c) = chosen.clone() {
                            this.piair.preview_voice(vendor_id, c);
                        }
                        cx.notify();
                    }));
                div().flex().items_center().gap_1().child(play).child(pick).into_any_element()
            }
        };
        let speed = p.speed;
        let id_minus = p.id.clone();
        let id_plus = p.id.clone();
        let speed_ctl = div()
            .flex()
            .items_center()
            .gap_1()
            .child(
                Button::new("voices-speed-less")
                    .icon(ui::icon("minus"))
                    .xsmall()
                    .ghost()
                    .on_click(cx.listener(move |_, _, _, cx| {
                        update_profile(&id_minus, |p| p.speed = ((speed - 0.1) * 10.0).round() / 10.0);
                        cx.notify();
                    }))
                    .disabled(speed <= 0.55),
            )
            .child(
                div()
                    .w(px(60.))
                    .text_center()
                    .text_size(px(12.))
                    .font_family(crate::ui::MONO)
                    .child(format!("{speed:.1}×")),
            )
            .child(
                Button::new("voices-speed-more")
                    .icon(ui::icon("plus"))
                    .xsmall()
                    .ghost()
                    .on_click(cx.listener(move |_, _, _, cx| {
                        update_profile(&id_plus, |p| p.speed = ((speed + 0.1) * 10.0).round() / 10.0);
                        cx.notify();
                    }))
                    .disabled(speed >= 1.95),
            );
        let id_try = p.id.clone();
        let key_missing =
            p.vendor.key_service().is_some_and(|s| matches!(self.piair.key_state(s), crate::piair::KeyState::Missing));
        div()
            .flex()
            .flex_col()
            .gap_1p5()
            .pl_4()
            .child(row("Provider", None, vendors))
            .when(key_missing, |d| {
                d.child(row_line(
                    "Key",
                    Some(&format!(
                        "Add your {} key in Connections",
                        p.vendor.key_service().map(crate::piair::keys::name_of).unwrap_or("")
                    )),
                    Button::new("voices-connections").label("Connections").xsmall().outline().on_click(
                        cx.listener(|this, _, _, cx| this.open_settings(super::settings::Page::Connections, cx)),
                    ),
                ))
            })
            .child(row("Voice", None, picker))
            .child(row("Speed", None, speed_ctl))
            .child(div().flex().justify_end().child(
                Button::new("voices-try-this").label("Say it on stream").xsmall().outline().on_click(cx.listener(
                    move |this, _, _, cx| {
                        this.say_on_voice_track("This is how this voice sounds on stream.", Some(&id_try), cx)
                    },
                )),
            ))
    }

    /// Says `text` on the Voice track (made when it's missing), with a profile or the default.
    pub(super) fn say_on_voice_track(&mut self, text: &str, profile: Option<&str>, cx: &mut Context<Self>) {
        if let Some(cmd) = super::live_api::voice_track_command(self.studio.read(cx).workspace()) {
            self.studio.update(cx, |s, cx| {
                let _ = s.execute(cmd, cx);
            });
        }
        if let Err(e) = crate::voice::say(text, profile, 1000) {
            self.studio.update(cx, |s, _| s.issue = Some(e.into()));
        }
        cx.notify();
    }
}
