//! Export as package: the selected objects, or a scene, written out as a `.airpkg`. A scene (and
//! a selection of objects) becomes a template: its `scene.json` with the media it uses copied
//! beside it, the show's paths to them made relative; a widget with its own code becomes a
//! widget, in the files a widget folder is read from. The sheet asks for the package's name,
//! version, author and licence; the manifest is written from them.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input as TextInput, InputState};
use gpui_kit::component::{Disableable, Sizable};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;
use lightair_model::{Clip, Id, SceneAsset, Workspace};
use lightair_packages::Kind;
use lightair_packages::export::{ExportItem, ExportSpec};
use serde_json::{Value, json};

use super::StudioWindow;
use crate::packages as pk;
use crate::ui::{self, tok};

/// What's being exported.
#[derive(Clone, Debug)]
pub(super) enum ExportFrom {
    /// Objects of a scene.
    Objects(Id, Vec<Id>),
    Scene(Id),
}

pub(super) struct ExportSheet {
    items: Vec<ExportItem>,
    name: Entity<InputState>,
    version: Entity<InputState>,
    author: Entity<InputState>,
    licence: Entity<InputState>,
    error: Option<String>,
    busy: bool,
}

fn file_name(p: &Path) -> String {
    p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
}

/// The paths a template's `scene.json` gives its media: each file the show points at (an
/// absolute path to a file that exists) copied in as `media/<name>`, names kept apart; a Live2D
/// model with its whole folder.
struct Media {
    /// Each file or model folder taken, and where it goes in the item.
    taken: HashMap<PathBuf, String>,
    files: Vec<(String, PathBuf)>,
}

impl Media {
    fn free(&self, name: &str) -> String {
        let used = |n: &str| self.taken.values().any(|v| v == n || v.starts_with(&format!("{n}/")));
        let first = format!("media/{name}");
        if !used(&first) {
            return first;
        }
        let (stem, ext) = match name.rsplit_once('.') {
            Some((s, e)) if !s.is_empty() => (s.to_string(), format!(".{e}")),
            _ => (name.to_string(), String::new()),
        };
        (2..).map(|n| format!("media/{stem} {n}{ext}")).find(|c| !used(c)).unwrap_or(first)
    }

    /// Where `path` goes in the item (copied in on first sight).
    fn take(&mut self, path: &Path) -> String {
        let live2d = file_name(path).to_lowercase().ends_with(".model3.json");
        if live2d && let Some(folder) = path.parent() {
            let rel = match self.taken.get(folder) {
                Some(r) => r.clone(),
                None => {
                    let rel = self.free(&file_name(folder));
                    for (inner, file) in files_under(folder) {
                        self.files.push((format!("{rel}/{inner}"), file));
                    }
                    self.taken.insert(folder.to_path_buf(), rel.clone());
                    rel
                }
            };
            return format!("{rel}/{}", file_name(path));
        }
        if let Some(r) = self.taken.get(path) {
            return r.clone();
        }
        let rel = self.free(&file_name(path));
        self.files.push((rel.clone(), path.to_path_buf()));
        self.taken.insert(path.to_path_buf(), rel.clone());
        rel
    }

    /// Takes the media `value` points at and rewrites those paths; drops package marks (the
    /// export is a package of its own).
    fn rewrite(&mut self, value: &mut Value) {
        match value {
            Value::String(s) => {
                let p = Path::new(s.as_str());
                if p.is_absolute() && p.is_file() {
                    let path = p.to_path_buf();
                    *s = self.take(&path);
                }
            }
            Value::Array(a) => a.iter_mut().for_each(|v| self.rewrite(v)),
            Value::Object(m) => {
                m.remove(pk::MARK);
                m.values_mut().for_each(|v| self.rewrite(v));
            }
            _ => {}
        }
    }
}

/// Every file in `dir` and the folders under it (hidden ones left out), by its path inside.
fn files_under(dir: &Path) -> Vec<(String, PathBuf)> {
    fn walk(dir: &Path, prefix: &str, depth: usize, out: &mut Vec<(String, PathBuf)>) {
        let Ok(entries) = std::fs::read_dir(dir) else { return };
        for e in entries.flatten() {
            let p = e.path();
            let name = file_name(&p);
            if name.starts_with('.') {
                continue;
            }
            let rel = if prefix.is_empty() { name } else { format!("{prefix}/{name}") };
            if p.is_dir() {
                if depth < 16 {
                    walk(&p, &rel, depth + 1, out);
                }
            } else if p.is_file() {
                out.push((rel, p));
            }
        }
    }
    let mut out = Vec::new();
    walk(dir, "", 0, &mut out);
    out
}

/// A scene (or objects as one) as a template item: `scene.json` and its media.
fn template_item(asset: &SceneAsset) -> Option<ExportItem> {
    let mut value = serde_json::to_value(asset).ok()?;
    let mut media = Media { taken: HashMap::new(), files: Vec::new() };
    media.rewrite(&mut value);
    let mut item = ExportItem::new(Kind::Template, &asset.name).json("scene.json", &value);
    for (rel, from) in media.files {
        item = item.copy(&rel, &from);
    }
    Some(item)
}

/// A widget with its own code as a widget item (and its size, as a `.lwidget` keeps it).
fn widget_item(name: &str, settings: &Value) -> Option<ExportItem> {
    let custom = &settings["widget"]["custom"];
    if !custom.is_object() {
        return None;
    }
    let text = |k: &str| custom[k].as_str().unwrap_or("").to_string();
    let size = [settings["width"].as_u64().unwrap_or(800), settings["height"].as_u64().unwrap_or(600)];
    Some(
        ExportItem::widget(name, &text("html"), &text("css"), &text("js"), &text("fields")).json(
            "manifest.json",
            &json!({"name": name, "version": "1.0.0", "size": size, "format": "lightair-widget/1"}),
        ),
    )
}

/// What `from` exports, and the name the package starts with.
fn items_of(workspace: &Workspace, from: &ExportFrom) -> (Vec<ExportItem>, String) {
    match from {
        ExportFrom::Scene(scene) => {
            let name = workspace.scene(*scene).map(|s| s.name.clone()).unwrap_or_default();
            let items = SceneAsset::of(workspace, *scene).and_then(|a| template_item(&a)).into_iter().collect();
            (items, name)
        }
        ExportFrom::Objects(scene_id, ids) => {
            let Some(scene) = workspace.scene(*scene_id) else { return (vec![], String::new()) };
            let mut items = Vec::new();
            let mut rest = Vec::new();
            for id in ids {
                let Some(object) = scene.object(*id) else { continue };
                let widget = object
                    .source
                    .and_then(|s| workspace.sources.get(&s))
                    .filter(|s| s.kind == lightair_obs::web::KIND)
                    .and_then(|s| widget_item(&object.name, &s.settings));
                match widget {
                    Some(item) => items.push(item),
                    None => rest.push(*id),
                }
            }
            let first = ids.first().and_then(|id| scene.object(*id)).map(|o| o.name.clone()).unwrap_or_default();
            if !rest.is_empty() {
                let name = rest.first().and_then(|id| scene.object(*id)).map(|o| o.name.clone()).unwrap_or_default();
                let clip = Clip::copy(workspace, *scene_id, &rest);
                if let Some(item) = clip.and_then(|clip| template_item(&SceneAsset { name, clip })) {
                    items.insert(0, item);
                }
            }
            (items, first)
        }
    }
}

fn field(label: &'static str, input: &Entity<InputState>) -> Div {
    div()
        .flex()
        .flex_col()
        .gap_1()
        .flex_1()
        .min_w_0()
        .child(div().text_size(px(11.)).text_color(tok::text_secondary()).child(label))
        .child(TextInput::new(input).small())
}

impl StudioWindow {
    /// Opens the Export sheet for `from`.
    pub(super) fn open_package_export(&mut self, from: ExportFrom, window: &mut Window, cx: &mut Context<Self>) {
        let workspace = self.studio.read(cx).workspace().clone();
        let (items, name) = items_of(&workspace, &from);
        if items.is_empty() {
            self.studio.update(cx, |s, cx| {
                s.issue = Some("Nothing there to export".into());
                cx.notify();
            });
            return;
        }
        let input = |placeholder: &'static str, value: String, window: &mut Window, cx: &mut Context<Self>| {
            cx.new(|cx| InputState::new(window, cx).placeholder(placeholder).default_value(value))
        };
        let name = input("Name", name, window, cx);
        let version = input("1.0.0", "1.0.0".into(), window, cx);
        let author = input("Optional", String::new(), window, cx);
        let licence = input("e.g. CC-BY-4.0", String::new(), window, cx);
        name.update(cx, |s, cx| s.focus(window, cx));
        self.packages.export = Some(ExportSheet { items, name, version, author, licence, error: None, busy: false });
        cx.notify();
    }

    /// Export as package from the menu bar: the selected objects, or else the selected scene.
    pub(super) fn export_selection_as_package(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let studio = self.studio.read(cx);
        let Some(scene) = studio.selected_scene else { return };
        let selection = studio.selection();
        let from = if selection.is_empty() { ExportFrom::Scene(scene) } else { ExportFrom::Objects(scene, selection) };
        self.open_package_export(from, window, cx);
    }

    fn close_package_export(&mut self, cx: &mut Context<Self>) {
        if self.packages.export.as_ref().is_some_and(|s| !s.busy) {
            self.packages.export = None;
            cx.notify();
        }
    }

    /// Asks where to write the package, then writes it.
    fn export_package(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(sheet) = &mut self.packages.export else { return };
        let value = |i: &Entity<InputState>| i.read(cx).value().trim().to_string();
        let name = value(&sheet.name);
        if name.is_empty() {
            sheet.error = Some("Give the package a name".into());
            return cx.notify();
        }
        let Ok(version) = semver::Version::parse(&value(&sheet.version)) else {
            sheet.error = Some("The version is three numbers, such as 1.0.0".into());
            return cx.notify();
        };
        let mut spec = ExportSpec::new(&name);
        spec.version = version;
        spec.author = value(&sheet.author);
        spec.licence = value(&sheet.licence);
        let items = sheet.items.clone();
        sheet.error = None;
        let desktop = std::env::home_dir().unwrap_or_default().join("Desktop");
        let file = format!("{}.{}", name.replace(['/', '\\', ':'], "-"), lightair_packages::ARCHIVE_EXTENSION);
        let path = cx.prompt_for_new_path(&desktop, Some(&file));
        cx.spawn_in(window, async move |this, cx| {
            let Ok(Ok(Some(mut path))) = path.await else { return };
            if !path.extension().is_some_and(|e| e.eq_ignore_ascii_case(lightair_packages::ARCHIVE_EXTENSION)) {
                path.set_extension(lightair_packages::ARCHIVE_EXTENSION);
            }
            let _ = this.update(cx, |this, cx| {
                if let Some(s) = &mut this.packages.export {
                    s.busy = true;
                }
                cx.notify();
            });
            let out = path.clone();
            let result = cx
                .background_executor()
                .spawn(async move { lightair_packages::export::write_package_file(&spec, &items, &out) })
                .await;
            let _ = this.update(cx, |this, cx| {
                match result {
                    Ok(_) => {
                        this.packages.export = None;
                        this.studio.update(cx, |s, cx| {
                            s.issue = Some(format!("Exported {}", file_name(&path)).into());
                            cx.notify();
                        });
                    }
                    Err(e) => {
                        if let Some(s) = &mut this.packages.export {
                            s.busy = false;
                            let text = e.to_string();
                            let mut c = text.chars();
                            s.error = Some(
                                c.next().map(|f| f.to_uppercase().collect::<String>() + c.as_str()).unwrap_or_default(),
                            );
                        }
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    pub(super) fn package_export_overlay(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let sheet = self.packages.export.as_ref()?;
        let mut contents = div().flex().flex_col().gap_1();
        for (i, item) in sheet.items.iter().enumerate() {
            let files = item.files.len();
            contents = contents.child(
                div()
                    .id(("export-item", i))
                    .flex()
                    .items_center()
                    .gap_2()
                    .h(px(28.))
                    .px_2()
                    .rounded(px(6.))
                    .bg(tok::bg_raised())
                    .child(ui::icon(pk::kind_icon(&item.kind)).size(px(14.)).text_color(tok::text_secondary()))
                    .child(div().flex_1().min_w_0().truncate().text_size(px(12.)).child(item.name.clone()))
                    .child(
                        div()
                            .flex_none()
                            .text_size(px(11.))
                            .text_color(tok::text_tertiary())
                            .child(pk::kind_label(&item.kind)),
                    )
                    .child(
                        div().flex_none().text_size(px(11.)).text_color(tok::text_tertiary()).child(if files == 1 {
                            "1 file".to_string()
                        } else {
                            format!("{files} files")
                        }),
                    ),
            );
        }
        let busy = sheet.busy;
        let section = |title: &'static str| {
            div().text_size(px(11.)).font_weight(FontWeight::SEMIBOLD).text_color(tok::text_secondary()).child(title)
        };
        Some(
            div()
                .absolute()
                .inset_0()
                .flex()
                .justify_center()
                .items_center()
                .bg(tok::scrim())
                .occlude()
                .on_mouse_down(MouseButton::Left, cx.listener(|this, _, _, cx| this.close_package_export(cx)))
                .child(
                    div()
                        .id("package-export")
                        .flex()
                        .flex_col()
                        .gap_4()
                        .w(px(440.))
                        .max_h(px(620.))
                        .overflow_y_scroll()
                        .p_5()
                        .rounded(px(14.))
                        .bg(tok::bg_panel())
                        .border_1()
                        .border_color(tok::border_strong())
                        .shadow_lg()
                        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                        .child(div().text_size(px(16.)).font_weight(FontWeight::SEMIBOLD).child("Export as package"))
                        .child(div().flex().flex_col().gap_2().child(section("CONTENTS")).child(contents))
                        .child(
                            div()
                                .flex()
                                .flex_col()
                                .gap_3()
                                .child(
                                    div()
                                        .flex()
                                        .gap_3()
                                        .child(field("Name", &sheet.name))
                                        .child(div().w(px(110.)).child(field("Version", &sheet.version))),
                                )
                                .child(
                                    div()
                                        .flex()
                                        .gap_3()
                                        .child(field("Author", &sheet.author))
                                        .child(field("Licence", &sheet.licence)),
                                ),
                        )
                        .when_some(sheet.error.clone(), |d, e| {
                            d.child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap_1p5()
                                    .text_size(px(12.))
                                    .text_color(tok::error())
                                    .child(ui::icon("circle-alert").size(px(13.)))
                                    .child(div().flex_1().child(e)),
                            )
                        })
                        .child(
                            div()
                                .flex()
                                .justify_end()
                                .gap_2()
                                .child(
                                    Button::new("export-cancel")
                                        .label("Cancel")
                                        .small()
                                        .ghost()
                                        .disabled(busy)
                                        .on_click(cx.listener(|this, _, _, cx| this.close_package_export(cx))),
                                )
                                .child(
                                    Button::new("export-go")
                                        .label(if busy { "Exporting…" } else { "Export…" })
                                        .small()
                                        .primary()
                                        .disabled(busy)
                                        .on_click(cx.listener(|this, _, window, cx| this.export_package(window, cx))),
                                ),
                        ),
                )
                .into_any_element(),
        )
    }
}
