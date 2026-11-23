//! The AI co-host object (piair/docs/design.md §1): an avatar object whose source carries a
//! `cohost` block. PiAir runs while the active co-host exists and is on; it drives that avatar's
//! face (`face_drive`), or a PNGtuber's mouth through its voice track; its voice plays on a mixer
//! track of its own, made and removed with it. One co-host is active at a time.

use std::time::{Duration, Instant};

use gpui_kit::component::Sizable;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;
use lightair_engine::avatar::KIND;
use lightair_engine::{face_drive, piair_voice};
use lightair_model::{Command, Id, Layout, Object, Source, Track};
use serde_json::{Value, json};

use super::StudioWindow;
use crate::piair::{COHOST, is_cohost};
use crate::ui::tok;

/// What the co-host check last did.
#[derive(Default)]
pub(crate) struct CohostSync {
    /// The co-host PiAir runs for.
    running_for: Option<Id>,
    /// The settings PiAir was last given.
    sent: Value,
    /// A question was brought to the streamer's eyes (the co-host selected).
    surfaced: bool,
    last: Option<Instant>,
    /// Development: the prompt file's last contents.
    dev_prompts: String,
}

/// The co-host's name for a new object and its voice track.
const NAME: &str = "AI co-host";

impl StudioWindow {
    /// Adds an AI co-host with this library avatar, its voice track with it (one undo step). It's
    /// the active one unless another co-host is.
    pub(super) fn add_cohost_object(&mut self, e: &crate::avatar_library::Entry, cx: &mut Context<Self>) {
        let studio = self.studio.read(cx);
        let workspace = studio.workspace().clone();
        let Some(scene) = studio.selected_scene else { return };
        let other_active =
            live_cohosts(&workspace).iter().any(|s| s.settings[COHOST]["active"].as_bool() != Some(false));
        let voice = Source::new(NAME, piair_voice::KIND, json!({}));
        let track = Track::new(NAME, voice.id);
        let png = crate::avatar_library::kind_of(&e.file) == "PNGtuber";
        let mut base = json!({
            "file": e.file,
            "device": "",
            "width": 1080,
            "height": 1080,
            "framing": if png { "full" } else { "bust" },
            "mirror": true,
            "library": e.id,
            COHOST: {"on": true, "active": !other_active, "voice_track": track.id.to_string(), "piair": {}},
        });
        // a PNGtuber's mouth follows a track's level: its own voice's
        if png {
            base["mic"] = json!(track.id.to_string());
        }
        let settings = crate::avatar_library::with_look(&base, &e.look);
        let (w, h) =
            (settings["width"].as_f64().unwrap_or(1080.0) as f32, settings["height"].as_f64().unwrap_or(1080.0) as f32);
        let layouts: Vec<(Id, Layout)> = workspace
            .canvases
            .iter()
            .map(|f| {
                let s = (f.width as f32 / w).min(f.height as f32 / h).min(1.0);
                let (lw, lh) = (w * s, h * s);
                (f.id, Layout::boxed((f.width as f32 - lw) * 0.5, (f.height as f32 - lh) * 0.5, lw, lh))
            })
            .collect();
        let source = Source::new(NAME, KIND, settings);
        let object = Object::new(NAME, source.id, layouts);
        let object_id = object.id;
        let commands = vec![
            Command::AddSource { source: voice },
            Command::AddTrack { track, index: None },
            Command::AddSource { source },
            Command::AddObject { scene, object, index: None },
        ];
        self.add_menu = false;
        self.exec(Command::Batch { label: format!("Add {NAME}"), commands }, cx);
        self.select_object(Some(object_id), cx);
    }

    /// Keeps PiAir in step with the co-host objects, a few times a second: started for the active
    /// one while it's on, stopped when it's off or gone, given its settings when they change; its
    /// face and voice routed to it. (Its voice track goes and comes with its object in the same undo
    /// step: `crate::piair::with_cohost_voice`.)
    pub(super) fn sync_cohost(&mut self, cx: &mut Context<Self>) {
        if self.cohost.last.is_some_and(|t| t.elapsed() < Duration::from_millis(200)) {
            return;
        }
        self.cohost.last = Some(Instant::now());
        self.dev_cohost_prompts(cx);
        let workspace = self.studio.read(cx).workspace().clone();
        let live = live_cohosts(&workspace);
        let active = active_cohost(&live);

        // a live co-host without its voice track (a project from elsewhere): a new one
        if let Some(s) = live.iter().find(|s| {
            let t = s.settings[COHOST]["voice_track"].as_str().unwrap_or("");
            !workspace.tracks.iter().any(|x| x.id.to_string() == t)
        }) {
            let voice = Source::new(NAME, piair_voice::KIND, json!({}));
            let track = Track::new(s.name.clone(), voice.id);
            let mut settings = s.settings.clone();
            settings[COHOST]["voice_track"] = json!(track.id.to_string());
            if crate::avatar_library::kind_of(settings["file"].as_str().unwrap_or("")) == "PNGtuber" {
                settings["mic"] = json!(track.id.to_string());
            }
            let commands = vec![
                Command::AddSource { source: voice },
                Command::AddTrack { track, index: None },
                Command::SetSourceSettings { source: s.id, settings },
            ];
            self.exec(Command::Batch { label: format!("Add {NAME}'s voice"), commands }, cx);
            return;
        }

        // PiAir for the active co-host while it's on
        let want = active.filter(|s| s.settings[COHOST]["on"].as_bool() != Some(false));
        let want_id = want.map(|s| s.id);
        if want_id != self.cohost.running_for {
            if self.cohost.running_for.is_some() {
                self.piair.stop();
            }
            if let Some(s) = want {
                let settings = s.settings[COHOST]["piair"].clone();
                self.piair.start(settings.clone());
                self.cohost.sent = settings;
            }
            self.cohost.running_for = want_id;
        } else if let Some(s) = want {
            let settings = &s.settings[COHOST]["piair"];
            if *settings != self.cohost.sent && settings.is_object() {
                self.piair.set_settings(settings.clone());
                self.cohost.sent = settings.clone();
            }
        }
        // its face and voice
        let running = self.piair.running();
        let face = want.filter(|_| running).map(|s| s.id.to_string());
        if face_drive::target() != face {
            face_drive::set_target(face);
        }
        let voice_source = want
            .and_then(|s| s.settings[COHOST]["voice_track"].as_str())
            .and_then(|t| workspace.tracks.iter().find(|x| x.id.to_string() == t))
            .map(|t| t.source.to_string());
        piair_voice::set_active(voice_source);
        // what it hears: people talking to it (they also tell it when someone's talking) and
        // content playing
        let hears = want.map(|s| crate::piair::hearing(&workspace, &s.settings[COHOST])).unwrap_or_default();
        *self.piair.0.mics.lock().unwrap_or_else(|e| e.into_inner()) =
            hears.iter().filter(|h| h.role == crate::piair::Role::Conversation).map(|h| h.track).collect();
        let mut current = self.piair.0.hears.lock().unwrap_or_else(|e| e.into_inner());
        if *current != hears {
            *current = hears;
        }
        drop(current);
        // the computer is waiting on the streamer's yes: show the co-host so they see it
        let asking = self.piair.state().question.is_some();
        if asking && !self.cohost.surfaced {
            if let Some(object) = want.and_then(|s| workspace.objects_using(s.id).next().map(|(_, o)| o.id)) {
                self.select_object(Some(object), cx);
            }
        }
        self.cohost.surfaced = asking;
    }

    /// The co-host's status for its Avatar card ("Listening", "Speaking", a problem and its fix),
    /// once its model is in; `None` for any other avatar.
    pub(super) fn cohost_line(&self, source: &Source, cx: &App) -> Option<(Hsla, String)> {
        if !is_cohost(source) {
            return None;
        }
        let workspace = self.studio.read(cx).workspace().clone();
        let live = live_cohosts(&workspace);
        if active_cohost(&live).map(|s| s.id) != Some(source.id) {
            return Some((tok::text_tertiary(), "Another co-host is active".into()));
        }
        if source.settings[COHOST]["on"].as_bool() == Some(false) {
            return Some((tok::text_tertiary(), "Off".into()));
        }
        let s = self.piair.state();
        if let Some(e) = &s.error {
            return Some((tok::error(), e.clone()));
        }
        if s.status == "error" {
            return Some((tok::error(), s.detail.clone().unwrap_or_else(|| "Something went wrong".into())));
        }
        if s.paused {
            return Some((tok::warn(), "Paused: you're using the mouse or keyboard".into()));
        }
        Some(match (s.running, s.status.as_str()) {
            (false, _) | (true, "starting") => (tok::warn(), "Starting…".into()),
            (true, "speaking") => (tok::ok(), "Speaking".into()),
            (true, "thinking") => (tok::warn(), "Thinking".into()),
            (true, "acting") => (tok::live(), "Using the computer".into()),
            _ => (tok::ok(), "Listening".into()),
        })
    }

    /// The co-host's On switch for its Avatar card's heading, or "Make active" when another
    /// co-host is the active one; `None` for any other avatar.
    pub(super) fn cohost_switch(&self, source: &Source, cx: &mut Context<Self>) -> Option<AnyElement> {
        if !is_cohost(source) {
            return None;
        }
        let id = source.id;
        let workspace = self.studio.read(cx).workspace().clone();
        let live = live_cohosts(&workspace);
        if active_cohost(&live).map(|s| s.id) != Some(id) {
            let others: Vec<Id> = live.iter().map(|s| s.id).filter(|o| *o != id).collect();
            let button = Button::new("cohost-make-active")
                .label("Make active")
                .xsmall()
                .on_click(cx.listener(move |this, _, _, cx| this.make_cohost_active(id, &others, cx)));
            return Some(button.into_any_element());
        }
        let on = source.settings[COHOST]["on"].as_bool() != Some(false);
        let mut settings = source.settings.clone();
        settings[COHOST]["on"] = json!(!on);
        Some(
            crate::piair::view::switch("cohost-on", on)
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.exec(Command::SetSourceSettings { source: id, settings: settings.clone() }, cx)
                }))
                .into_any_element(),
        )
    }

    fn make_cohost_active(&mut self, id: Id, others: &[Id], cx: &mut Context<Self>) {
        let workspace = self.studio.read(cx).workspace().clone();
        let mut commands = Vec::new();
        for (sid, active) in std::iter::once((id, true)).chain(others.iter().map(|o| (*o, false))) {
            if let Some(s) = workspace.sources.get(&sid) {
                let mut settings = s.settings.clone();
                settings[COHOST]["active"] = json!(active);
                commands.push(Command::SetSourceSettings { source: sid, settings });
            }
        }
        self.exec(Command::Batch { label: format!("Make this the active {NAME}"), commands }, cx);
    }

    /// The co-host's sections under its Avatar card (Talk, Brain, Voice, Persona, Behaviour).
    pub(super) fn cohost_sections(&self, source: &Source, cx: &mut Context<Self>) -> AnyElement {
        let id = source.id;
        let workspace = self.studio.read(cx).workspace().clone();
        let active = active_cohost(&live_cohosts(&workspace)).map(|s| s.id) == Some(id);
        self.cohost_view.update(cx, |v, _| v.set_source(Some(id), active));
        div().child(self.cohost_view.clone()).into_any_element()
    }

    /// Settings → Connections: the AI co-host's keys, one row per service, each with its state.
    pub(super) fn ai_keys_group(&self, cx: &mut Context<Self>) -> Div {
        use crate::piair::KeyState;
        let mut group = super::settings::group_tip(
            "AI co-host",
            "Keys for your co-host's brain and voice. Copy a key from the service's own site, then choose Paste key: Lightair keeps it in the system's credential store, checks it works and never shows it. A subscription (Claude Pro/Max, ChatGPT, GitHub Copilot) is used from the co-host's Brain section instead.",
        );
        for (id, name, _, hint) in crate::piair::keys::SERVICES {
            let state = self.piair.key_state(id);
            let (text, colour) = match &state {
                KeyState::Unknown => ("…".to_string(), tok::text_tertiary()),
                KeyState::Missing => ("Not set".to_string(), tok::text_tertiary()),
                KeyState::Saved => ("Saved".to_string(), tok::ok()),
                KeyState::Checking => ("Checking…".to_string(), tok::warn()),
                KeyState::Works(None) => ("Works".to_string(), tok::ok()),
                KeyState::Works(Some(note)) => (note.clone(), tok::warn()),
                KeyState::Problem(e) => (e.clone(), tok::error()),
            };
            let set = !matches!(state, KeyState::Missing | KeyState::Unknown);
            let service: &'static str = id;
            let control = div()
                .flex()
                .items_center()
                .gap_1()
                .child(
                    Button::new(SharedString::from(format!("ai-key-{service}")))
                        .label(if set { "Paste new key" } else { "Paste key" })
                        .xsmall()
                        .outline()
                        .on_click(cx.listener(move |this, _, _, cx| this.paste_ai_key(service, cx))),
                )
                .when(set, |d| {
                    d.child(
                        Button::new(SharedString::from(format!("ai-key-remove-{service}")))
                            .label("Remove")
                            .xsmall()
                            .ghost()
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.piair.save_key(service, None);
                                cx.notify();
                            })),
                    )
                });
            group = group.child(super::settings::row_status(name, &text, hint, control).text_color(colour));
        }
        group
    }

    /// Saves the key on the clipboard for an AI service.
    pub(super) fn paste_ai_key(&mut self, service: &'static str, cx: &mut Context<Self>) {
        let key = cx
            .read_from_clipboard()
            .and_then(|c| c.text())
            .map(|t| t.trim().to_string())
            .filter(|t| t.len() >= 16 && !t.contains(char::is_whitespace));
        match key {
            Some(k) => self.piair.save_key(service, Some(k)),
            None => self.studio.update(cx, |s, _| {
                s.issue = Some(
                    format!(
                        "Copy your {} key first (the clipboard doesn't hold one).",
                        crate::piair::keys::name_of(service)
                    )
                    .into(),
                )
            }),
        }
        cx.notify();
    }

    /// Development only (debug builds): `LIGHTAIR_PIAIR_PROMPT_FILE` sends its newest line to
    /// PiAir as a prompt from the streamer, or runs a command (`!add <avatar file>`, `!settings`,
    /// `!drives`, `!call <method> <json>`, `!chat <who>: <text>`, `!allow <bundle id>`, `!stop`).
    fn dev_cohost_prompts(&mut self, cx: &mut Context<Self>) {
        let Some(f) = std::env::var_os("LIGHTAIR_PIAIR_PROMPT_FILE").filter(|_| cfg!(debug_assertions)) else { return };
        let text = std::fs::read_to_string(&f).unwrap_or_default();
        if text == self.cohost.dev_prompts {
            return;
        }
        self.cohost.dev_prompts = text.clone();
        let Some(line) = text.lines().rev().find(|l| !l.trim().is_empty()).map(|l| l.trim().to_string()) else {
            return;
        };
        match line.as_str() {
            "!drives" => {
                if let Err(e) = self.piair.set_ai_drives(true) {
                    tracing::warn!(target: "piair", "AI drives: {e}");
                }
            }
            "!stop" => self.piair.kill_switch(),
            "!settings" => self.open_settings(super::settings::Page::CoHost, cx),
            "!connections" => self.open_settings(super::settings::Page::Connections, cx),
            l if l.starts_with("!settings-scroll ") => {
                let y: f32 = l[17..].trim().parse().unwrap_or(0.0);
                self.settings_scroll.set_offset(point(px(0.), px(-y)));
            }
            l if l.starts_with("!paste-key ") => {
                if let Some((id, ..)) = crate::piair::keys::SERVICES.iter().find(|s| s.0 == l[11..].trim()) {
                    self.paste_ai_key(id, cx);
                }
            }
            l if l.starts_with("!remove-key ") => {
                if let Some((id, ..)) = crate::piair::keys::SERVICES.iter().find(|s| s.0 == l[12..].trim()) {
                    self.piair.save_key(id, None);
                }
            }
            l if l.starts_with("!add ") => {
                let id = self.avatar_library.for_file(std::path::Path::new(l[5..].trim()));
                self.avatar_library.save();
                if let Some(e) = self.avatar_library.get(id).cloned() {
                    self.add_cohost_object(&e, cx);
                }
            }
            l if l.starts_with("!allow ") => {
                let mut a = self.piair.allowed();
                a.push(l[7..].trim().to_string());
                self.piair.set_allowed(a);
            }
            l if l.starts_with("!chat ") => {
                let hub = self.studio.read(cx).chat.clone();
                let (who, msg) = l[6..].split_once(": ").unwrap_or(("viewer", &l[6..]));
                let mut ev = lightair_live::ChatEvent::new("twitch", "test", lightair_live::Kind::Message);
                ev.user = Some(lightair_live::User {
                    id: who.to_lowercase(),
                    login: who.to_lowercase(),
                    display_name: who.to_string(),
                    ..Default::default()
                });
                ev.text = msg.to_string();
                ev.test = true;
                hub.inject(lightair_live::Channel { platform: "twitch".into(), channel: "test".into() }, ev);
            }
            l if l.starts_with("!call ") => {
                let (method, params) = l[6..].split_once(' ').unwrap_or((&l[6..], "{}"));
                let (host, method) = (self.piair.clone(), method.to_string());
                let params: Value = serde_json::from_str(params).unwrap_or(json!({}));
                std::thread::spawn(move || {
                    let r = host.answer(&method, params);
                    tracing::info!(target: "piair", "CALL {method} → {}", match r { Ok(v) => v.to_string(), Err(e) => format!("error {} {}", e.0, e.1) });
                });
            }
            l => self.piair.prompt(l),
        }
        cx.notify();
    }
}

/// Co-host sources that an object still uses, in the workspace's order.
fn live_cohosts(w: &lightair_model::Workspace) -> Vec<Source> {
    let mut out: Vec<Source> = w
        .sources
        .values()
        .filter(|s| is_cohost(s) && w.objects_using(s.id).next().is_some())
        .map(|s| (**s).clone())
        .collect();
    out.sort_by_key(|s| s.id);
    out
}

/// The active co-host: the one marked active, else the first.
fn active_cohost(live: &[Source]) -> Option<&Source> {
    live.iter()
        .find(|s| s.settings[COHOST]["active"].as_bool() == Some(true))
        .or_else(|| live.iter().find(|s| s.settings[COHOST]["active"].as_bool() != Some(false)))
        .or(live.first())
}
