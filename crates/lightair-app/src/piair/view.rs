//! The AI co-host's sections in the Inspector, under its Avatar card: Talk, Brain, Voice, Persona
//! and Behaviour. The co-host's settings live on its object (the avatar source's `cohost` block),
//! so they're saved with the project and undoable; Lightair passes them on to PiAir. Keys live in
//! Settings → Connections (the Keychain). Labels and values only; what each does is behind an ⓘ.

use std::collections::HashSet;
use std::sync::atomic::Ordering;
use std::time::Duration;

use gpui_kit::component::Sizable;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input as TextInput, InputEvent, InputState, Textarea, TextareaState};
use gpui_kit::component::menu::{DropdownMenu, PopupMenuItem};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;
use lightair_model::{Command, Id};
use serde_json::{Value, json};

use super::voice_sheet::VoiceSheet;
use super::{COHOST, Host, KeyState, Voices, keys};
use crate::studio::Studio;
use crate::ui::{self, tok};

const VENDORS: &[(&str, &str)] =
    &[("cartesia", "Cartesia"), ("elevenlabs", "ElevenLabs"), ("gemini", "Gemini"), ("none", "None")];
const CHATTINESS: &[&str] = &["Quiet", "Normal", "Lively", "Chatty"];
/// Lines of the conversation the Talk section shows.
const TALK_LINES: usize = 6;

/// What the co-host's sections ask the window for.
pub enum CohostEvent {
    /// Settings → Connections (to add a key or sign in to a subscription).
    OpenConnections,
}

pub struct CohostView {
    host: Host,
    studio: Entity<Studio>,
    /// The co-host object's source the Inspector shows, and whether it's the active co-host.
    source: Option<Id>,
    active: bool,
    prompt: Entity<InputState>,
    name: Entity<InputState>,
    personality: Entity<TextareaState>,
    /// The voice picker (a sheet over the window), and the preview last drawn.
    sheet: Entity<VoiceSheet>,
    shown_preview: (Option<String>, Option<String>),
    collapsed: HashSet<&'static str>,
    _subs: Vec<Subscription>,
}

impl EventEmitter<CohostEvent> for CohostView {}

impl CohostView {
    pub fn new(host: Host, studio: Entity<Studio>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let prompt = cx.new(|cx| InputState::new(window, cx).placeholder("Talk to your co-host…"));
        let name = cx.new(|cx| InputState::new(window, cx).placeholder("Pi"));
        let personality = cx.new(|cx| {
            TextareaState::new(window, cx).auto_grow(3, 8).placeholder("How it talks: warm, quick and playful…")
        });
        let sheet = {
            let (host, me) = (host.clone(), cx.weak_entity());
            cx.new(|cx| VoiceSheet::new(host, me, window, cx))
        };
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
        subs.push(cx.subscribe_in(&name, window, |this, state, ev: &InputEvent, _, cx| {
            if matches!(ev, InputEvent::PressEnter { .. } | InputEvent::Blur) {
                let v = state.read(cx).value().trim().to_string();
                if !v.is_empty() && this.setting("/persona/name", cx).as_str() != Some(v.as_str()) {
                    this.set(json!({"persona": {"name": v}}), cx);
                }
            }
        }));
        subs.push(cx.subscribe_in(&personality, window, |this, state, ev: &InputEvent, _, cx| {
            if matches!(ev, InputEvent::Blur) {
                let v = state.read(cx).value().trim().to_string();
                if !v.is_empty() && this.setting("/persona/personality", cx).as_str() != Some(v.as_str()) {
                    this.set(json!({"persona": {"personality": v}}), cx);
                }
            }
        }));
        // what PiAir says shows as it happens; the persona fields show what's set
        cx.spawn_in(window, async move |this, cx| {
            loop {
                cx.background_executor().timer(Duration::from_millis(120)).await;
                let ok = this.update_in(cx, |this, window, cx| {
                    let want = this.setting("/persona/name", cx).as_str().unwrap_or("").to_string();
                    let input = this.name.clone();
                    if !want.is_empty()
                        && !input.read(cx).focus_handle(cx).is_focused(window)
                        && input.read(cx).value() != want.as_str()
                    {
                        input.update(cx, |s, cx| s.set_value(want, window, cx));
                    }
                    let want = this.setting("/persona/personality", cx).as_str().unwrap_or("").to_string();
                    let area = this.personality.clone();
                    if !want.is_empty()
                        && !area.read(cx).focus_handle(cx).is_focused(window)
                        && area.read(cx).value() != want.as_str()
                    {
                        area.update(cx, |s, cx| s.set_value(want, window, cx));
                    }
                    // a preview started or ended: its button shows it
                    let preview = this.host.preview_state();
                    if preview != this.shown_preview {
                        this.shown_preview = preview;
                        cx.notify();
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
            name,
            personality,
            sheet,
            shown_preview: (None, None),
            collapsed: HashSet::new(),
            _subs: subs,
        }
    }

    /// Opens the voice picker (a development aid for captures).
    pub fn open_voices(&mut self, cx: &mut Context<Self>) {
        let vendor = self.setting("/voice/vendor", cx).as_str().unwrap_or("gemini").to_string();
        self.sheet.update(cx, |s, cx| s.open(&vendor, cx));
    }

    /// Shows this co-host object's settings (`active`: it's the one PiAir runs for).
    pub fn set_source(&mut self, source: Option<Id>, active: bool) {
        self.source = source;
        self.active = active;
    }

    /// The object's `cohost` block, or Null.
    fn cohost(&self, cx: &App) -> Value {
        let Some(id) = self.source else { return Value::Null };
        self.studio.read(cx).workspace().sources.get(&id).map(|s| s.settings[COHOST].clone()).unwrap_or(Value::Null)
    }

    /// A PiAir setting: the object's, else what PiAir is using (its defaults).
    pub(super) fn setting(&self, pointer: &str, cx: &App) -> Value {
        let own = self.cohost(cx)["piair"].pointer(pointer).cloned().filter(|v| !v.is_null());
        own.or_else(|| self.host.state().settings.pointer(pointer).cloned()).unwrap_or(Value::Null)
    }

    /// Changes the object's `cohost` block (one undo step).
    fn set_cohost(&mut self, patch: Value, cx: &mut Context<Self>) {
        let Some(id) = self.source else { return };
        let Some(source) = self.studio.read(cx).workspace().sources.get(&id).cloned() else { return };
        let mut settings = source.settings.clone();
        merge(&mut settings[COHOST], &patch);
        let _ = self.studio.update(cx, |s, cx| s.execute(Command::SetSourceSettings { source: id, settings }, cx));
        cx.notify();
    }

    /// Changes the object's PiAir settings; Lightair passes them on to PiAir.
    pub(super) fn set(&mut self, patch: Value, cx: &mut Context<Self>) {
        self.set_cohost(json!({"piair": patch}), cx);
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
            .gap_2p5()
            .pb_3()
            .border_b_1()
            .border_color(tok::border())
            .child(self.header(title, cx))
            .when(open, |d| d.children(body))
            .into_any_element()
    }

    /// A key's state in a line, with a way to Settings → Connections when it's needed.
    fn key_line(&self, service: &'static str, cx: &mut Context<Self>) -> AnyElement {
        let state = self.host.key_state(service);
        let name = keys::name_of(service);
        let (text, colour, needs) = match state {
            KeyState::Saved | KeyState::Works(None) => (format!("{name} key saved"), tok::ok(), false),
            KeyState::Works(Some(note)) => (note, tok::warn(), false),
            KeyState::Checking => ("Checking the key…".into(), tok::warn(), false),
            KeyState::Problem(e) => (e, tok::error(), true),
            KeyState::Missing => (format!("No {name} key"), tok::warn(), true),
            KeyState::Unknown => ("…".into(), tok::text_tertiary(), false),
        };
        div()
            .flex()
            .items_center()
            .gap_2()
            .child(div().flex_none().size(px(7.)).rounded_full().bg(colour))
            .child(div().flex_1().min_w_0().text_size(px(12.)).text_color(tok::text_secondary()).child(text))
            .child(
                Button::new(SharedString::from(format!("cohost-keys-{service}")))
                    .label(if needs { "Add key" } else { "Keys" })
                    .xsmall()
                    .when(needs, |b| b.outline())
                    .when(!needs, |b| b.ghost())
                    .on_click(cx.listener(|_, _, _, cx| cx.emit(CohostEvent::OpenConnections))),
            )
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
        let mut body = vec![
            row(
                "Provider",
                Some(
                    "Any provider Pi supports, with an API key or a subscription (Claude Pro/Max, ChatGPT, GitHub Copilot) where it has one: both are set up in Settings → Connections. ✓ marks the ones ready to use.",
                ),
                provider_menu.into_any_element(),
            ),
            row(
                "Model",
                Some("A fast model answers sooner. Using the computer needs a model that can see images."),
                model_menu.into_any_element(),
            ),
        ];
        // signed in, or where to: subscriptions and keys are in Settings → Connections. A
        // subscription signed in for another provider is offered as the one to use.
        if signed_in {
            body.push(status_line(tok::ok(), "Signed in".into()));
        } else {
            let other = providers
                .iter()
                .filter(|x| x["id"] != provider.as_str() && x["signedIn"].as_bool() == Some(true))
                .filter(|x| x["auth"].as_array().is_some_and(|a| a.iter().any(|k| k == "oauth")))
                .max_by_key(|x| related(&provider, x["id"].as_str().unwrap_or("")))
                .cloned();
            let name = p["name"].as_str().unwrap_or(&provider).to_string();
            let text = match &other {
                Some(o) => format!(
                    "{name} isn't signed in. You're signed in to {}: use it instead?",
                    o["name"].as_str().unwrap_or("a subscription")
                ),
                None if oauth => format!("{name} isn't signed in: sign in or add a key in Connections."),
                None => format!("No {name} key yet: add one in Connections."),
            };
            let line = div()
                .flex()
                .items_start()
                .gap_2()
                .child(div().flex_none().mt(px(5.)).size(px(7.)).rounded_full().bg(tok::warn()))
                .child(div().flex_1().min_w_0().text_size(px(12.)).text_color(tok::text_secondary()).child(text));
            let mut buttons = div().flex().flex_wrap().gap_1p5().pl(px(15.));
            if let Some(o) = other {
                let id = o["id"].as_str().unwrap_or("").to_string();
                let first = o["models"]
                    .as_array()
                    .and_then(|m| m.first())
                    .and_then(|m| m["id"].as_str())
                    .unwrap_or("")
                    .to_string();
                let label = format!("Use {}", o["name"].as_str().unwrap_or(&id));
                buttons = buttons.child(Button::new("cohost-brain-use").label(label).xsmall().primary().on_click(
                    cx.listener(move |this, _, _, cx| this.set(json!({"provider": id, "model": first}), cx)),
                ));
            }
            buttons = buttons.child(
                Button::new("cohost-brain-connections")
                    .label("Open Connections")
                    .xsmall()
                    .outline()
                    .on_click(cx.listener(|_, _, _, cx| cx.emit(CohostEvent::OpenConnections))),
            );
            body.push(div().flex().flex_col().gap_1p5().child(line).child(buttons).into_any_element());
        }
        self.section("Brain", body, cx)
    }

    // ---------------------------------------------------------------- Voice

    fn voice(&self, cx: &mut Context<Self>) -> AnyElement {
        let vendor = self.setting("/voice/vendor", cx).as_str().unwrap_or("gemini").to_string();
        let mut chosen = self.setting("/voice/voice", cx).as_str().unwrap_or("").to_string();
        // Gemini speaks as Puck until another is chosen
        if chosen.is_empty() && vendor == "gemini" {
            chosen = "Puck".into();
        }
        let chips = chips(
            "cohost-vendor",
            VENDORS.iter().map(|(id, name)| (name.to_string(), vendor == *id)).collect(),
            cx,
            |this, i, cx| this.set(json!({"voice": {"vendor": VENDORS[i].0, "voice": ""}}), cx),
        );
        let mut body = vec![stacked(
            "Provider",
            Some(
                "Cartesia and ElevenLabs answer in about a tenth of a second and give timings for the mouth. Gemini works with a Google key. None: it writes in Talk instead of speaking. It plays on the co-host's own mixer track.",
            ),
            chips,
        )];
        if vendor == "none" {
            return self.section("Voice", body, cx);
        }
        let service = keys::service_for_voice(&vendor);
        body.push(stacked(
            "Voice",
            Some("▶ plays a short sample on this computer, never on stream."),
            self.voice_picker(&vendor, &chosen, cx),
        ));
        body.push(self.key_line(service, cx));
        self.section("Voice", body, cx)
    }

    /// The voice picker, a sheet over the window (for the window to draw).
    pub fn sheet(&self) -> Entity<VoiceSheet> {
        self.sheet.clone()
    }

    /// The chosen voice: ▶ to hear it, Change to pick another in the voice sheet.
    fn voice_picker(&self, vendor: &str, chosen: &str, cx: &mut Context<Self>) -> AnyElement {
        let voices = self.host.voices(vendor);
        let (loading, playing) = self.host.preview_state();
        let preview_error = self.host.0.ai.lock().unwrap_or_else(|e| e.into_inner()).preview_error.clone();
        let current = match &voices {
            Voices::Ready(l) => l.iter().find(|v| v.id == chosen).cloned(),
            _ => None,
        };
        let play = current.clone().map(|v| {
            let (is_loading, is_playing) =
                (loading.as_deref() == Some(v.id.as_str()), playing.as_deref() == Some(v.id.as_str()));
            let vendor = vendor.to_string();
            Button::new("cohost-voice-play")
                .icon(ui::icon(if is_playing {
                    "square"
                } else if is_loading {
                    "loader"
                } else {
                    "play"
                }))
                .xsmall()
                .ghost()
                .tooltip(if is_playing { "Stop" } else { "Hear it (on this computer only)" })
                .on_click(cx.listener(move |this, _, _, cx| {
                    if is_playing {
                        this.host.stop_preview();
                    } else {
                        this.host.preview_voice(&vendor, v.clone());
                    }
                    cx.notify();
                }))
        });
        let tags = current.as_ref().filter(|v| !v.tags.is_empty()).map(|v| {
            div().flex().flex_wrap().gap_1().children(v.tags.iter().take(4).map(|t| {
                div()
                    .px_1p5()
                    .rounded(px(4.))
                    .border_1()
                    .border_color(tok::border_strong())
                    .text_size(px(10.))
                    .text_color(tok::text_secondary())
                    .child(t.clone())
            }))
        });
        let card = div()
            .flex()
            .items_center()
            .gap_2()
            .p_2()
            .rounded(px(8.))
            .bg(tok::bg_raised())
            .children(play)
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .flex_1()
                    .min_w_0()
                    .child(div().text_size(px(13.)).font_weight(FontWeight::MEDIUM).truncate().child(
                        match (&voices, &current) {
                            (_, Some(v)) => v.name.clone(),
                            (Voices::Loading, _) => "Loading voices…".into(),
                            (Voices::Failed(_), _) => "Voices unavailable".into(),
                            _ if chosen.is_empty() => "No voice chosen".into(),
                            _ => chosen.to_string(),
                        },
                    ))
                    .children(tags),
            )
            .child(
                Button::new("cohost-voice-change")
                    .label(if current.is_some() { "Change" } else { "Choose" })
                    .xsmall()
                    .outline()
                    .on_click({
                        let vendor = vendor.to_string();
                        cx.listener(move |this, _, _, cx| {
                            this.sheet.update(cx, |s, cx| s.open(&vendor, cx));
                        })
                    }),
            );
        let problem = match (&voices, preview_error) {
            (Voices::Failed(e), _) => Some(e.clone()),
            (_, Some(e)) if !self.sheet.read(cx).is_open() => Some(e),
            _ => None,
        };
        div()
            .flex()
            .flex_col()
            .gap_1p5()
            .child(card)
            .children(problem.map(|e| div().text_size(px(11.)).text_color(tok::error()).child(e)))
            .into_any_element()
    }

    // ---------------------------------------------------------------- Persona, Behaviour

    fn persona(&self, cx: &mut Context<Self>) -> AnyElement {
        let ff = self.setting("/persona/familyFriendly", cx).as_bool().unwrap_or(true);
        self.section(
            "Persona",
            vec![
                stacked("Name", Some("It answers when chat or you say its name."), TextInput::new(&self.name).small().into_any_element()),
                stacked(
                    "Personality",
                    Some("How it talks and what it's like, in your words: its humour, what it knows about you and your stream, what to avoid."),
                    Textarea::new(&self.personality).into_any_element(),
                ),
                row(
                    "Family friendly",
                    Some("No swearing or adult topics; anything that slips through is cut before it's said."),
                    switch("cohost-ff", ff).on_click(cx.listener(move |this, _, _, cx| this.set(json!({"persona": {"familyFriendly": !ff}}), cx))).into_any_element(),
                ),
            ],
            cx,
        )
    }

    fn behaviour(&self, cx: &mut Context<Self>) -> AnyElement {
        let chattiness = self.setting("/behaviour/chattiness", cx).as_u64().unwrap_or(1) as usize;
        let flag = |pointer: &str, default: bool| self.setting(pointer, cx).as_bool().unwrap_or(default);
        let (answer, yield_) = (flag("/behaviour/answerChat", true), flag("/behaviour/stopWhenStreamerTalks", true));
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
                stacked("Chattiness", Some("Quiet: only when spoken to. Normal: also picks a chat message, or something playing, now and then. Lively and Chatty: more often."), chatty),
                row("Answer chat", Some("Reads chat from every platform and answers some of it. Chat can never make it change Lightair or use the computer."), toggle("cohost-chat", "answerChat", answer, cx)),
                stacked(
                    "Hears",
                    Some("Mixer tracks it listens to, with on-device speech recognition (nothing leaves this computer for it). Conversation: people talking to it (your mic, a guest's): it waits while they speak and answers its name. Content: something playing (a video, a game): it follows along, may react now and then, and never takes it as someone talking to it."),
                    self.hears_list(cx),
                ),
                row("Stop when someone talks", Some("It goes quiet when a conversation track picks someone up."), toggle("cohost-yield", "stopWhenStreamerTalks", yield_, cx)),
            ],
            cx,
        )
    }

    /// The tracks it hears, each as conversation or content, and a way to add one.
    fn hears_list(&self, cx: &mut Context<Self>) -> AnyElement {
        let w = self.studio.read(cx).workspace().clone();
        let cohost = self.cohost(cx);
        let hears = super::hearing(&w, &cohost);
        // as it'll be saved: [{track, role}]
        let saved: Vec<Value> = hears
            .iter()
            .map(|h| json!({"track": h.track.to_string(), "role": if h.role == super::Role::Content { "content" } else { "conversation" }}))
            .collect();
        let status = self.host.0.hear_status.lock().unwrap_or_else(|e| e.into_inner()).clone();
        let running = self.active && self.host.running();
        let mut list = div().flex().flex_col().gap_1();
        for (i, h) in hears.iter().enumerate() {
            let content = h.role == super::Role::Content;
            // how listening to it is going (while the co-host is on)
            let line = status.get(&h.track).filter(|_| running).map(|st| {
                let colour = match st.level {
                    0 => tok::ok(),
                    1 => tok::warn(),
                    _ => tok::error(),
                };
                div()
                    .flex()
                    .items_start()
                    .gap_1p5()
                    .child(div().flex_none().mt(px(4.)).size(px(6.)).rounded_full().bg(colour))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .text_size(px(11.))
                            .text_color(if st.level == 2 { tok::error() } else { tok::text_secondary() })
                            .child(st.text.clone()),
                    )
            });
            let set_role = {
                let saved = saved.clone();
                move |this: &mut CohostView, role: &str, cx: &mut Context<CohostView>| {
                    let mut s = saved.clone();
                    s[i]["role"] = json!(role);
                    this.set_cohost(json!({"hears": s}), cx);
                }
            };
            let (to_conv, to_content) = (set_role.clone(), set_role);
            let remove = {
                let mut s = saved.clone();
                s.remove(i);
                s
            };
            let seg =
                |id: &'static str, label: &'static str, on: bool| seg((id, i), label, on).flex_1().justify_center();
            list = list.child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1p5()
                    .p_2()
                    .rounded(px(7.))
                    .bg(tok::bg_raised())
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(
                                ui::icon(if content { "monitor-play" } else { "mic" })
                                    .size(px(13.))
                                    .text_color(tok::text_secondary()),
                            )
                            .child(div().flex_1().min_w_0().truncate().text_size(px(12.)).child(h.name.clone()))
                            .child(
                                div()
                                    .id(("cohost-hears-remove", i))
                                    .cursor_pointer()
                                    .p_0p5()
                                    .child(ui::icon("x").size(px(12.)).text_color(tok::text_tertiary()))
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.set_cohost(json!({"hears": remove.clone()}), cx)
                                    })),
                            ),
                    )
                    .child(
                        div()
                            .flex()
                            .gap_0p5()
                            .p_0p5()
                            .rounded(px(6.))
                            .bg(tok::bg_panel())
                            .child(seg("cohost-hears-conv", "Conversation", !content).when(content, |d| {
                                d.on_click(cx.listener(move |this, _, _, cx| to_conv(this, "conversation", cx)))
                            }))
                            .child(seg("cohost-hears-content", "Content", content).when(!content, |d| {
                                d.on_click(cx.listener(move |this, _, _, cx| to_content(this, "content", cx)))
                            })),
                    )
                    .children(line),
            );
        }
        // every mixer track but the co-hosts' own voices and those already heard
        let candidates: Vec<(String, String, bool)> = w
            .tracks
            .iter()
            .filter(|t| !hears.iter().any(|h| h.track == t.id))
            .filter_map(|t| {
                let s = w.sources.get(&t.source)?;
                if s.kind == lightair_engine::piair_voice::KIND {
                    return None;
                }
                let talk = s.kind == lightair_model::kinds::MIC || s.kind == lightair_model::kinds::AUDIO_DEVICE;
                Some((t.id.to_string(), t.name.clone(), talk))
            })
            .collect();
        let me = cx.entity().downgrade();
        let add =
            Button::new("cohost-hears-add").label("Add a track").xsmall().outline().dropdown_caret(true).dropdown_menu(
                move |mut menu, _, _| {
                    for (id, name, talk) in &candidates {
                        let (me, id, saved) = (me.clone(), id.clone(), saved.clone());
                        // a mic starts as conversation, anything else as content
                        let role = if *talk { "conversation" } else { "content" };
                        menu = menu.item(PopupMenuItem::new(name.clone()).on_click(move |_, _, cx| {
                            if let Some(me) = me.upgrade() {
                                let mut s = saved.clone();
                                s.push(json!({"track": id, "role": role}));
                                me.update(cx, |this, cx| this.set_cohost(json!({"hears": s}), cx));
                            }
                        }));
                    }
                    menu
                },
            );
        if hears.is_empty() {
            list = list
                .child(div().text_size(px(12.)).text_color(tok::text_tertiary()).child("Nothing: it only reads chat."));
        }
        list.child(div().flex().child(add)).into_any_element()
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

/// How close another provider is to the chosen one, for offering its sign-in instead: the same
/// company's first (`github-copilot` for `github-…`).
fn related(chosen: &str, other: &str) -> u8 {
    let company = |id: &str| id.split('-').next().unwrap_or(id).to_string();
    u8::from(company(chosen) == company(other))
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

/// One choice of a small segmented control.
fn seg(id: impl Into<ElementId>, label: &'static str, on: bool) -> Stateful<Div> {
    div()
        .id(id)
        .px_2()
        .h(px(20.))
        .flex()
        .items_center()
        .rounded(px(5.))
        .text_size(px(11.))
        .when(on, |d| d.bg(tok::bg_selected()).text_color(tok::text()))
        .when(!on, |d| d.cursor_pointer().text_color(tok::text_secondary()).hover(|d| d.bg(tok::bg_raised())))
        .child(label)
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

/// A setting whose control takes the whole width, under its name (text, choices).
fn stacked(name: &str, detail: Option<&str>, control: AnyElement) -> AnyElement {
    div().flex().flex_col().gap_1p5().child(label(name, detail)).child(control).into_any_element()
}

fn status_line(colour: Hsla, text: String) -> AnyElement {
    div()
        .flex()
        .items_center()
        .gap_2()
        .child(div().flex_none().size(px(7.)).rounded_full().bg(colour))
        .child(div().text_size(px(12.)).text_color(tok::text_secondary()).child(text))
        .into_any_element()
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
