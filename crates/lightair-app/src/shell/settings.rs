//! Settings (⌘,; docs/14-data-persistence-settings.md, "Settings"): a window with a page per
//! area. Recording, streaming, clips and canvases travel with the project; General is this
//! Mac's. Each destination has its own settings too, in Outputs (click it).

use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::menu::DropdownMenu;
use gpui_kit::component::{Disableable, Sizable};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;
use lightair_core::{ClipSettings, Command, Format, Reconnect, RecordingSettings, VideoEncoder};

use gpui_kit::component::input::{Input, InputState};

use super::StudioWindow;
use crate::ui::{self, tok};

/// A canvas being edited: its name and size fields, and the frame rate picked.
pub(crate) struct CanvasEdit {
    id: lightair_core::Id,
    name: Entity<InputState>,
    width: Entity<InputState>,
    height: Entity<InputState>,
    fps: u32,
    error: Option<String>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Page {
    General,
    Audio,
    Camera,
    Recording,
    Streaming,
    Clips,
    Canvases,
    Shortcuts,
    About,
}

const PAGES: &[(Page, &str, &str)] = &[
    (Page::General, "General", "settings"),
    (Page::Audio, "Audio", "audio-lines"),
    (Page::Recording, "Recording", "circle-dot"),
    (Page::Streaming, "Streaming", "radio"),
    (Page::Clips, "Clips", "scissors"),
    (Page::Canvases, "Canvases", "proportions"),
    (Page::Camera, "Camera", "camera"),
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

    pub(super) fn edit_canvas(&mut self, id: lightair_core::Id, window: &mut Window, cx: &mut Context<Self>) {
        let Some(f) = self.studio.read(cx).project().format(id).cloned() else { return };
        let field = |value: String, window: &mut Window, cx: &mut Context<Self>| {
            let e = cx.new(|cx| InputState::new(window, cx));
            e.update(cx, |i, cx| i.set_value(value, window, cx));
            e
        };
        let name = field(f.name.clone(), window, cx);
        let width = field(f.width.to_string(), window, cx);
        let height = field(f.height.to_string(), window, cx);
        let fps = f.fps_num / f.fps_den.max(1);
        self.canvas_edit = Some(CanvasEdit { id, name, width, height, fps, error: None });
        cx.notify();
    }

    /// Applies the canvas edit: the new size, with every top-level object reframed into it.
    fn apply_canvas_edit(&mut self, cx: &mut Context<Self>) {
        let Some(edit) = &mut self.canvas_edit else { return };
        let project = self.studio.read(cx).project().clone();
        let Some(old) = project.format(edit.id).cloned() else { return };
        let num = |e: &Entity<InputState>, cx: &App| e.read(cx).value().trim().parse::<u32>().ok();
        let (Some(w), Some(h)) = (num(&edit.width, cx), num(&edit.height, cx)) else {
            edit.error = Some("Width and height are whole numbers of pixels.".into());
            cx.notify();
            return;
        };
        if !(16..=7680).contains(&w) || !(16..=7680).contains(&h) {
            edit.error = Some("A canvas is 16 to 7680 pixels each way.".into());
            cx.notify();
            return;
        }
        let name = edit.name.read(cx).value().trim().to_string();
        let new = Format {
            name: if name.is_empty() { old.name.clone() } else { name },
            width: w & !1,
            height: h & !1,
            fps_num: edit.fps,
            fps_den: 1,
            ..old.clone()
        };
        let mut commands = vec![Command::UpdateFormat { format: new.clone() }];
        if (new.width, new.height) != (old.width, old.height) {
            for scene in &project.scenes {
                for layer in scene.layers.iter().filter(|l| l.parent.is_none()) {
                    if let Some(l) = layer.layouts.get(&old.id) {
                        let layout = crate::reframe::reframe(l, &old, &new);
                        commands.push(Command::SetLayout { scene: scene.id, layer: layer.id, format: old.id, layout });
                    }
                }
            }
        }
        self.canvas_edit = None;
        self.exec(Command::Batch { label: format!("Change {}", new.name.to_lowercase()), commands }, cx);
    }

    /// The keys pressed for the global shortcut being set.
    fn capture_hotkey(&mut self, k: &Keystroke, cx: &mut Context<Self>) {
        let Some(action) = self.hotkey_capture else { return };
        if k.key == "escape" {
            self.hotkey_capture = None;
            cx.notify();
            return;
        }
        // Wait for a key, not just modifiers.
        if matches!(k.key.as_str(), "shift" | "control" | "alt" | "cmd" | "platform" | "function" | "") {
            return;
        }
        let mut parts = Vec::new();
        for (on, name) in [
            (k.modifiers.control, "ctrl"),
            (k.modifiers.alt, "alt"),
            (k.modifiers.shift, "shift"),
            (k.modifiers.platform, "cmd"),
        ] {
            if on {
                parts.push(name.to_string());
            }
        }
        parts.push(k.key.to_lowercase());
        let keys = parts.join("-");
        if crate::hotkeys::parse(&keys).is_none() {
            self.hotkey_note = Some("Use ⌘, ⌃ or ⌥ with a letter or number, or an F key.".into());
            cx.notify();
            return;
        }
        self.hotkey_capture = None;
        let mut list = self.prefs.global_hotkeys.clone();
        list.retain(|h| h.action != action && h.keys != keys);
        list.push(crate::prefs::GlobalHotkey { action, keys: keys.clone() });
        let refused = crate::hotkeys::register(&list.iter().map(|h| (h.action, h.keys.clone())).collect::<Vec<_>>());
        if refused.contains(&action) {
            list.retain(|h| h.action != action);
            crate::hotkeys::register(&list.iter().map(|h| (h.action, h.keys.clone())).collect::<Vec<_>>());
            self.hotkey_note = Some(format!("{} is taken by macOS or another app.", crate::hotkeys::display(&keys)));
        } else {
            self.hotkey_note = None;
        }
        self.set_pref(|p| p.global_hotkeys = list, cx);
    }

    fn clear_hotkey(&mut self, action: crate::hotkeys::GlobalAction, cx: &mut Context<Self>) {
        let mut list = self.prefs.global_hotkeys.clone();
        list.retain(|h| h.action != action);
        crate::hotkeys::register(&list.iter().map(|h| (h.action, h.keys.clone())).collect::<Vec<_>>());
        self.set_pref(|p| p.global_hotkeys = list, cx);
    }

    /// The audio engine's buffer (frames), now and from now on.
    fn set_audio_block(&mut self, frames: usize, cx: &mut Context<Self>) {
        let project = self.studio.read(cx).project().clone();
        let done = self.studio.read(cx).engine().call(move |s| s.reconciler.set_audio_block(frames, &project));
        if done {
            self.set_pref(|p| p.audio_block = frames, cx);
        } else {
            self.studio.update(cx, |s, _| s.issue = Some("Change the buffer after the recording ends.".into()));
        }
    }

    /// Sets the mixer up as a template (one undo step), with the settings it wants.
    pub(super) fn use_audio_template(&mut self, t: lightair_core::templates::Template, cx: &mut Context<Self>) {
        let project = self.studio.read(cx).project().clone();
        self.exec(crate::mixer::template_command(&project, t), cx);
        if let Some(frames) = t.block() {
            self.set_audio_block(frames, cx);
        }
        if t == lightair_core::templates::Template::Podcast {
            crate::prefs::LOUDNESS_TARGET.store(-16, std::sync::atomic::Ordering::Relaxed);
            self.set_pref(|p| p.loudness_target = -16, cx);
        }
        cx.notify();
    }

    /// Tells the audio engine where and what to monitor, from the preferences.
    fn apply_monitor(&self, cx: &mut Context<Self>) {
        let p = &self.prefs;
        let device = p.direct_monitor.then(|| p.monitor_device.clone().map_or(String::new(), |(name, _)| name));
        let program = p.monitor_program;
        self.studio.read(cx).engine().call(move |s| s.reconciler.set_monitor(device, program));
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
                        this.hotkey_capture = None;
                        if p == Page::Audio {
                            this.monitor_devices =
                                this.studio.read(cx).engine().call(|_| lightair_engine::monitoring::devices());
                        }
                        cx.notify();
                    })),
            );
        }
        let content = match page {
            Page::General => self.general_page(cx),
            Page::Audio => self.audio_page(cx),
            Page::Camera => self.camera_page(cx),
            Page::Recording => self.recording_page(cx),
            Page::Streaming => self.streaming_page(cx),
            Page::Clips => self.clips_page(cx),
            Page::Canvases => self.canvases_page(cx),
            Page::Shortcuts => self.shortcuts_page(cx),
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
                            if this.hotkey_capture.is_some() {
                                cx.stop_propagation();
                                this.capture_hotkey(&ev.keystroke, cx);
                            } else if ev.keystroke.key == "escape" {
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

    fn audio_page(&self, cx: &mut Context<Self>) -> Div {
        let prefs = &self.prefs;
        let current = prefs.monitor_device.clone().unwrap_or(("System default".into(), "default".into()));
        let devices = self.monitor_devices.clone();
        let me = cx.entity().downgrade();
        let device = Button::new("monitor-device").label(current.0.clone()).small().outline().dropdown_menu(
            move |mut menu, _, _| {
                for (name, id) in devices.iter().cloned() {
                    let me = me.clone();
                    let on = id == current.1;
                    menu = menu.item(gpui_kit::component::menu::PopupMenuItem::new(name.clone()).checked(on).on_click(
                        move |_, _, cx| {
                            if let Some(me) = me.upgrade() {
                                let (n, i) = (name.clone(), id.clone());
                                me.update(cx, |this, cx| {
                                    let (n2, i2) = (n.clone(), i.clone());
                                    this.studio
                                        .read(cx)
                                        .engine()
                                        .call(move |_| lightair_engine::monitoring::set(&n2, &i2));
                                    this.set_pref(|p| p.monitor_device = (i != "default").then_some((n, i)), cx);
                                    this.apply_monitor(cx);
                                });
                            }
                        },
                    ));
                }
                menu
            },
        );
        let (direct, program) = (prefs.direct_monitor, prefs.monitor_program);
        let mut hear = div().flex().gap_0p5().p_0p5().rounded(px(7.)).bg(tok::bg_panel());
        for (i, (p, label)) in [(false, "Cued tracks"), (true, "The stream")].into_iter().enumerate() {
            hear = hear.child(pref_chip(("monitor-hear", i), label.to_string(), program == p).on_click(cx.listener(
                move |this, _, _, cx| {
                    this.set_pref(|pr| pr.monitor_program = p, cx);
                    this.apply_monitor(cx);
                },
            )));
        }
        let rate = prefs.sample_rate;
        let running = crate::prefs::RUNNING_SAMPLE_RATE.load(std::sync::atomic::Ordering::Relaxed);
        let rates: Vec<(String, bool, bool, Option<u32>)> = [(48_000, "48 kHz"), (44_100, "44.1 kHz")]
            .into_iter()
            .map(|(r, l)| (l.to_string(), rate == r, true, Some(r)))
            .collect();
        let mut rate_chips = div().flex().gap_0p5().p_0p5().rounded(px(7.)).bg(tok::bg_panel());
        for (i, (label, on, _, r)) in rates.into_iter().enumerate() {
            rate_chips = rate_chips.child(pref_chip(("sample-rate", i), label, on).on_click(cx.listener(
                move |this, _, _, cx| {
                    if let Some(r) = r {
                        this.set_pref(|p| p.sample_rate = r, cx);
                    }
                },
            )));
        }
        let block = prefs.audio_block;
        let mut blocks = div().flex().gap_0p5().p_0p5().rounded(px(7.)).bg(tok::bg_panel());
        for (i, frames) in lightair_engine::audio_host::BLOCKS.into_iter().enumerate() {
            let ms = frames as f32 * 1000.0 / running.max(1) as f32;
            blocks = blocks.child(
                pref_chip(("audio-block", i), format!("{frames} · {ms:.1} ms"), block == frames)
                    .on_click(cx.listener(move |this, _, _, cx| this.set_audio_block(frames, cx))),
            );
        }
        let mut templates = group("Start from a template");
        for (i, t) in lightair_core::templates::Template::ALL.into_iter().enumerate() {
            templates = templates.child(row(
                t.name(),
                Some(t.description()),
                Button::new(("audio-template", i)).label("Use").small().outline().on_click(
                    cx.listener(move |this, _, _, cx| this.use_audio_template(t, cx)),
                ),
            ));
        }
        templates = templates.child(note("Replaces the mixer's tracks, buses and mixes. Undo brings them back."));
        let target = prefs.loudness_target;
        let mut targets = div().flex().gap_0p5().p_0p5().rounded(px(7.)).bg(tok::bg_panel());
        for (i, (t, label)) in
            [(-14, "−14 Streaming"), (-16, "−16 Podcasts"), (-23, "−23 Broadcast")].into_iter().enumerate()
        {
            targets = targets.child(pref_chip(("loudness-target", i), label.to_string(), target == t).on_click(
                cx.listener(move |this, _, _, cx| {
                    crate::prefs::LOUDNESS_TARGET.store(t, std::sync::atomic::Ordering::Relaxed);
                    this.set_pref(|p| p.loudness_target = t, cx);
                }),
            ));
        }
        pages()
            .child(
                group("Listening")
                    .child(row("Headphones", Some("Where Listen in headphones plays."), device))
                    .child(row(
                        "Low-latency monitoring",
                        Some("Straight from the mixer: a few milliseconds, for singing and playing along."),
                        switch("direct-monitor", direct).on_click(cx.listener(move |this, _, _, cx| {
                            this.set_pref(|p| p.direct_monitor = !direct, cx);
                            this.apply_monitor(cx);
                        })),
                    ))
                    .child(row("Hear", Some("The tracks you cue, or exactly what viewers hear."), hear)),
            )
            .child(templates)
            .child(
                group("Loudness")
                    .child(row("Target", Some("The mixer's LUFS readout turns green on it."), targets))
                    .child(note("YouTube, Twitch and Spotify even out to about −14 LUFS; Apple Podcasts −16; TV −23.")),
            )
            .child(
                group("Engine")
                    .child(row("Sample rate", Some("48 kHz suits video; 44.1 kHz matches music files."), rate_chips))
                    .child(row(
                        "Buffer",
                        Some("Smaller: less delay when you monitor yourself. Larger: lighter on the Mac."),
                        blocks,
                    ))
                    .when(rate != running, |g| {
                        g.child(
                            div()
                                .px_1()
                                .text_size(px(11.))
                                .text_color(tok::warn())
                                .child("Takes effect the next time Lightair opens."),
                        )
                    }),
            )
    }

    fn camera_page(&self, cx: &mut Context<Self>) -> Div {
        let cam = self.prefs.camera;
        let mut sizes = div().flex().gap_0p5().p_0p5().rounded(px(7.)).bg(tok::bg_panel());
        for (i, (h, label)) in
            [(None, "Best"), (Some(720), "720p"), (Some(1080), "1080p"), (Some(2160), "4K")].into_iter().enumerate()
        {
            sizes = sizes.child(
                pref_chip(("camera-size", i), label.to_string(), cam.height == h)
                    .on_click(cx.listener(move |this, _, _, cx| this.set_pref(|p| p.camera.height = h, cx))),
            );
        }
        let studio_on = cam.studio_on;
        pages()
            .child(
                group("New cameras")
                    .child(row("Resolution", Some("Best uses the most the camera offers."), sizes))
                    .child(row(
                        "Beauty on",
                        Some("Start with the Beauty effect (adjust it in the Inspector)."),
                        switch("camera-studio", studio_on).on_click(
                            cx.listener(move |this, _, _, cx| this.set_pref(|p| p.camera.studio_on = !studio_on, cx)),
                        ),
                    )),
            )
            .child(note("Cameras already added keep their settings; change them in their Inspector."))
    }

    fn shortcuts_page(&self, cx: &mut Context<Self>) -> Div {
        use crate::hotkeys::{GlobalAction, display};
        let mut global = group("Anywhere on your Mac");
        for (i, action) in GlobalAction::ALL.iter().copied().enumerate() {
            let keys = self.prefs.global_hotkeys.iter().find(|h| h.action == action).map(|h| h.keys.clone());
            let capturing = self.hotkey_capture == Some(action);
            let control = div()
                .flex()
                .items_center()
                .gap_1()
                .child(if capturing {
                    div().text_size(px(12.)).text_color(tok::warn()).child("Press keys… (Esc cancels)")
                } else {
                    value(&keys.as_deref().map(display).unwrap_or_else(|| "—".into()))
                })
                .child(
                    Button::new(("set-hotkey", i))
                        .label(if keys.is_some() { "Change" } else { "Set" })
                        .xsmall()
                        .outline()
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.hotkey_capture = Some(action);
                            this.hotkey_note = None;
                            cx.notify();
                        })),
                )
                .when(keys.is_some(), |d| {
                    d.child(
                        Button::new(("clear-hotkey", i))
                            .icon(ui::icon("x"))
                            .xsmall()
                            .ghost()
                            .on_click(cx.listener(move |this, _, _, cx| this.clear_hotkey(action, cx))),
                    )
                });
            global = global.child(row(&action.label(), None, control));
        }
        if let Some(n) = &self.hotkey_note {
            global = global.child(div().px_1().text_size(px(11.)).text_color(tok::warn()).child(n.clone()));
        }
        let mut page = pages().child(global.child(note(
            "These work whichever app is in front: start a recording or switch scenes from a game or a call.",
        )));
        for (title, items) in SHORTCUTS {
            let mut g = group(title);
            for (what, keys) in *items {
                g = g.child(row(what, None, value(keys)));
            }
            page = page.child(g);
        }
        page
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
            .child(self.multitrack_group(&rec, cx))
    }

    /// Multitrack audio: each track (or those marked Record) to its own file.
    fn multitrack_group(&self, rec: &RecordingSettings, cx: &mut Context<Self>) -> Div {
        use lightair_core::{StemFormat, StemPoint};
        let set = |r: RecordingSettings| Command::SetRecordingSettings { settings: r };
        let (multi, only) = (rec.multitrack, rec.audio_only);
        let (m, o) = (set(RecordingSettings { multitrack: !multi, ..rec.clone() }), set(RecordingSettings { audio_only: !only, ..rec.clone() }));
        let formats: Vec<(String, bool, bool, Command)> = [(StemFormat::Pcm24, "24-bit"), (StemFormat::Float32, "32-bit float")]
            .into_iter()
            .map(|(f, l)| (l.to_string(), rec.stem_format == f, multi || only, set(RecordingSettings { stem_format: f, ..rec.clone() })))
            .collect();
        let points: Vec<(String, bool, bool, Command)> =
            [(StemPoint::Raw, "Before effects"), (StemPoint::PreFader, "After effects"), (StemPoint::PostFader, "As mixed")]
                .into_iter()
                .map(|(p, l)| (l.to_string(), rec.stem_point == p, multi || only, set(RecordingSettings { stem_point: p, ..rec.clone() })))
                .collect();
        let project = self.studio.read(cx).project().clone();
        let mixes: Vec<(String, bool, bool, Command)> = project
            .audio
            .mixes
            .iter()
            .take(5)
            .map(|m| {
                let on = rec.file_mixes.contains(&m.id);
                let mut file_mixes = rec.file_mixes.clone();
                if on {
                    file_mixes.retain(|x| *x != m.id);
                } else {
                    file_mixes.push(m.id);
                }
                (m.name.clone(), on, true, set(RecordingSettings { file_mixes, ..rec.clone() }))
            })
            .collect();
        group("Multitrack audio")
            .when(!mixes.is_empty(), |g| {
                g.child(row(
                    "Mixes in the video file",
                    Some("Extra audio tracks after the program (MOV or MKV keep them all)."),
                    chips("rec-file-mixes", mixes, cx),
                ))
            })
            .child(row(
                "Each track to its own file",
                Some("WAV files beside the video, lined up to the sample. Tracks marked Record, or all of them."),
                switch("rec-multi", multi).on_click(cx.listener(move |this, _, _, cx| this.exec(m.clone(), cx))),
            ))
            .child(row(
                "Audio only",
                Some("Podcasts and music: the track files and the mix, no video."),
                switch("rec-audio-only", only).on_click(cx.listener(move |this, _, _, cx| this.exec(o.clone(), cx))),
            ))
            .child(row(
                "Format",
                Some(match rec.stem_format {
                    StemFormat::Pcm24 => "Opens in every editor and DAW.",
                    StemFormat::Float32 => "Can't clip: for music.",
                }),
                chips("rec-stem-format", formats, cx),
            ))
            .child(row(
                "Record",
                Some(match rec.stem_point {
                    StemPoint::Raw => "The clean sources, to process later.",
                    StemPoint::PreFader => "With each track's effects, at full level.",
                    StemPoint::PostFader => "As the mix has them: faders and pan.",
                }),
                chips("rec-stem-point", points, cx),
            ))
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
        let busy = lightair_engine::outputs::runtime().any_stream_running()
            || lightair_engine::outputs::runtime().any_recording_running();
        let mut list = group("Canvases");
        for (i, f) in project.formats.iter().enumerate() {
            let id = f.id;
            if let Some(edit) = self.canvas_edit.as_ref().filter(|e| e.id == id) {
                list = list.child(self.canvas_editor(f, edit, busy, cx));
                continue;
            }
            let detail = format!("{}×{} · {} fps", f.width, f.height, f.fps_num / f.fps_den.max(1));
            let actions = div()
                .flex()
                .gap_1()
                .child(
                    Button::new(("edit-format", i))
                        .label("Edit")
                        .xsmall()
                        .outline()
                        .on_click(cx.listener(move |this, _, window, cx| this.edit_canvas(id, window, cx))),
                )
                .child(if id == primary {
                    div().px_2().text_size(px(11.)).text_color(tok::text_tertiary()).child("Main").into_any_element()
                } else {
                    Button::new(("remove-format", i))
                        .label("Remove")
                        .xsmall()
                        .ghost()
                        .on_click(
                            cx.listener(move |this, _, _, cx| this.exec(Command::RemoveFormat { format: id }, cx)),
                        )
                        .into_any_element()
                });
            list = list.child(row(&f.name, Some(&detail), actions));
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

    /// Editing one canvas: name, size (presets for its shape, or typed), frame rate.
    fn canvas_editor(&self, f: &Format, edit: &CanvasEdit, busy: bool, cx: &mut Context<Self>) -> Div {
        let vertical = f.height > f.width;
        let square = f.height == f.width;
        let presets: &[(u32, u32)] = if square {
            &[(1080, 1080), (720, 720), (1440, 1440)]
        } else if vertical {
            &[(1080, 1920), (720, 1280), (1440, 2560), (2160, 3840)]
        } else {
            &[(1920, 1080), (1280, 720), (2560, 1440), (3840, 2160)]
        };
        let (fw, fh) = (edit.width.clone(), edit.height.clone());
        let mut sizes = div().flex().flex_wrap().gap_1();
        for (i, (w, h)) in presets.iter().copied().enumerate() {
            let (fw, fh) = (fw.clone(), fh.clone());
            sizes =
                sizes.child(Button::new(("canvas-preset", i)).label(format!("{w}×{h}")).xsmall().outline().on_click(
                    cx.listener(move |_, _, window, cx| {
                        fw.update(cx, |i, cx| i.set_value(w.to_string(), window, cx));
                        fh.update(cx, |i, cx| i.set_value(h.to_string(), window, cx));
                    }),
                ));
        }
        let mut rates = div().flex().gap_0p5().p_0p5().rounded(px(7.)).bg(tok::bg_panel());
        for (i, r) in [24, 25, 30, 50, 60].into_iter().enumerate() {
            let on = edit.fps == r;
            rates = rates.child(
                div()
                    .id(("canvas-fps", i))
                    .px_2()
                    .h(px(24.))
                    .flex()
                    .items_center()
                    .rounded(px(5.))
                    .text_size(px(11.))
                    .cursor_pointer()
                    .when(on, |d| d.bg(tok::bg_selected()).text_color(tok::text()))
                    .when(!on, |d| d.text_color(tok::text_secondary()).hover(|d| d.bg(tok::bg_raised())))
                    .child(format!("{r} fps"))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if let Some(e) = &mut this.canvas_edit {
                            e.fps = r;
                        }
                        cx.notify();
                    })),
            );
        }
        let label = |t: &'static str| div().w(px(90.)).text_size(px(12.)).text_color(tok::text_secondary()).child(t);
        div()
            .flex()
            .flex_col()
            .gap_2()
            .p_3()
            .rounded(px(8.))
            .bg(tok::bg_raised())
            .border_1()
            .border_color(tok::border_strong())
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(label("Name"))
                    .child(div().flex_1().child(Input::new(&edit.name).small())),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(label("Size"))
                    .child(div().w(px(80.)).child(Input::new(&edit.width).small()))
                    .child("×")
                    .child(div().w(px(80.)).child(Input::new(&edit.height).small()))
                    .child(div().text_size(px(11.)).text_color(tok::text_tertiary()).child("pixels")),
            )
            .child(div().flex().items_center().gap_2().child(div().w(px(90.))).child(sizes))
            .child(div().flex().items_center().gap_2().child(label("Frame rate")).child(rates))
            .child(note("Objects are reframed to keep their place when the size changes."))
            .when(busy, |d| {
                d.child(
                    div()
                        .text_size(px(11.))
                        .text_color(tok::warn())
                        .child("Stop recording and streaming to change a canvas."),
                )
            })
            .when_some(edit.error.clone(), |d, e| d.child(div().text_size(px(11.)).text_color(tok::error()).child(e)))
            .child(
                div()
                    .flex()
                    .gap_2()
                    .child(
                        Button::new("canvas-apply")
                            .label("Apply")
                            .small()
                            .primary()
                            .disabled(busy)
                            .on_click(cx.listener(|this, _, _, cx| this.apply_canvas_edit(cx))),
                    )
                    .child(Button::new("canvas-cancel").label("Cancel").small().ghost().on_click(cx.listener(
                        |this, _, _, cx| {
                            this.canvas_edit = None;
                            cx.notify();
                        },
                    ))),
            )
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

fn pref_chip(id: impl Into<ElementId>, label: String, on: bool) -> Stateful<Div> {
    div()
        .id(id)
        .px_2()
        .h(px(24.))
        .flex()
        .items_center()
        .rounded(px(5.))
        .text_size(px(11.))
        .cursor_pointer()
        .when(on, |d| d.bg(tok::bg_selected()).text_color(tok::text()))
        .when(!on, |d| d.text_color(tok::text_secondary()).hover(|d| d.bg(tok::bg_raised())))
        .child(label)
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
