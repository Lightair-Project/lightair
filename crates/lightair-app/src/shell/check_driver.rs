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
//! `scroll <px>`, `settings-shortcuts`, `assets-open <folder>`.

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
                        let (key, file) = rest.split_once(' ').unwrap_or((rest, ""));
                        self.open_anchor_sheet(
                            scene,
                            avatar_object,
                            avatar_source,
                            None,
                            Some(PathBuf::from(file)),
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
            "popout" => {
                if let Some(panel) = self.docked_panel(rest, cx) {
                    self.pop_out(panel, window, cx);
                }
            }
            "settings-shortcuts" => self.open_settings(super::settings::Page::Shortcuts, cx),
            _ => tracing::warn!(line, "CHECK unknown command"),
        }
        cx.notify();
    }
}
