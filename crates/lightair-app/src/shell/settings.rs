//! The Settings sheet (⌘,): recording and streaming (saved with the project) and app
//! preferences (saved on this Mac).

use gpui_kit::component::Sizable;
use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;
use lightair_core::{Command, RecordingSettings};

use super::StudioWindow;
use crate::ui::tok;

impl StudioWindow {
    pub(super) fn toggle_settings(&mut self, cx: &mut Context<Self>) {
        self.settings_open = !self.settings_open;
        cx.notify();
    }

    fn set_recording(&mut self, f: impl FnOnce(&mut RecordingSettings), cx: &mut Context<Self>) {
        let mut settings = self.studio.read(cx).project().outputs.recording.clone();
        f(&mut settings);
        self.exec(Command::SetRecordingSettings { settings }, cx);
    }

    fn choose_recording_folder(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let paths =
            cx.prompt_for_paths(PathPromptOptions { files: false, directories: true, multiple: false, prompt: None });
        cx.spawn_in(window, async move |this, cx| {
            let Ok(Ok(Some(paths))) = paths.await else { return };
            let Some(dir) = paths.into_iter().next() else { return };
            let _ = this.update(cx, |this, cx| this.set_recording(|r| r.folder = Some(dir), cx));
        })
        .detach();
    }

    fn set_review(&mut self, on: bool, cx: &mut Context<Self>) {
        self.prefs.review_before_live = on;
        if let Err(e) = crate::prefs::save_to(&crate::prefs::path(), &self.prefs) {
            self.studio.update(cx, |s, _| s.issue = Some(format!("Couldn't save preferences: {e}").into()));
        }
        cx.notify();
    }

    pub(super) fn settings_overlay(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        if !self.settings_open {
            return None;
        }
        let studio = self.studio.read(cx);
        let project = studio.project().clone();
        let folder = studio.recording_folder();
        let rec = project.outputs.recording.clone();
        let stream_kbps = project.outputs.stream_bitrate_kbps;
        let review = self.prefs.review_before_live;

        let label = |text: &'static str| div().w(px(110.)).text_color(tok::text_secondary()).child(text);
        let row = || div().flex().items_center().gap_2().min_h(px(30.));
        let heading = |text: &'static str| {
            div()
                .pt_2()
                .text_size(px(11.))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(tok::text_secondary())
                .child(text)
        };
        let stepper = |id: &'static str, value: String, minus: Command, plus: Command| {
            div()
                .flex()
                .items_center()
                .gap_1()
                .child(
                    Button::new((id, 0usize))
                        .label("−")
                        .xsmall()
                        .ghost()
                        .on_click(cx.listener(move |this, _, _, cx| this.exec(minus.clone(), cx))),
                )
                .child(div().w(px(96.)).text_center().font_family("Menlo").child(value))
                .child(
                    Button::new((id, 1usize))
                        .label("+")
                        .xsmall()
                        .ghost()
                        .on_click(cx.listener(move |this, _, _, cx| this.exec(plus.clone(), cx))),
                )
        };
        let rec_bitrate = |kbps: u32| Command::SetRecordingSettings {
            settings: RecordingSettings { bitrate_kbps: kbps.clamp(2_000, 100_000), ..rec.clone() },
        };

        let mut containers = div().flex().gap_1();
        for (i, (value, text)) in [("mp4", "MP4"), ("mov", "MOV"), ("mkv", "MKV")].into_iter().enumerate() {
            let b = Button::new(("container", i))
                .label(text)
                .xsmall()
                .on_click(cx.listener(move |this, _, _, cx| this.set_recording(|r| r.container = value.into(), cx)));
            containers = containers.child(if rec.container == value { b.primary() } else { b.ghost() });
        }

        let body = div()
            .flex()
            .flex_col()
            .gap_1()
            .child(heading("RECORDING"))
            .child(
                row()
                    .child(label("Folder"))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .overflow_hidden()
                            .text_ellipsis()
                            .text_color(tok::text_tertiary())
                            .child(folder.to_string_lossy().into_owned()),
                    )
                    .child(
                        Button::new("rec-folder")
                            .label("Choose…")
                            .xsmall()
                            .outline()
                            .on_click(cx.listener(|this, _, window, cx| this.choose_recording_folder(window, cx))),
                    ),
            )
            .child(row().child(label("Container")).child(containers).child(
                div().text_size(px(11.)).text_color(tok::text_tertiary()).child(match rec.container.as_str() {
                    "mkv" => "Safe if the app quits; markers not supported",
                    "mov" => "Editor-friendly",
                    _ => "Crash-safe while recording, normal MP4 after",
                }),
            ))
            .child(row().child(label("Bitrate")).child(stepper(
                "rec-bitrate",
                format!("{} kbps", rec.bitrate_kbps),
                rec_bitrate(rec.bitrate_kbps.saturating_sub(2_000)),
                rec_bitrate(rec.bitrate_kbps + 2_000),
            )))
            .child(heading("STREAMING"))
            .child(row().child(label("Bitrate")).child(stepper(
                "stream-bitrate",
                format!("{stream_kbps} kbps"),
                Command::SetStreamBitrate { kbps: stream_kbps.saturating_sub(500).max(1_000) },
                Command::SetStreamBitrate { kbps: (stream_kbps + 500).min(20_000) },
            )))
            .child(div().text_size(px(11.)).text_color(tok::text_tertiary()).child(
                "Twitch allows up to 6000 kbps (8000 with Enhanced Broadcasting); YouTube up to 9000 for 1080p60.",
            ))
            .child(heading("THIS MAC"))
            .child(
                row()
                    .id("pref-review")
                    .cursor_pointer()
                    .on_click(cx.listener(move |this, _, _, cx| this.set_review(!review, cx)))
                    .child(
                        div()
                            .size(px(14.))
                            .rounded(px(3.))
                            .border_1()
                            .border_color(if review { tok::accent_primary() } else { tok::border_strong() })
                            .when(review, |d| d.bg(tok::accent_primary())),
                    )
                    .child("Review destinations and checks before going live"),
            );

        Some(
            div()
                .absolute()
                .inset_0()
                .flex()
                .justify_center()
                .items_center()
                .bg(tok::scrim())
                .on_mouse_down(MouseButton::Left, cx.listener(|this, _, _, cx| this.toggle_settings(cx)))
                .child(
                    div()
                        .id("settings")
                        .flex()
                        .flex_col()
                        .gap_3()
                        .w(px(520.))
                        .p_5()
                        .rounded(px(14.))
                        .bg(tok::bg_panel())
                        .border_1()
                        .border_color(tok::border_strong())
                        .shadow_lg()
                        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                        .child(div().text_size(px(16.)).font_weight(FontWeight::SEMIBOLD).child("Settings"))
                        .child(body)
                        .child(
                            div().flex().justify_end().child(
                                Button::new("settings-done")
                                    .label("Done")
                                    .small()
                                    .primary()
                                    .on_click(cx.listener(|this, _, _, cx| this.toggle_settings(cx))),
                            ),
                        ),
                )
                .into_any_element(),
        )
    }
}
