//! The Inspector's Avatar section (docs/plans/avatars.md): the camera that follows the face,
//! Calibrate, framing, size, mirror, zoom and an expression, with each expression's shortcut.
//! The model itself is the Source section's, chosen from the avatar library; the header opens the
//! library and adds an accessory.

use gpui_kit::component::Disableable;
use gpui_kit::component::Sizable;
use gpui_kit::component::Selectable;
use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::menu::{DropdownMenu, PopupMenuItem};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;
use lightair_model::{Command, Id, Source};
use serde_json::{Value, json};

use super::StudioWindow;
use crate::ui::{self, tok};

pub(super) const KIND: &str = lightair_engine::avatar::KIND;

/// An expression as the Inspector shows it: a VRM's preset names capitalised ("happy" → "Happy"),
/// a 2D model's own as they are; "" is none.
fn expression_label(e: &str) -> String {
    let mut c = e.chars();
    match c.next() {
        None => "None".into(),
        Some(f) if e.chars().all(|c| c.is_ascii_lowercase()) => f.to_uppercase().chain(c).collect(),
        Some(_) => e.to_string(),
    }
}

fn with(settings: &Value, key: &str, value: Value) -> Value {
    let mut s = if settings.is_object() { settings.clone() } else { json!({}) };
    s[key] = value;
    s
}

impl StudioWindow {
    /// Asks the engine what the selected avatar (or the avatar the selected object is under) is
    /// doing (on the Inspector's timer).
    pub(super) fn refresh_avatar(&mut self, cx: &mut Context<Self>) {
        let studio = self.studio.read(cx);
        let scene = studio.selected_scene.and_then(|s| studio.workspace().scene(s));
        let object = studio.selected_object.and_then(|l| scene.and_then(|s| s.object(l)));
        let is_avatar = |s: &Id| studio.workspace().sources.get(s).is_some_and(|i| i.kind == KIND);
        let Some(source) = object.and_then(|l| l.source).filter(is_avatar).or_else(|| {
            object.and_then(|l| l.parent).and_then(|p| scene.and_then(|s| s.object(p))).and_then(|p| p.source).filter(is_avatar)
        }) else {
            return;
        };
        let status = studio.engine().call(move |s| s.reconciler.avatar_status(source));
        if let Some(status) = status
            && self.avatar_status.get(&source) != Some(&status)
        {
            self.avatar_status.insert(source, status);
            cx.notify();
        }
    }

    /// The keys pressed for an expression's shortcut: checked against Settings' shortcuts, the
    /// automations', other expressions' and what the system already has.
    pub(super) fn capture_expression_key(&mut self, k: &Keystroke, cx: &mut Context<Self>) {
        let Some((source, expression)) = self.expression_capture.clone() else { return };
        if k.key == "escape" {
            self.expression_capture = None;
            cx.notify();
            return;
        }
        let Some(keys) = super::settings::keystroke_keys(k) else { return };
        let shown = crate::shortcuts::display(&keys);
        let note = |this: &mut Self, text: String, cx: &mut Context<Self>| {
            this.avatar_note = Some((source, text));
            cx.notify();
        };
        if crate::shortcuts::parse(&keys).is_none() {
            let hint = if cfg!(windows) { "Use Ctrl or Alt with a letter or number, or an F key." } else { "Use ⌘, ⌃ or ⌥ with a letter or number, or an F key." };
            note(self, hint.into(), cx);
            return;
        }
        self.expression_capture = None;
        if let Some(what) = crate::shortcuts::taken_by(&keys) {
            note(self, format!("{shown} is already {what}."), cx);
            return;
        }
        let workspace = self.studio.read(cx).workspace().clone();
        let mut list = super::avatar_sheets::expression_keys(&workspace);
        if let Some((s, e, _)) = list.iter().find(|(s, e, k)| *k == keys && !(*s == source && *e == expression)) {
            let who = workspace.sources.get(s).map(|i| i.name.clone()).unwrap_or_default();
            note(self, format!("{shown} is already {who}'s {}.", expression_label(e)), cx);
            return;
        }
        // the system's word: another app may have it
        list.retain(|(s, e, _)| !(*s == source && *e == expression));
        list.push((source, expression.clone(), keys.clone()));
        list.sort_by(|a, b| (a.0.to_string(), &a.1).cmp(&(b.0.to_string(), &b.1)));
        let index = list.iter().position(|(s, e, _)| *s == source && *e == expression).unwrap_or(0) as u16;
        let refused = crate::shortcuts::set_expression_keys(list);
        if refused.contains(&crate::shortcuts::GlobalAction::Expression(index)) {
            crate::shortcuts::set_expression_keys(super::avatar_sheets::expression_keys(&workspace));
            note(self, format!("{shown} is taken by {} or another app.", ui::OS), cx);
            return;
        }
        let Some(settings) = workspace.sources.get(&source).map(|i| i.settings.clone()) else { return };
        let mut all = if settings["expression_keys"].is_object() { settings["expression_keys"].clone() } else { json!({}) };
        all[expression.as_str()] = json!(keys);
        self.avatar_note = None;
        self.exec(Command::SetSourceSettings { source, settings: with(&settings, "expression_keys", all) }, cx);
    }

    fn calibrate_avatar(&mut self, source: Id, cx: &mut Context<Self>) {
        let studio = self.studio.read(cx);
        let Some(settings) = studio.workspace().sources.get(&source).map(|i| i.settings.clone()) else { return };
        match studio.engine().call(move |s| s.reconciler.avatar_calibrate(source)) {
            Some(cal) => {
                self.avatar_note = Some((source, "Calibrated to your face now.".into()));
                self.exec(
                    Command::SetSourceSettings { source, settings: with(&settings, "calibration", json!(cal)) },
                    cx,
                );
            }
            None => self.avatar_note = Some((source, "No face in view: look at the camera and try again.".into())),
        }
        cx.notify();
    }

    pub(super) fn avatar_section(&self, source: &Source, cx: &mut Context<Self>) -> AnyElement {
        let id = source.id;
        let s = source.settings.clone();
        let set = |key: &str, v: Value| Command::SetSourceSettings { source: id, settings: with(&s, key, v) };
        let label = |text: &'static str| div().text_size(px(11.)).text_color(tok::text_tertiary()).child(text);
        let status = self.avatar_status.get(&id).cloned().unwrap_or_default();
        let device = s["device"].as_str().unwrap_or("").to_string();
        let file = s["file"].as_str().unwrap_or("");

        // what it's doing, in a line
        let (dot, line) = if file.is_empty() {
            (tok::text_tertiary(), "Choose an avatar".to_string())
        } else if !status.error.is_empty() {
            (tok::error(), format!("The model didn't load: {}", status.error))
        } else if !status.loaded {
            (tok::text_tertiary(), "Loading the model…".to_string())
        } else if device.is_empty() {
            (tok::text_tertiary(), "Not following a face: choose a camera".to_string())
        } else if status.tracking {
            (tok::ok(), "Following your face".to_string())
        } else {
            (tok::warn(), "Looking for a face…".to_string())
        };
        let body_line =
            (!status.body.is_empty() && !device.is_empty() && status.loaded).then(|| format!("Body: {}", status.body));
        let state = div()
            .flex()
            .flex_col()
            .gap_0p5()
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(div().flex_none().size(px(7.)).rounded_full().bg(dot))
                    .child(div().flex_1().min_w_0().text_size(px(12.)).text_color(tok::text_secondary()).child(line)),
            )
            .children(
                body_line.map(|l| div().pl(px(15.)).text_size(px(11.)).text_color(tok::text_tertiary()).child(l)),
            );
        let show = s["show_tracking"].as_bool().unwrap_or(false);
        let show_cmd = set("show_tracking", json!(!show));

        // the camera that sees the face
        let cameras: Vec<(String, String)> = self.connected.as_ref().map(|c| c.cameras.clone()).unwrap_or_default();
        let shown = if device.is_empty() {
            "None".to_string()
        } else {
            cameras.iter().find(|(d, _)| *d == device).map_or_else(|| "Not connected".to_string(), |(_, n)| n.clone())
        };
        let me = cx.entity().downgrade();
        let base = s.clone();
        let current = device.clone();
        let camera =
            Button::new("avatar-camera").label(shown).small().outline().dropdown_menu(move |mut menu, _, _| {
                let mut choices = vec![(String::new(), "None (no face following)".to_string())];
                choices.extend(cameras.iter().cloned());
                for (value, name) in choices {
                    let me = me.clone();
                    let cmd = Command::SetSourceSettings { source: id, settings: with(&base, "device", json!(value)) };
                    menu = menu.item(PopupMenuItem::new(name).checked(value == current).on_click(move |_, _, cx| {
                        if let Some(me) = me.upgrade() {
                            me.update(cx, |this, cx| this.exec(cmd.clone(), cx));
                        }
                    }));
                }
                menu
            });

        let body = s["body"].as_str().unwrap_or("upper");
        let bodies = [("Face only", "off"), ("Upper body", "upper"), ("Full body", "full")]
            .into_iter()
            .map(|(l, v)| (l, set("body", json!(v)), body == v))
            .collect();
        let framing = s["framing"].as_str().unwrap_or("bust");
        let framings = [("Head", "head"), ("Bust", "bust"), ("Half", "half"), ("Full", "full")]
            .into_iter()
            .map(|(l, v)| (l, set("framing", json!(v)), framing == v))
            .collect();
        let (w, h) = (s["width"].as_i64().unwrap_or(1080), s["height"].as_i64().unwrap_or(1080));
        let sizes = [("Square", 1080, 1080), ("Portrait", 1080, 1920), ("Landscape", 1920, 1080)]
            .into_iter()
            .map(|(l, sw, sh)| {
                let mut both = with(&s, "width", json!(sw));
                both["height"] = json!(sh);
                (l, Command::SetSourceSettings { source: id, settings: both }, (w, h) == (sw, sh))
            })
            .collect();
        let expression = s["expression"].as_str().unwrap_or("");
        // (a flat model follows the face only)
        let flat = crate::avatar_library::kind_of(file) != "VRM";
        // the model's own (a VRM's moods and its own, a Live2D model's, an Inochi2D or Umamo
        // puppet's parameters the face doesn't drive), after none
        let no_expressions = status.expressions.is_empty();
        let choices: Vec<String> = std::iter::once(String::new()).chain(status.expressions.iter().cloned()).collect();
        let keys = s["expression_keys"].clone();
        let key_of = |e: &str| keys[e].as_str().filter(|k| !k.is_empty()).map(crate::shortcuts::display);
        let mut expressions = div().flex().flex_wrap().gap_1();
        for (i, value) in choices.iter().enumerate() {
            let on = expression == value;
            let cmd = set("expression", json!(value));
            expressions = expressions.child(
                div()
                    .id(("avatar-expression", i))
                    .flex()
                    .items_center()
                    .gap_1()
                    .px_2()
                    .py_1()
                    .rounded(px(6.))
                    .text_size(px(11.))
                    .cursor_pointer()
                    .when(on, |d| d.bg(tok::bg_selected()).text_color(tok::text()))
                    .when(!on, |d| {
                        d.bg(tok::bg_raised()).text_color(tok::text_secondary()).hover(|d| d.text_color(tok::text()))
                    })
                    .child(expression_label(value))
                    .children(key_of(value).map(|k| div().text_color(tok::text_tertiary()).child(k)))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if !on {
                            this.exec(cmd.clone(), cx)
                        }
                    })),
            );
        }
        // each expression's shortcut, when the list is open
        let open = self.expression_keys_open;
        let capturing = self.expression_capture.as_ref().filter(|(src, _)| *src == id).map(|(_, e)| e.clone());
        let mut shortcuts = div().flex().flex_col().gap_1();
        if open {
            for (i, value) in choices.iter().enumerate() {
                let bound = key_of(value);
                let waiting = capturing.as_deref() == Some(value.as_str());
                let (v1, v2) = (value.clone(), value.clone());
                let clear = {
                    let mut k = if keys.is_object() { keys.clone() } else { json!({}) };
                    if let Some(m) = k.as_object_mut() {
                        m.remove(value);
                    }
                    set("expression_keys", k)
                };
                shortcuts = shortcuts.child(
                    div()
                        .flex()
                        .items_center()
                        .gap_2()
                        .child(div().flex_1().min_w_0().overflow_hidden().text_ellipsis().whitespace_nowrap().text_size(px(12.)).child(expression_label(value)))
                        .child(if waiting {
                            div().text_size(px(11.)).text_color(tok::warn()).child("Press keys… (Esc cancels)")
                        } else {
                            div().text_size(px(11.)).font_family(crate::ui::MONO).text_color(tok::text_secondary()).child(bound.clone().unwrap_or_else(|| "—".into()))
                        })
                        .child(
                            Button::new(("expression-key-set", i))
                                .label(if bound.is_some() { "Change" } else { "Set" })
                                .xsmall()
                                .outline()
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    this.expression_capture = Some((id, v1.clone()));
                                    this.avatar_note = None;
                                    window.focus(&this.focus, cx);
                                    cx.notify();
                                })),
                        )
                        .when(bound.is_some(), |d| {
                            d.child(
                                Button::new(("expression-key-clear", i))
                                    .icon(ui::icon("x"))
                                    .xsmall()
                                    .ghost()
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        let _ = &v2;
                                        this.exec(clear.clone(), cx)
                                    })),
                            )
                        }),
                );
            }
        }
        let shortcuts_toggle = Button::new("avatar-expression-keys")
            .icon(ui::icon("keyboard"))
            .xsmall()
            .ghost()
            .when(open, |b| b.selected(true))
            .tooltip("Shortcuts: press one in any app to switch to that expression; again to go back to none")
            .on_click(cx.listener(|this, _, _, cx| {
                this.expression_keys_open = !this.expression_keys_open;
                this.expression_capture = None;
                cx.notify();
            }));
        let key_note = self
            .avatar_note
            .as_ref()
            .filter(|(src, n)| *src == id && open && !n.starts_with("Calibrated") && !n.starts_with("No face"))
            .map(|(_, n)| div().text_size(px(11.)).text_color(tok::warn()).child(n.clone()));
        let mirror = s["mirror"].as_bool().unwrap_or(true);
        let mirror_cmd = set("mirror", json!(!mirror));
        let zoom = s["zoom"].as_f64().unwrap_or(1.0) as f32;
        let base = s.clone();
        let zoom_slider = self.slider(
            format!("avatar-zoom-{id}"),
            (0.5, 2.0, 0.05),
            zoom,
            move |v| Command::SetSourceSettings { source: id, settings: with(&base, "zoom", json!(v)) },
            cx,
        );

        // the avatar object (for an accessory under it)
        let studio = self.studio.read(cx);
        let avatar_object = studio
            .selected_object
            .filter(|o| studio.selected_scene.and_then(|s| studio.workspace().scene(s)).and_then(|s| s.object(*o)).is_some_and(|o| o.source == Some(id)));
        let header = div()
            .flex()
            .items_center()
            .gap_1()
            .child(
                Button::new("avatar-library")
                    .icon(ui::icon("users"))
                    .label("Avatars")
                    .xsmall()
                    .ghost()
                    .on_click(cx.listener(move |this, _, window, cx| this.open_avatar_library(Some(id), window, cx))),
            )
            .children(avatar_object.map(|object| {
                Button::new("avatar-accessory")
                    .icon(ui::icon("plus"))
                    .label("Accessory")
                    .xsmall()
                    .ghost()
                    .tooltip("Add a picture or video that follows a bone or part of the avatar")
                    .on_click(cx.listener(move |this, _, window, cx| this.add_accessory(object, id, window, cx)))
            }));
        self.section("Avatar", Some(header.into_any_element()), cx)
            .child(state)
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap_2()
                    .child(div().flex().items_center().gap_1().child(label("Camera")).child(ui::info("avatar-camera-info", "The camera that sees your face. Its picture is never shown, only the avatar.")))
                    .child(camera),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        Button::new("avatar-calibrate")
                            .icon(ui::icon("crosshair"))
                            .label("Calibrate")
                            .small()
                            .outline()
                            .disabled(device.is_empty())
                            .on_click(cx.listener(move |this, _, _, cx| this.calibrate_avatar(id, cx))),
                    )
                    // the result once calibrated (state); how to, in the ⓘ
                    .child(ui::info("calibrate-tip", "Look at the camera, face relaxed, then click."))
                    .children(self.avatar_note.as_ref().filter(|(s, _)| *s == id).map(|(_, n)| {
                        div().flex_1().min_w_0().text_size(px(11.)).line_height(px(15.)).text_color(tok::text_tertiary()).child(n.clone())
                    })),
            )
            // (a flat model follows the face only)
            .when(!flat, |d| {
                d.child(
                    div()
                        .flex()
                        .items_center()
                        .gap_1()
                        .child(label("Follows"))
                        .child(ui::info("avatar-body-info", if cfg!(target_os = "macos") {
                            "Upper body: your arms, hands and fingers too, when the camera sees them. Full body: your legs as well (stand back so the camera sees all of you)."
                        } else {
                            "Body following comes to Windows later: the face only, for now."
                        })),
                )
                .child(self.segmented("avatar-body", bodies, cx))
            })
            .child(label("Framing"))
            .child(self.segmented("avatar-framing", framings, cx))
            .child(div().flex().items_center().justify_between().child(label("Zoom")).child(div().text_size(px(11.)).text_color(tok::text_tertiary()).child(format!("{:.0}%", zoom * 100.0))))
            .child(zoom_slider)
            .child(label("Size"))
            .child(self.segmented("avatar-size", sizes, cx))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .items_center()
                            .gap_1()
                            .child(div().text_size(px(12.)).child("Mirror"))
                            .child(ui::info("avatar-mirror-info", "On: it moves like your reflection. Off: as the camera sees you.")),
                    )
                    .child(super::settings::switch("avatar-mirror", mirror).on_click(cx.listener(move |this, _, _, cx| this.exec(mirror_cmd.clone(), cx)))),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .items_center()
                            .gap_1()
                            .child(div().text_size(px(12.)).child("Show tracking"))
                            .child(ui::info("avatar-show-tracking-info", "The camera's picture in the avatar's corner, with what's followed marked: face green, body red (orange: flat, no depth), hands yellow. Turn it off before going live.")),
                    )
                    .child(super::settings::switch("avatar-show-tracking", show).on_click(cx.listener(move |this, _, _, cx| this.exec(show_cmd.clone(), cx)))),
            )
            .when(!no_expressions, |d| {
                d.child(div().flex().items_center().justify_between().child(label("Expression")).child(shortcuts_toggle))
                    .child(expressions)
                    .when(open, |d| d.child(shortcuts).children(key_note))
            })
            .into_any_element()
    }
}
