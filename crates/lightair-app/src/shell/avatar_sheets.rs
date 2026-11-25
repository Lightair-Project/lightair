//! The avatar library's sheet, "Pin to" and accessories (docs/plans/avatars.md, "Library",
//! "Accessories and pins").
//!
//! - **Avatars**: the library ([`crate::avatar_library`]) as a sheet of cards, like the source
//!   picker's: click one to use it (on the selected avatar, or as a new avatar object), right-click
//!   to rename, duplicate or delete; "Add from file…" takes a model from disk.
//! - **Pin to**: an object whose parent is an avatar follows one of its anchors (a VRM's bones, a
//!   2D model's parts or art meshes); the picker lists them by group, searchable.
//! - **Add accessory**: a picture or video, then its anchor, in one go: it's added under the
//!   avatar, on that anchor, pinned.

use std::path::{Path, PathBuf};
use std::rc::Rc;
#[cfg(any(feature = "libobs", target_os = "macos"))]
use std::time::{Duration, Instant};

use gpui_kit::component::Sizable;
use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::input::{Input as TextInput, InputEvent, InputState};
use gpui_kit::component::menu::{ContextMenuExt, PopupMenuItem};
use gpui_kit::component::scroll::ScrollableElement;
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;
use lightair_model::{Command, Id, Layout, Object, Source};
use lightair_obs::avatar_pins::{Placement, on_canvas};
use serde_json::{Value, json};

use super::StudioWindow;
use super::avatar_props::KIND;
use crate::avatar_library as lib;
use crate::ui::{self, tok};

pub(super) struct LibrarySheet {
    /// The avatar to switch (its source); `None`: add a new avatar object.
    source: Option<Id>,
    renaming: Option<(Id, Entity<InputState>)>,
    /// The cards' scrolling (the chosen one scrolled into view on open).
    scroll: ScrollHandle,
    _subs: Vec<Subscription>,
}

/// The sheet's size: three cards across, and at most this tall (less in a small window).
const SHEET_W: f32 = 588.0;
const SHEET_H: f32 = 620.0;
/// A card's picture area.
const PICTURE_W: f32 = 160.0;
const PICTURE_H: f32 = 116.0;

pub(super) struct AnchorSheet {
    scene: Id,
    avatar_object: Id,
    avatar_source: Id,
    /// The object to pin, or a new accessory's file (added once its anchor is chosen).
    object: Option<Id>,
    file: Option<PathBuf>,
    search: Entity<InputState>,
    _events: Subscription,
}

/// The library entry an avatar's settings name.
pub(super) fn entry_of(settings: &Value) -> Option<Id> {
    serde_json::from_value(settings.get("library")?.clone()).ok()
}

/// The anchor an object is pinned to, from the avatar's settings.
pub(super) fn pin_of(settings: &Value, object: Id) -> Option<String> {
    let pin = &settings["pins"][object.to_string()];
    pin.as_str().or_else(|| pin["anchor"].as_str()).filter(|k| !k.is_empty()).map(str::to_string)
}

/// `settings` with `object` pinned to `key` (`None`: not pinned), with where the anchor is at rest
/// now (`at`), which its offset is kept from.
pub(super) fn with_pin(
    settings: &Value,
    object: Id,
    key: Option<&str>,
    at: Option<lightair_obs::avatar_pins::Spot>,
) -> Value {
    let mut s = if settings.is_object() { settings.clone() } else { json!({}) };
    if !s["pins"].is_object() {
        s["pins"] = json!({});
    }
    match key {
        Some(k) => {
            s["pins"][object.to_string()] = match at {
                Some(a) => json!({"anchor": k, "at": [a.x, a.y, a.angle, a.scale]}),
                None => json!(k),
            }
        }
        None => {
            if let Some(m) = s["pins"].as_object_mut() {
                m.remove(&object.to_string());
            }
        }
    }
    s
}

fn is_video(path: &Path) -> bool {
    let e = path.extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default();
    ["mov", "mp4", "m4v", "webm"].contains(&e.as_str())
}

impl StudioWindow {
    // ------------------------------------------------------------------- the library

    /// Opens the library: to switch `source`'s avatar, or (`None`) to add an avatar object. With
    /// nothing in it yet, asks for a model file straight away.
    pub(super) fn open_avatar_library(&mut self, source: Option<Id>, window: &mut Window, cx: &mut Context<Self>) {
        self.add_menu = false;
        if self.avatar_library.avatars.is_empty() {
            self.add_avatar_from_file(source, window, cx);
            return;
        }
        // the chosen one in view
        let scroll = ScrollHandle::new();
        let current =
            source.and_then(|s| self.studio.read(cx).workspace().sources.get(&s).and_then(|i| entry_of(&i.settings)));
        if let Some(i) = current.and_then(|c| self.avatar_library.avatars.iter().position(|e| e.id == c)) {
            scroll.scroll_to_item(i);
        }
        self.library_sheet = Some(LibrarySheet { source, renaming: None, scroll, _subs: Vec::new() });
        // pictures for the ones without (a model no scene shows is drawn on its own)
        let missing: Vec<Id> = self
            .avatar_library
            .avatars
            .iter()
            .filter(|e| {
                let wide = e.picture_path().is_some_and(|p| lib::picture_size(&p) == Some(OLD_PICTURE));
                (e.picture_path().is_none() || wide)
                    && !self.avatar_pictures.contains(&e.id)
                    && Path::new(&e.file).exists()
            })
            .map(|e| e.id)
            .collect();
        for id in missing {
            self.avatar_pictures.insert(id);
            self.picture_queue.push_back(id);
        }
        self.next_picture(cx);
        cx.notify();
    }

    /// Takes the next queued library picture from the model's file, one at a time: a private
    /// avatar source draws it (once loaded) into a preview that's read back.
    #[cfg(feature = "libobs")]
    fn next_picture(&mut self, cx: &mut Context<Self>) {
        if self.picture_busy {
            return;
        }
        let Some(entry) = self.picture_queue.pop_front() else { return };
        let Some(e) = self.avatar_library.get(entry).cloned() else { return self.next_picture(cx) };
        let framing = if lib::kind_of(&e.file) == "PNGtuber" {
            "full".to_string()
        } else {
            e.look["framing"].as_str().unwrap_or("bust").to_string()
        };
        let engine = self.studio.read(cx).engine().clone();
        let file = e.file.clone();
        let Some(src) = engine.call(move |s| s.reconciler.avatar_snapshot_source(&file, &framing)) else {
            return self.next_picture(cx);
        };
        self.picture_busy = true;
        let preview = lightair_preview::Preview::attach();
        preview.set_own_size(true);
        preview.set_size(Some(picture_size(1080, 1080)));
        preview.set_source(Some(src.clone()));
        cx.spawn(async move |this, cx| {
            let start = Instant::now();
            // loaded (a big model takes a few seconds), then a few frames drawn
            let mut seen_at = None;
            let frame = loop {
                cx.background_executor().timer(Duration::from_millis(150)).await;
                let status = lightair_obs::avatar::status_of(&src);
                if !status.error.is_empty() || start.elapsed() > Duration::from_secs(20) {
                    break None;
                }
                if status.loaded && seen_at.is_none() {
                    seen_at = Some(preview.frames());
                }
                if seen_at.is_some_and(|f| preview.frames() >= f + 8)
                    && let Some(f) = preview.latest_frame()
                {
                    break Some(f);
                }
            };
            drop(preview);
            let png = frame.as_ref().and_then(picture_png);
            let _ = this.update(cx, |this, cx| {
                match png {
                    Some(png) => {
                        this.avatar_library.set_picture(entry, &png);
                        this.avatar_library.save();
                    }
                    None => tracing::warn!(%entry, "no library picture: the model didn't load"),
                }
                this.picture_busy = false;
                this.next_picture(cx);
                cx.notify();
            });
        })
        .detach();
    }

    /// Takes the next queued library picture from the model's file, one at a time: the
    /// compositor's avatar renderer draws it at rest, off screen, in the background.
    #[cfg(not(feature = "libobs"))]
    fn next_picture(&mut self, cx: &mut Context<Self>) {
        if self.picture_busy {
            return;
        }
        let Some(entry) = self.picture_queue.pop_front() else { return };
        let Some(e) = self.avatar_library.get(entry).cloned() else { return self.next_picture(cx) };
        let framing = if lib::kind_of(&e.file) == "PNGtuber" {
            "full".to_string()
        } else {
            e.look["framing"].as_str().unwrap_or("bust").to_string()
        };
        self.picture_busy = true;
        let settings =
            json!({"file": e.file, "framing": framing, "width": 1080, "height": 1080, "mirror": true, "device": ""});
        cx.spawn(async move |this, cx| {
            let png = cx.background_executor().spawn(async move { picture_at_rest(&settings) }).await;
            let _ = this.update(cx, |this, cx| {
                match png {
                    Some(png) => {
                        this.avatar_library.set_picture(entry, &png);
                        this.avatar_library.save();
                    }
                    None => tracing::warn!(%entry, "no library picture: the model didn't load"),
                }
                this.picture_busy = false;
                this.next_picture(cx);
                cx.notify();
            });
        })
        .detach();
    }

    fn close_avatar_library(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.library_sheet = None;
        self.adding_cohost = false;
        window.focus(&self.focus, cx);
        cx.notify();
    }

    /// Asks for a model file (only the types Lightair loads), keeps it in the library and uses it.
    fn add_avatar_from_file(&mut self, source: Option<Id>, window: &mut Window, cx: &mut Context<Self>) {
        let picked = crate::file_pick::pick(crate::file_pick::AVATAR, cx);
        cx.spawn_in(window, async move |this, cx| {
            let Ok(Some(path)) = picked.await else { return };
            let _ = this.update(cx, |this, cx| {
                let id = this.avatar_library.for_file(&path);
                this.avatar_library.save();
                this.use_avatar(id, source, cx);
            });
        })
        .detach();
    }

    /// Puts a library avatar on `source` (keeping the one it had in the library as it was), or
    /// adds it as a new avatar object.
    pub(super) fn use_avatar(&mut self, entry: Id, source: Option<Id>, cx: &mut Context<Self>) {
        let Some(e) = self.avatar_library.get(entry).cloned() else { return };
        self.library_sheet = None;
        match source {
            Some(source) => {
                let workspace = self.studio.read(cx).workspace().clone();
                let Some(i) = workspace.sources.get(&source) else { return };
                self.keep_avatar_look(&i.settings);
                let mut settings = lib::with_look(&i.settings, &e.look);
                settings["file"] = json!(e.file);
                settings["library"] = json!(e.id);
                // (expressions are the model's own)
                settings["expression"] = json!("");
                self.exec(Command::SetSourceSettings { source, settings }, cx);
            }
            None if std::mem::take(&mut self.adding_cohost) => self.add_cohost_object(&e, cx),
            None => {
                let device = self.face_camera(cx);
                let mut base = json!({"file": e.file, "device": device, "width": 1080, "height": 1080, "framing": "bust", "mirror": true, "library": e.id});
                // a PNGtuber: listening to a mic, its whole picture shown
                if lib::kind_of(&e.file) == "PNGtuber" {
                    base["mic"] = json!(self.default_mic(cx));
                    base["framing"] = json!("full");
                    base["device"] = json!("");
                }
                let settings = lib::with_look(&base, &e.look);
                let size = (
                    settings["width"].as_f64().unwrap_or(1080.0) as f32,
                    settings["height"].as_f64().unwrap_or(1080.0) as f32,
                );
                self.add_source_object(&e.name, KIND, settings, size, cx);
            }
        }
        cx.notify();
    }

    /// Keeps an avatar's look in its library entry.
    fn keep_avatar_look(&mut self, settings: &Value) {
        let Some(id) = entry_of(settings) else { return };
        let look = lib::look_of(settings);
        if let Some(e) = self.avatar_library.get_mut(id)
            && e.look != look
            && e.file == settings["file"].as_str().unwrap_or("")
        {
            e.look = look;
            self.avatar_library.save();
        }
    }

    fn start_avatar_rename(&mut self, id: Id, window: &mut Window, cx: &mut Context<Self>) {
        let Some(name) = self.avatar_library.get(id).map(|e| e.name.clone()) else { return };
        let input = cx.new(|cx| InputState::new(window, cx));
        input.update(cx, |s, cx| {
            s.set_value(name, window, cx);
            s.focus(window, cx);
            s.select_all(window, cx);
        });
        let sub = cx.subscribe_in(&input, window, |this, _, ev: &InputEvent, _, cx| {
            if matches!(ev, InputEvent::PressEnter { .. } | InputEvent::Blur) {
                this.finish_avatar_rename(cx);
            }
        });
        if let Some(sheet) = self.library_sheet.as_mut() {
            sheet.renaming = Some((id, input));
            sheet._subs = vec![sub];
        }
        cx.notify();
    }

    fn finish_avatar_rename(&mut self, cx: &mut Context<Self>) {
        let Some((id, input)) = self.library_sheet.as_mut().and_then(|s| s.renaming.take()) else { return };
        let name = input.read(cx).value().trim().to_string();
        if !name.is_empty()
            && let Some(e) = self.avatar_library.get_mut(id)
        {
            e.name = name;
            self.avatar_library.save();
        }
        cx.notify();
    }

    /// The selected object's avatar (its source and settings), if it is one.
    fn selected_avatar(&self, cx: &App) -> Option<(Id, Value)> {
        let studio = self.studio.read(cx);
        let scene = studio.selected_scene.and_then(|s| studio.workspace().scene(s))?;
        let source = scene.object(studio.selected_object?)?.source?;
        let i = studio.workspace().sources.get(&source).filter(|i| i.kind == KIND)?;
        Some((source, i.settings.clone()))
    }

    /// Takes a picture of an avatar for its library entry (it must be on screen).
    #[cfg(feature = "libobs")]
    fn take_avatar_picture(&mut self, source: Id, entry: Id, cx: &mut Context<Self>) {
        self.avatar_pictures.insert(entry);
        let engine = self.studio.read(cx).engine().clone();
        let Some(src) = engine.call(move |e| e.reconciler.source_ref(source)) else {
            return;
        };
        let (sw, sh) = src.size();
        let preview = lightair_preview::Preview::attach();
        preview.set_own_size(true);
        preview.set_size(Some(picture_size(sw, sh)));
        preview.set_source(Some(src));
        self.read_avatar_picture(preview, entry, cx);
    }

    /// Without libobs: the avatar drawn at rest, off screen, with its source's settings.
    #[cfg(not(feature = "libobs"))]
    fn take_avatar_picture(&mut self, source: Id, entry: Id, cx: &mut Context<Self>) {
        self.avatar_pictures.insert(entry);
        let workspace = self.studio.read(cx).workspace().clone();
        let Some(src) = workspace.sources.get(&source) else { return };
        let mut settings = src.settings.clone();
        settings["device"] = json!("");
        cx.spawn(async move |this, cx| {
            let png = cx.background_executor().spawn(async move { picture_at_rest(&settings) }).await;
            let Some(png) = png else { return };
            let _ = this.update(cx, |this, cx| {
                this.avatar_library.set_picture(entry, &png);
                this.avatar_library.save();
                cx.notify();
            });
        })
        .detach();
    }

    /// Reads `preview` back once it has drawn a few frames: the library picture of `entry`.
    #[cfg(feature = "libobs")]
    fn read_avatar_picture(&mut self, preview: lightair_preview::Preview, entry: Id, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            let start = Instant::now();
            let frame = loop {
                cx.background_executor().timer(Duration::from_millis(100)).await;
                if preview.frames() >= 6
                    && let Some(f) = preview.latest_frame()
                {
                    break Some(f);
                }
                if start.elapsed() > Duration::from_secs(4) {
                    break None;
                }
            };
            drop(preview);
            let Some(png) = frame.as_ref().and_then(picture_png) else { return };
            let _ = this.update(cx, |this, cx| {
                this.avatar_library.set_picture(entry, &png);
                this.avatar_library.save();
                cx.notify();
            });
        })
        .detach();
    }

    /// On the Inspector's timer: avatars' expression shortcuts registered, the selected avatar's
    /// look kept in the library, and its picture taken if it has none.
    pub(super) fn sync_avatars(&mut self, cx: &mut Context<Self>) {
        let workspace = self.studio.read(cx).workspace().clone();
        crate::shortcuts::set_expression_keys(expression_keys(&workspace));
        crate::shortcuts::set_push_to_talk_keys(push_to_talk_keys(&workspace));
        let Some((source, settings)) = self.selected_avatar(cx) else { return };
        self.keep_avatar_look(&settings);
        let loaded = self.avatar_status.get(&source).is_some_and(|s| s.loaded);
        if let Some(entry) = entry_of(&settings)
            && loaded
            && !self.avatar_pictures.contains(&entry)
            && self
                .avatar_library
                .get(entry)
                .is_some_and(|e| e.picture_path().is_none() && e.file == settings["file"].as_str().unwrap_or(""))
        {
            self.take_avatar_picture(source, entry, cx);
        }
    }

    pub(super) fn library_sheet_overlay(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let sheet = self.library_sheet.as_ref()?;
        let target = sheet.source;
        let workspace = self.studio.read(cx).workspace().clone();
        let current = target.and_then(|s| workspace.sources.get(&s)).and_then(|i| entry_of(&i.settings));
        let for_name = target.and_then(|s| workspace.sources.get(&s)).map(|i| format!("for {}", i.name));
        let me = cx.entity().downgrade();

        let mut grid = div()
            .id("avatar-library-body")
            .flex_1()
            .min_h_0()
            .grid()
            .grid_cols(3)
            .gap_2()
            .content_start()
            .pr_2()
            .track_scroll(&sheet.scroll)
            .overflow_y_scroll();
        for (i, e) in self.avatar_library.avatars.iter().enumerate() {
            let id = e.id;
            let selected = current == Some(id);
            let renaming = sheet.renaming.as_ref().filter(|(r, _)| *r == id).map(|(_, input)| input.clone());
            let missing = !Path::new(&e.file).exists();
            let picture = match e.picture_path() {
                // its own shape, fitted in (never stretched)
                Some(p) => {
                    let (w, h) = lib::fit(&p, PICTURE_W, PICTURE_H);
                    img(p).w(px(w)).h(px(h)).into_any_element()
                }
                None => ui::icon("person-standing").size(px(28.)).text_color(tok::text_tertiary()).into_any_element(),
            };
            let menu_me = me.clone();
            let can_picture = target.is_some() && selected;
            grid = grid.child(
                div()
                    .id(("avatar-card", i))
                    .flex()
                    .flex_col()
                    .gap_1()
                    .p_1p5()
                    .rounded(px(10.))
                    .border_1()
                    .border_color(if selected { tok::accent_primary() } else { tok::bg_panel() })
                    .when(selected, |d| d.bg(tok::bg_selected()))
                    .cursor_pointer()
                    .hover(|d| d.bg(tok::bg_selected()))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .justify_center()
                            .h(px(PICTURE_H))
                            .rounded(px(8.))
                            .overflow_hidden()
                            .bg(tok::bg_app())
                            .child(picture),
                    )
                    .child(match renaming {
                        Some(input) => TextInput::new(&input).small().into_any_element(),
                        None => div()
                            .flex()
                            .items_center()
                            .gap_1()
                            .px_0p5()
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .overflow_hidden()
                                    .text_ellipsis()
                                    .whitespace_nowrap()
                                    .font_weight(FontWeight::MEDIUM)
                                    .child(e.name.clone()),
                            )
                            .when(selected, |d| {
                                d.child(ui::icon("check").size(px(13.)).text_color(tok::accent_primary()))
                            })
                            .into_any_element(),
                    })
                    .child(
                        div()
                            .px_0p5()
                            .text_size(px(11.))
                            .text_color(if missing { tok::warn() } else { tok::text_tertiary() })
                            .child(if missing { "File not found".to_string() } else { e.kind().to_string() }),
                    )
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if this.library_sheet.as_ref().is_some_and(|s| s.renaming.is_some()) {
                            return;
                        }
                        this.use_avatar(id, target, cx);
                    }))
                    .context_menu(move |menu, _, _| {
                        let (a, b, c, d) = (menu_me.clone(), menu_me.clone(), menu_me.clone(), menu_me.clone());
                        menu.item(PopupMenuItem::new("Rename").icon(ui::icon("pencil")).on_click(
                            move |_, window, cx| {
                                if let Some(s) = a.upgrade() {
                                    s.update(cx, |w, cx| w.start_avatar_rename(id, window, cx));
                                }
                            },
                        ))
                        .item(PopupMenuItem::new("Duplicate").icon(ui::icon("copy")).on_click(move |_, _, cx| {
                            if let Some(s) = b.upgrade() {
                                s.update(cx, |w, cx| {
                                    w.avatar_library.duplicate(id);
                                    w.avatar_library.save();
                                    cx.notify();
                                });
                            }
                        }))
                        .when(can_picture, |m| {
                            m.item(PopupMenuItem::new("New picture").icon(ui::icon("camera")).on_click(
                                move |_, _, cx| {
                                    if let Some(s) = d.upgrade() {
                                        s.update(cx, |w, cx| {
                                            if let Some(source) = target {
                                                w.take_avatar_picture(source, id, cx);
                                            }
                                        });
                                    }
                                },
                            ))
                        })
                        .separator()
                        .item(PopupMenuItem::new("Delete").icon(ui::icon("trash")).on_click(
                            move |_, _, cx| {
                                if let Some(s) = c.upgrade() {
                                    s.update(cx, |w, cx| {
                                        w.avatar_library.remove(id);
                                        w.avatar_library.save();
                                        cx.notify();
                                    });
                                }
                            },
                        ))
                    }),
            );
        }

        Some(
            div()
                .absolute()
                .inset_0()
                .flex()
                .justify_center()
                .items_start()
                .pt(px(80.))
                .bg(tok::scrim())
                .occlude()
                .on_mouse_down(MouseButton::Left, cx.listener(|this, _, window, cx| this.close_avatar_library(window, cx)))
                .child(
                    div()
                        .id("avatar-library")
                        .flex()
                        .flex_col()
                        .gap_2()
                        .w(px(SHEET_W))
                        .h(px(SHEET_H))
                        .max_h(relative(0.86))
                        .p_3()
                        .rounded(px(14.))
                        .bg(tok::bg_panel())
                        .border_1()
                        .border_color(tok::border_strong())
                        .shadow_lg()
                        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                        .capture_key_down(cx.listener(|this, ev: &KeyDownEvent, window, cx| {
                            if ev.keystroke.key == "escape" && this.library_sheet.as_ref().is_some_and(|s| s.renaming.is_none()) {
                                this.close_avatar_library(window, cx);
                                cx.stop_propagation();
                            }
                        }))
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .gap_3()
                                .child(div().flex_none().text_size(px(15.)).font_weight(FontWeight::SEMIBOLD).child("Avatars"))
                                .child(
                                    div()
                                        .flex_1()
                                        .min_w_0()
                                        .overflow_hidden()
                                        .text_ellipsis()
                                        .whitespace_nowrap()
                                        .text_size(px(11.))
                                        .text_color(tok::text_tertiary())
                                        .child(for_name.unwrap_or_else(|| "Add an avatar".into())),
                                )
                                .child(ui::info("avatar-library-info", "Your avatars, in every workspace. Each keeps its framing, size and expression shortcuts. Right-click one to rename, duplicate or delete it (its file stays)."))
                                .child(
                                    Button::new("avatar-new-png")
                                        .icon(ui::icon("image"))
                                        .label("New PNGtuber…")
                                        .small()
                                        .ghost()
                                        .on_click(cx.listener(move |this, _, _, cx| {
                                            this.library_sheet = None;
                                            this.open_png_editor(None, target, cx)
                                        })),
                                )
                                .child(
                                    Button::new("avatar-add-file")
                                        .icon(ui::icon("plus"))
                                        .label("Add from file…")
                                        .small()
                                        .outline()
                                        .on_click(cx.listener(move |this, _, window, cx| this.add_avatar_from_file(target, window, cx))),
                                ),
                        )
                        .child(grid.vertical_scrollbar(&sheet.scroll)),
                )
                .into_any_element(),
        )
    }

    /// Opens the PNGtuber editor: a new one, or the one at `path`. Saved, it goes in the library,
    /// and on `source` (a new one), or `source` reads it again.
    pub(super) fn open_png_editor(&mut self, path: Option<PathBuf>, source: Option<Id>, cx: &mut Context<Self>) {
        self.open_png_editor_with(path, Vec::new(), source, cx)
    }

    /// `open_png_editor`, with pictures added at once (a check script's).
    pub(super) fn open_png_editor_with(
        &mut self,
        path: Option<PathBuf>,
        add: Vec<PathBuf>,
        source: Option<Id>,
        cx: &mut Context<Self>,
    ) {
        let workspace = self.studio.read(cx).workspace().clone();
        // the mixer's tracks, microphones first
        let mut tracks: Vec<(Id, String, bool)> = workspace
            .tracks
            .iter()
            .map(|t| {
                (
                    t.id,
                    t.name.clone(),
                    workspace.sources.get(&t.source).is_some_and(|i| crate::console::is_mic(&i.kind)),
                )
            })
            .collect();
        tracks.sort_by_key(|t| !t.2);
        let mic = source
            .and_then(|s| workspace.sources.get(&s))
            .and_then(|i| serde_json::from_value::<Id>(i.settings["mic"].clone()).ok());
        let me = cx.entity().downgrade();
        let on_saved: Rc<dyn Fn(PathBuf, &mut App)> = Rc::new(move |saved: PathBuf, cx: &mut App| {
            let Some(me) = me.upgrade() else { return };
            me.update(cx, |this, cx| this.png_saved(&saved, source, cx));
        });
        crate::pngtuber_editor::open(
            path,
            add,
            tracks.into_iter().map(|(id, n, _)| (id, n)).collect(),
            mic,
            on_saved,
            cx,
        );
    }

    /// A PNGtuber saved in its editor: in the library (its picture taken again), and on the avatar
    /// it was opened from (read again), or as a new avatar object.
    fn png_saved(&mut self, path: &Path, source: Option<Id>, cx: &mut Context<Self>) {
        let entry = self.avatar_library.for_file(path);
        if let Some(e) = self.avatar_library.get_mut(entry) {
            if let Ok(l) = lightair_pngtuber::file::read_path(path) {
                e.name = l.avatar.name;
            }
            e.picture = None;
        }
        self.avatar_pictures.remove(&entry);
        self.avatar_library.save();
        let workspace = self.studio.read(cx).workspace().clone();
        let file = path.to_string_lossy().into_owned();
        // the avatar it was opened from, and every other one showing this file: read again
        let target = source.and_then(|s| workspace.sources.get(&s)).filter(|i| i.kind == KIND).map(|i| i.id);
        let mic = self.default_mic(cx);
        let commands: Vec<Command> = workspace
            .sources
            .values()
            .filter(|i| i.kind == KIND && (Some(i.id) == target || i.settings["file"].as_str() == Some(file.as_str())))
            .map(|i| {
                let mut settings = i.settings.clone();
                settings["file"] = json!(file);
                settings["library"] = json!(entry);
                settings["revision"] = json!(settings["revision"].as_i64().unwrap_or(0) + 1);
                if settings["mic"].as_str().is_none_or(str::is_empty)
                    && let Some(t) = mic
                {
                    settings["mic"] = json!(t);
                }
                Command::SetSourceSettings { source: i.id, settings }
            })
            .collect();
        if commands.is_empty() {
            // a new one: on the canvas
            self.use_avatar(entry, None, cx);
        } else {
            self.exec(Command::Batch { label: "Save PNGtuber".into(), commands }, cx);
        }
    }

    /// The mic a new PNGtuber listens to: the mixer's first microphone, else its first track.
    pub(super) fn default_mic(&self, cx: &App) -> Option<Id> {
        let w = self.studio.read(cx).workspace();
        w.tracks
            .iter()
            .find(|t| w.sources.get(&t.source).is_some_and(|i| crate::console::is_mic(&i.kind)))
            .or(w.tracks.first())
            .map(|t| t.id)
    }

    // ------------------------------------------------------------------- pin to

    /// Opens the anchor picker: to pin `object`, or to add `file` as an accessory pinned where
    /// chosen.
    pub(super) fn open_anchor_sheet(
        &mut self,
        scene: Id,
        avatar_object: Id,
        avatar_source: Id,
        object: Option<Id>,
        file: Option<PathBuf>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let search = cx.new(|cx| InputState::new(window, cx).placeholder("Search"));
        search.update(cx, |s, cx| s.focus(window, cx));
        let events = cx.subscribe_in(&search, window, |_, _, ev: &InputEvent, _, cx| {
            if matches!(ev, InputEvent::Change) {
                cx.notify();
            }
        });
        // the avatar's anchors, fresh
        let engine = self.studio.read(cx).engine().clone();
        if let Some(status) = engine.call(move |s| s.reconciler.avatar_status(avatar_source)) {
            self.avatar_status.insert(avatar_source, status);
        }
        self.anchor_sheet =
            Some(AnchorSheet { scene, avatar_object, avatar_source, object, file, search, _events: events });
        cx.notify();
    }

    fn close_anchor_sheet(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.anchor_sheet = None;
        window.focus(&self.focus, cx);
        cx.notify();
    }

    /// Where an anchor is at rest now, in the avatar's picture.
    fn anchor_at_rest(
        &self,
        avatar_source: Id,
        key: Option<&str>,
        cx: &App,
    ) -> Option<lightair_obs::avatar_pins::Spot> {
        let key = key?.to_string();
        self.studio.read(cx).engine().call(move |e| e.reconciler.avatar_anchor_rest(avatar_source, &key, None))
    }

    /// Asks for a picture or video, then the anchor it goes on.
    pub(super) fn add_accessory(
        &mut self,
        avatar_object: Id,
        avatar_source: Id,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(scene) = self.studio.read(cx).selected_scene else { return };
        let picked = crate::file_pick::pick(crate::file_pick::ACCESSORY, cx);
        cx.spawn_in(window, async move |this, cx| {
            let Ok(Some(path)) = picked.await else { return };
            let _ = this.update_in(cx, |this, window, cx| {
                this.open_anchor_sheet(scene, avatar_object, avatar_source, None, Some(path), window, cx)
            });
        })
        .detach();
    }

    pub(super) fn choose_anchor(&mut self, key: Option<String>, window: &mut Window, cx: &mut Context<Self>) {
        let Some(sheet) = self.anchor_sheet.take() else { return };
        let workspace = self.studio.read(cx).workspace().clone();
        let Some(avatar) = workspace.sources.get(&sheet.avatar_source) else { return };
        match (sheet.object, sheet.file) {
            (Some(object), _) => {
                let at = self.anchor_at_rest(sheet.avatar_source, key.as_deref(), cx);
                let settings = with_pin(&avatar.settings, object, key.as_deref(), at);
                self.exec(Command::SetSourceSettings { source: sheet.avatar_source, settings }, cx);
            }
            (None, Some(file)) => {
                self.add_accessory_object(sheet.scene, sheet.avatar_object, sheet.avatar_source, &file, key, cx)
            }
            (None, None) => {}
        }
        window.focus(&self.focus, cx);
        cx.notify();
    }

    /// Adds `file` under the avatar object, on the anchor (or near the top of the avatar), about a
    /// quarter of its size, pinned there: one undo step.
    fn add_accessory_object(
        &mut self,
        scene: Id,
        avatar_object: Id,
        avatar_source: Id,
        file: &Path,
        key: Option<String>,
        cx: &mut Context<Self>,
    ) {
        let workspace = self.studio.read(cx).workspace().clone();
        let (Some(s), Some(avatar)) = (workspace.scene(scene), workspace.sources.get(&avatar_source)) else { return };
        let (aw, ah) = (
            avatar.settings["width"].as_f64().unwrap_or(1080.0) as f32,
            avatar.settings["height"].as_f64().unwrap_or(1080.0) as f32,
        );
        let engine = self.studio.read(cx).engine().clone();
        let rest =
            key.clone().and_then(|k| engine.call(move |e| e.reconciler.avatar_anchor_rest(avatar_source, &k, None)));
        let at = rest.map(|s| (s.x, s.y));
        let mut layouts = Vec::new();
        for canvas in &workspace.canvases {
            let (Some(host), Some(frame)) =
                (s.world(avatar_object, canvas.id), s.frame_of(Some(avatar_object), canvas.id))
            else {
                continue;
            };
            let size = host.width.min(host.height) * 0.28;
            let (x, y) = match at {
                Some((x, y)) => on_canvas(&Placement::of(&host), aw, ah, x, y),
                None => (host.x + host.width / 2.0, host.y + host.height * 0.3),
            };
            layouts.push((canvas.id, Layout::boxed(x - size / 2.0, y - size / 2.0, size, size).to_local(&frame)));
        }
        let name = file.file_stem().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "Accessory".into());
        let path = file.to_string_lossy();
        let source = if is_video(file) {
            Source::new(
                &name,
                "ffmpeg_source",
                json!({"local_file": path, "is_local_file": true, "looping": true, "restart_on_activate": true}),
            )
        } else {
            Source::new(&name, "image_source", json!({"file": path}))
        };
        let mut object = Object::new(&name, source.id, layouts);
        object.parent = Some(avatar_object);
        let object_id = object.id;
        let mut commands = vec![Command::AddSource { source }, Command::AddObject { scene, object, index: None }];
        if let Some(k) = key.as_deref() {
            commands.push(Command::SetSourceSettings {
                source: avatar_source,
                settings: with_pin(&avatar.settings, object_id, Some(k), rest),
            });
        }
        self.exec(Command::Batch { label: format!("Add {name}"), commands }, cx);
        self.select_object(Some(object_id), cx);
    }

    /// The Inspector's "Pin to" section, for an object whose parent is an avatar.
    pub(super) fn pin_section(
        &self,
        scene: Id,
        object: Id,
        avatar_object: Id,
        avatar: &Source,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let avatar_source = avatar.id;
        let key = pin_of(&avatar.settings, object).unwrap_or_default();
        let anchors = self.avatar_status.get(&avatar_source).map(|s| s.anchors.clone()).unwrap_or_default();
        let (value, detail) = if key.is_empty() {
            ("Not pinned".to_string(), String::new())
        } else {
            match anchors.iter().find(|a| a.key == key && a.group != "Suggested") {
                Some(a) => (
                    a.label.clone(),
                    match (a.label == a.key, a.label == a.group) {
                        (true, _) => a.group.clone(),
                        (false, true) => a.key.clone(),
                        (false, false) => format!("{} · {}", a.group, a.key),
                    },
                ),
                None => (key.clone(), "Not in this model".to_string()),
            }
        };
        let vrm = lib::kind_of(avatar.settings["file"].as_str().unwrap_or("")) == "VRM";
        let tip = if vrm {
            "It follows that bone: where you place it with the avatar at rest is where it stays on the bone."
        } else {
            "It follows that part of the model: where you place it with the avatar at rest is where it stays on it."
        };
        let pinned = !key.is_empty();
        let unpin = Command::SetSourceSettings {
            source: avatar_source,
            settings: with_pin(&avatar.settings, object, None, None),
        };
        self.section(
            "Pin to",
            Some(
                div()
                    .flex()
                    .items_center()
                    .gap_1()
                    .when(pinned, |d| {
                        d.child(
                            Button::new("unpin")
                                .label("Unpin")
                                .xsmall()
                                .ghost()
                                .on_click(cx.listener(move |this, _, _, cx| this.exec(unpin.clone(), cx))),
                        )
                    })
                    .child(ui::info("pin-info", tip))
                    .into_any_element(),
            ),
            cx,
        )
        .child(
            self.source_chip(
                "pin-anchor",
                if vrm { "bone" } else { "pin" },
                value,
                detail,
                if pinned { "Change" } else { "Choose" },
            )
            .on_click(cx.listener(move |this, _, window, cx| {
                this.open_anchor_sheet(scene, avatar_object, avatar_source, Some(object), None, window, cx)
            })),
        )
        .into_any_element()
    }

    pub(super) fn anchor_sheet_overlay(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let sheet = self.anchor_sheet.as_ref()?;
        let workspace = self.studio.read(cx).workspace().clone();
        let avatar = workspace.sources.get(&sheet.avatar_source)?;
        let vrm = lib::kind_of(avatar.settings["file"].as_str().unwrap_or("")) == "VRM";
        let query = sheet.search.read(cx).value().to_lowercase();
        let current = sheet.object.and_then(|o| pin_of(&avatar.settings, o));
        let what = match (&sheet.object, &sheet.file) {
            (Some(o), _) => {
                workspace.scene(sheet.scene).and_then(|s| s.object(*o)).map(|o| o.name.clone()).unwrap_or_default()
            }
            (None, Some(f)) => f.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(),
            _ => String::new(),
        };
        let status = self.avatar_status.get(&sheet.avatar_source).cloned().unwrap_or_default();
        let anchors: Vec<_> = status
            .anchors
            .iter()
            .filter(|a| {
                query.is_empty()
                    || a.label.to_lowercase().contains(&query)
                    || a.key.to_lowercase().contains(&query)
                    || a.group.to_lowercase().contains(&query)
            })
            .cloned()
            .collect();
        let row = |id: (&'static str, usize),
                   title: String,
                   detail: String,
                   on: bool,
                   key: Option<String>,
                   cx: &mut Context<Self>| {
            div()
                .id(id)
                .flex()
                .items_center()
                .gap_2()
                .px_2()
                .py_1()
                .rounded(px(7.))
                .cursor_pointer()
                .when(on, |d| d.bg(tok::bg_selected()))
                .hover(|d| d.bg(tok::bg_selected()))
                .child(div().flex_1().min_w_0().overflow_hidden().text_ellipsis().whitespace_nowrap().child(title))
                .when(!detail.is_empty(), |d| {
                    d.child(
                        div()
                            .flex_none()
                            .max_w(px(220.))
                            .overflow_hidden()
                            .text_ellipsis()
                            .whitespace_nowrap()
                            .text_size(px(11.))
                            .font_family(crate::ui::MONO)
                            .text_color(tok::text_tertiary())
                            .child(detail),
                    )
                })
                .when(on, |d| d.child(ui::icon("check").size(px(13.)).text_color(tok::accent_primary())))
                .on_click(cx.listener(move |this, _, window, cx| this.choose_anchor(key.clone(), window, cx)))
        };
        let mut body =
            div().id("anchor-body").flex().flex_col().gap_0p5().flex_shrink(1.).min_h_0().overflow_y_scroll();
        let none_label = if sheet.object.is_some() { "Not pinned" } else { "Don't pin it" };
        body = body.child(row(
            ("anchor-none", 0),
            none_label.into(),
            String::new(),
            sheet.object.is_some() && current.is_none(),
            None,
            cx,
        ));
        if !status.loaded {
            body = body
                .child(div().p_4().text_center().text_color(tok::text_tertiary()).child("The model is still loading…"));
        } else if anchors.is_empty() {
            body = body.child(div().p_4().text_center().text_color(tok::text_tertiary()).child("Nothing found"));
        }
        let mut group = String::new();
        for (i, a) in anchors.into_iter().enumerate() {
            if a.group != group {
                group = a.group.clone();
                body = body.child(
                    div()
                        .pt_2()
                        .px_2()
                        .text_size(px(11.))
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(tok::text_tertiary())
                        .child(group.to_uppercase()),
                );
            }
            let on = current.as_deref() == Some(a.key.as_str());
            let detail = if a.label == a.key { String::new() } else { a.key.clone() };
            body = body.child(row(("anchor", i), a.label.clone(), detail, on, Some(a.key.clone()), cx));
        }
        let title = match (vrm, sheet.file.is_some()) {
            (true, false) => "Pin to a bone",
            (false, false) => "Pin to a part",
            (true, true) => "Which bone does it go on?",
            (false, true) => "Which part does it go on?",
        };
        Some(
            div()
                .absolute()
                .inset_0()
                .flex()
                .justify_center()
                .items_start()
                .pt(px(80.))
                .bg(tok::scrim())
                .occlude()
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _, window, cx| this.close_anchor_sheet(window, cx)),
                )
                .child(
                    div()
                        .id("anchor-picker")
                        .flex()
                        .flex_col()
                        .gap_2()
                        .w(px(480.))
                        .max_h(px(560.))
                        .p_3()
                        .rounded(px(14.))
                        .bg(tok::bg_panel())
                        .border_1()
                        .border_color(tok::border_strong())
                        .shadow_lg()
                        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                        .capture_key_down(cx.listener(|this, ev: &KeyDownEvent, window, cx| {
                            if ev.keystroke.key == "escape" {
                                this.close_anchor_sheet(window, cx);
                                cx.stop_propagation();
                            }
                        }))
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .justify_between()
                                .gap_3()
                                .child(
                                    div().flex_none().text_size(px(15.)).font_weight(FontWeight::SEMIBOLD).child(title),
                                )
                                .child(
                                    div()
                                        .min_w_0()
                                        .overflow_hidden()
                                        .text_ellipsis()
                                        .whitespace_nowrap()
                                        .text_size(px(11.))
                                        .text_color(tok::text_tertiary())
                                        .child(format!("for {what}")),
                                ),
                        )
                        .child(TextInput::new(&sheet.search))
                        .child(body),
                )
                .into_any_element(),
        )
    }
}

/// Every PNGtuber's push to talk keys: (avatar source, keys), in a steady order.
pub(super) fn push_to_talk_keys(workspace: &lightair_model::Workspace) -> Vec<(Id, String)> {
    let mut out: Vec<(Id, String)> = workspace
        .sources
        .values()
        .filter(|i| i.kind == KIND)
        .filter_map(|i| {
            i.settings["push_to_talk_keys"]
                .as_str()
                .filter(|k| crate::shortcuts::parse(k).is_some())
                .map(|k| (i.id, k.to_string()))
        })
        .collect();
    out.sort_by_key(|(s, _)| s.to_string());
    out
}

/// Every avatar's expression shortcuts: (avatar source, expression, keys), in a steady order.
pub(super) fn expression_keys(workspace: &lightair_model::Workspace) -> Vec<(Id, String, String)> {
    let mut out: Vec<(Id, String, String)> = workspace
        .sources
        .values()
        .filter(|i| i.kind == KIND)
        .flat_map(|i| {
            i.settings["expression_keys"].as_object().into_iter().flatten().filter_map(move |(e, k)| {
                k.as_str().filter(|k| crate::shortcuts::parse(k).is_some()).map(|k| (i.id, e.clone(), k.to_string()))
            })
        })
        .collect();
    out.sort_by(|a, b| (a.0.to_string(), &a.1).cmp(&(b.0.to_string(), &b.1)));
    out
}

/// The pictures taken before they kept the avatar's shape: the canvas's 16:9 at 320 wide, the
/// avatar stretched into it. Retaken when the library opens.
const OLD_PICTURE: (u32, u32) = (320, 180);

/// The size a library picture is drawn at: the avatar's own shape, its long side 360.
#[cfg_attr(not(feature = "libobs"), allow(dead_code))]
fn picture_size(w: u32, h: u32) -> (u32, u32) {
    let (w, h) = (w.max(1) as f32, h.max(1) as f32);
    let k = 360.0 / w.max(h);
    ((w * k).round() as u32, (h * k).round() as u32)
}

/// A frame as a PNG picture for the library: at most 360 wide, cut to what's drawn when the
/// background is see-through.
#[cfg_attr(not(feature = "libobs"), allow(dead_code))]
fn picture_png(frame: &lightair_preview::Frame) -> Option<Vec<u8>> {
    let (w, h, rgba) = lightair_preview::frame_rgba(frame, 360)?;
    png_of(image::RgbaImage::from_raw(w as u32, h as u32, rgba)?)
}

/// No library pictures of avatars here yet without libobs: the compositor's avatar renderer runs
/// on macOS only.
#[cfg(all(not(feature = "libobs"), not(target_os = "macos")))]
fn picture_at_rest(_settings: &Value) -> Option<Vec<u8>> {
    None
}

/// An avatar's model (`settings`: an avatar source's) drawn at rest by the compositor's renderer,
/// as a library picture; `None` when it doesn't load (in 20 seconds). Off the UI thread.
#[cfg(all(not(feature = "libobs"), target_os = "macos"))]
fn picture_at_rest(settings: &Value) -> Option<Vec<u8>> {
    use lightair_render::media::Picture;
    let mut r = lightair_preview::avatars::AvatarRenderer::new(settings).ok()?;
    let start = Instant::now();
    loop {
        let status = r.status();
        if !status.error.is_empty() || start.elapsed() > Duration::from_secs(20) {
            return None;
        }
        if status.loaded {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    // a few frames, settled
    let mut shot = None;
    for _ in 0..8 {
        shot = r.draw_at_rest().or(shot);
        std::thread::sleep(Duration::from_millis(16));
    }
    let Picture::Texture(texture) = shot?.picture else { return None };
    let (w, h) = (texture.width(), texture.height());
    let rgba = lightair_render::compose::read_texture(&texture)?;
    let img = image::RgbaImage::from_raw(w, h, rgba)?;
    let k = 360.0 / w.max(h) as f32;
    let img = if k < 1.0 {
        image::imageops::resize(
            &img,
            (w as f32 * k) as u32,
            (h as f32 * k) as u32,
            image::imageops::FilterType::Triangle,
        )
    } else {
        img
    };
    png_of(img)
}

/// A picture cut to what's drawn (where the background is see-through), as a PNG.
fn png_of(img: image::RgbaImage) -> Option<Vec<u8>> {
    let (w, h) = img.dimensions();
    let mut img = img;
    let (mut x0, mut y0, mut x1, mut y1) = (u32::MAX, u32::MAX, 0, 0);
    for (x, y, p) in img.enumerate_pixels() {
        if p[3] > 8 {
            x0 = x0.min(x);
            y0 = y0.min(y);
            x1 = x1.max(x);
            y1 = y1.max(y);
        }
    }
    if x0 < x1 && y0 < y1 && (x1 - x0 + 1, y1 - y0 + 1) != (w, h) {
        img = image::imageops::crop_imm(&img, x0, y0, x1 - x0 + 1, y1 - y0 + 1).to_image();
    }
    let mut png = std::io::Cursor::new(Vec::new());
    image::DynamicImage::ImageRgba8(img).write_to(&mut png, image::ImageFormat::Png).ok()?;
    Some(png.into_inner())
}
