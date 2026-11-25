//! An avatar's Inspector (docs/plans/avatar-inspector.md), before the Transform every object has:
//!
//! - **Avatar:** its picture, name and type, Change (the avatar library), and what it's doing in a
//!   line that says what to do about a problem;
//! - **Tracking:** the camera, how much of the body follows (a VRM's, on macOS), Calibrate with
//!   when it was (how to and Reset in its ⓘ menu), Mirror; Check tracking in the header shows the
//!   trackers' picture for 15 s;
//! - **Framing:** head, bust, half or full with the zoom, and the shape (the picture and its box
//!   together);
//! - **Expressions:** the model's own, with their shortcuts' keys, a search past 12, and Weight
//!   (VRM, Live2D);
//! - **Accessories:** what's under it and what each is pinned to; Add in the header.

use gpui_kit::component::Disableable;
use gpui_kit::component::Selectable;
use gpui_kit::component::Sizable;
use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::input::Input as TextInput;
use gpui_kit::component::menu::{DropdownMenu, PopupMenuItem};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;
use lightair_model::{Command, Id, Layout, Object, Scene, Source};
use serde_json::{Value, json};

use super::StudioWindow;
use crate::ui::{self, tok};

pub(super) const KIND: &str = lightair_host::avatar::KIND;

/// An expression as the Inspector shows it, readably: a VRM's preset names capitalised ("happy"
/// → "Happy"); a model's parameter names without "Param", in words, a side last
/// ("ParamBrowLAngle" → "Brow angle (left)", "Arm:: Left:: Move" → "Arm move (left)"); "" is none.
/// The stored setting and the chip's tooltip keep the name as it is.
fn expression_label(e: &str) -> String {
    if e.is_empty() {
        return "None".into();
    }
    let rest = e.strip_prefix("Param").filter(|r| r.starts_with(|c: char| c.is_uppercase())).unwrap_or(e);
    // words: at separators, and where camelCase turns ("BrowLAngle" → Brow, L, Angle)
    let chars: Vec<char> = rest.chars().collect();
    let mut words: Vec<String> = Vec::new();
    let mut cur = String::new();
    for (i, &c) in chars.iter().enumerate() {
        if !c.is_alphanumeric() {
            if !cur.is_empty() {
                words.push(std::mem::take(&mut cur));
            }
            continue;
        }
        let prev = i.checked_sub(1).map(|j| chars[j]);
        let next = chars.get(i + 1).copied();
        let turn = c.is_uppercase()
            && prev.is_some_and(|p| {
                p.is_lowercase() || p.is_ascii_digit() || (p.is_uppercase() && next.is_some_and(char::is_lowercase))
            });
        if turn && !cur.is_empty() {
            words.push(std::mem::take(&mut cur));
        }
        cur.push(c);
    }
    if !cur.is_empty() {
        words.push(cur);
    }
    // a side, said once at the end
    let side = words.iter().find_map(|w| match w.as_str() {
        "L" | "Left" | "left" => Some("left"),
        "R" | "Right" | "right" => Some("right"),
        _ => None,
    });
    words.retain(|w| !matches!(w.as_str(), "L" | "Left" | "left" | "R" | "Right" | "right"));
    let mut out: Vec<String> = words
        .iter()
        .enumerate()
        .map(|(i, w)| {
            let lower = w.to_lowercase();
            if i == 0 {
                let mut c = lower.chars();
                c.next().map(|f| f.to_uppercase().chain(c).collect()).unwrap_or_default()
            } else {
                lower
            }
        })
        .collect();
    if out.is_empty() {
        out.push(e.to_string());
    }
    let label = out.join(" ");
    match side {
        Some(s) => format!("{label} ({s})"),
        None => label,
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
            object
                .and_then(|l| l.parent)
                .and_then(|p| scene.and_then(|s| s.object(p)))
                .and_then(|p| p.source)
                .filter(is_avatar)
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

    /// A PNGtuber's push to talk held, or let go.
    pub(super) fn push_to_talk(&mut self, source: Id, held: bool, cx: &mut Context<Self>) {
        if held {
            self.push_to_talk_on.insert(source);
        } else {
            self.push_to_talk_on.remove(&source);
        }
        self.studio.read(cx).engine().post(move |s| s.reconciler.avatar_push_to_talk(source, held));
        cx.notify();
    }

    /// A shortcut let go: push to talk stops.
    pub(super) fn global_release(&mut self, action: crate::shortcuts::GlobalAction, cx: &mut Context<Self>) {
        if let crate::shortcuts::GlobalAction::PushToTalk(i) = action
            && let Some(source) = crate::shortcuts::push_to_talk_for(i)
        {
            self.push_to_talk(source, false, cx);
        }
    }

    /// What already has these keys among avatars' shortcuts (an expression's, a push to talk's),
    /// other than `except`'s (source, expression; None: its push to talk).
    fn avatar_key_taken(&self, keys: &str, except: (Id, Option<&str>), cx: &App) -> Option<String> {
        let workspace = self.studio.read(cx).workspace();
        let name = |s: &Id| workspace.sources.get(s).map(|i| i.name.clone()).unwrap_or_default();
        for (s, e, k) in super::avatar_sheets::expression_keys(workspace) {
            if k == keys && (s, Some(e.as_str())) != except {
                return Some(format!("{}'s {}", name(&s), expression_label(&e)));
            }
        }
        for (s, k) in super::avatar_sheets::push_to_talk_keys(workspace) {
            if k == keys && (s, None) != except {
                return Some(format!("{}'s push to talk", name(&s)));
            }
        }
        None
    }

    /// The keys pressed for a PNGtuber's push to talk (checked as an expression's are).
    pub(super) fn capture_push_to_talk_key(&mut self, k: &Keystroke, cx: &mut Context<Self>) {
        let Some(source) = self.push_to_talk_capture else { return };
        if k.key == "escape" {
            self.push_to_talk_capture = None;
            cx.notify();
            return;
        }
        let Some(keys) = super::settings::keystroke_keys(k) else { return };
        let shown = crate::shortcuts::display(&keys);
        if crate::shortcuts::parse(&keys).is_none() {
            let hint = if cfg!(windows) {
                "Use Ctrl or Alt with a letter or number, or an F key."
            } else {
                "Use ⌘, ⌃ or ⌥ with a letter or number, or an F key."
            };
            self.avatar_note = Some((source, hint.into()));
            cx.notify();
            return;
        }
        self.push_to_talk_capture = None;
        let taken = crate::shortcuts::taken_by(&keys).or_else(|| self.avatar_key_taken(&keys, (source, None), cx));
        if let Some(what) = taken {
            self.avatar_note = Some((source, format!("{shown} is already {what}.")));
            cx.notify();
            return;
        }
        let workspace = self.studio.read(cx).workspace().clone();
        let mut list = super::avatar_sheets::push_to_talk_keys(&workspace);
        list.retain(|(s, _)| *s != source);
        list.push((source, keys.clone()));
        let index = list.iter().position(|(s, _)| *s == source).unwrap_or(0) as u16;
        let refused = crate::shortcuts::set_push_to_talk_keys(list);
        if refused.contains(&crate::shortcuts::GlobalAction::PushToTalk(index)) {
            crate::shortcuts::set_push_to_talk_keys(super::avatar_sheets::push_to_talk_keys(&workspace));
            self.avatar_note = Some((source, format!("{shown} is taken by {} or another app.", ui::OS)));
            cx.notify();
            return;
        }
        let Some(settings) = workspace.sources.get(&source).map(|i| i.settings.clone()) else { return };
        self.avatar_note = None;
        self.exec(
            Command::SetSourceSettings { source, settings: with(&settings, "push_to_talk_keys", json!(keys)) },
            cx,
        );
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
            let hint = if cfg!(windows) {
                "Use Ctrl or Alt with a letter or number, or an F key."
            } else {
                "Use ⌘, ⌃ or ⌥ with a letter or number, or an F key."
            };
            note(self, hint.into(), cx);
            return;
        }
        self.expression_capture = None;
        if let Some(what) = crate::shortcuts::taken_by(&keys) {
            note(self, format!("{shown} is already {what}."), cx);
            return;
        }
        if let Some(what) = self.avatar_key_taken(&keys, (source, Some(expression.as_str())), cx) {
            note(self, format!("{shown} is already {what}."), cx);
            return;
        }
        let workspace = self.studio.read(cx).workspace().clone();
        let mut list = super::avatar_sheets::expression_keys(&workspace);
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
        let mut all =
            if settings["expression_keys"].is_object() { settings["expression_keys"].clone() } else { json!({}) };
        all[expression.as_str()] = json!(keys);
        self.avatar_note = None;
        self.exec(Command::SetSourceSettings { source, settings: with(&settings, "expression_keys", all) }, cx);
    }

    fn calibrate_avatar(&mut self, source: Id, cx: &mut Context<Self>) {
        let studio = self.studio.read(cx);
        let Some(settings) = studio.workspace().sources.get(&source).map(|i| i.settings.clone()) else { return };
        match studio.engine().call(move |s| s.reconciler.avatar_calibrate(source)) {
            Some(cal) => {
                self.avatar_note = None;
                let mut settings = with(&settings, "calibration", json!(cal));
                settings["calibrated_at"] = json!(chrono::Utc::now().timestamp_millis());
                self.exec(Command::SetSourceSettings { source, settings }, cx);
            }
            None => self.avatar_note = Some((source, "No face in view: look at the camera and try again.".into())),
        }
        cx.notify();
    }

    /// Check tracking: the trackers' picture in the avatar's corner for 15 s, then off on its own
    /// (it goes on air like the rest of the avatar).
    pub(super) fn check_tracking(&mut self, source: Id, on: bool, cx: &mut Context<Self>) {
        let Some(settings) = self.studio.read(cx).workspace().sources.get(&source).map(|i| i.settings.clone()) else {
            return;
        };
        self.exec(Command::SetSourceSettings { source, settings: with(&settings, "show_tracking", json!(on)) }, cx);
        if !on {
            self.tracking_check = None;
            return;
        }
        let started = std::time::Instant::now();
        self.tracking_check = Some((source, started));
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(std::time::Duration::from_secs(15)).await;
            let _ = this.update(cx, |this, cx| {
                // (unless checked again since, or turned off by hand)
                if this.tracking_check == Some((source, started)) {
                    this.check_tracking(source, false, cx);
                }
            });
        })
        .detach();
    }

    /// Changes the avatar's shape (square, portrait, landscape): the source's pixel size and the
    /// box of every object showing it, about the box's centre, so the character never shrinks into
    /// bars. What's pinned to it stays where it was on its anchor (each pin re-measured for the new
    /// picture). One undo step.
    pub(super) fn set_avatar_shape(&mut self, source: Id, (w, h): (u32, u32), cx: &mut Context<Self>) {
        use lightair_host::avatar_pins::{Placement, Spot, on_canvas};
        let workspace = self.studio.read(cx).workspace().clone();
        let engine = self.studio.read(cx).engine().clone();
        let Some(settings) = workspace.sources.get(&source).map(|i| i.settings.clone()) else { return };
        let (ow, oh) =
            (settings["width"].as_f64().unwrap_or(1080.0) as f32, settings["height"].as_f64().unwrap_or(1080.0) as f32);
        let bone = crate::avatar_library::kind_of(settings["file"].as_str().unwrap_or("")) == "VRM";
        let mut s = with(&settings, "width", json!(w));
        s["height"] = json!(h);
        let mut commands = Vec::new();
        let aspect = w as f32 / h as f32;
        // where an anchor is at rest: now (as pinned, or measured) and at the new size
        let rest = |key: &str, size: Option<(u32, u32)>| {
            let key = key.to_string();
            engine.call(move |e| e.reconciler.avatar_anchor_rest(source, &key, size))
        };
        for scene in &workspace.scenes {
            for o in scene.objects.iter().filter(|o| o.source == Some(source)) {
                let pinned: Vec<(Id, String)> = scene
                    .children(Some(o.id))
                    .filter_map(|c| super::avatar_sheets::pin_of(&settings, c.id).map(|k| (c.id, k)))
                    .collect();
                let spots: Vec<(Id, String, Option<Spot>, Option<Spot>)> = pinned
                    .into_iter()
                    .map(|(child, key)| {
                        let pin = &settings["pins"][child.to_string()];
                        let then = pin["at"].as_array().filter(|a| a.len() == 4).map(|a| {
                            let f = |i: usize| a[i].as_f64().unwrap_or(0.0) as f32;
                            Spot { x: f(0), y: f(1), angle: f(2), scale: f(3).max(1e-6) }
                        });
                        let then = then.or_else(|| rest(&key, None));
                        let now = rest(&key, Some((w, h)));
                        (child, key, then, now)
                    })
                    .collect();
                for (canvas, l) in &o.layouts {
                    let (cx_, cy_) = (l.x + l.width / 2.0, l.y + l.height / 2.0);
                    let (nw, nh) = (l.height * aspect, l.height);
                    let layout = Layout { x: cx_ - nw / 2.0, y: cy_ - nh / 2.0, width: nw, height: nh, ..*l };
                    commands.push(Command::SetLayout { scene: scene.id, object: o.id, canvas: *canvas, layout });
                    // its pinned accessories: the same place on their anchor in the new picture
                    let (Some(outer), Some(host_old)) = (scene.frame_of(o.parent, *canvas), scene.world(o.id, *canvas))
                    else {
                        continue;
                    };
                    let host_new = layout.to_world(&outer);
                    let inner = outer.then(&layout.frame());
                    for (child, _, then, now) in &spots {
                        let (Some(then), Some(now), Some(cw)) = (then, now, scene.world(*child, *canvas)) else {
                            continue;
                        };
                        let p0 = on_canvas(&Placement::of(&host_old), ow, oh, then.x, then.y);
                        let p1 = on_canvas(&Placement::of(&host_new), w as f32, h as f32, now.x, now.y);
                        // the anchor's size on the canvas, before and after
                        let fit = |host: &Layout, pw: f32, ph: f32| (host.width / pw).min(host.height / ph);
                        let focal = |ph: f32| if bone { ph } else { 1.0 };
                        let k = (now.scale * fit(&host_new, w as f32, h as f32) * focal(h as f32))
                            / (then.scale * fit(&host_old, ow, oh) * focal(oh)).max(1e-6);
                        let k = if k.is_finite() { k.clamp(0.2, 5.0) } else { 1.0 };
                        let (ccx, ccy) = (cw.x + cw.width / 2.0, cw.y + cw.height / 2.0);
                        let (nx, ny) = (p1.0 + (ccx - p0.0) * k, p1.1 + (ccy - p0.1) * k);
                        let (nw, nh) = (cw.width * k, cw.height * k);
                        let world = Layout { x: nx - nw / 2.0, y: ny - nh / 2.0, width: nw, height: nh, ..cw };
                        commands.push(Command::SetLayout {
                            scene: scene.id,
                            object: *child,
                            canvas: *canvas,
                            layout: world.to_local(&inner),
                        });
                    }
                }
                for (child, key, _, now) in &spots {
                    if let Some(a) = now {
                        s["pins"][child.to_string()] = json!({"anchor": key, "at": [a.x, a.y, a.angle, a.scale]});
                    }
                }
            }
        }
        commands.insert(0, Command::SetSourceSettings { source, settings: s });
        self.exec(Command::Batch { label: "Change avatar shape".into(), commands }, cx);
    }

    /// The Inspector's sections for an avatar object, in order: the avatar, tracking, framing, its
    /// expressions (when it has some) and its accessories (docs/plans/avatar-inspector.md).
    pub(super) fn avatar_sections(
        &self,
        scene: &Scene,
        object: &Object,
        source: &Source,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        // a PNGtuber listens to a mic; the others follow a face
        let png = crate::avatar_library::kind_of(source.settings["file"].as_str().unwrap_or("")) == "PNGtuber";
        let follow = if png { self.voice_section(source, cx) } else { self.tracking_section(source, cx) };
        // an AI co-host: PiAir's sections instead of tracking and expressions (PiAir drives
        // those); how it sits on screen and what it wears stay
        if crate::piair::is_cohost(source) {
            return vec![
                self.avatar_card(source, cx),
                self.cohost_sections(source, cx),
                self.framing_section(source, cx),
                self.accessories_section(scene, object, source, cx),
            ];
        }
        let mut out = vec![self.avatar_card(source, cx), follow, self.framing_section(source, cx)];
        out.extend(self.expressions_section(source, cx));
        out.push(self.accessories_section(scene, object, source, cx));
        out
    }

    /// The avatar: its picture, name and type, Change (the library), and what it's doing in a line.
    fn avatar_card(&self, source: &Source, cx: &mut Context<Self>) -> AnyElement {
        let id = source.id;
        let s = &source.settings;
        let status = self.avatar_status.get(&id).cloned().unwrap_or_default();
        let file = s["file"].as_str().unwrap_or("").to_string();
        let device = s["device"].as_str().unwrap_or("").to_string();
        let entry = super::avatar_sheets::entry_of(s).and_then(|e| self.avatar_library.get(e)).cloned();
        let name = entry.as_ref().map_or_else(|| source.name.clone(), |e| e.name.clone());
        let kind = crate::avatar_library::kind_of(&file);
        let picture = entry.as_ref().and_then(|e| e.picture_path());
        let connected = self.connected.as_ref().is_none_or(|c| c.cameras.iter().any(|(d, _)| *d == device));

        // what it's doing, in a line: a problem says what to do
        let png = kind == "PNGtuber";
        let mic = s["mic"]
            .as_str()
            .and_then(|m| self.studio.read(cx).workspace().tracks.iter().find(|t| t.id.to_string() == m))
            .map(|t| t.name.clone());
        let (dot, line) = if file.is_empty() {
            (tok::text_tertiary(), "No model: choose one with Change".to_string())
        } else if !status.error.is_empty() {
            (tok::error(), load_problem(&file, &status.error))
        } else if !status.loaded {
            (tok::text_tertiary(), "Loading…".to_string())
        } else if png {
            match &mic {
                Some(m) => (tok::ok(), format!("Listening to {m}")),
                None => (tok::warn(), "No mic: choose one to make it talk".to_string()),
            }
        } else if device.is_empty() {
            (tok::text_tertiary(), "Not following a face: choose a camera".to_string())
        } else if !connected {
            (tok::warn(), "Camera not connected: plug it in, or choose another".to_string())
        } else if status.tracking {
            (tok::ok(), "Following your face".to_string())
        } else {
            (tok::warn(), "Looking for your face…".to_string())
        };
        // an AI co-host says what PiAir is doing, once its model is in
        let (dot, line) = match self.cohost_line(source, cx) {
            Some(c) if !file.is_empty() && status.error.is_empty() && status.loaded => c,
            _ => (dot, line),
        };
        // the body only when it needs something: following it, and not seeing it
        let body = s["body"].as_str().unwrap_or("upper");
        let body_line = (status.loaded && !device.is_empty() && body != "off" && status.tracking)
            .then(|| match status.body.as_str() {
                b if b.starts_with("no body tracker") => {
                    Some("Body following isn't available here: the face only".to_string())
                }
                b if b.starts_with("no body") && body == "full" => {
                    Some("Full body needs all of you in view".to_string())
                }
                b if b.starts_with("no body") => Some("Upper body needs more of you in view".to_string()),
                _ => None,
            })
            .flatten();

        let card =
            div()
                .flex()
                .items_center()
                .gap_3()
                .child(
                    div()
                        .flex()
                        .items_center()
                        .justify_center()
                        .flex_none()
                        .size(px(52.))
                        .rounded(px(8.))
                        .overflow_hidden()
                        .bg(tok::bg_app())
                        .child(match picture {
                            Some(p) => {
                                // its own shape, fitted in
                                let (w, h) = crate::avatar_library::fit(&p, 52.0, 52.0);
                                img(p).w(px(w)).h(px(h)).into_any_element()
                            }
                            None => ui::icon("person-standing")
                                .size(px(22.))
                                .text_color(tok::text_tertiary())
                                .into_any_element(),
                        }),
                )
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .flex_1()
                        .min_w_0()
                        .child(
                            div()
                                .overflow_hidden()
                                .text_ellipsis()
                                .whitespace_nowrap()
                                .font_weight(FontWeight::MEDIUM)
                                .child(name),
                        )
                        .child(div().text_size(px(11.)).text_color(tok::text_tertiary()).child(if file.is_empty() {
                            "No model"
                        } else {
                            kind
                        })),
                )
                .when(png, |d| {
                    let path = std::path::PathBuf::from(&file);
                    d.child(Button::new("avatar-edit").label("Edit").small().outline().on_click(
                        cx.listener(move |this, _, _, cx| this.open_png_editor(Some(path.clone()), Some(id), cx)),
                    ))
                })
                .child(
                    Button::new("avatar-change").label("Change").small().outline().on_click(
                        cx.listener(move |this, _, window, cx| this.open_avatar_library(Some(id), window, cx)),
                    ),
                );
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
        let action = self.cohost_switch(source, cx);
        self.section("Avatar", action, cx).child(card).child(state).into_any_element()
    }

    /// A PNGtuber's Voice: the mixer track whose sound makes it talk.
    fn voice_section(&self, source: &Source, cx: &mut Context<Self>) -> AnyElement {
        let id = source.id;
        let s = source.settings.clone();
        let workspace = self.studio.read(cx).workspace().clone();
        let current = s["mic"].as_str().unwrap_or("").to_string();
        let tracks: Vec<(String, String)> =
            workspace.tracks.iter().map(|t| (t.id.to_string(), t.name.clone())).collect();
        let shown = tracks.iter().find(|(t, _)| *t == current).map_or("None".to_string(), |(_, n)| n.clone());
        let me = cx.entity().downgrade();
        let menu = Button::new("avatar-mic").label(shown).small().outline().dropdown_menu(move |mut m, _, _| {
            for (value, name) in tracks.clone() {
                let me = me.clone();
                let cmd = Command::SetSourceSettings { source: id, settings: with(&s, "mic", json!(value)) };
                m = m.item(PopupMenuItem::new(name).checked(value == current).on_click(move |_, _, cx| {
                    if let Some(me) = me.upgrade() {
                        me.update(cx, |this, cx| this.exec(cmd.clone(), cx));
                    }
                }));
            }
            m
        });
        // push to talk: it talks only while these keys are held
        let ptt =
            source.settings["push_to_talk_keys"].as_str().filter(|k| !k.is_empty()).map(crate::shortcuts::display);
        let waiting = self.push_to_talk_capture == Some(id);
        let held = self.push_to_talk_on.contains(&id);
        let mut cleared = source.settings.clone();
        if let Some(m) = cleared.as_object_mut() {
            m.remove("push_to_talk_keys");
        }
        let clear = Command::SetSourceSettings { source: id, settings: cleared };
        let note = self.avatar_note.as_ref().filter(|(s, _)| *s == id).map(|(_, n)| n.clone());
        let ptt_row = div()
            .flex()
            .items_center()
            .justify_between()
            .gap_2()
            .child(div().flex().items_center().gap_1().child(div().text_size(px(12.)).text_color(tok::text_secondary()).child("Push to talk")).child(ui::info(
                "avatar-ptt-info",
                if cfg!(windows) {
                    "With keys set, it talks only after you press them, until you press them again. Works in any app."
                } else {
                    "With keys set, it talks only while you hold them. Works in any app."
                },
            )))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_1()
                    .child(if waiting {
                        div().text_size(px(11.)).text_color(tok::warn()).child("Press keys… (Esc cancels)")
                    } else {
                        div()
                            .text_size(px(11.))
                            .font_family(crate::ui::MONO)
                            .text_color(if held { tok::ok() } else { tok::text_secondary() })
                            .child(ptt.clone().unwrap_or_else(|| "Off".into()))
                    })
                    .child(Button::new("avatar-ptt-set").label(if ptt.is_some() { "Change" } else { "Set" }).xsmall().outline().on_click(cx.listener(move |this, _, window, cx| {
                        this.push_to_talk_capture = Some(id);
                        this.avatar_note = None;
                        window.focus(&this.focus, cx);
                        cx.notify();
                    })))
                    .when(ptt.is_some(), |d| {
                        d.child(Button::new("avatar-ptt-clear").icon(ui::icon("x")).xsmall().ghost().on_click(cx.listener(move |this, _, _, cx| this.exec(clear.clone(), cx))))
                    }),
            );
        self.section("Voice", None, cx)
            .child(ptt_row)
            .children(note.map(|n| div().text_size(px(11.)).text_color(tok::warn()).child(n)))
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap_2()
                    .child(div().flex().items_center().gap_1().child(div().text_size(px(12.)).text_color(tok::text_secondary()).child("Mic")).child(ui::info(
                        "avatar-mic-info",
                        "Its mouth opens with this mixer track's sound. When it opens and closes is set in its editor (Edit).",
                    )))
                    .child(menu),
            )
            .into_any_element()
    }

    /// Tracking: the camera, how much of the body follows, Calibrate, Mirror; Check tracking in the
    /// header.
    fn tracking_section(&self, source: &Source, cx: &mut Context<Self>) -> AnyElement {
        let id = source.id;
        let s = source.settings.clone();
        let set = |key: &str, v: Value| Command::SetSourceSettings { source: id, settings: with(&s, key, v) };
        let label = |text: &'static str| div().text_size(px(12.)).text_color(tok::text_secondary()).child(text);
        let file = s["file"].as_str().unwrap_or("");
        let device = s["device"].as_str().unwrap_or("").to_string();
        let flat = crate::avatar_library::kind_of(file) != "VRM";

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
        let row = |name: &'static str, info: Option<(&'static str, &'static str)>, control: AnyElement| {
            div()
                .flex()
                .items_center()
                .justify_between()
                .gap_2()
                .child(
                    div().flex().items_center().gap_1().child(label(name)).children(info.map(|(k, t)| ui::info(k, t))),
                )
                .child(control)
        };

        // how much follows: a VRM's body on macOS; the face only otherwise (said in its ⓘ)
        let follows = if !flat && cfg!(target_os = "macos") {
            let body = s["body"].as_str().unwrap_or("upper");
            let bodies = [("Face", "off"), ("Upper body", "upper"), ("Full body", "full")]
                .into_iter()
                .map(|(l, v)| (l, set("body", json!(v)), body == v))
                .collect();
            div()
                .flex()
                .flex_col()
                .gap_1()
                .child(div().flex().items_center().gap_1().child(label("Follows")).child(ui::info(
                    "avatar-body-info",
                    "Upper body: your arms, hands and fingers too, when the camera sees them. Full body: your legs as well (stand back so the camera sees all of you).",
                )))
                .child(self.segmented("avatar-body", bodies, cx))
                .into_any_element()
        } else {
            let why = if flat {
                "2D models follow the face."
            } else {
                "Body following comes to Windows later: the face, for now."
            };
            row(
                "Follows",
                Some(("avatar-body-info", why)),
                div().text_size(px(12.)).text_color(tok::text_tertiary()).child("Face").into_any_element(),
            )
            .into_any_element()
        };

        // Calibrate, and when it was; how to and Reset in its ⓘ menu
        let calibrated = s["calibrated_at"]
            .as_i64()
            .or_else(|| s["calibration"].as_str().filter(|c| !c.is_empty()).map(|_| 0))
            .map(|ms| if ms == 0 { "Calibrated".to_string() } else { format!("Calibrated {}", when(ms)) });
        let note =
            self.avatar_note.as_ref().filter(|(src, n)| *src == id && n.starts_with("No face")).map(|(_, n)| n.clone());
        let mut reset_settings = s.clone();
        if let Some(m) = reset_settings.as_object_mut() {
            m.remove("calibration");
            m.remove("calibrated_at");
        }
        let reset = Command::SetSourceSettings { source: id, settings: reset_settings };
        let has_calibration = calibrated.is_some();
        let me = cx.entity().downgrade();
        let calibrate_menu =
            Button::new("calibrate-menu").icon(ui::icon("info")).xsmall().ghost().dropdown_menu(move |menu, _, _| {
                let me = me.clone();
                let reset = reset.clone();
                menu.item(PopupMenuItem::new("Look at the camera, face relaxed, then click Calibrate.").disabled(true))
                    .separator()
                    .item(PopupMenuItem::new("Reset calibration").disabled(!has_calibration).on_click(
                        move |_, _, cx| {
                            if let Some(me) = me.upgrade() {
                                me.update(cx, |this, cx| this.exec(reset.clone(), cx));
                            }
                        },
                    ))
            });
        let calibrate = div()
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
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .text_size(px(11.))
                    .text_color(if note.is_some() { tok::warn() } else { tok::text_tertiary() })
                    .child(note.or(calibrated).unwrap_or_else(|| "Not calibrated".into())),
            )
            .child(calibrate_menu);

        let mirror = s["mirror"].as_bool().unwrap_or(true);
        let mirror_cmd = set("mirror", json!(!mirror));
        let mirror_row = row(
            "Mirror",
            Some(("avatar-mirror-info", "On: it moves like your reflection. Off: as the camera sees you.")),
            super::settings::switch("avatar-mirror", mirror)
                .on_click(cx.listener(move |this, _, _, cx| this.exec(mirror_cmd.clone(), cx)))
                .into_any_element(),
        );

        // Check tracking: the trackers' picture for 15 s
        let checking = s["show_tracking"].as_bool().unwrap_or(false);
        let check = div()
            .flex()
            .items_center()
            .gap_1()
            .child(
                Button::new("avatar-check-tracking")
                    .icon(ui::icon(if checking { "eye-off" } else { "scan-face" }))
                    .label(if checking { "Stop checking" } else { "Check tracking" })
                    .xsmall()
                    .ghost()
                    .disabled(device.is_empty())
                    .on_click(cx.listener(move |this, _, _, cx| this.check_tracking(id, !checking, cx))),
            )
            .child(ui::info(
                "avatar-check-tracking-info",
                "Shows the camera's picture in the avatar's corner for 15 seconds, with what's followed marked: face green, body red (orange: flat, no depth), hands yellow. It shows on air too.",
            ));

        self.section("Tracking", Some(check.into_any_element()), cx)
            .child(row(
                "Camera",
                Some((
                    "avatar-camera-info",
                    "The camera that sees your face. Its picture is never shown, only the avatar.",
                )),
                camera.into_any_element(),
            ))
            .child(follows)
            .child(calibrate)
            .child(mirror_row)
            .into_any_element()
    }

    /// Framing: head, bust, half or full and the zoom in one row; the shape (the avatar's picture
    /// and its box together).
    fn framing_section(&self, source: &Source, cx: &mut Context<Self>) -> AnyElement {
        let id = source.id;
        let s = source.settings.clone();
        let set = |key: &str, v: Value| Command::SetSourceSettings { source: id, settings: with(&s, key, v) };
        let framing = s["framing"].as_str().unwrap_or("bust");
        let framings = [("Head", "head"), ("Bust", "bust"), ("Half", "half"), ("Full", "full")]
            .into_iter()
            .map(|(l, v)| (l, set("framing", json!(v)), framing == v))
            .collect();
        let zoom = s["zoom"].as_f64().unwrap_or(1.0) as f32;
        let base = s.clone();
        let zoom_field = self
            .number(
                format!("avatar-zoom-{id}"),
                ui::icon("zoom-in").size(px(11.)).into_any_element(),
                zoom * 100.0,
                "%",
                1.0,
                move |v| Command::SetSourceSettings {
                    source: id,
                    settings: with(&base, "zoom", json!((v / 100.0).clamp(0.5, 2.0))),
                },
                cx,
            )
            .w(px(78.))
            .flex_none();
        let (w, h) = (s["width"].as_u64().unwrap_or(1080) as u32, s["height"].as_u64().unwrap_or(1080) as u32);
        let mut shapes = div().flex().gap_0p5().p_0p5().rounded(px(6.)).bg(tok::bg_raised());
        for (i, (name, sw, sh)) in
            [("Square", 1080u32, 1080u32), ("Portrait", 1080, 1920), ("Landscape", 1920, 1080)].into_iter().enumerate()
        {
            let on = (w, h) == (sw, sh);
            shapes = shapes.child(
                div()
                    .id(("avatar-shape", i))
                    .flex()
                    .flex_1()
                    .justify_center()
                    .py_1()
                    .rounded(px(5.))
                    .text_size(px(12.))
                    .cursor_pointer()
                    .when(on, |d| d.bg(tok::bg_selected()).text_color(tok::text()))
                    .when(!on, |d| d.text_color(tok::text_secondary()).hover(|d| d.text_color(tok::text())))
                    .child(name)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if !on {
                            this.set_avatar_shape(id, (sw, sh), cx)
                        }
                    })),
            );
        }
        self.section("Framing", None, cx)
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(div().flex_1().min_w_0().child(self.segmented("avatar-framing", framings, cx)))
                    .child(zoom_field),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_1()
                            .w(px(56.))
                            .flex_none()
                            .child(div().text_size(px(12.)).text_color(tok::text_secondary()).child("Shape"))
                            .child(ui::info(
                                "avatar-shape-info",
                                "The avatar's picture and its box change together, about the box's middle.",
                            )),
                    )
                    .child(shapes.flex_1()),
            )
            .into_any_element()
    }

    /// Expressions: the model's own as chips with their shortcuts' keys, a search past 12, how
    /// strongly it shows (VRM, Live2D), and the shortcut list behind the keyboard button.
    fn expressions_section(&self, source: &Source, cx: &mut Context<Self>) -> Option<AnyElement> {
        let id = source.id;
        let s = source.settings.clone();
        let set = |key: &str, v: Value| Command::SetSourceSettings { source: id, settings: with(&s, key, v) };
        let status = self.avatar_status.get(&id).cloned().unwrap_or_default();
        if status.expressions.is_empty() {
            return None;
        }
        let file = s["file"].as_str().unwrap_or("");
        let kind = crate::avatar_library::kind_of(file);
        let expression = s["expression"].as_str().unwrap_or("");
        let choices: Vec<String> = std::iter::once(String::new()).chain(status.expressions.iter().cloned()).collect();
        // past 12, a search (the chosen one always shows)
        let search = self
            .props
            .expression_search
            .as_ref()
            .filter(|(src, _, _)| *src == id && choices.len() > 12)
            .map(|(_, e, _)| e.clone());
        let query = search.as_ref().map(|e| e.read(cx).value().to_lowercase()).unwrap_or_default();
        let keys = s["expression_keys"].clone();
        let key_of = |e: &str| keys[e].as_str().filter(|k| !k.is_empty()).map(crate::shortcuts::display);
        let mut chips = div().flex().flex_wrap().gap_1();
        for (i, value) in choices.iter().enumerate() {
            let on = expression == value;
            if !query.is_empty()
                && !on
                && !value.is_empty()
                && !value.to_lowercase().contains(&query)
                && !expression_label(value).to_lowercase().contains(&query)
            {
                continue;
            }
            let cmd = set("expression", json!(value));
            chips = chips.child(
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
                    // a costume (a PNGtuber's): a shirt before its name
                    .when(status.costumes.contains(value), |d| d.child(ui::icon("shirt").size(px(11.))))
                    .child(expression_label(value))
                    .children(key_of(value).map(|k| div().text_color(tok::text_tertiary()).child(k)))
                    // the model's own name for it, when it's shown otherwise
                    .when(
                        expression_label(value) != *value && !value.is_empty() && value.chars().any(char::is_uppercase),
                        |d| {
                            let raw = value.clone();
                            d.tooltip(move |w, cx| gpui_kit::component::tooltip::Tooltip::new(raw.clone()).build(w, cx))
                        },
                    )
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if !on {
                            this.exec(cmd.clone(), cx)
                        }
                    })),
            );
        }
        // how strongly (a VRM's, a Live2D model's; Inochi2D and Umamo hold theirs at full)
        let weight = (matches!(kind, "VRM" | "Live2D") && !expression.is_empty()).then(|| {
            let w = s["expression_weight"].as_f64().unwrap_or(1.0) as f32;
            let base = s.clone();
            let slider = self.slider(
                format!("avatar-weight-{id}"),
                (0.0, 1.0, 0.05),
                w,
                move |v| Command::SetSourceSettings {
                    source: id,
                    settings: with(&base, "expression_weight", json!(v)),
                },
                cx,
            );
            div()
                .flex()
                .items_center()
                .gap_2()
                .child(
                    div().w(px(56.)).flex_none().text_size(px(12.)).text_color(tok::text_secondary()).child("Weight"),
                )
                .child(div().flex_1().child(slider))
                .child(
                    div()
                        .w(px(40.))
                        .text_right()
                        .text_size(px(11.))
                        .font_family(crate::ui::MONO)
                        .text_color(tok::text_tertiary())
                        .child(format!("{:.0}%", w * 100.0)),
                )
        });

        // each expression's shortcut, when the list is open
        let open = self.expression_keys_open;
        let capturing = self.expression_capture.as_ref().filter(|(src, _)| *src == id).map(|(_, e)| e.clone());
        let mut shortcuts = div().flex().flex_col().gap_1();
        if open {
            for (i, value) in choices.iter().enumerate() {
                let bound = key_of(value);
                let waiting = capturing.as_deref() == Some(value.as_str());
                let v1 = value.clone();
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
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .overflow_hidden()
                                .text_ellipsis()
                                .whitespace_nowrap()
                                .text_size(px(12.))
                                .child(expression_label(value)),
                        )
                        .child(if waiting {
                            div().text_size(px(11.)).text_color(tok::warn()).child("Press keys… (Esc cancels)")
                        } else {
                            div()
                                .text_size(px(11.))
                                .font_family(crate::ui::MONO)
                                .text_color(tok::text_secondary())
                                .child(bound.clone().unwrap_or_else(|| "—".into()))
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
                                    .on_click(cx.listener(move |this, _, _, cx| this.exec(clear.clone(), cx))),
                            )
                        }),
                );
            }
        }
        let header = div()
            .flex()
            .items_center()
            .gap_1()
            .when(kind == "Inochi2D" || kind == "Umamo", |d| {
                d.child(ui::info(
                    "avatar-expressions-info",
                    "This model's file has no expressions: these are its parameters the face doesn't move. Choosing one holds it fully on.",
                ))
            })
            .child(
                Button::new("avatar-expression-keys")
                    .icon(ui::icon("keyboard"))
                    .xsmall()
                    .ghost()
                    .when(open, |b| b.selected(true))
                    .tooltip("Shortcuts: press one in any app to switch to that expression; again to go back to none")
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.expression_keys_open = !this.expression_keys_open;
                        this.expression_capture = None;
                        cx.notify();
                    })),
            );
        let key_note = self
            .avatar_note
            .as_ref()
            .filter(|(src, n)| *src == id && open && !n.starts_with("No face"))
            .map(|(_, n)| div().text_size(px(11.)).text_color(tok::warn()).child(n.clone()));
        Some(
            self.section("Expressions", Some(header.into_any_element()), cx)
                .children(search.map(|e| TextInput::new(&e).small()))
                .child(chips)
                .children(weight)
                .when(open, |d| d.child(shortcuts).children(key_note))
                .into_any_element(),
        )
    }

    /// Accessories: what's under the avatar, each with what it's pinned to; click one to select it.
    fn accessories_section(
        &self,
        scene: &Scene,
        object: &Object,
        source: &Source,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let (avatar_object, id) = (object.id, source.id);
        let anchors = self.avatar_status.get(&id).map(|s| s.anchors.clone()).unwrap_or_default();
        let mut list = div().flex().flex_col().gap_0p5();
        let children: Vec<&Object> = scene.children(Some(avatar_object)).collect();
        for (i, child) in children.iter().enumerate() {
            let pinned = super::avatar_sheets::pin_of(&source.settings, child.id);
            let on = pinned.as_ref().map(|k| {
                anchors.iter().find(|a| a.key == *k && a.group != "Suggested").map_or(k.clone(), |a| a.label.clone())
            });
            let kind = child
                .source
                .and_then(|s| self.studio.read(cx).workspace().sources.get(&s).map(|i| i.kind.clone()))
                .unwrap_or_default();
            let child_id = child.id;
            list = list.child(
                div()
                    .id(("avatar-accessory", i))
                    .flex()
                    .items_center()
                    .gap_2()
                    .px_2()
                    .py_1()
                    .rounded(px(6.))
                    .cursor_pointer()
                    .hover(|d| d.bg(tok::bg_raised()))
                    .child(ui::icon(ui::source_icon(&kind)).size(px(13.)).text_color(tok::text_secondary()))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .overflow_hidden()
                            .text_ellipsis()
                            .whitespace_nowrap()
                            .text_size(px(12.))
                            .child(child.name.clone()),
                    )
                    .child(
                        div()
                            .flex_none()
                            .text_size(px(11.))
                            .text_color(tok::text_tertiary())
                            .child(on.unwrap_or_else(|| "Not pinned".into())),
                    )
                    .on_click(cx.listener(move |this, _, _, cx| this.select_object(Some(child_id), cx))),
            );
        }
        if children.is_empty() {
            list = list.child(div().text_size(px(11.)).text_color(tok::text_tertiary()).child("None"));
        }
        let add = Button::new("avatar-accessory-add")
            .icon(ui::icon("plus"))
            .label("Add")
            .xsmall()
            .ghost()
            .tooltip("A picture or video that follows a bone or part of the avatar")
            .on_click(cx.listener(move |this, _, window, cx| this.add_accessory(avatar_object, id, window, cx)));
        self.section("Accessories", Some(add.into_any_element()), cx).child(list).into_any_element()
    }
}

/// When something was done, as a person reads it: "14:02" today, else "22 Nov 14:02".
fn when(ms: i64) -> String {
    use chrono::TimeZone;
    let Some(t) = chrono::Local.timestamp_millis_opt(ms).single() else { return String::new() };
    if t.date_naive() == chrono::Local::now().date_naive() {
        t.format("%H:%M").to_string()
    } else {
        t.format("%-d %b %H:%M").to_string()
    }
}

/// Why a model didn't load, in people's words, with what to do.
fn load_problem(file: &str, error: &str) -> String {
    let e = error.to_lowercase();
    if !std::path::Path::new(file).exists() {
        "The model file isn't there any more: choose it again with Change".into()
    } else if e.contains("not a vrm") {
        "This file has no VRM data: export it as VRM and choose it again".into()
    } else if e.contains("humanoid") {
        "This VRM has no humanoid head or hips: it can't follow a face".into()
    } else {
        "The model couldn't be read: try exporting it again".into()
    }
}

#[cfg(test)]
mod tests {
    use super::expression_label;

    #[test]
    fn model_parameter_names_read_as_words_with_the_side_last() {
        for (name, label) in [
            ("ParamBrowLAngle", "Brow angle (left)"),
            ("ParamEyeROpen", "Eye open (right)"),
            ("ParamMouthOpenY", "Mouth open y"),
            ("Arm:: Left:: Move", "Arm move (left)"),
            ("eyeBlinkRight", "Eye blink (right)"),
        ] {
            assert_eq!(expression_label(name), label, "{name}");
        }
    }

    #[test]
    fn presets_are_capitalised_and_no_expression_reads_none() {
        assert_eq!(expression_label("happy"), "Happy");
        assert_eq!(expression_label(""), "None");
        assert_eq!(expression_label("Paramedic"), "Paramedic", "only a \"Param\" prefix is dropped");
    }
}
