//! Development only: `LIGHTAIR_OPEN=check-driver` with `LIGHTAIR_CHECK_FILE=<file>` steps the
//! window through what a check needs to look at, one command at a time, so a script can capture
//! each state from a single running Lightair (no relaunching, no clicking). Each time the file's
//! contents change, its last line runs, and "CHECK done <command>" is logged.
//!
//! Commands: `add-avatar <file>`, `select <object name>`, `library`, `close`, `keys-open`,
//! `press <keys>` (keys sent to the window, e.g. "ctrl-alt-1"), `capture-key <expression>`,
//! `expression <i>` (as its global shortcut does), `set <json>` (merged into the selected
//! object's source settings), `anchor-sheet`, `accessory <anchor key> <file>`, `pin <anchor key>`,
//! `drop-tree <object name|-> <above|into|below> <paths…>`, `drop-assets <paths…>`, `save-scene`, `undo`,
//! `scroll <px>`, `settings-shortcuts`, `settings-voices`, `say <text>`, `voice-level`, `test-event <kind>`,
//! `assets-open <folder>`, `cohost-window <page>`, `cohost <avatar file>` (an AI co-host object),
//! `install <package id>` (from Lightair's catalog, as the co-host's Install button does),
//! `revoke <package id> <permission>` (Take back), `piair-call <method> <json>` (one of PiAir's calls),
//! `full-access-ask` (the full-access question with sample text; nothing installs),
//! `streamer-policy <json>` (AI streamer mode's policy), `streamer-log` (the action log, logged),
//! `permission-allow` / `permission-deny` (answer the optional-permission sheet), `api <method> <json>`,
//! `grants <package id>`.

use std::path::PathBuf;
use std::time::Duration;

use gpui_kit::*;
use lightair_model::{Command, Id};

use super::StudioWindow;
use super::file_drop::DropTo;
use super::hierarchy::TreeDrop;

impl StudioWindow {
    pub(super) fn start_check_driver(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(file) = std::env::var_os("LIGHTAIR_CHECK_FILE").map(PathBuf::from) else { return };
        cx.spawn_in(window, async move |this, cx| {
            let mut last = String::new();
            loop {
                cx.background_executor().timer(Duration::from_millis(250)).await;
                let text = std::fs::read_to_string(&file).unwrap_or_default();
                if text == last {
                    continue;
                }
                last = text.clone();
                let Some(line) = text.lines().rev().find(|l| !l.trim().is_empty()).map(str::to_string) else {
                    continue;
                };
                if this.update_in(cx, |this, window, cx| this.check_command(&line, window, cx)).is_err() {
                    break;
                }
                tracing::info!("CHECK done {line}");
            }
        })
        .detach();
    }

    fn object_named(&self, name: &str, cx: &App) -> Option<Id> {
        let studio = self.studio.read(cx);
        let scene = studio.selected_scene.and_then(|s| studio.workspace().scene(s))?;
        scene.objects.iter().find(|o| o.name == name).map(|o| o.id)
    }

    fn check_command(&mut self, line: &str, window: &mut Window, cx: &mut Context<Self>) {
        let (cmd, rest) = line.trim().split_once(' ').unwrap_or((line.trim(), ""));
        let workspace = self.studio.read(cx).workspace().clone();
        let selected_source = || {
            let studio = self.studio.read(cx);
            let scene = studio.selected_scene.and_then(|s| studio.workspace().scene(s))?;
            scene.object(studio.selected_object?)?.source
        };
        match cmd {
            "cohost" => {
                let id = self.avatar_library.for_file(std::path::Path::new(rest));
                self.avatar_library.save();
                if let Some(e) = self.avatar_library.get(id).cloned() {
                    self.add_cohost_object(&e, cx);
                }
            }
            // take a package's permission back, as the Packages panel's Take back does
            "revoke" => {
                if let Some((id, permission)) = rest.split_once(' ') {
                    crate::grants::revoke(id, permission.trim());
                }
            }
            // one of PiAir's calls, answered as PiAir's would be (its permissions checked)
            // answers the open request for optional permissions: every one ticked is allowed
            "permission-allow" | "permission-deny" => {
                let allow = cmd == "permission-allow";
                if let Some((r, ticks)) = self.packages.permission_ask.take() {
                    let allowed: Vec<String> = if allow {
                        r.permissions.iter().zip(&ticks).filter(|(_, t)| **t).map(|(p, _)| p.clone()).collect()
                    } else {
                        Vec::new()
                    };
                    tracing::info!(package = %r.name, reason = %r.reason, asked = ?r.permissions, ?allowed, "CHECK permission sheet");
                    r.answer(allowed);
                } else {
                    tracing::info!("CHECK permission sheet: none open");
                }
            }
            // a remote API method, as Lightair Live's Remote calls it
            "api" => {
                let (method, params) = rest.split_once(' ').unwrap_or((rest, "{}"));
                let params = serde_json::from_str(params).unwrap_or(serde_json::json!({}));
                let call = lightair_remote::Call { method: method.to_string(), params, client: "check", device: None };
                let result = self.live_call(&call, cx);
                tracing::info!(method, ?result, "CHECK api");
            }
            "grants" => {
                for i in crate::packages::installed().iter().filter(|i| i.id().as_str() == rest.trim()) {
                    tracing::info!(id = %rest.trim(), granted = ?crate::packages::granted_permissions(i), optional = ?crate::grants::optional_granted(rest.trim()), "CHECK grants");
                }
            }
            "piair-call" => {
                let (method, params) = rest.split_once(' ').unwrap_or((rest, "{}"));
                let params = serde_json::from_str(params).unwrap_or(serde_json::json!({}));
                // off the UI thread, as PiAir's own calls arrive (some wait for the window)
                let (host, method) = (self.piair.clone(), method.to_string());
                std::thread::spawn(move || {
                    let result = host.answer(&method, params);
                    tracing::info!(method, ?result, "CHECK piair-call");
                });
            }
            "install" => {
                crate::catalogs::request_install(rest, None, None);
                // a window in the background may not draw: handle the link now
                self.handle_links(window, cx);
            }
            "add-avatar" => {
                let id = self.avatar_library.for_file(std::path::Path::new(rest));
                self.avatar_library.save();
                self.use_avatar(id, None, cx);
            }
            "select" => {
                let id = self.object_named(rest, cx);
                self.select_object(id, cx);
            }
            "library" => {
                let source = selected_source();
                self.open_avatar_library(source, window, cx);
            }
            "close" => {
                self.library_sheet = None;
                self.anchor_sheet = None;
                self.settings_open = false;
            }
            "keys-open" => self.expression_keys_open = true,
            "cohost-window" => {
                use crate::piair::view::Page;
                let page = match rest {
                    "talk" => Page::Talk,
                    "voice" => Page::Voice,
                    "hearing" => Page::Hearing,
                    "persona" => Page::Persona,
                    "behaviour" => Page::Behaviour,
                    _ => Page::Brain,
                };
                self.cohost_window.update(cx, |w, cx| w.open(page, window, cx));
            }
            "press" => {
                // (sent from outside this update: the window's key handlers update this view)
                if let Ok(k) = Keystroke::parse(Box::leak(rest.to_string().into_boxed_str())) {
                    cx.spawn_in(window, async move |_, cx| {
                        let _ = cx.update(|window, cx| window.dispatch_keystroke(k, cx));
                    })
                    .detach();
                }
            }
            "capture-key" => {
                if let Some(source) = selected_source() {
                    self.expression_keys_open = true;
                    self.expression_capture = Some((source, rest.to_string()));
                    window.focus(&self.focus, cx);
                }
            }
            "expression" => {
                if let Ok(i) = rest.parse::<u16>() {
                    self.global_action(crate::shortcuts::GlobalAction::Expression(i), window, cx);
                }
            }
            "set" => {
                if let (Some(source), Ok(patch)) = (selected_source(), serde_json::from_str::<serde_json::Value>(rest))
                    && let Some(i) = workspace.sources.get(&source)
                {
                    let mut settings = i.settings.clone();
                    for (k, v) in patch.as_object().into_iter().flatten() {
                        settings[k] = v.clone();
                    }
                    self.exec(Command::SetSourceSettings { source, settings }, cx);
                }
            }
            "anchor-sheet" | "accessory" | "pin" => {
                // the avatar: the selected one, or the selected object's parent
                let studio = self.studio.read(cx);
                let Some(scene) = studio.selected_scene else { return };
                let sc = workspace.scene(scene).cloned();
                let sel = studio.selected_object;
                let Some(sc) = sc else { return };
                let is_avatar = |o: Id| {
                    sc.object(o)
                        .and_then(|o| o.source)
                        .and_then(|s| workspace.sources.get(&s))
                        .is_some_and(|i| i.kind == super::avatar_props::KIND)
                };
                let (avatar_object, object) = match sel {
                    Some(o) if is_avatar(o) => (o, None),
                    Some(o) => match sc.object(o).and_then(|l| l.parent) {
                        Some(p) if is_avatar(p) => (p, Some(o)),
                        _ => return,
                    },
                    None => return,
                };
                let Some(avatar_source) = sc.object(avatar_object).and_then(|o| o.source) else { return };
                match cmd {
                    "anchor-sheet" => {
                        self.open_anchor_sheet(scene, avatar_object, avatar_source, object, None, window, cx)
                    }
                    "accessory" => {
                        // (the file is the part from " /": an anchor's name may have spaces)
                        let (key, file) =
                            rest.rsplit_once(" /").map(|(k, f)| (k, format!("/{f}"))).unwrap_or((rest, String::new()));
                        self.open_anchor_sheet(
                            scene,
                            avatar_object,
                            avatar_source,
                            None,
                            Some(PathBuf::from(&file)),
                            window,
                            cx,
                        );
                        self.choose_anchor(Some(key.to_string()), window, cx);
                    }
                    _ => {
                        if let Some(object) = object {
                            self.open_anchor_sheet(scene, avatar_object, avatar_source, Some(object), None, window, cx);
                            self.choose_anchor(Some(rest.to_string()), window, cx);
                        }
                    }
                }
            }
            "drop-tree" => {
                let mut parts = rest.splitn(3, ' ');
                let (name, zone, paths) =
                    (parts.next().unwrap_or("-"), parts.next().unwrap_or("into"), parts.next().unwrap_or(""));
                let zone = match zone {
                    "above" => TreeDrop::Above,
                    "below" => TreeDrop::Below,
                    _ => TreeDrop::Into,
                };
                let target = (name != "-").then(|| self.object_named(name, cx)).flatten().map(|id| (id, zone));
                let paths: Vec<PathBuf> = paths.split('|').filter(|p| !p.is_empty()).map(PathBuf::from).collect();
                self.drop_files(&paths, DropTo::Tree(target), window, cx);
            }
            "drop-assets" => {
                let paths: Vec<PathBuf> = rest.split('|').filter(|p| !p.is_empty()).map(PathBuf::from).collect();
                if let Some(folder) = self.asset_dir(cx) {
                    self.drop_files(&paths, DropTo::Assets(folder), window, cx);
                }
            }
            "assets-open" => {
                let root = self.studio.read(cx).assets_dir();
                self.asset_folder = root.map(|r| r.join(rest)).filter(|_| !rest.is_empty());
                self.refresh_assets();
            }
            "save-scene" => {
                if let Some(scene) = self.studio.read(cx).selected_scene {
                    self.save_scene_asset(scene, cx);
                }
            }
            "undo" => self.studio.update(cx, |s, cx| s.undo(cx)),
            "scroll" => {
                if let Ok(px_) = rest.parse::<f32>() {
                    self.inspector_scroll.set_offset(point(px(0.), px(-px_)));
                }
            }
            // the open panel, for a look (what's chosen goes nowhere)
            "picker" => {
                let choose = if rest == "accessory" { crate::file_pick::ACCESSORY } else { crate::file_pick::AVATAR };
                let picked = crate::file_pick::pick(choose, cx);
                cx.spawn(async move |_, _| {
                    let path = picked.await.ok().flatten();
                    tracing::info!(?path, "CHECK picker answered");
                })
                .detach();
            }
            // an avatar's Shape ("shape 1080 1920"), and Check tracking
            "shape" => {
                let mut n = rest.split(' ').filter_map(|v| v.parse::<u32>().ok());
                if let (Some(source), Some(w), Some(h)) = (selected_source(), n.next(), n.next()) {
                    self.set_avatar_shape(source, (w, h), cx);
                }
            }
            "check-tracking" => {
                if let Some(source) = selected_source() {
                    self.check_tracking(source, true, cx);
                }
            }
            // the PNGtuber editor: new, with a folder's pictures ("png-new <folder>"), or a file's
            "png-new" => {
                let source = selected_source()
                    .filter(|s| workspace.sources.get(s).is_some_and(|i| i.kind == super::avatar_props::KIND));
                self.open_png_editor_with(None, vec![PathBuf::from(rest)], source, cx);
            }
            "png" => crate::pngtuber_editor::check(rest, cx),
            // a PNGtuber's push to talk: "ptt-keys" waits for its keys; "ptt on" / "ptt off"
            "ptt-keys" => {
                self.push_to_talk_capture = selected_source();
                window.focus(&self.focus, cx);
            }
            "ptt" => {
                if let Some(source) = selected_source() {
                    self.push_to_talk(source, rest == "on", cx);
                }
            }
            "png-edit" => {
                let source = selected_source()
                    .filter(|s| workspace.sources.get(s).is_some_and(|i| i.kind == super::avatar_props::KIND));
                self.open_png_editor(Some(PathBuf::from(rest)), source, cx);
            }
            "popout" => {
                if let Some(panel) = self.docked_panel(rest, cx) {
                    self.pop_out(panel, window, cx);
                }
            }
            "settings-shortcuts" => self.open_settings(super::settings::Page::Shortcuts, cx),
            "settings-voices" => self.open_settings(super::settings::Page::Voices, cx),
            // AI streamer mode's whole policy, as JSON (mode, sites, scenes, games, chat_browse, chat_plays)
            "streamer-policy" => match serde_json::from_str::<crate::piair::streamer::Policy>(rest) {
                Ok(p) => self.piair.set_policy(p),
                Err(e) => tracing::warn!("CHECK streamer-policy: {e}"),
            },
            "streamer-log" => {
                for e in crate::piair::streamer::recent(20) {
                    tracing::info!(cause = %e.cause, method = %e.method, what = %e.what, allowed = e.allowed, reason = %e.reason, "CHECK action");
                }
            }
            // the full-access question as a full-access package would open it (nothing installs)
            "full-access-ask" => {
                let (tx, _rx) = futures_channel::oneshot::channel::<bool>();
                self.ask_full_access(
                    "Give \u{201c}Stage Lights Pro\u{201d} full access?".into(),
                    "This plugin runs like any app you install: it can read your files, use the network and your devices. Lightair can't limit or watch what it does.\n\nIt isn't checked by Lightair.".into(),
                    tx,
                    cx,
                );
            }
            // says text on the Voice track (made when it's missing), with the default voice
            "say" => self.say_on_voice_track(rest, None, cx),
            // a made-up chat event (follow, sub, paid, cheer, raid, message), as widgets.test sends
            "test-event" => {
                if let Some(w) = &self.studio.read(cx).widgets {
                    w.test(rest, "twitch");
                }
            }
            // logs the Voice track's loudest level now ("CHECK voice-level <dBFS>")
            "voice-level" => {
                let track = workspace.tracks.iter().find(|t| {
                    workspace.sources.get(&t.source).is_some_and(|s| s.kind == lightair_host::piair_voice::VOICE_KIND)
                });
                let level = track
                    .and_then(|t| lightair_host::meters::meters().get(t.id))
                    .map(|c| c.levels().magnitude.iter().copied().fold(f32::NEG_INFINITY, f32::max));
                tracing::info!(track = track.is_some(), ?level, "CHECK voice-level");
            }
            // packages and catalogs (shell/packages/catalog_ui.rs, `package_check`)
            c if c.starts_with("pkg-") => self.package_check(c, rest, window, cx),
            // script plugins (crate::scripts_host)
            c if c.starts_with("script") => crate::scripts_host::check(c, rest),
            _ => tracing::warn!(line, "CHECK unknown command"),
        }
        cx.notify();
    }
}
