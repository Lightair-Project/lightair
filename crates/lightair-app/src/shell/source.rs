//! The Inspector's Source section: which camera, display, window, app, microphone or file a
//! layer shows. Changing it changes the input, so it changes in every scene that uses it.

use std::path::PathBuf;

use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;
use lightair_core::{Command, Id, Input};
use serde_json::{Value, json};

use super::StudioWindow;
use crate::ui::{self, tok};

/// The list setting that picks the source for an input kind (screen capture depends on its mode).
fn device_key(kind: &str, settings: &Value) -> Option<&'static str> {
    match kind {
        "macos-avcapture" | "macos-avcapture-fast" | "av_capture_input_v2" | "av_capture_input" => Some("device"),
        "screen_capture" => Some(match settings["type"].as_i64().unwrap_or(0) {
            1 => "window",
            2 => "application",
            _ => "display_uuid",
        }),
        "coreaudio_input_capture" | "coreaudio_output_capture" => Some("device_id"),
        _ => None,
    }
}

fn file_key(kind: &str) -> Option<&'static str> {
    match kind {
        "image_source" => Some("file"),
        "ffmpeg_source" => Some("local_file"),
        _ => None,
    }
}

fn merged(settings: &Value, key: &str, value: Value) -> Value {
    let mut s = if settings.is_object() { settings.clone() } else { json!({}) };
    s[key] = value;
    s
}

impl StudioWindow {
    fn source_options(
        &self,
        input: Id,
        key: &'static str,
        refresh: bool,
        cx: &mut Context<Self>,
    ) -> Vec<(String, Value)> {
        let cache_key = (input, key);
        if !refresh && let Some(list) = self.source_cache.borrow().get(&cache_key) {
            return list.clone();
        }
        let list = self.studio.read(cx).engine().call(move |s| s.reconciler.input_options(input, key));
        self.source_cache.borrow_mut().insert(cache_key, list.clone());
        list
    }

    fn replace_file(&mut self, input: Id, key: &'static str, window: &mut Window, cx: &mut Context<Self>) {
        let paths =
            cx.prompt_for_paths(PathPromptOptions { files: true, directories: false, multiple: false, prompt: None });
        cx.spawn_in(window, async move |this, cx| {
            let Ok(Ok(Some(paths))) = paths.await else { return };
            let Some(path): Option<PathBuf> = paths.into_iter().next() else { return };
            let _ = this.update(cx, |this, cx| {
                let project = this.studio.read(cx).project().clone();
                let Some(i) = project.inputs.get(&input) else { return };
                let settings = merged(&i.settings, key, json!(path.to_string_lossy()));
                this.exec(Command::SetInputSettings { input, settings }, cx);
            });
        })
        .detach();
    }

    /// The Source section for an input, if its kind has a choosable source.
    pub(super) fn source_section(&self, input: &Input, cx: &mut Context<Self>) -> Option<AnyElement> {
        let id = input.id;
        if let Some(key) = file_key(&input.kind) {
            let path = input.settings[key].as_str().unwrap_or("");
            let name =
                std::path::Path::new(path).file_name().map_or("No file".into(), |n| n.to_string_lossy().into_owned());
            return Some(
                self.section("Source", None, cx)
                    .child(
                        self.chip("source-file", ui::input_icon(&input.kind), name, "Replace…")
                            .on_click(cx.listener(move |this, _, window, cx| this.replace_file(id, key, window, cx))),
                    )
                    .into_any_element(),
            );
        }
        let key = device_key(&input.kind, &input.settings)?;
        let current = input.settings[key].clone();
        let open = self.source_menu == Some(id);
        let options = self.source_options(id, key, false, cx);
        let label = options.iter().find(|(_, v)| *v == current).map(|(l, _)| l.clone()).unwrap_or_else(|| {
            if current.is_null() || current == json!("") { "Choose…".into() } else { "Not connected".into() }
        });

        let mut col = self.section("Source", None, cx);
        // Screen capture: what to capture first.
        if input.kind == "screen_capture" {
            let mode = input.settings["type"].as_i64().unwrap_or(0);
            col = col.child(
                self.segmented(
                    "capture-mode",
                    [(0, "Display"), (1, "Window"), (2, "App")]
                        .into_iter()
                        .map(|(m, label)| {
                            (
                                label,
                                Command::SetInputSettings {
                                    input: id,
                                    settings: merged(&input.settings, "type", json!(m)),
                                },
                                mode == m,
                            )
                        })
                        .collect(),
                    cx,
                ),
            );
        }
        col = col.child(
            self.chip("source-device", ui::input_icon(&input.kind), label, if open { "Close" } else { "Change" })
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.source_menu = if this.source_menu == Some(id) { None } else { Some(id) };
                    if this.source_menu.is_some() {
                        this.source_options(id, key, true, cx);
                    }
                    cx.notify();
                })),
        );
        if open {
            let mut list = div().id("source-list").flex().flex_col().gap_0p5().max_h(px(240.)).overflow_y_scroll();
            if options.is_empty() {
                list = list.child(div().px_2().py_1().text_color(tok::text_tertiary()).child("Nothing found"));
            }
            for (i, (name, value)) in options.into_iter().enumerate() {
                let selected = value == current;
                let settings = merged(&input.settings, key, value);
                list = list.child(
                    div()
                        .id(("source-option", i))
                        .flex()
                        .items_center()
                        .gap_2()
                        .h(px(28.))
                        .px_2()
                        .rounded(px(5.))
                        .cursor_pointer()
                        .text_size(px(12.))
                        .when(selected, |d| d.bg(tok::bg_selected()))
                        .hover(|d| d.bg(tok::bg_raised()))
                        .child(div().flex_1().min_w_0().overflow_hidden().text_ellipsis().child(name))
                        .when(selected, |d| d.child(ui::icon("check").size(px(12.)).text_color(tok::text_secondary())))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.source_menu = None;
                            this.exec(Command::SetInputSettings { input: id, settings: settings.clone() }, cx);
                        })),
                );
            }
            col = col.child(list);
        }
        Some(col.into_any_element())
    }

    /// A full-width row: icon, value, and an action word.
    fn chip(&self, id: &'static str, icon: &'static str, value: String, action: &'static str) -> Stateful<Div> {
        div()
            .id(id)
            .flex()
            .items_center()
            .gap_2()
            .h(px(32.))
            .px_2()
            .rounded(px(6.))
            .bg(tok::bg_raised())
            .cursor_pointer()
            .hover(|d| d.bg(tok::bg_selected()))
            .child(ui::icon(icon).size(px(14.)).text_color(tok::text_secondary()))
            .child(div().flex_1().min_w_0().overflow_hidden().text_ellipsis().text_size(px(12.)).child(value))
            .child(div().text_size(px(11.)).text_color(tok::text_tertiary()).child(action))
    }
}
