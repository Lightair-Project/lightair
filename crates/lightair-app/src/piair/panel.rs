//! The PiAir panel: PiAir on or off, what it's doing, what was said, a box to talk to it, and its
//! settings (brain, voice, persona, behaviour, the co-host's avatar, the computer).
//! Labels and values only; what each does is behind an ⓘ.

use std::sync::atomic::Ordering;
use std::time::Duration;

use gpui_kit::component::Sizable;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input as TextInput, InputEvent, InputState};
use gpui_kit::component::menu::{DropdownMenu, PopupMenuItem};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;
use serde_json::{Value, json};

use super::Host;
use crate::studio::Studio;
use crate::ui::{self, tok};

/// Lightair's own PiAir settings (the co-host's avatar, the allowed apps), beside PiAir's.
#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct Prefs {
    pub enabled: bool,
    pub avatar: Option<String>,
    pub allowed: Vec<String>,
}

impl Prefs {
    fn path() -> std::path::PathBuf {
        lightair_platform::paths::data_dir().join("PiAir").join("lightair.json")
    }
    pub fn load() -> Prefs {
        std::fs::read_to_string(Self::path()).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default()
    }
    fn save(&self) {
        let p = Self::path();
        if let Some(d) = p.parent() {
            let _ = std::fs::create_dir_all(d);
        }
        let _ = std::fs::write(p, serde_json::to_string_pretty(self).unwrap_or_default());
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Tab {
    Talk,
    Settings,
}

pub struct PiAirPanel {
    host: Host,
    studio: Entity<Studio>,
    focus: FocusHandle,
    prefs: Prefs,
    tab: Tab,
    prompt: Entity<InputState>,
    key: Entity<InputState>,
    voice_key: Entity<InputState>,
    name: Entity<InputState>,
    personality: Entity<InputState>,
    code: Entity<InputState>,
    scroll: ScrollHandle,
    shown_lines: usize,
    apps: Vec<(String, String)>,
    dev_prompts: String,
    _subs: Vec<Subscription>,
}

const VENDORS: &[(&str, &str)] =
    &[("cartesia", "Cartesia"), ("elevenlabs", "ElevenLabs"), ("gemini", "Gemini"), ("none", "None")];
const CHATTINESS: &[&str] = &["Quiet", "Normal", "Lively", "Chatty"];

impl PiAirPanel {
    pub fn new(host: Host, studio: Entity<Studio>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let prefs = Prefs::load();
        host.0.computer.allowed.lock().unwrap_or_else(|e| e.into_inner()).clone_from(&prefs.allowed);
        let prompt = cx.new(|cx| InputState::new(window, cx).placeholder("Talk to PiAir…"));
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
                    let provider = this.setting("/provider").as_str().unwrap_or("google").to_string();
                    this.host.send(json!({"t": "login", "provider": provider, "kind": "api_key", "key": k}));
                    state.update(cx, |s, cx| s.set_value("", window, cx));
                }
            }
        }));
        subs.push(cx.subscribe_in(&voice_key, window, |this, state, ev: &InputEvent, window, cx| {
            if matches!(ev, InputEvent::PressEnter { .. } | InputEvent::Blur) {
                let k = state.read(cx).value().trim().to_string();
                if !k.is_empty() {
                    let vendor = this.setting("/voice/vendor").as_str().unwrap_or("gemini").to_string();
                    if let Ok(e) = keyring::Entry::new(super::KEYCHAIN_SERVICE, &format!("voice:{vendor}")) {
                        let _ = e.set_password(&k);
                    }
                    this.host.send(json!({"t": "voice_key"}));
                    state.update(cx, |s, cx| s.set_value("", window, cx));
                }
            }
        }));
        for (input, pointer) in [(name.clone(), "name"), (personality.clone(), "personality")] {
            subs.push(cx.subscribe_in(&input, window, move |this, state, ev: &InputEvent, _, cx| {
                if matches!(ev, InputEvent::PressEnter { .. } | InputEvent::Blur) {
                    let v = state.read(cx).value().trim().to_string();
                    if !v.is_empty() && this.setting(&format!("/persona/{pointer}")).as_str() != Some(v.as_str()) {
                        this.host.set_settings(json!({"persona": {pointer: v}}));
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
        // what PiAir says and does shows as it happens; the mics and the avatar follow the workspace
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(Duration::from_millis(100)).await;
                let ok = this.update(cx, |this, cx| {
                    let w = this.studio.read(cx).workspace().clone();
                    let mics: Vec<_> = w.tracks.iter().filter(|t| w.sources.get(&t.source).is_some_and(|s| s.kind == lightair_model::kinds::MIC)).map(|t| t.id).collect();
                    *this.host.0.mics.lock().unwrap_or_else(|e| e.into_inner()) = mics;
                    // the co-host's avatar, by id (or by name, as a development aid may write it)
                    let want = this.prefs.avatar.as_ref().and_then(|a| {
                        w.sources.values().find(|s| s.kind == lightair_engine::avatar::KIND && (s.id.to_string() == *a || s.name == *a)).map(|s| s.id.to_string())
                    });
                    // kept by Id from then on (renaming the object doesn't lose it)
                    if want.is_some() && want != this.prefs.avatar {
                        this.prefs.avatar = want.clone();
                        this.prefs.save();
                    }
                    // the AI face only while PiAir runs: off, the avatar is its camera's again
                    let want = want.filter(|_| this.host.running());
                    if lightair_engine::face_drive::target() != want {
                        lightair_engine::face_drive::set_target(want);
                    }
                    // development: prompts from a file (LIGHTAIR_PIAIR_PROMPT_FILE), one per new last line
                    if let Some(f) = std::env::var_os("LIGHTAIR_PIAIR_PROMPT_FILE") {
                        let text = std::fs::read_to_string(&f).unwrap_or_default();
                        if text != this.dev_prompts {
                            this.dev_prompts = text.clone();
                            if let Some(line) = text.lines().rev().find(|l| !l.trim().is_empty()) {
                                match line.trim() {
                                    "!on" => this.set_enabled(true, cx),
                                    "!off" => this.set_enabled(false, cx),
                                    "!drives" => this.set_drives(true, cx),
                                    "!stop" => this.host.kill_switch(),
                                    l if l.starts_with("!avatar ") => this.set_avatar(Some(l[8..].trim().to_string()), cx),
                                    l if l.starts_with("!allow ") => {
                                        let mut a = this.prefs.allowed.clone();
                                        a.push(l[7..].trim().to_string());
                                        this.set_allowed(a, cx);
                                    }
                                    l if l.starts_with("!settings ") => {
                                        if let Ok(v) = serde_json::from_str(&l[10..]) {
                                            this.host.set_settings(v);
                                        }
                                    }
                                    l if l.starts_with("!chat ") => {
                                        let hub = this.studio.read(cx).chat.clone();
                                        let (who, msg) = l[6..].split_once(": ").unwrap_or(("viewer", &l[6..]));
                                        let mut ev = lightair_live::ChatEvent::new("twitch", "test", lightair_live::Kind::Message);
                                        ev.user = Some(lightair_live::User { id: who.to_lowercase(), login: who.to_lowercase(), display_name: who.to_string(), ..Default::default() });
                                        ev.text = msg.to_string();
                                        ev.test = true;
                                        hub.inject(lightair_live::Channel { platform: "twitch".into(), channel: "test".into() }, ev);
                                    }
                                    l if l.starts_with("!call ") => {
                                        let (method, params) = l[6..].split_once(' ').unwrap_or((&l[6..], "{}"));
                                        let (host, method) = (this.host.clone(), method.to_string());
                                        let params: Value = serde_json::from_str(params).unwrap_or(json!({}));
                                        std::thread::spawn(move || {
                                            let r = host.answer(&method, params);
                                            tracing::info!(target: "piair", "CALL {method} → {}", match r { Ok(v) => v.to_string(), Err(e) => format!("error {} {}", e.0, e.1) });
                                        });
                                    }
                                    "!talk" => this.tab = Tab::Talk,
                                    "!settings-tab" => this.tab = Tab::Settings,
                                    l => this.host.prompt(l),
                                }
                                cx.notify();
                            }
                        }
                    }
                    // the screenshot glyph goes away on its own
                    let shot = this.host.state().last_shot.is_some_and(|t| t.elapsed() < Duration::from_secs(5));
                    if this.host.0.dirty.swap(false, Ordering::Relaxed) || shot {
                        let n = this.host.state().log.len();
                        if n != this.shown_lines {
                            this.shown_lines = n;
                            this.scroll.scroll_to_bottom();
                        }
                        cx.notify();
                    }
                });
                if ok.is_err() {
                    break;
                }
            }
        })
        .detach();
        let this = PiAirPanel {
            host,
            studio,
            focus: cx.focus_handle(),
            prefs,
            tab: Tab::Talk,
            prompt,
            key,
            voice_key,
            name,
            personality,
            code,
            scroll: ScrollHandle::new(),
            shown_lines: 0,
            apps: Vec::new(),
            dev_prompts: String::new(),
            _subs: subs,
        };
        if this.prefs.enabled {
            this.host.start();
        }
        this
    }

    fn setting(&self, pointer: &str) -> Value {
        self.host.state().settings.pointer(pointer).cloned().unwrap_or(Value::Null)
    }

    fn set_enabled(&mut self, on: bool, cx: &mut Context<Self>) {
        self.prefs.enabled = on;
        self.prefs.save();
        if on {
            // its voice needs a track to play on
            let w = self.studio.read(cx).workspace().clone();
            if let Some(cmd) = super::voice_track_commands(&w) {
                self.studio.update(cx, |s, cx| {
                    let _ = s.execute(cmd, cx);
                });
            }
            self.host.start();
        } else {
            self.host.stop();
        }
        cx.notify();
    }

    fn set_drives(&mut self, on: bool, cx: &mut Context<Self>) {
        #[cfg(target_os = "macos")]
        if on {
            // the human must always be able to take over: no watching, no driving
            if !super::input_mac::watch() {
                lightair_platform::input::request_listen();
                self.host.state().detail =
                    Some("Allow Lightair in Input Monitoring so you can always take over.".into());
                cx.notify();
                return;
            }
            if !super::input_mac::can_post(true) {
                self.host.state().detail =
                    Some("Allow Lightair in Accessibility so PiAir can move the pointer.".into());
                cx.notify();
                return;
            }
        }
        self.host.0.computer.ai_drives.store(on, Ordering::Relaxed);
        self.host.send(json!({"t": "computer", "ai_drives": on, "paused": false}));
        cx.notify();
    }

    fn set_avatar(&mut self, id: Option<String>, cx: &mut Context<Self>) {
        self.prefs.avatar = id;
        self.prefs.save();
        cx.notify();
    }

    fn set_allowed(&mut self, allowed: Vec<String>, cx: &mut Context<Self>) {
        self.prefs.allowed = allowed.clone();
        self.prefs.save();
        *self.host.0.computer.allowed.lock().unwrap_or_else(|e| e.into_inner()) = allowed;
        cx.notify();
    }

    // ---------------------------------------------------------------- pieces

    fn header(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let s = self.host.state();
        let on = self.prefs.enabled;
        let (label, color) = match (s.running, s.status.as_str()) {
            (false, _) if s.error.is_some() => ("Stopped", tok::error()),
            (false, _) => ("Off", tok::text_tertiary()),
            (true, "starting") => ("Starting", tok::warn()),
            (true, "speaking") => ("Speaking", tok::ok()),
            (true, "thinking") => ("Thinking", tok::warn()),
            (true, "acting") => ("Using the computer", tok::live()),
            (true, "error") => ("Problem", tok::error()),
            _ => ("Listening", tok::ok()),
        };
        let paused = s.paused;
        // a picture of the screen just went to the model
        let shot = s.last_shot.is_some_and(|t| t.elapsed() < Duration::from_secs(4));
        let provider = s.providers.as_array().and_then(|l| {
            let id = s.settings.pointer("/provider")?.as_str()?;
            l.iter().find(|p| p["id"] == id)?["name"].as_str().map(str::to_owned)
        });
        drop(s);
        div()
            .flex()
            .items_center()
            .gap_2()
            .px_3()
            .py_2()
            .border_b_1()
            .border_color(tok::border())
            .child(div().size(px(8.)).rounded_full().bg(color))
            .child(div().text_size(px(13.)).font_weight(FontWeight::MEDIUM).child(if paused {
                "Paused: you're in control"
            } else {
                label
            }))
            .child(div().flex_1())
            .when(shot, |d| {
                let tip = format!(
                    "A picture of the screen went to {} just now: only the allowed apps' windows, the rest blacked out.",
                    provider.clone().unwrap_or_else(|| "the AI provider".into())
                );
                d.child(div().id("piair-shot").flex_none().child(ui::icon("camera").size(px(14.)).text_color(tok::warn())).tooltip(ui::tip(tip)))
            })
            .when(self.host.running(), |d| {
                d.child(
                    Button::new("piair-stop")
                        .label("Stop")
                        .xsmall()
                        .outline()
                        .tooltip("Stops speech and anything PiAir is doing (also the kill-switch shortcut)")
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.host.kill_switch();
                            cx.notify();
                        })),
                )
            })
            .child(switch("piair-on", on).on_click(cx.listener(move |this, _, _, cx| this.set_enabled(!on, cx))))
    }

    fn tabs(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let tab = |id: &'static str, label: &'static str, t: Tab, cx: &mut Context<Self>| {
            let on = self.tab == t;
            div()
                .id(id)
                .px_2()
                .h(px(24.))
                .flex()
                .items_center()
                .rounded(px(5.))
                .text_size(px(12.))
                .cursor_pointer()
                .when(on, |d| d.bg(tok::bg_selected()).text_color(tok::text()))
                .when(!on, |d| d.text_color(tok::text_secondary()).hover(|d| d.bg(tok::bg_raised())))
                .child(label)
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.tab = t;
                    if t == Tab::Settings {
                        #[cfg(target_os = "macos")]
                        {
                            this.apps = super::input_mac::running_apps();
                        }
                    }
                    cx.notify();
                }))
        };
        div().flex().gap_0p5().px_3().py_1p5().child(tab("piair-tab-talk", "Talk", Tab::Talk, cx)).child(tab(
            "piair-tab-settings",
            "Settings",
            Tab::Settings,
            cx,
        ))
    }

    /// A destructive action waiting for the streamer, a sign-in step, or a problem.
    fn notices(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let s = self.host.state();
        if let Some(q) = &s.question {
            let text = q.text.clone();
            return Some(
                card(tok::warn())
                    .child(div().text_size(px(13.)).child(text))
                    .child(
                        div()
                            .flex()
                            .gap_2()
                            .child(Button::new("piair-allow").label("Allow").small().primary().on_click(cx.listener(
                                |this, _, _, cx| {
                                    this.host.answer_question(true);
                                    cx.notify();
                                },
                            )))
                            .child(Button::new("piair-deny").label("Don't").small().outline().on_click(cx.listener(
                                |this, _, _, cx| {
                                    this.host.answer_question(false);
                                    cx.notify();
                                },
                            ))),
                    )
                    .into_any_element(),
            );
        }
        if let Some(a) = s.auth.clone().filter(|a| !a.done || a.error) {
            let color = if a.error { tok::error() } else { tok::accent_primary() };
            return Some(
                card(color)
                    .child(div().text_size(px(13.)).child(if a.provider.is_empty() {
                        a.message.clone()
                    } else {
                        format!("{}: {}", a.provider, a.message)
                    }))
                    .children(
                        a.code.clone().map(|c| div().text_size(px(18.)).font_weight(FontWeight::SEMIBOLD).child(c)),
                    )
                    .when(a.wants_code, |d| d.child(TextInput::new(&self.code).small()))
                    .child(
                        div()
                            .flex()
                            .gap_2()
                            .children(a.url.clone().map(|url| {
                                Button::new("piair-auth-open")
                                    .label("Open sign-in page")
                                    .small()
                                    .outline()
                                    .on_click(move |_, _, cx| cx.open_url(&url))
                            }))
                            .child(Button::new("piair-auth-close").label("Close").small().ghost().on_click(
                                cx.listener(|this, _, _, cx| {
                                    this.host.state().auth = None;
                                    cx.notify();
                                }),
                            )),
                    )
                    .into_any_element(),
            );
        }
        let problem = s.error.clone().or_else(|| (s.status == "error").then(|| s.detail.clone()).flatten());
        problem.map(|p| card(tok::error()).child(div().text_size(px(12.)).child(p)).into_any_element())
    }

    fn talk(&self, _cx: &mut Context<Self>) -> impl IntoElement {
        let s = self.host.state();
        let lines: Vec<_> = s.log.iter().cloned().collect();
        let running = s.running;
        drop(s);
        let body = if lines.is_empty() {
            div()
                .flex_1()
                .flex()
                .items_center()
                .justify_center()
                .p_6()
                .text_size(px(12.))
                .text_color(tok::text_tertiary())
                .text_center()
                .child(if running {
                    "What PiAir hears and says shows here."
                } else {
                    "Turn PiAir on to bring your co-host in."
                })
                .into_any_element()
        } else {
            div()
                .id("piair-log")
                .flex_1()
                .min_h_0()
                .overflow_y_scroll()
                .track_scroll(&self.scroll)
                .px_3()
                .py_2()
                .flex()
                .flex_col()
                .gap_1p5()
                .children(lines.into_iter().enumerate().map(|(i, l)| {
                    let (who, color) = match l.role.as_str() {
                        "piair" => {
                            (self.setting("/persona/name").as_str().unwrap_or("Pi").to_string(), tok::accent_primary())
                        }
                        "streamer" => ("You".to_string(), tok::text_secondary()),
                        "tool" => (String::new(), tok::text_tertiary()),
                        _ => (l.who.clone().unwrap_or_default(), tok::text_secondary()),
                    };
                    if l.role == "tool" {
                        return div()
                            .id(("piair-line", i))
                            .text_size(px(11.))
                            .text_color(color)
                            .child(format!("· {}", tool_label(&l.text)))
                            .into_any_element();
                    }
                    div()
                        .id(("piair-line", i))
                        .text_size(px(13.))
                        .child(div().text_size(px(11.)).font_weight(FontWeight::SEMIBOLD).text_color(color).child(who))
                        .child(div().text_color(tok::text()).child(l.text))
                        .into_any_element()
                }))
                .into_any_element()
        };
        div().flex().flex_col().flex_1().min_h_0().child(body).child(
            div()
                .p_2()
                .border_t_1()
                .border_color(tok::border())
                .child(TextInput::new(&self.prompt).small().disabled(!running)),
        )
    }

    fn settings(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let st = self.host.state();
        let ready = st.ready;
        let providers = st.providers.as_array().cloned().unwrap_or_default();
        drop(st);
        let provider = self.setting("/provider").as_str().unwrap_or("").to_string();
        let model = self.setting("/model").as_str().unwrap_or("").to_string();
        let p = providers.iter().find(|x| x["id"] == provider.as_str()).cloned().unwrap_or(Value::Null);
        let signed_in = p["signedIn"].as_bool().unwrap_or(false);
        let oauth = p["auth"].as_array().is_some_and(|a| a.iter().any(|x| x == "oauth"));
        let vendor = self.setting("/voice/vendor").as_str().unwrap_or("gemini").to_string();
        let chattiness = self.setting("/behaviour/chattiness").as_u64().unwrap_or(1) as usize;
        let flag = |pointer: &str| self.setting(pointer).as_bool().unwrap_or(false);
        let w = self.studio.read(cx).workspace().clone();
        let avatars: Vec<(String, String)> = w
            .sources
            .values()
            .filter(|s| s.kind == lightair_engine::avatar::KIND)
            .map(|s| (s.id.to_string(), s.name.clone()))
            .collect();
        let avatar_name = self
            .prefs
            .avatar
            .as_ref()
            .and_then(|id| avatars.iter().find(|a| &a.0 == id))
            .map_or("None".to_string(), |a| a.1.clone());
        let drives = self.host.0.computer.ai_drives.load(Ordering::Relaxed);
        let me = cx.entity().downgrade();

        // Brain
        let provider_menu = {
            let me = me.clone();
            let label =
                p["name"].as_str().unwrap_or(if provider.is_empty() { "Choose" } else { &provider }).to_string();
            let list = providers.clone();
            Button::new("piair-provider").label(label).xsmall().outline().dropdown_caret(true).dropdown_menu(
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
                                me.update(cx, |this, _| {
                                    this.host.set_settings(json!({"provider": id, "model": first}))
                                });
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
            Button::new("piair-model").label(label).xsmall().outline().dropdown_caret(true).dropdown_menu(
                move |mut menu, _, _| {
                    for m in list.iter().take(60) {
                        let id = m["id"].as_str().unwrap_or("").to_string();
                        let name = m["name"].as_str().unwrap_or(&id).to_string();
                        let me = me.clone();
                        menu = menu.item(PopupMenuItem::new(name).on_click(move |_, _, cx| {
                            if let Some(me) = me.upgrade() {
                                me.update(cx, |this, _| this.host.set_settings(json!({"model": id})));
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
                .child(Button::new("piair-oauth").label("Use subscription").xsmall().outline().on_click(cx.listener(
                    move |this, _, _, cx| {
                        this.host.send(json!({"t": "login", "provider": provider, "kind": "oauth"}));
                        cx.notify();
                    },
                )))
                .child(div().w(px(130.)).child(TextInput::new(&self.key).xsmall()))
                .into_any_element()
        } else {
            div().w(px(160.)).child(TextInput::new(&self.key).xsmall()).into_any_element()
        };

        // Voice
        let vendors = chips(
            "piair-vendor",
            VENDORS.iter().map(|(id, name)| (name.to_string(), vendor == *id)).collect(),
            cx,
            |this, i, _| this.host.set_settings(json!({"voice": {"vendor": VENDORS[i].0, "voice": ""}})),
        );
        let chatty = chips(
            "piair-chatty",
            CHATTINESS.iter().enumerate().map(|(i, n)| (n.to_string(), i == chattiness)).collect(),
            cx,
            |this, i, _| this.host.set_settings(json!({"behaviour": {"chattiness": i}})),
        );
        let toggle = |id: &'static str, pointer: &'static str, on: bool, cx: &mut Context<Self>| {
            switch(id, on).on_click(cx.listener(move |this, _, _, _| {
                let parts: Vec<&str> = pointer.trim_start_matches('/').split('/').collect();
                let mut v = json!(!on);
                for p in parts.iter().rev() {
                    v = json!({ *p: v });
                }
                this.host.set_settings(v);
            }))
        };

        // Avatar
        let avatar_menu = {
            let me = me.clone();
            let list = avatars.clone();
            Button::new("piair-avatar").label(avatar_name).xsmall().outline().dropdown_caret(true).dropdown_menu(
                move |mut menu, _, _| {
                    for (id, name) in std::iter::once((String::new(), "None".to_string())).chain(list.iter().cloned()) {
                        let me = me.clone();
                        menu = menu.item(PopupMenuItem::new(name).on_click(move |_, _, cx| {
                            if let Some(me) = me.upgrade() {
                                let id = (!id.is_empty()).then(|| id.clone());
                                me.update(cx, |this, cx| this.set_avatar(id, cx));
                            }
                        }));
                    }
                    menu
                },
            )
        };

        // Computer: allowed apps
        let allowed = self.prefs.allowed.clone();
        let app_name = |id: &str| self.apps.iter().find(|a| a.0 == id).map_or(id.to_string(), |a| a.1.clone());
        let add_app = {
            let me = me.clone();
            let list: Vec<(String, String)> =
                self.apps.iter().filter(|a| !allowed.contains(&a.0) && !a.0.contains("lightair")).cloned().collect();
            Button::new("piair-add-app").label("Add app").xsmall().outline().dropdown_caret(true).dropdown_menu(
                move |mut menu, _, _| {
                    for (id, name) in &list {
                        let me = me.clone();
                        let id = id.clone();
                        menu = menu.item(PopupMenuItem::new(name.clone()).on_click(move |_, _, cx| {
                            if let Some(me) = me.upgrade() {
                                me.update(cx, |this, cx| {
                                    let mut a = this.prefs.allowed.clone();
                                    a.push(id.clone());
                                    this.set_allowed(a, cx);
                                });
                            }
                        }));
                    }
                    menu
                },
            )
        };
        let app_rows = allowed
            .iter()
            .enumerate()
            .map(|(i, id)| {
                let id2 = id.clone();
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .pl_3()
                    .pr_1()
                    .h(px(28.))
                    .rounded(px(6.))
                    .bg(tok::bg_raised())
                    .child(div().flex_1().text_size(px(12.)).truncate().child(app_name(id)))
                    .child(
                        div()
                            .id(("piair-remove-app", i))
                            .cursor_pointer()
                            .p_1()
                            .child(ui::icon("x").size(px(12.)).text_color(tok::text_tertiary()))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                let a: Vec<String> =
                                    this.prefs.allowed.iter().filter(|x| **x != id2).cloned().collect();
                                this.set_allowed(a, cx);
                            })),
                    )
                    .into_any_element()
            })
            .collect::<Vec<_>>();

        div()
            .id("piair-settings")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .px_3()
            .py_2()
            .flex()
            .flex_col()
            .gap_3()
            .when(!ready, |d| d.child(div().text_size(px(12.)).text_color(tok::text_tertiary()).child("Turn PiAir on to change its brain, voice and persona.")))
            .when(ready, |d| {
                d.child(section("Brain").child(row("Provider", Some("Any provider Pi supports: an API key, or your subscription (Claude Pro/Max, ChatGPT, GitHub Copilot) where it has one. Keys stay in the Keychain."), provider_menu)).child(row("Model", Some("A fast model answers sooner. Computer use needs a model that can see images."), model_menu)).child(row("Sign in", None, sign_in)))
                    .child(
                        section("Voice")
                            .child(row("Voice", Some("Cartesia and ElevenLabs answer in about a tenth of a second and give timings for the mouth. Gemini works with a Gemini key. None: PiAir writes here instead of speaking. Plays on the PiAir mixer track."), vendors))
                            .when(vendor != "none", |d| d.child(row("Voice key", Some("Kept in the Keychain."), div().w(px(160.)).child(TextInput::new(&self.voice_key).xsmall())))),
                    )
                    .child(section("Persona").child(row("Name", Some("PiAir answers when chat or you say its name."), div().w(px(160.)).child(TextInput::new(&self.name).xsmall()))).child(row("Personality", Some("A few words on how it talks."), div().w(px(200.)).child(TextInput::new(&self.personality).xsmall()))).child(row("Family friendly", Some("No swearing or adult topics; anything that slips through is cut before it's said."), toggle("piair-ff", "/persona/familyFriendly", flag("/persona/familyFriendly"), cx))))
                    .child(
                        section("Behaviour")
                            .child(row("Chattiness", Some("Quiet: only when spoken to. Normal: also picks a chat message now and then. Lively and Chatty: more often."), chatty))
                            .child(row("Answer chat", Some("Reads chat from every platform and answers some of it. Chat can never make PiAir change Lightair or use the computer."), toggle("piair-chat", "/behaviour/answerChat", flag("/behaviour/answerChat"), cx)))
                            .child(row("Listen to me", Some("Hears you with on-device speech recognition and answers when you say its name."), toggle("piair-listen", "/behaviour/listen", flag("/behaviour/listen"), cx)))
                            .child(row("Stop when I talk", Some("PiAir goes quiet when your mic picks you up."), toggle("piair-yield", "/behaviour/stopWhenStreamerTalks", flag("/behaviour/stopWhenStreamerTalks"), cx))),
                    )
            })
            .child(section("Avatar").child(row("Co-host avatar", Some("The avatar PiAir speaks through: its mouth follows the voice, it blinks, looks around and shows a mood. With a camera on it too, the camera keeps the head and PiAir moves the mouth."), avatar_menu)))
            .child(
                section("Computer")
                    .child(row("AI drives", Some("Lets PiAir move the pointer, click and type in the allowed apps when you ask it to. Off again when Lightair restarts. Touching the mouse or keyboard pauses it at once; the kill-switch shortcut stops it."), switch("piair-drives", drives).on_click(cx.listener(move |this, _, _, cx| this.set_drives(!drives, cx)))))
                    .child(row("Allowed apps", Some("PiAir only acts in these apps, never in Lightair, never types into password fields, and asks before anything that quits, deletes, sends or buys."), add_app))
                    .children(app_rows),
            )
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
    div().m_2().p_3().rounded(px(8.)).border_1().border_color(color).bg(tok::bg_raised()).flex().flex_col().gap_2()
}

fn section(title: &str) -> Div {
    div().flex().flex_col().gap_1p5().child(
        div()
            .text_size(px(11.))
            .font_weight(FontWeight::SEMIBOLD)
            .text_color(tok::text_tertiary())
            .child(title.to_uppercase()),
    )
}

fn row(label: &str, detail: Option<&str>, control: impl IntoElement) -> Div {
    div()
        .flex()
        .items_center()
        .gap_3()
        .min_h(px(40.))
        .px_3()
        .py_1p5()
        .rounded(px(8.))
        .bg(tok::bg_raised())
        .child(
            div()
                .flex()
                .items_center()
                .gap_1()
                .flex_1()
                .min_w_0()
                .child(div().min_w_0().truncate().text_size(px(13.)).child(label.to_string()))
                .children(detail.map(|d| ui::info(SharedString::from(format!("piair-tip-{label}")), d.to_string()))),
        )
        .child(div().flex_none().child(control))
}

/// The house switch: accent green when on.
fn switch(id: impl Into<ElementId>, on: bool) -> Stateful<Div> {
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
    cx: &mut Context<PiAirPanel>,
    pick: impl Fn(&mut PiAirPanel, usize, &mut Context<PiAirPanel>) + Clone + 'static,
) -> AnyElement {
    let mut row = div().flex().flex_wrap().justify_end().gap_0p5().p_0p5().rounded(px(7.)).bg(tok::bg_panel());
    for (i, (label, on)) in options.into_iter().enumerate() {
        let pick = pick.clone();
        row = row.child(
            div()
                .id((id, i))
                .px_2()
                .h(px(22.))
                .flex()
                .items_center()
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

impl Render for PiAirPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // the persona fields show what's set (when not being edited)
        let notices = self.notices(cx);
        let body = match self.tab {
            Tab::Talk => self.talk(cx).into_any_element(),
            Tab::Settings => self.settings(cx).into_any_element(),
        };
        div()
            .flex()
            .flex_col()
            .size_full()
            .bg(tok::bg_panel())
            .child(self.header(cx))
            .child(self.tabs(cx))
            .children(notices)
            .child(body)
    }
}

impl gpui_kit::component::dock::BasePanel for PiAirPanel {
    fn panel_name(&self) -> &'static str {
        "piair"
    }
}

impl gpui_kit::component::dock::Panel for PiAirPanel {
    fn tab_name(&self, _: &App) -> Option<SharedString> {
        None
    }

    fn title(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        ui::panel_title("bot", "PiAir")
    }

    fn title_style(&self, _: &App) -> Option<gpui_kit::component::dock::TitleStyle> {
        Some(ui::panel_header_style())
    }

    fn zoom_control(&self, _: &App) -> Option<gpui_kit::component::dock::PanelControl> {
        None
    }

    fn inner_padding(&self, _: &App) -> bool {
        false
    }
}

impl EventEmitter<gpui_kit::component::dock::PanelEvent> for PiAirPanel {}

impl Focusable for PiAirPanel {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}
