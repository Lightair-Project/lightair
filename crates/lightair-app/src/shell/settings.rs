//! Settings (⌘,; docs/14-data-persistence-settings.md, "Settings"): a window with a page per
//! area. Recording, streaming, clips and canvases travel with the project; General is this
//! Mac's. Each destination has its own settings too, in Outputs (click it).

use gpui_kit::component::Sizable;
use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;
use lightair_core::{ClipSettings, Command, Format, Reconnect, RecordingSettings, VideoEncoder};

use super::StudioWindow;
use crate::ui::{self, tok};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Page {
    General,
    Recording,
    Streaming,
    Clips,
    Canvases,
    Shortcuts,
    About,
}

const PAGES: &[(Page, &str, &str)] = &[
    (Page::General, "General", "settings"),
    (Page::Recording, "Recording", "circle-dot"),
    (Page::Streaming, "Streaming", "radio"),
    (Page::Clips, "Clips", "scissors"),
    (Page::Canvases, "Canvases", "proportions"),
    (Page::Shortcuts, "Shortcuts", "keyboard"),
    (Page::About, "About", "info"),
];

/// Every keyboard shortcut, as the Shortcuts page lists them.
const SHORTCUTS: &[(&str, &[(&str, &str)])] = &[
    (
        "Objects",
        &[
            ("Cut, copy, paste", "⌘X  ⌘C  ⌘V"),
            ("Duplicate", "⌘D"),
            ("Delete", "⌫"),
            ("Rename", "↩"),
            ("Select all", "⌘A"),
            ("Deselect", "Esc"),
            ("Group / ungroup", "⌘G  ⇧⌘G"),
            ("Create empty / inside", "⇧⌘N  ⌥⇧N"),
            ("Bring forward / send backward", "⌘]  ⌘["),
            ("To front / to back", "⇧⌘]  ⇧⌘["),
            ("Show or hide / lock", "⇧⌘H  ⇧⌘L"),
            ("Nudge", "Arrow keys"),
            ("Rotate gizmo", "E"),
        ],
    ),
    (
        "Canvas",
        &[
            ("Zoom in / out", "⌘=  ⌘−"),
            ("Fit / actual size", "⌘0  ⌥⌘0"),
            ("Pan", "Scroll, Space-drag"),
            ("Crop", "⌥-drag a corner, drag an edge"),
        ],
    ),
    (
        "Show",
        &[
            ("Scene 1–9", "⌘1 … ⌘9"),
            ("Take (with Cue scenes)", "⌘↩"),
            ("Add marker", "⌘M"),
            ("Mute microphones", "⇧⌘M"),
            ("Undo / redo", "⌘Z  ⇧⌘Z"),
            ("Command palette", "⌘K"),
            ("Settings", "⌘,"),
        ],
    ),
];

impl StudioWindow {
    pub(super) fn toggle_settings(&mut self, cx: &mut Context<Self>) {
        self.settings_open = !self.settings_open;
        cx.notify();
    }

    /// Opens Settings at a page.
    pub(super) fn open_settings(&mut self, page: Page, cx: &mut Context<Self>) {
        self.settings_open = true;
        self.settings_page = page;
        cx.notify();
    }

    fn set_recording(&mut self, f: impl FnOnce(&mut RecordingSettings), cx: &mut Context<Self>) {
        let mut settings = self.studio.read(cx).project().outputs.recording.clone();
        f(&mut settings);
        self.exec(Command::SetRecordingSettings { settings }, cx);
    }

    fn set_clips(&mut self, f: impl FnOnce(&mut ClipSettings), cx: &mut Context<Self>) {
        let mut settings = self.studio.read(cx).project().outputs.clips;
        f(&mut settings);
        self.exec(Command::SetClipSettings { settings }, cx);
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

    fn set_pref(&mut self, f: impl FnOnce(&mut crate::prefs::Preferences), cx: &mut Context<Self>) {
        f(&mut self.prefs);
        if let Err(e) = crate::prefs::save_to(&crate::prefs::path(), &self.prefs) {
            self.studio.update(cx, |s, _| s.issue = Some(format!("Couldn't save preferences: {e}").into()));
        }
        cx.notify();
    }

    pub(super) fn settings_overlay(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        if !self.settings_open {
            return None;
        }
        let page = self.settings_page;
        let mut nav =
            div().flex().flex_col().gap_0p5().w(px(176.)).flex_none().p_2().border_r_1().border_color(tok::border());
        for (i, (p, label, icon)) in PAGES.iter().enumerate() {
            let on = *p == page;
            let p = *p;
            nav = nav.child(
                div()
                    .id(("settings-page", i))
                    .flex()
                    .items_center()
                    .gap_2()
                    .h(px(30.))
                    .px_2()
                    .rounded(px(6.))
                    .cursor_pointer()
                    .text_size(px(13.))
                    .when(on, |d| d.bg(tok::bg_selected()).text_color(tok::text()))
                    .when(!on, |d| d.text_color(tok::text_secondary()).hover(|d| d.bg(tok::bg_raised())))
                    .child(ui::icon(icon).size(px(14.)))
                    .child(*label)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.settings_page = p;
                        cx.notify();
                    })),
            );
        }
        let content = match page {
            Page::General => self.general_page(cx),
            Page::Recording => self.recording_page(cx),
            Page::Streaming => self.streaming_page(cx),
            Page::Clips => self.clips_page(cx),
            Page::Canvases => self.canvases_page(cx),
            Page::Shortcuts => shortcuts_page(),
            Page::About => self.about_page(cx),
        };
        let title = PAGES.iter().find(|(p, ..)| *p == page).map_or("", |(_, l, _)| *l);
        let live = lightair_engine::outputs::runtime().any_stream_running()
            || lightair_engine::outputs::runtime().any_recording_running();
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
                        .w(px(780.))
                        .h(px(580.))
                        .rounded(px(14.))
                        .overflow_hidden()
                        .bg(tok::bg_panel())
                        .border_1()
                        .border_color(tok::border_strong())
                        .shadow_lg()
                        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                        .capture_key_down(cx.listener(|this, ev: &KeyDownEvent, _, cx| {
                            if ev.keystroke.key == "escape" {
                                this.toggle_settings(cx);
                            }
                        }))
                        .child(
                            nav.child(div().flex_1()).child(
                                div()
                                    .px_2()
                                    .pb_1()
                                    .text_size(px(10.))
                                    .text_color(tok::text_tertiary())
                                    .child("Saved as you change them"),
                            ),
                        )
                        .child(
                            div()
                                .flex()
                                .flex_col()
                                .flex_1()
                                .min_w_0()
                                .child(
                                    div()
                                        .flex()
                                        .items_center()
                                        .justify_between()
                                        .px_5()
                                        .pt_4()
                                        .pb_2()
                                        .child(div().text_size(px(18.)).font_weight(FontWeight::SEMIBOLD).child(title))
                                        .child(
                                            Button::new("settings-done")
                                                .label("Done")
                                                .small()
                                                .primary()
                                                .on_click(cx.listener(|this, _, _, cx| this.toggle_settings(cx))),
                                        ),
                                )
                                .when(live, |d| {
                                    d.child(
                                        div()
                                            .px_5()
                                            .pb_2()
                                            .text_size(px(11.))
                                            .text_color(tok::warn())
                                            .child("Recording or live: changes apply the next time it starts."),
                                    )
                                })
                                .child(
                                    div()
                                        .id("settings-content")
                                        .flex_1()
                                        .min_h_0()
                                        .overflow_y_scroll()
                                        .px_5()
                                        .pb_5()
                                        .child(content),
                                ),
                        ),
                )
                .into_any_element(),
        )
    }

    // ------------------------------------------------------------ pages

    fn general_page(&self, cx: &mut Context<Self>) -> Div {
        let review = self.prefs.review_before_live;
        let confirm = self.prefs.confirm_end_stream;
        let studio = self.studio.read(cx);
        let project_dir = studio.project_dir();
        let name = studio.project().name.clone();
        pages()
            .child(
                group("Going live")
                    .child(row(
                        "Review before going live",
                        Some("Show the destinations and checks first."),
                        switch("pref-review", review).on_click(
                            cx.listener(move |this, _, _, cx| this.set_pref(|p| p.review_before_live = !review, cx)),
                        ),
                    ))
                    .child(row(
                        "Ask before ending a stream",
                        Some("Ending takes a second click on the Live button."),
                        switch("pref-confirm-end", confirm).on_click(
                            cx.listener(move |this, _, _, cx| this.set_pref(|p| p.confirm_end_stream = !confirm, cx)),
                        ),
                    )),
            )
            .child(group("Project").child(row(
                name.as_str(),
                project_dir.as_ref().map(|d| d.to_string_lossy().into_owned()).as_deref(),
                reveal_button("reveal-project", project_dir),
            )))
            .child(note("General settings belong to this Mac. The rest are saved with the project."))
    }

    fn recording_page(&self, cx: &mut Context<Self>) -> Div {
        let studio = self.studio.read(cx);
        let project = studio.project().clone();
        let caps = studio.engine().capabilities.encoders.clone();
        let folder = studio.recording_folder();
        let rec = project.outputs.recording.clone();
        let targets = rec.targets(&project);

        let mut canvases = group("Canvases");
        for (i, f) in project.formats.iter().enumerate() {
            let on = targets.contains(&f.id);
            let (fid, r2, p2) = (f.id, rec.clone(), project.clone());
            canvases = canvases.child(row(
                &format!("{} {}×{}", f.name, f.width, f.height),
                None,
                switch(("rec-canvas", i), on).on_click(cx.listener(move |this, _, _, cx| {
                    let settings = r2.with_format(&p2, fid, !on);
                    this.exec(Command::SetRecordingSettings { settings }, cx);
                })),
            ));
        }
        let prores = rec.encoder == VideoEncoder::ProRes;
        let encoder_choices: Vec<(String, bool, bool, Command)> =
            [VideoEncoder::H264, VideoEncoder::Hevc, VideoEncoder::ProRes, VideoEncoder::X264]
                .into_iter()
                .map(|e| {
                    // ProRes lives in MOV files.
                    let container = if e == VideoEncoder::ProRes { "mov".to_string() } else { rec.container.clone() };
                    let settings = RecordingSettings { encoder: e, container, ..rec.clone() };
                    (
                        e.label().to_string(),
                        rec.encoder == e,
                        crate::encoding::available(&caps, e),
                        Command::SetRecordingSettings { settings },
                    )
                })
                .collect();
        let containers: Vec<(String, bool, bool, Command)> = [("mp4", "MP4"), ("mov", "MOV"), ("mkv", "MKV")]
            .into_iter()
            .map(|(v, label)| {
                let settings = RecordingSettings { container: v.into(), ..rec.clone() };
                (
                    label.to_string(),
                    rec.container == v,
                    !prores || v == "mov",
                    Command::SetRecordingSettings { settings },
                )
            })
            .collect();
        let heights = height_choices(rec.height, |height| Command::SetRecordingSettings {
            settings: RecordingSettings { height, ..rec.clone() },
        });
        let audio: Vec<(String, bool, bool, Command)> = [128, 192, 256, 320]
            .into_iter()
            .map(|k| {
                let settings = RecordingSettings { audio_kbps: k, ..rec.clone() };
                (format!("{k} kbps"), rec.audio_kbps == k, true, Command::SetRecordingSettings { settings })
            })
            .collect();
        let splits: Vec<(String, bool, bool, Command)> = [None, Some(15), Some(30), Some(60)]
            .into_iter()
            .map(|m| {
                let settings = RecordingSettings { split_minutes: m, ..rec.clone() };
                (
                    m.map_or("Never".to_string(), |m| format!("{m} min")),
                    rec.split_minutes == m,
                    true,
                    Command::SetRecordingSettings { settings },
                )
            })
            .collect();
        let bitrate = |kbps: u32| Command::SetRecordingSettings {
            settings: RecordingSettings { bitrate_kbps: kbps.clamp(2_000, 150_000), ..rec.clone() },
        };
        pages()
            .child(canvases.child(note("Record records each canvas that's on, each to its own file.")))
            .child(
                group("Files")
                    .child(row(
                        "Folder",
                        Some(folder.to_string_lossy().as_ref()),
                        Button::new("rec-folder")
                            .label("Choose…")
                            .xsmall()
                            .outline()
                            .on_click(cx.listener(|this, _, window, cx| this.choose_recording_folder(window, cx)))
                            .into_any_element(),
                    ))
                    .child(row(
                        "File type",
                        Some(match rec.container.as_str() {
                            "mkv" => "Survives a crash; no markers.",
                            "mov" => "For editors; ProRes needs it.",
                            _ => "Crash-safe while recording, a normal MP4 after.",
                        }),
                        chips("rec-container", containers, cx),
                    ))
                    .child(row(
                        "New file every",
                        Some("Long shows in parts, without a gap."),
                        chips("rec-split", splits, cx),
                    )),
            )
            .child(
                group("Video")
                    .child(row(
                        "Encoder",
                        Some(match rec.encoder {
                            VideoEncoder::H264 => "Plays everywhere. Uses the Mac's media engine.",
                            VideoEncoder::Hevc => "Half the size at the same quality.",
                            VideoEncoder::ProRes => "For editing: big files, no quality loss.",
                            VideoEncoder::X264 => "Software: uses the CPU.",
                        }),
                        chips("rec-encoder", encoder_choices, cx),
                    ))
                    .when(!prores, |g| {
                        g.child(row(
                            "Quality",
                            Some("Higher is sharper and bigger: about 90 MB a minute at 12 Mbps."),
                            stepper(
                                "rec-bitrate",
                                format!("{:.0} Mbps", rec.bitrate_kbps as f32 / 1000.0),
                                bitrate(rec.bitrate_kbps.saturating_sub(2_000)),
                                bitrate(rec.bitrate_kbps + 2_000),
                                cx,
                            ),
                        ))
                    })
                    .child(row(
                        "Resolution",
                        Some("Smaller than the canvas is scaled down."),
                        chips("rec-height", heights, cx),
                    )),
            )
            .child(group("Audio").child(row("Quality", None, chips("rec-audio", audio, cx))))
    }

    fn streaming_page(&self, cx: &mut Context<Self>) -> Div {
        let project = self.studio.read(cx).project().clone();
        let kbps = project.outputs.stream_bitrate_kbps;
        let rc = project.outputs.reconnect;
        let set = |r: Reconnect| Command::SetReconnect { reconnect: r };
        let retries: Vec<(String, bool, bool, Command)> =
            [(0, "Off"), (5, "5 tries"), (20, "20 tries"), (100, "Keep trying")]
                .into_iter()
                .map(|(n, label)| (label.to_string(), rc.retries == n, true, set(Reconnect { retries: n, ..rc })))
                .collect();
        let delays: Vec<(String, bool, bool, Command)> = [2, 5, 10]
            .into_iter()
            .map(|d| (format!("{d} s"), rc.delay_s == d, rc.retries > 0, set(Reconnect { delay_s: d, ..rc })))
            .collect();
        let custom = project.outputs.destinations.iter().filter(|d| d.encoding.is_some()).count();
        pages()
            .child(
                group("Quality")
                    .child(row(
                        "Bitrate",
                        Some("For destinations on Automatic, capped at what each platform takes."),
                        stepper(
                            "stream-bitrate",
                            format!("{:.1} Mbps", kbps as f32 / 1000.0),
                            Command::SetStreamBitrate { kbps: kbps.saturating_sub(500).max(1_000) },
                            Command::SetStreamBitrate { kbps: (kbps + 500).min(51_000) },
                            cx,
                        ),
                    ))
                    .child(note(&format!(
                        "1080p60 wants 6–9 Mbps, 720p 3–5. {}",
                        if custom > 0 {
                            format!(
                                "{custom} destination{} use{} their own settings.",
                                plural(custom),
                                if custom == 1 { "s" } else { "" }
                            )
                        } else {
                            "Click a destination in Outputs to give it its own encoder, bitrate and size.".to_string()
                        }
                    ))),
            )
            .child(
                group("When the connection drops")
                    .child(row(
                        "Reconnect",
                        Some("Keep the stream going through short drop-outs."),
                        chips("reconnect", retries, cx),
                    ))
                    .child(row("Between tries", None, chips("reconnect-delay", delays, cx))),
            )
    }

    fn clips_page(&self, cx: &mut Context<Self>) -> Div {
        let project = self.studio.read(cx).project().clone();
        let clips = project.outputs.clips;
        let on = clips.enabled;
        let seconds: Vec<(String, bool, bool, Command)> = [15, 30, 60, 120, 300]
            .into_iter()
            .map(|s| {
                let settings = ClipSettings { seconds: s, ..clips };
                let label = if s >= 60 { format!("{} min", s / 60) } else { format!("{s} s") };
                (label, clips.seconds == s, true, Command::SetClipSettings { settings })
            })
            .collect();
        let canvases: Vec<(String, bool, bool, Command)> = project
            .formats
            .iter()
            .map(|f| {
                let format = (f.id != project.primary_format().id).then_some(f.id);
                let settings = ClipSettings { format, ..clips };
                (f.name.clone(), clips.format == format, true, Command::SetClipSettings { settings })
            })
            .collect();
        pages().child(
            group("Clips")
                .child(row(
                    "Keep the last moments",
                    Some("The Clip button saves them as a file."),
                    switch("clips-on", on)
                        .on_click(cx.listener(move |this, _, _, cx| this.set_clips(|c| c.enabled = !on, cx))),
                ))
                .child(row("Length", None, chips("clips-length", seconds, cx)))
                .when(project.formats.len() > 1, |g| g.child(row("Canvas", None, chips("clips-canvas", canvases, cx)))),
        )
    }

    fn canvases_page(&self, cx: &mut Context<Self>) -> Div {
        let project = self.studio.read(cx).project().clone();
        let primary = project.primary_format().id;
        let mut list = group("Canvases");
        for (i, f) in project.formats.iter().enumerate() {
            let id = f.id;
            let detail = format!("{}×{} · {} fps", f.width, f.height, f.fps_num / f.fps_den.max(1));
            list = list.child(row(
                &f.name,
                Some(&detail),
                if id == primary {
                    div().text_size(px(11.)).text_color(tok::text_tertiary()).child("Main").into_any_element()
                } else {
                    Button::new(("remove-format", i))
                        .label("Remove")
                        .xsmall()
                        .ghost()
                        .on_click(
                            cx.listener(move |this, _, _, cx| this.exec(Command::RemoveFormat { format: id }, cx)),
                        )
                        .into_any_element()
                },
            ));
        }
        let has = |w: u32, h: u32| project.formats.iter().any(|f| f.width == w && f.height == h);
        let mut add = div().flex().flex_wrap().gap_1();
        for (i, f) in [Format::vertical(), Format::square(), Format::portrait()].into_iter().enumerate() {
            if has(f.width, f.height) {
                continue;
            }
            add = add.child(
                Button::new(("settings-add-format", i))
                    .label(format!("{} {}×{}", f.name, f.width, f.height))
                    .xsmall()
                    .outline()
                    .on_click(cx.listener(move |this, _, _, cx| this.add_format(f.clone(), cx))),
            );
        }
        pages()
            .child(list.child(note(
                "Each canvas is another shape of every scene (vertical for TikTok, say), recorded and streamed on its own.",
            )))
            .child(group("Add a canvas").child(div().px_3().py_2().child(add)))
    }

    fn about_page(&self, cx: &mut Context<Self>) -> Div {
        let studio = self.studio.read(cx);
        let libobs = studio.engine().version.clone();
        let folders = [
            ("Project", studio.project_dir()),
            ("Recordings", Some(studio.recording_folder())),
            ("Preferences", crate::prefs::path().parent().map(|p| p.to_path_buf())),
        ];
        let mut where_ = group("Where things are");
        for (i, (label, dir)) in folders.into_iter().enumerate() {
            let shown = dir.as_ref().map(|d| d.to_string_lossy().into_owned());
            where_ = where_.child(row(label, shown.as_deref(), reveal_button(("reveal", i), dir)));
        }
        pages()
            .child(group("Lightair").child(row("Version", None, value(env!("CARGO_PKG_VERSION")))).child(row(
                "libobs",
                Some("The OBS engine Lightair runs on."),
                value(&libobs),
            )))
            .child(where_)
    }
}

fn shortcuts_page() -> Div {
    let mut page = pages();
    for (title, items) in SHORTCUTS {
        let mut g = group(title);
        for (what, keys) in *items {
            g = g.child(row(what, None, value(keys)));
        }
        page = page.child(g);
    }
    page
}

// ---------------------------------------------------------------- building blocks

fn pages() -> Div {
    div().flex().flex_col().gap_5().pt_1()
}

/// A titled card of rows.
fn group(title: &str) -> Div {
    div().flex().flex_col().gap_1p5().child(
        div()
            .text_size(px(11.))
            .font_weight(FontWeight::SEMIBOLD)
            .text_color(tok::text_tertiary())
            .child(title.to_uppercase()),
    )
}

/// A setting: its name (and what it does) on the left, its control on the right.
fn row(label: &str, detail: Option<&str>, control: impl IntoElement) -> Div {
    div()
        .flex()
        .items_center()
        .gap_4()
        .min_h(px(44.))
        .px_3()
        .py_2()
        .rounded(px(8.))
        .bg(tok::bg_raised())
        .child(
            div()
                .flex()
                .flex_col()
                .flex_1()
                .min_w_0()
                .child(div().text_size(px(13.)).child(label.to_string()))
                .children(detail.map(|d| {
                    div()
                        .text_size(px(11.))
                        .text_color(tok::text_tertiary())
                        .overflow_hidden()
                        .text_ellipsis()
                        .child(d.to_string())
                })),
        )
        .child(div().flex_none().child(control))
}

fn note(text: &str) -> Div {
    div().px_1().text_size(px(11.)).text_color(tok::text_tertiary()).child(text.to_string())
}

fn value(text: &str) -> Div {
    div().text_size(px(12.)).font_family("Menlo").text_color(tok::text_secondary()).child(text.to_string())
}

fn plural(n: usize) -> &'static str {
    if n == 1 { "" } else { "s" }
}

pub(crate) fn switch(id: impl Into<ElementId>, on: bool) -> Stateful<Div> {
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

/// A row of choices: (label, chosen, available, what choosing runs).
fn chips(id: &'static str, options: Vec<(String, bool, bool, Command)>, cx: &mut Context<StudioWindow>) -> AnyElement {
    let mut row = div().flex().flex_wrap().justify_end().gap_0p5().p_0p5().rounded(px(7.)).bg(tok::bg_panel());
    for (i, (label, on, enabled, cmd)) in options.into_iter().enumerate() {
        row = row.child(
            div()
                .id((id, i))
                .px_2()
                .h(px(24.))
                .flex()
                .items_center()
                .rounded(px(5.))
                .text_size(px(11.))
                .when(on, |d| d.bg(tok::bg_selected()).text_color(tok::text()))
                .when(!on && enabled, |d| {
                    d.cursor_pointer().text_color(tok::text_secondary()).hover(|d| d.bg(tok::bg_raised()))
                })
                .when(!enabled, |d| d.text_color(tok::text_tertiary()).opacity(0.5))
                .child(label)
                .when(enabled && !on, |d| d.on_click(cx.listener(move |this, _, _, cx| this.exec(cmd.clone(), cx)))),
        );
    }
    row.into_any_element()
}

fn stepper(
    id: &'static str,
    value: String,
    minus: Command,
    plus: Command,
    cx: &mut Context<StudioWindow>,
) -> AnyElement {
    div()
        .flex()
        .items_center()
        .gap_1()
        .child(
            Button::new((id, 0usize))
                .icon(ui::icon("minus"))
                .xsmall()
                .ghost()
                .on_click(cx.listener(move |this, _, _, cx| this.exec(minus.clone(), cx))),
        )
        .child(div().w(px(80.)).text_center().text_size(px(12.)).font_family("Menlo").child(value))
        .child(
            Button::new((id, 1usize))
                .icon(ui::icon("plus"))
                .xsmall()
                .ghost()
                .on_click(cx.listener(move |this, _, _, cx| this.exec(plus.clone(), cx))),
        )
        .into_any_element()
}

/// Canvas size, 1080p, 720p, 480p.
pub(crate) fn height_choices(
    current: Option<u32>,
    make: impl Fn(Option<u32>) -> Command,
) -> Vec<(String, bool, bool, Command)> {
    [None, Some(1080), Some(720), Some(480)]
        .into_iter()
        .map(|h| (h.map_or("Canvas".to_string(), |h| format!("{h}p")), current == h, true, make(h)))
        .collect()
}

fn reveal_button(id: impl Into<ElementId>, dir: Option<std::path::PathBuf>) -> AnyElement {
    match dir {
        Some(dir) => ui::icon_button(id, "folder-open", tok::text_secondary())
            .on_click(move |_, _, cx| {
                let _ = std::fs::create_dir_all(&dir);
                cx.reveal_path(&dir)
            })
            .into_any_element(),
        None => div().into_any_element(),
    }
}
