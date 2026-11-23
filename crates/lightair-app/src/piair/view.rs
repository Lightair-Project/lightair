//! The AI co-host's sections in the Inspector, under its Avatar card: Talk, Brain, Voice, Persona
//! and Behaviour. The co-host's settings live on its object (the avatar source's `cohost.piair`),
//! so they're saved with the project and undoable; Lightair passes them on to PiAir. Keys and
//! sign-ins stay in the Keychain. Labels and values only; what each does is behind an ⓘ.

use std::collections::HashSet;
use std::sync::atomic::Ordering;
use std::time::Duration;

use gpui_kit::component::Sizable;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input as TextInput, InputEvent, InputState};
use gpui_kit::component::menu::{DropdownMenu, PopupMenuItem};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;
use lightair_model::{Command, Id};
use serde_json::{Value, json};

use super::{COHOST, Host};
use crate::studio::Studio;
use crate::ui::{self, tok};

const VENDORS: &[(&str, &str)] =
    &[("cartesia", "Cartesia"), ("elevenlabs", "ElevenLabs"), ("gemini", "Gemini"), ("none", "None")];
const CHATTINESS: &[&str] = &["Quiet", "Normal", "Lively", "Chatty"];
/// Lines of the conversation the Talk section shows.
const TALK_LINES: usize = 6;

pub struct CohostView {
    host: Host,
    studio: Entity<Studio>,
    /// The co-host object's source the Inspector shows, and whether it's the active co-host.
    source: Option<Id>,
    active: bool,
    prompt: Entity<InputState>,
    key: Entity<InputState>,
    voice_key: Entity<InputState>,
    name: Entity<InputState>,
    personality: Entity<InputState>,
    code: Entity<InputState>,
    collapsed: HashSet<&'static str>,
    _subs: Vec<Subscription>,
}

impl CohostView {
    pub fn new(host: Host, studio: Entity<Studio>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let prompt = cx.new(|cx| InputState::new(window, cx).placeholder("Talk to your co-host…"));
        let key = cx.new(|cx| InputState::new(window, cx).placeholder("API key").masked(true));
        let voice_key = cx.new(|cx| InputState::new(window, cx).placeholder("Voice API key").masked(true));
        let name = cx.new(|cx| InputState::new(window, cx).placeholder("Pi"));
        let personality = cx.new(|cx| InputState::new(window, cx).placeholder("Warm, quick and playful"));
        let code = cx.new(|cx| InputState::new(window, cx).placeholder("Code from the sign-in page"));
        let mut subs = Vec::new();
        subs.push(cx.subscribe_in(&prompt, window, |this, state, ev: &InputEvent, window, cx| {
            if matches!(ev, InputEvent::PressEnter { .. }) {
                let text = state.read(cx).value().trim().to_string();
                if !text.is_empty() {
                    state.update(cx, |s, cx| s.set_value("", window, cx));
                    this.host.prompt(&text);
                }
            }
        }));
        subs.push(cx.subscribe_in(&key, window, |this, state, ev: &InputEvent, window, cx| {
            if matches!(ev, InputEvent::PressEnter { .. } | InputEvent::Blur) {
                let k = state.read(cx).value().trim().to_string();
                if !k.is_empty() {
                    let provider = this.setting("/provider", cx).as_str().unwrap_or("google").to_string();
                    this.host.send(json!({"t": "login", "provider": provider, "kind": "api_key", "key": k}));
                    state.update(cx, |s, cx| s.set_value("", window, cx));
                }
            }
        }));
        subs.push(cx.subscribe_in(&voice_key, window, |this, state, ev: &InputEvent, window, cx| {
            if matches!(ev, InputEvent::PressEnter { .. } | InputEvent::Blur) {
                let k = state.read(cx).value().trim().to_string();
                if !k.is_empty() {
                    let vendor = this.setting("/voice/vendor", cx).as_str().unwrap_or("gemini").to_string();
                    if let Ok(e) = keyring::Entry::new(super::KEYCHAIN_SERVICE, &format!("voice:{vendor}")) {
                        let _ = e.set_password(&k);
                    }
                    this.host.send(json!({"t": "voice_key"}));
                    state.update(cx, |s, cx| s.set_value("", window, cx));
                }
            }
        }));
        for (input, field) in [(name.clone(), "name"), (personality.clone(), "personality")] {
            subs.push(cx.subscribe_in(&input, window, move |this, state, ev: &InputEvent, _, cx| {
                if matches!(ev, InputEvent::PressEnter { .. } | InputEvent::Blur) {
                    let v = state.read(cx).value().trim().to_string();
                    if !v.is_empty() && this.setting(&format!("/persona/{field}"), cx).as_str() != Some(v.as_str()) {
                        this.set(json!({"persona": {field: v}}), cx);
                    }
                }
            }));
        }
        subs.push(cx.subscribe_in(&code, window, |this, state, ev: &InputEvent, window, cx| {
            if matches!(ev, InputEvent::PressEnter { .. }) {
                let c = state.read(cx).value().trim().to_string();
                this.host.send(json!({"t": "auth_code", "code": c}));
                state.update(cx, |s, cx| s.set_value("", window, cx));
            }
        }));
        // what PiAir says shows as it happens; the persona fields show what's set
        cx.spawn_in(window, async move |this, cx| {
            loop {
                cx.background_executor().timer(Duration::from_millis(120)).await;
                let ok = this.update_in(cx, |this, window, cx| {
                    for (input, pointer) in
                        [(this.name.clone(), "/persona/name"), (this.personality.clone(), "/persona/personality")]
                    {
                        let want = this.setting(pointer, cx).as_str().unwrap_or("").to_string();
                        if !want.is_empty()
                            && !input.read(cx).focus_handle(cx).is_focused(window)
                            && input.read(cx).value() != want.as_str()
                        {
                            input.update(cx, |s, cx| s.set_value(want, window, cx));
                        }
                    }
                    if this.host.0.dirty.swap(false, Ordering::Relaxed) {
                        cx.notify();
                    }
                });
                if ok.is_err() {
                    break;
                }
            }
        })
        .detach();
        CohostView {
            host,
            studio,
            source: None,
            active: false,
            prompt,
            key,
            voice_key,
            name,
            personality,
            code,
            collapsed: HashSet::new(),
            _subs: subs,
        }
    }

    /// Shows this co-host object's settings (`active`: it's the one PiAir runs for).
    pub fn set_source(&mut self, source: Option<Id>, active: bool) {
        self.source = source;
        self.active = active;
    }

    /// The object's own PiAir settings (`cohost.piair`), or Null.
    fn object_settings(&self, cx: &App) -> Value {
        let Some(id) = self.source else { return Value::Null };
        self.studio
            .read(cx)
            .workspace()
            .sources
            .get(&id)
            .map(|s| s.settings[COHOST]["piair"].clone())
            .unwrap_or(Value::Null)
    }

    /// A setting: the object's, else what PiAir is using (its defaults).
    fn setting(&self, pointer: &str, cx: &App) -> Value {
        let own = self.object_settings(cx).pointer(pointer).cloned().filter(|v| !v.is_null());
        own.or_else(|| self.host.state().settings.pointer(pointer).cloned()).unwrap_or(Value::Null)
    }

    /// Changes the object's settings (one undo step); Lightair passes them on to PiAir.
    fn set(&mut self, patch: Value, cx: &mut Context<Self>) {
        let Some(id) = self.source else { return };
        let Some(source) = self.studio.read(cx).workspace().sources.get(&id).cloned() else { return };
        let mut settings = source.settings.clone();
        let mut piair = settings[COHOST]["piair"].clone();
        if !piair.is_object() {
            piair = json!({});
        }
        merge(&mut piair, &patch);
        settings[COHOST]["piair"] = piair;
        let _ = self.studio.update(cx, |s, cx| s.execute(Command::SetSourceSettings { source: id, settings }, cx));
        cx.notify();
    }

    fn header(&self, title: &'static str, cx: &mut Context<Self>) -> impl IntoElement {
        let open = !self.collapsed.contains(title);
        div()
            .id(SharedString::from(format!("cohost-section-{title}")))
            .flex()
            .items_center()
            .gap_1()
            .h(px(34.))
            .cursor_pointer()
            .text_size(px(11.))
            .font_weight(FontWeight::SEMIBOLD)
            .text_color(tok::text_secondary())
            .hover(|d| d.text_color(tok::text()))
            .child(ui::icon(if open { "chevron-down" } else { "chevron-right" }).size(px(12.)))
            .child(title.to_uppercase())
            .on_click(cx.listener(move |this, _, _, cx| {
                if !this.collapsed.remove(title) {
                    this.collapsed.insert(title);
                }
                cx.notify();
            }))
    }

    fn section(&self, title: &'static str, body: Vec<AnyElement>, cx: &mut Context<Self>) -> AnyElement {
        let open = !self.collapsed.contains(title);
        div()
            .flex()
            .flex_col()
            .gap_2()
            .pb_3()
            .border_b_1()
            .border_color(tok::border())
            .child(self.header(title, cx))
            .when(open, |d| d.children(body))
            .into_any_element()
    }

    // ---------------------------------------------------------------- Talk

    fn talk(&self, cx: &mut Context<Self>) -> AnyElement {
        if !self.active {
            let line = div()
                .text_size(px(12.))
                .text_color(tok::text_tertiary())
                .child("Make it the active co-host to talk with it.");
            return self.section("Talk", vec![line.into_any_element()], cx);
        }
        let s = self.host.state();
        let running = s.running;
        let lines: Vec<_> =
            s.log.iter().rev().take(TALK_LINES).cloned().collect::<Vec<_>>().into_iter().rev().collect();
        let question = s.question.as_ref().map(|q| q.text.clone());
        let auth = s.auth.clone().filter(|a| !a.done || a.error);
        drop(s);
        let name = self.setting("/persona/name", cx).as_str().unwrap_or("Pi").to_string();
        let mut body: Vec<AnyElement> = Vec::new();
        if let Some(q) = question {
            body.push(
                card(tok::warn())
                    .child(div().text_size(px(12.)).child(q))
                    .child(
                        div()
                            .flex()
                            .gap_2()
                            .child(Button::new("cohost-allow").label("Allow").small().primary().on_click(cx.listener(
                                |this, _, _, cx| {
                                    this.host.answer_question(true);
                                    cx.notify();
                                },
                            )))
                            .child(Button::new("cohost-deny").label("Don't").small().outline().on_click(cx.listener(
                                |this, _, _, cx| {
                                    this.host.answer_question(false);
                                    cx.notify();
                                },
                            ))),
                    )
                    .into_any_element(),
            );
        }
        if let Some(a) = auth {
            let color = if a.error { tok::error() } else { tok::accent_primary() };
            body.push(
                card(color)
                    .child(div().text_size(px(12.)).child(a.message.clone()))
                    .children(
                        a.code.clone().map(|c| div().text_size(px(16.)).font_weight(FontWeight::SEMIBOLD).child(c)),
                    )
                    .when(a.wants_code, |d| d.child(TextInput::new(&self.code).xsmall()))
                    .child(
                        div()
                            .flex()
                            .gap_2()
                            .children(a.url.clone().map(|url| {
                                Button::new("cohost-auth-open")
                                    .label("Open sign-in page")
                                    .xsmall()
                                    .outline()
                                    .on_click(move |_, _, cx| cx.open_url(&url))
                            }))
                            .child(Button::new("cohost-auth-close").label("Close").xsmall().ghost().on_click(
                                cx.listener(|this, _, _, cx| {
                                    this.host.state().auth = None;
                                    cx.notify();
                                }),
                            )),
                    )
                    .into_any_element(),
            );
        }
        if lines.is_empty() {
            body.push(
                div()
                    .text_size(px(12.))
                    .text_color(tok::text_tertiary())
                    .child(if running {
                        "What it hears and says shows here."
                    } else {
                        "Turn it on to bring your co-host in."
                    })
                    .into_any_element(),
            );
        } else {
            for (i, l) in lines.into_iter().enumerate() {
                let el = if l.role == "tool" {
                    div()
                        .id(("cohost-line", i))
                        .text_size(px(11.))
                        .text_color(tok::text_tertiary())
                        .child(format!("· {}", tool_label(&l.text)))
                } else {
                    let (who, color) = match l.role.as_str() {
                        "piair" => (name.clone(), tok::ok()),
                        "streamer" => ("You".to_string(), tok::text_secondary()),
                        _ => (l.who.clone().unwrap_or_default(), tok::text_secondary()),
                    };
                    div()
                        .id(("cohost-line", i))
                        .text_size(px(12.))
                        .child(div().text_size(px(11.)).font_weight(FontWeight::SEMIBOLD).text_color(color).child(who))
                        .child(div().text_color(tok::text()).child(l.text))
                };
                body.push(el.into_any_element());
            }
        }
        body.push(TextInput::new(&self.prompt).small().disabled(!running).into_any_element());
        self.section("Talk", body, cx)
    }

    // ---------------------------------------------------------------- Brain

    fn brain(&self, cx: &mut Context<Self>) -> AnyElement {
        let st = self.host.state();
        let ready = st.ready;
        let providers = st.providers.as_array().cloned().unwrap_or_default();
        drop(st);
        if !ready {
            let line =
                div().text_size(px(12.)).text_color(tok::text_tertiary()).child("Turn it on to choose its brain.");
            return self.section("Brain", vec![line.into_any_element()], cx);
        }
        let provider = self.setting("/provider", cx).as_str().unwrap_or("").to_string();
        let model = self.setting("/model", cx).as_str().unwrap_or("").to_string();
        let p = providers.iter().find(|x| x["id"] == provider.as_str()).cloned().unwrap_or(Value::Null);
        let signed_in = p["signedIn"].as_bool().unwrap_or(false);
        let oauth = p["auth"].as_array().is_some_and(|a| a.iter().any(|x| x == "oauth"));
        let me = cx.entity().downgrade();
        let provider_menu = {
            let me = me.clone();
            let label =
                p["name"].as_str().unwrap_or(if provider.is_empty() { "Choose" } else { &provider }).to_string();
            let list = providers.clone();
            Button::new("cohost-provider").label(label).xsmall().outline().dropdown_caret(true).dropdown_menu(
                move |mut menu, _, _| {
                    for x in &list {
                        let id = x["id"].as_str().unwrap_or("").to_string();
                        let first = x["models"]
                            .as_array()
                            .and_then(|m| m.first())
                            .and_then(|m| m["id"].as_str())
                            .unwrap_or("")
                            .to_string();
                        let name = format!(
                            "{}{}",
                            x["name"].as_str().unwrap_or(&id),
                            if x["signedIn"].as_bool() == Some(true) { "  ✓" } else { "" }
                        );
                        let me = me.clone();
                        menu = menu.item(PopupMenuItem::new(name).on_click(move |_, _, cx| {
                            if let Some(me) = me.upgrade() {
                                me.update(cx, |this, cx| this.set(json!({"provider": id, "model": first}), cx));
                            }
                        }));
                    }
                    menu
                },
            )
        };
        let model_menu = {
            let me = me.clone();
            let list = p["models"].as_array().cloned().unwrap_or_default();
            let label = list
                .iter()
                .find(|m| m["id"] == model.as_str())
                .and_then(|m| m["name"].as_str())
                .unwrap_or(&model)
                .to_string();
            Button::new("cohost-model").label(label).xsmall().outline().dropdown_caret(true).dropdown_menu(
                move |mut menu, _, _| {
                    for m in list.iter().take(60) {
                        let id = m["id"].as_str().unwrap_or("").to_string();
                        let name = m["name"].as_str().unwrap_or(&id).to_string();
                        let me = me.clone();
                        menu = menu.item(PopupMenuItem::new(name).on_click(move |_, _, cx| {
                            if let Some(me) = me.upgrade() {
                                me.update(cx, |this, cx| this.set(json!({"model": id}), cx));
                            }
                        }));
                    }
                    menu
                },
            )
        };
        let sign_in: AnyElement = if signed_in {
            div().text_size(px(12.)).text_color(tok::ok()).child("Signed in").into_any_element()
        } else if oauth {
            let provider = provider.clone();
            div()
                .flex()
                .gap_1()
                .child(Button::new("cohost-oauth").label("Use subscription").xsmall().outline().on_click(cx.listener(
                    move |this, _, _, cx| {
                        this.host.send(json!({"t": "login", "provider": provider, "kind": "oauth"}));
                        cx.notify();
                    },
                )))
                .child(field(&self.key, 120.))
                .into_any_element()
        } else {
            field(&self.key, 170.).into_any_element()
        };
        self.section(
            "Brain",
            vec![
                row("Provider", Some("Any provider Pi supports: an API key, or a subscription (Claude Pro/Max, ChatGPT, GitHub Copilot) where it has one. Keys stay in the Keychain."), provider_menu.into_any_element()),
                row("Model", Some("A fast model answers sooner. Using the computer needs a model that can see images."), model_menu.into_any_element()),
                row("Sign in", None, sign_in),
            ],
            cx,
        )
    }

    // ---------------------------------------------------------------- Voice, Persona, Behaviour

    fn voice(&self, cx: &mut Context<Self>) -> AnyElement {
        let vendor = self.setting("/voice/vendor", cx).as_str().unwrap_or("gemini").to_string();
        let chips = chips(
            "cohost-vendor",
            VENDORS.iter().map(|(id, name)| (name.to_string(), vendor == *id)).collect(),
            cx,
            |this, i, cx| this.set(json!({"voice": {"vendor": VENDORS[i].0, "voice": ""}}), cx),
        );
        let mut body = vec![stacked(
            "Provider",
            Some(
                "Cartesia and ElevenLabs answer in about a tenth of a second and give timings for the mouth. Gemini works with a Gemini key. None: it writes in Talk instead of speaking. It plays on the co-host's own mixer track.",
            ),
            chips,
        )];
        if vendor != "none" {
            body.push(row("Key", Some("Kept in the Keychain."), field(&self.voice_key, 170.).into_any_element()));
        }
        self.section("Voice", body, cx)
    }

    fn persona(&self, cx: &mut Context<Self>) -> AnyElement {
        let ff = self.setting("/persona/familyFriendly", cx).as_bool().unwrap_or(true);
        self.section(
            "Persona",
            vec![
                row(
                    "Name",
                    Some("It answers when chat or you say its name."),
                    field(&self.name, 170.).into_any_element(),
                ),
                row(
                    "Personality",
                    Some("A few words on how it talks."),
                    field(&self.personality, 190.).into_any_element(),
                ),
                row(
                    "Family friendly",
                    Some("No swearing or adult topics; anything that slips through is cut before it's said."),
                    switch("cohost-ff", ff)
                        .on_click(
                            cx.listener(move |this, _, _, cx| {
                                this.set(json!({"persona": {"familyFriendly": !ff}}), cx)
                            }),
                        )
                        .into_any_element(),
                ),
            ],
            cx,
        )
    }

    fn behaviour(&self, cx: &mut Context<Self>) -> AnyElement {
        let chattiness = self.setting("/behaviour/chattiness", cx).as_u64().unwrap_or(1) as usize;
        let flag = |pointer: &str, default: bool| self.setting(pointer, cx).as_bool().unwrap_or(default);
        let (answer, listen, yield_) = (
            flag("/behaviour/answerChat", true),
            flag("/behaviour/listen", true),
            flag("/behaviour/stopWhenStreamerTalks", true),
        );
        let chatty = chips(
            "cohost-chatty",
            CHATTINESS.iter().enumerate().map(|(i, n)| (n.to_string(), i == chattiness)).collect(),
            cx,
            |this, i, cx| this.set(json!({"behaviour": {"chattiness": i}}), cx),
        );
        let toggle = |id: &'static str, key: &'static str, on: bool, cx: &mut Context<Self>| {
            switch(id, on)
                .on_click(cx.listener(move |this, _, _, cx| this.set(json!({"behaviour": {key: !on}}), cx)))
                .into_any_element()
        };
        self.section(
            "Behaviour",
            vec![
                stacked("Chattiness", Some("Quiet: only when spoken to. Normal: also picks a chat message now and then. Lively and Chatty: more often."), chatty),
                row("Answer chat", Some("Reads chat from every platform and answers some of it. Chat can never make it change Lightair or use the computer."), toggle("cohost-chat", "answerChat", answer, cx)),
                row("Listen to me", Some("Hears you with on-device speech recognition and answers when you say its name."), toggle("cohost-listen", "listen", listen, cx)),
                row("Stop when I talk", Some("It goes quiet when your mic picks you up."), toggle("cohost-yield", "stopWhenStreamerTalks", yield_, cx)),
            ],
            cx,
        )
    }
}

impl Render for CohostView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .flex()
            .flex_col()
            .gap_3()
            .child(self.talk(cx))
            .child(self.brain(cx))
            .child(self.voice(cx))
            .child(self.persona(cx))
            .child(self.behaviour(cx))
    }
}

/// Deep-merges `patch` into `base` (objects merged key by key, anything else replaced).
fn merge(base: &mut Value, patch: &Value) {
    match (base, patch) {
        (Value::Object(b), Value::Object(p)) => {
            for (k, v) in p {
                merge(b.entry(k.clone()).or_insert(Value::Null), v);
            }
        }
        (b, p) => *b = p.clone(),
    }
}

fn tool_label(name: &str) -> String {
    match name {
        "computer_task" => "Using the computer".into(),
        "computer_screenshot" => "Looking at the screen".into(),
        "computer_click" => "Clicking".into(),
        "computer_type" => "Typing".into(),
        "computer_key" => "Pressing keys".into(),
        "computer_scroll" => "Scrolling".into(),
        "computer_move" => "Pointing".into(),
        "computer_zoom_to" => "Zooming in".into(),
        "remember" => "Remembering".into(),
        "recall" => "Recalling".into(),
        other => other.replace('_', " "),
    }
}

fn card(color: Hsla) -> Div {
    div().p_3().rounded(px(8.)).border_1().border_color(color).bg(tok::bg_raised()).flex().flex_col().gap_2()
}

fn label(name: &str, detail: Option<&str>) -> Div {
    div()
        .flex()
        .flex_none()
        .items_center()
        .gap_1()
        .child(div().text_size(px(12.)).text_color(tok::text_secondary()).child(name.to_string()))
        .children(detail.map(|d| ui::info(SharedString::from(format!("cohost-tip-{name}")), d.to_string())))
}

/// A setting as the Inspector shows one: its name (with an ⓘ) left, its control right.
fn row(name: &str, detail: Option<&str>, control: AnyElement) -> AnyElement {
    div()
        .flex()
        .items_center()
        .gap_2()
        .min_h(px(28.))
        .child(label(name, detail))
        .child(div().flex_1().min_w_0().flex().justify_end().child(control))
        .into_any_element()
}

/// A setting whose choices go on their own line under its name.
fn stacked(name: &str, detail: Option<&str>, control: AnyElement) -> AnyElement {
    div().flex().flex_col().gap_1p5().child(label(name, detail)).child(control).into_any_element()
}

/// A text field: as wide as there's room for, up to `max`.
fn field(input: &Entity<InputState>, max: f32) -> Div {
    div().w_full().max_w(px(max)).child(TextInput::new(input).xsmall())
}

/// The house switch: green when on.
pub(crate) fn switch(id: impl Into<ElementId>, on: bool) -> Stateful<Div> {
    div()
        .id(id)
        .flex_none()
        .w(px(34.))
        .h(px(20.))
        .rounded_full()
        .cursor_pointer()
        .bg(if on { tok::ok() } else { tok::border_strong() })
        .child(div().size(px(16.)).m(px(2.)).rounded_full().bg(rgb(0xFFFFFF)).when(on, |d| d.ml(px(16.))))
}

fn chips(
    id: &'static str,
    options: Vec<(String, bool)>,
    cx: &mut Context<CohostView>,
    pick: impl Fn(&mut CohostView, usize, &mut Context<CohostView>) + Clone + 'static,
) -> AnyElement {
    let mut row = div().flex().gap_0p5().p_0p5().rounded(px(7.)).bg(tok::bg_panel());
    for (i, (label, on)) in options.into_iter().enumerate() {
        let pick = pick.clone();
        row = row.child(
            div()
                .id((id, i))
                .flex_1()
                .px_1()
                .h(px(22.))
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(5.))
                .text_size(px(11.))
                .when(on, |d| d.bg(tok::bg_selected()).text_color(tok::text()))
                .when(!on, |d| d.cursor_pointer().text_color(tok::text_secondary()).hover(|d| d.bg(tok::bg_raised())))
                .child(label)
                .when(!on, |d| d.on_click(cx.listener(move |this, _, _, cx| pick(this, i, cx)))),
        );
    }
    row.into_any_element()
}
