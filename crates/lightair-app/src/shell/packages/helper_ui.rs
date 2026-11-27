//! A helper package's section in the Packages panel (plugins.md §4.8, helper-protocol.md): how
//! it's doing, its switch, Stop or Start again, and the settings it declared. And the window's
//! part of the helper host: the show's tracks for `audio.levels`.

use gpui_kit::component::Sizable;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::Input as TextInput;
use gpui_kit::component::menu::{DropdownMenu, PopupMenuItem};
use gpui_kit::component::switch::Switch;
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;
use lightair_packages::{Installed, Kind};
use serde_json::{Value, json};

use super::PackagesPanel;
use crate::helpers_host as hh;
use crate::shell::StudioWindow;
use crate::ui::tok;

impl PackagesPanel {
    /// The section, for a helper package other than PiAir (PiAir is the co-host's).
    pub(super) fn helper_section(&self, i: usize, p: &Installed, cx: &mut Context<Self>) -> Option<Div> {
        let id = p.id().as_str().to_string();
        if id == crate::piair::proc::PACKAGE || !p.manifest.contents.iter().any(|c| c.kind == Kind::Helper) {
            return None;
        }
        let on = hh::enabled(&id);
        let status = hh::status(&id);
        let (colour, line) = match (&status, on) {
            (_, false) => (tok::text_tertiary(), "Off".to_string()),
            (Some(s), true) if s.error.is_some() => (tok::error(), s.error.clone().unwrap_or_default()),
            (Some(s), true) if s.ready && s.full_access => (tok::warn(), "Running with full access".to_string()),
            (Some(s), true) if s.ready => (tok::ok(), "Running".to_string()),
            (Some(s), true) if s.running => (tok::warn(), "Starting…".to_string()),
            (Some(_), true) => (tok::text_tertiary(), "Stopped".to_string()),
            (None, true) => (tok::text_tertiary(), "Not running".to_string()),
        };
        let gave_up = status.as_ref().and_then(|s| s.error.as_ref()).is_some_and(|e| e.contains("three times"));
        let toggle_id = id.clone();
        let mut section = div()
            .flex()
            .flex_col()
            .gap_1()
            .pl(px(42.))
            .pr_3()
            .py_2()
            .border_b_1()
            .border_color(tok::border())
            .bg(tok::bg_raised())
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .text_size(px(11.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(tok::text_secondary())
                            .child("Helper"),
                    )
                    .child(div().size(px(7.)).rounded_full().bg(colour))
                    .child(div().flex_1().min_w_0().truncate().text_size(px(12.)).text_color(tok::text()).child(line))
                    .when(gave_up, |d| {
                        let id = id.clone();
                        d.child(Button::new(("helper-start", i)).label("Start again").xsmall().ghost().on_click(
                            cx.listener(move |_, _, _, cx| {
                                hh::start_again(&id);
                                cx.notify();
                            }),
                        ))
                    })
                    .child(Switch::new(("helper-on", i)).checked(on).small().on_click(cx.listener(
                        move |_, checked: &bool, _, cx| {
                            hh::set_enabled(&toggle_id, *checked);
                            cx.notify();
                        },
                    ))),
            );
        // the settings it declared, once it said (it's asked when it starts)
        let Some(declared) = hh::declared(&id) else { return Some(section) };
        if declared.settings.is_empty() {
            return Some(section);
        }
        let values = hh::setting_values(&id);
        section = section.child(
            div()
                .pt_1()
                .text_size(px(11.))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(tok::text_secondary())
                .child("Settings"),
        );
        for (j, s) in declared.settings.iter().enumerate() {
            let value = values.get(&s.key).cloned().unwrap_or(Value::Null);
            let key = s.key.clone();
            let id2 = id.clone();
            let control: AnyElement = match s.kind.as_str() {
                "toggle" => Switch::new(("helper-set-toggle", i * 1000 + j))
                    .checked(value.as_bool().unwrap_or(false))
                    .small()
                    .on_click(cx.listener(move |_, checked: &bool, _, cx| {
                        hh::set_setting(&id2, &key, json!(*checked));
                        cx.notify();
                    }))
                    .into_any_element(),
                "choice" => {
                    let options = s.options.clone();
                    Button::new(("helper-set-choice", i * 1000 + j))
                        .label(value.as_str().unwrap_or("Choose…").to_string())
                        .xsmall()
                        .outline()
                        .dropdown_menu(move |mut menu, _, _| {
                            for o in &options {
                                let (id, key, o) = (id2.clone(), key.clone(), o.clone());
                                menu = menu.item(
                                    PopupMenuItem::new(o.clone())
                                        .on_click(move |_, _, _| hh::set_setting(&id, &key, json!(o))),
                                );
                            }
                            menu
                        })
                        .into_any_element()
                }
                _ => {
                    let editing = self.helper_editing.as_ref() == Some(&(id.clone(), s.key.clone()));
                    if editing {
                        let number = s.kind == "number";
                        div()
                            .flex()
                            .items_center()
                            .gap_1()
                            .child(div().w(px(160.)).child(TextInput::new(&self.helper_edit).small()))
                            .child(
                                Button::new(("helper-set-save", i * 1000 + j))
                                    .label("Save")
                                    .xsmall()
                                    .outline()
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        let text = this.helper_edit.read(cx).value().trim().to_string();
                                        let v = if number {
                                            text.parse::<f64>().map(|n| json!(n)).unwrap_or(json!(text))
                                        } else {
                                            json!(text)
                                        };
                                        hh::set_setting(&id2, &key, v);
                                        this.helper_editing = None;
                                        cx.notify();
                                    })),
                            )
                            .into_any_element()
                    } else {
                        let shown = match &value {
                            Value::String(t) if t.is_empty() => "—".to_string(),
                            Value::String(t) => t.clone(),
                            Value::Null => "—".to_string(),
                            v => v.to_string(),
                        };
                        let start = shown.clone();
                        Button::new(("helper-set-edit", i * 1000 + j))
                            .label(shown)
                            .xsmall()
                            .ghost()
                            .on_click(cx.listener(move |this, _, window, cx| {
                                let start = if start == "—" { String::new() } else { start.clone() };
                                this.helper_edit.update(cx, |s, cx| s.set_value(start, window, cx));
                                this.helper_editing = Some((id2.clone(), key.clone()));
                                cx.notify();
                            }))
                            .into_any_element()
                    }
                }
            };
            section = section.child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .min_h(px(24.))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_size(px(12.))
                            .text_color(tok::text())
                            .child(s.label.clone()),
                    )
                    .child(control),
            );
        }
        Some(section)
    }
}

impl StudioWindow {
    /// The show's tracks to the helper host every second, for helpers' `audio.levels`.
    pub(crate) fn keep_helpers_told(&mut self, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(std::time::Duration::from_secs(1)).await;
                let ok = this.update(cx, |this, cx| {
                    let w = this.studio.read(cx).workspace().clone();
                    hh::set_tracks(w.tracks.iter().map(|t| (t.id, t.name.clone())).collect());
                });
                if ok.is_err() {
                    break;
                }
            }
        })
        .detach();
    }
}

impl StudioWindow {
    /// File › Export Workspace with Packages…: a copy of the open show with the `.airpkg` files of
    /// the packages it uses inside (`Packages/`), so it opens whole on another computer whatever
    /// happens to its authors' releases.
    pub(crate) fn export_workspace_with_packages(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(dir) = self.studio.read(cx).workspace_dir() else {
            self.package_note("This workspace isn't saved in a folder yet", cx);
            return;
        };
        // what the show uses, locked now
        let workspace = self.studio.read(cx).workspace().clone();
        crate::packages::record_show(&dir, &workspace);
        let desktop = std::env::home_dir().unwrap_or_default().join("Desktop");
        let name = dir.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| "Show".into());
        let path = cx.prompt_for_new_path(&desktop, Some(&format!("{name} with packages.lightair")));
        cx.spawn_in(window, async move |this, cx| {
            let Ok(Ok(Some(mut dest))) = path.await else { return };
            if dest.extension().is_none_or(|e| e != "lightair") {
                dest.set_extension("lightair");
            }
            let shown = dest.display().to_string();
            let result = cx.background_executor().spawn(async move { crate::packages::export_show(&dir, &dest) }).await;
            let note = match result {
                Ok(0) => format!("Exported to {shown} (it uses no packages)"),
                Ok(1) => format!("Exported to {shown} with its package"),
                Ok(n) => format!("Exported to {shown} with its {n} packages"),
                Err(e) => format!("Couldn't export the workspace: {e}"),
            };
            let _ = this.update(cx, |this, cx| this.package_note(note, cx));
        })
        .detach();
    }
}
