//! The Inspector's Source section and its picker: which camera, display, window, app,
//! microphone or file a layer shows. Changing it changes the input, so it changes in every scene
//! that uses it.

use std::path::PathBuf;

use gpui_kit::component::input::{Input as TextInput, InputEvent, InputState};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;
use lightair_core::{Command, Id, Input, kinds};
use serde_json::{Value, json};

use super::StudioWindow;
use crate::ui::{self, tok};

/// The open source picker.
pub(super) struct SourcePicker {
    input: Id,
    search: Entity<InputState>,
    _events: Subscription,
}

/// The setting that picks the source for an input kind (screen capture depends on its mode).
pub(super) fn device_key(kind: &str, settings: &Value) -> Option<&'static str> {
    kinds::device_key(kind, settings)
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

/// Apps whose windows are never what you want to capture (and Lightair itself: a mirror).
const HIDDEN_APPS: &[&str] = &[
    "Dock",
    "Window Server",
    "Control Center",
    "Control Centre",
    "Notification Center",
    "Notification Centre",
    "SystemUIServer",
    "Wallpaper",
    "Spotlight",
    "TextInputMenuAgent",
    "Lightair",
    "lightair",
    // Windows
    "Program Manager",
    "explorer.exe",
    "TextInputHost.exe",
    "ApplicationFrameHost.exe",
    "lightair.exe",
];

/// "[App] Title" (macOS) or "[app.exe]: Title" (Windows): the app and the window's title.
fn app_and_title(label: &str) -> (String, String) {
    match label.strip_prefix('[').and_then(|r| r.split_once(']')) {
        Some((app, title)) => (app.trim().to_string(), title.trim_start_matches(':').trim().to_string()),
        None => (String::new(), label.to_string()),
    }
}

/// One choice, split for display: what it is (title), what it belongs to or measures (detail).
#[derive(Clone)]
pub(super) struct Choice {
    pub title: String,
    pub detail: String,
    pub group: String,
    pub value: Value,
}

/// Turns libobs's labels into titles: "[App] Window title" for windows, "Name: WxH @ x,y" for
/// displays. Drops placeholders and system windows. (`key` is the portable setting.)
pub(super) fn choices(key: &str, options: &[(String, Value)]) -> Vec<Choice> {
    let mut out = Vec::new();
    for (label, value) in options {
        let label = label.trim();
        if label.is_empty() {
            continue;
        }
        let choice = match key {
            "window" => {
                let (app, title) = app_and_title(label);
                if HIDDEN_APPS.contains(&app.as_str()) || title.is_empty() {
                    continue;
                }
                Choice { title, detail: app.clone(), group: app, value: value.clone() }
            }
            "display" => {
                let (name, rest) = label.split_once(": ").unwrap_or((label, ""));
                let size = rest.split(" @").next().unwrap_or("").replace('x', " × ");
                Choice { title: name.to_string(), detail: size, group: String::new(), value: value.clone() }
            }
            "app" => {
                // Windows lists the app's windows: the app is what's shown
                let (app, title) = app_and_title(label);
                let (app, detail) = if app.is_empty() { (title, String::new()) } else { (app, title) };
                if HIDDEN_APPS.contains(&app.as_str()) {
                    continue;
                }
                Choice { title: app.trim_end_matches(".exe").to_string(), detail, group: String::new(), value: value.clone() }
            }
            _ => Choice { title: label.to_string(), detail: String::new(), group: String::new(), value: value.clone() },
        };
        out.push(choice);
    }
    out
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
        // the setting's name in the platform's libobs source
        let obs_key = self
            .studio
            .read(cx)
            .project()
            .inputs
            .get(&input)
            .and_then(|i| lightair_engine::kinds::libobs_key(&i.kind, key))
            .unwrap_or(key);
        let list = self.studio.read(cx).engine().call(move |s| s.reconciler.input_options(input, obs_key));
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

    pub(super) fn open_source_picker(&mut self, input: Id, window: &mut Window, cx: &mut Context<Self>) {
        let search = cx.new(|cx| InputState::new(window, cx).placeholder("Search"));
        search.update(cx, |s, cx| s.focus(window, cx));
        let events = cx.subscribe_in(&search, window, |_, _, ev: &InputEvent, _, cx| {
            if matches!(ev, InputEvent::Change) {
                cx.notify();
            }
        });
        // Fresh lists: windows come and go.
        let project = self.studio.read(cx).project().clone();
        if let Some(i) = project.inputs.get(&input)
            && let Some(key) = device_key(&i.kind, &i.settings)
        {
            self.source_options(input, key, true, cx);
        }
        self.source_picker = Some(SourcePicker { input, search, _events: events });
        cx.notify();
    }

    fn close_source_picker(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.source_picker = None;
        window.focus(&self.focus, cx);
        cx.notify();
    }

    /// The label for an input's current source, from the cached list.
    fn current_source(&self, input: &Input, cx: &mut Context<Self>) -> Option<(String, String)> {
        let key = device_key(&input.kind, &input.settings)?;
        let current = input.settings[key].clone();
        let list = choices(key, &self.source_options(input.id, key, false, cx));
        Some(match list.into_iter().find(|c| c.value == current) {
            Some(c) => (c.title, c.detail),
            None if current.is_null() || current == json!("") || current == json!(0) => {
                ("Choose…".into(), String::new())
            }
            None => ("Not available".into(), "Pick another".into()),
        })
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
                        self.source_chip("source-file", ui::input_icon(&input.kind), name, String::new(), "Replace…")
                            .on_click(cx.listener(move |this, _, window, cx| this.replace_file(id, key, window, cx))),
                    )
                    .into_any_element(),
            );
        }
        let (title, detail) = self.current_source(input, cx)?;
        let icon = source_icon(&input.kind, &input.settings);
        Some(
            self.section("Source", None, cx)
                .child(
                    self.source_chip("source-device", icon, title, detail, "Change")
                        .on_click(cx.listener(move |this, _, window, cx| this.open_source_picker(id, window, cx))),
                )
                .into_any_element(),
        )
    }

    /// A full-width row: icon, value with a detail line, and an action word.
    fn source_chip(
        &self,
        id: &'static str,
        icon: &'static str,
        value: String,
        detail: String,
        action: &'static str,
    ) -> Stateful<Div> {
        div()
            .id(id)
            .flex()
            .items_center()
            .gap_2()
            .min_h(px(40.))
            .px_2()
            .py_1()
            .rounded(px(8.))
            .bg(tok::bg_raised())
            .cursor_pointer()
            .hover(|d| d.bg(tok::bg_selected()))
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_center()
                    .flex_none()
                    .size(px(28.))
                    .rounded(px(6.))
                    .bg(tok::bg_app())
                    .child(ui::icon(icon).size(px(14.)).text_color(tok::text_secondary())),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_w_0()
                    .child(div().overflow_hidden().text_ellipsis().text_size(px(12.)).child(value))
                    .when(!detail.is_empty(), |d| {
                        d.child(
                            div()
                                .overflow_hidden()
                                .text_ellipsis()
                                .text_size(px(11.))
                                .text_color(tok::text_tertiary())
                                .child(detail),
                        )
                    }),
            )
            .child(div().text_size(px(11.)).text_color(tok::text_tertiary()).child(action))
    }

    /// The picker dialog: mode (screen capture), search, and the choices as cards or, for windows,
    /// grouped by app.
    pub(super) fn source_picker_overlay(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let picker = self.source_picker.as_ref()?;
        let project = self.studio.read(cx).project().clone();
        let input = project.inputs.get(&picker.input)?.clone();
        let key = device_key(&input.kind, &input.settings)?;
        let id = input.id;
        let query = picker.search.read(cx).value().to_lowercase();
        let current = input.settings[key].clone();
        let list: Vec<Choice> = choices(key, &self.source_options(id, key, false, cx))
            .into_iter()
            .filter(|c| {
                query.is_empty() || c.title.to_lowercase().contains(&query) || c.detail.to_lowercase().contains(&query)
            })
            .collect();
        let icon = source_icon(&input.kind, &input.settings);
        let title = match key {
            "device" if input.kind == kinds::CAMERA => "Choose a camera",
            "device" => "Choose a microphone",
            "display" => "Choose a display",
            "window" => "Choose a window",
            "app" => "Choose an app",
            _ => "Choose an input",
        };

        let card = |i: usize, c: &Choice, cx: &mut Context<Self>| {
            let selected = c.value == current;
            let settings = merged(&input.settings, key, c.value.clone());
            div()
                .id(("source-choice", i))
                .flex()
                .items_center()
                .gap_3()
                .p_2()
                .rounded(px(8.))
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
                        .flex_none()
                        .size(px(32.))
                        .rounded(px(8.))
                        .bg(tok::bg_app())
                        .child(ui::icon(icon).size(px(16.)).text_color(tok::text())),
                )
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .flex_1()
                        .min_w_0()
                        .child(div().overflow_hidden().text_ellipsis().child(c.title.clone()))
                        .when(!c.detail.is_empty() && key != "window", |d| {
                            d.child(div().text_size(px(11.)).text_color(tok::text_tertiary()).child(c.detail.clone()))
                        }),
                )
                .when(selected, |d| d.child(ui::icon("check").size(px(14.)).text_color(tok::accent_primary())))
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.exec(Command::SetInputSettings { input: id, settings: settings.clone() }, cx);
                    this.close_source_picker(window, cx);
                }))
        };

        let mut body = div().id("source-body").flex().flex_col().gap_1().flex_shrink(1.).min_h_0().overflow_y_scroll();
        if list.is_empty() {
            body = body.child(div().p_4().text_center().text_color(tok::text_tertiary()).child("Nothing found"));
        } else if key == "window" {
            // Windows, grouped under their app.
            let mut groups: Vec<(String, Vec<(usize, Choice)>)> = Vec::new();
            for (i, c) in list.into_iter().enumerate() {
                match groups.iter_mut().find(|(g, _)| *g == c.group) {
                    Some((_, items)) => items.push((i, c)),
                    None => groups.push((c.group.clone(), vec![(i, c)])),
                }
            }
            for (app, items) in groups {
                body = body.child(
                    div()
                        .pt_2()
                        .px_1()
                        .text_size(px(11.))
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(tok::text_tertiary())
                        .child(if app.is_empty() { "OTHER".into() } else { app.to_uppercase() }),
                );
                for (i, c) in items {
                    body = body.child(card(i, &c, cx));
                }
            }
        } else {
            let mut grid = div().grid().grid_cols(2).gap_1();
            for (i, c) in list.iter().enumerate() {
                grid = grid.child(card(i, c, cx));
            }
            body = body.child(grid);
        }

        let mode = (input.kind == kinds::SCREEN).then(|| {
            let m = input.settings["mode"].as_i64().unwrap_or(kinds::DISPLAY);
            self.segmented(
                "capture-mode",
                [(kinds::DISPLAY, "Display"), (kinds::WINDOW, "Window"), (kinds::APP, "App"), (kinds::GAME, "Game")]
                    .into_iter()
                    // game capture is Windows's
                    .filter(|(v, _)| *v != kinds::GAME || cfg!(windows))
                    .map(|(v, label)| {
                        (
                            label,
                            Command::SetInputSettings {
                                input: id,
                                settings: merged(&input.settings, "mode", json!(v)),
                            },
                            m == v,
                        )
                    })
                    .collect(),
                cx,
            )
        });

        Some(
            div()
                .absolute()
                .inset_0()
                .flex()
                .justify_center()
                .items_start()
                .pt(px(80.))
                .bg(tok::scrim())
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _, window, cx| this.close_source_picker(window, cx)),
                )
                .child(
                    div()
                        .id("source-picker")
                        .flex()
                        .flex_col()
                        .gap_2()
                        .w(px(560.))
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
                                this.close_source_picker(window, cx);
                                cx.stop_propagation();
                            }
                        }))
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .justify_between()
                                .child(div().text_size(px(15.)).font_weight(FontWeight::SEMIBOLD).child(title))
                                .child(
                                    div()
                                        .text_size(px(11.))
                                        .text_color(tok::text_tertiary())
                                        .child(format!("for {}", input.name)),
                                ),
                        )
                        .children(mode)
                        .child(TextInput::new(&picker.search))
                        .child(body),
                )
                .into_any_element(),
        )
    }
}

fn source_icon(kind: &str, settings: &Value) -> &'static str {
    match kind {
        kinds::SCREEN => match settings["mode"].as_i64().unwrap_or(kinds::DISPLAY) {
            kinds::WINDOW => "app-window",
            kinds::GAME => "gamepad-2",
            kinds::APP => "layout-grid",
            _ => "monitor",
        },
        _ => ui::input_icon(kind),
    }
}

#[cfg(test)]
mod tests {
    use super::choices;
    use serde_json::json;

    #[test]
    fn labels_become_titles() {
        let windows = [
            ("[Claude] Claude".to_string(), json!(1)),
            ("[Dock] Dock".to_string(), json!(2)),
            ("[Lightair] Lightair".to_string(), json!(3)),
            ("[Notes] Shopping list".to_string(), json!(4)),
            (" ".to_string(), json!(0)),
        ];
        let c = choices("window", &windows);
        let titles: Vec<_> = c.iter().map(|c| (c.group.as_str(), c.title.as_str())).collect();
        assert_eq!(
            titles,
            [("Claude", "Claude"), ("Notes", "Shopping list")],
            "system windows and Lightair are hidden"
        );
        let displays = [("Built-in Retina Display: 3024x1964 @ 0,0".to_string(), json!("uuid"))];
        let d = &choices("display", &displays)[0];
        assert_eq!((d.title.as_str(), d.detail.as_str()), ("Built-in Retina Display", "3024 × 1964"));

        // Windows's window_capture labels
        let windows = [("[chrome.exe]: Lightair docs".to_string(), json!("a:b:chrome.exe"))];
        let c = &choices("window", &windows)[0];
        assert_eq!((c.group.as_str(), c.title.as_str()), ("chrome.exe", "Lightair docs"));
        let a = &choices("app", &windows)[0];
        assert_eq!((a.title.as_str(), a.detail.as_str()), ("chrome", "Lightair docs"));
    }
}
