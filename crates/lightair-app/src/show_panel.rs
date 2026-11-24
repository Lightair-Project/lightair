//! Panels for running the show (docs/13-extensibility-and-compatibility.md, "Built-in
//! replacements"): the Soundboard (pads that play sounds into the mix), Notes (each scene's
//! notes, and a teleprompter that scrolls them) and the Rundown (the show's segments in order,
//! with a clock for each and Next to move on, switching scenes as it goes).
//!
//! What they keep is the workspace's (`Show` and `Scene::notes`), so it's undoable and travels
//! with the show; what's running (the pad playing, the segment on, the prompter's place) is
//! the panel's.

use std::time::{Duration, Instant};

use gpui_kit::component::{Disableable, Sizable};
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Editor, EditorState, Input as TextInput, InputEvent, InputState};
use gpui_kit::component::menu::{ContextMenuExt, PopupMenuItem};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;
use lightair_model::{Command, Id, Pad, Segment, Show, Source, Track, Workspace};
use serde_json::json;

use crate::studio::Studio;
use crate::ui::{self, tok};

fn exec(studio: &Entity<Studio>, cmd: Command, cx: &mut App) {
    studio.update(cx, |s, cx| {
        let _ = s.execute(cmd, cx);
    });
}

fn set_show(studio: &Entity<Studio>, label: &str, f: impl FnOnce(&mut Show), cx: &mut App) {
    let mut show = studio.read(cx).workspace().show.clone();
    f(&mut show);
    exec(studio, Command::SetShow { show, label: label.into() }, cx);
}

/// A chip that's on or off (its click is the caller's).
fn chip(id: impl Into<ElementId>, label: impl Into<SharedString>, on: bool) -> Stateful<Div> {
    div()
        .id(id)
        .px_2()
        .h(px(22.))
        .flex()
        .items_center()
        .rounded(px(5.))
        .text_size(px(11.))
        .cursor_pointer()
        .when(on, |d| d.bg(tok::bg_selected()).text_color(tok::text()))
        .when(!on, |d| d.text_color(tok::text_secondary()).hover(|d| d.bg(tok::bg_raised())))
        .child(label.into())
}

fn empty(icon: &str, title: &str, about: &str) -> Div {
    div()
        .flex()
        .flex_col()
        .items_center()
        .justify_center()
        .gap_2()
        .flex_1()
        .p_6()
        .child(ui::icon(icon).size(px(28.)).text_color(tok::text_tertiary()))
        .child(div().text_size(px(13.)).text_color(tok::text_secondary()).child(title.to_string()))
        .child(div().max_w(px(360.)).text_size(px(12.)).text_color(tok::text_tertiary()).text_center().child(about.to_string()))
}

fn toolbar() -> Div {
    div().flex().items_center().gap_1p5().px_3().py_2().border_b_1().border_color(tok::border())
}

macro_rules! panel_traits {
    ($ty:ty, $name:expr, $icon:expr, $title:expr) => {
        impl gpui_kit::component::dock::BasePanel for $ty {
            fn panel_name(&self) -> &'static str {
                $name
            }
        }
        impl gpui_kit::component::dock::Panel for $ty {
            fn tab_name(&self, _: &App) -> Option<SharedString> {
                None
            }
            fn title(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
                ui::panel_title($icon, $title)
            }
            fn title_style(&self, _: &App) -> Option<gpui_kit::component::dock::TitleStyle> {
                Some(ui::panel_header_style())
            }
            fn zoom_control(&self, _: &App) -> Option<gpui_kit::component::dock::PanelControl> {
                None
            }
            fn inner_padding(&self, _: &App) -> bool {
                false
            }
        }
        impl EventEmitter<gpui_kit::component::dock::PanelEvent> for $ty {}
        impl Focusable for $ty {
            fn focus_handle(&self, _: &App) -> FocusHandle {
                self.focus.clone()
            }
        }
    };
}

// ---------------------------------------------------------------------------------------------
// Soundboard

/// The soundboard's source in `w`, if it has one.
pub fn board_source(w: &Workspace) -> Option<Id> {
    w.sources.values().find(|s| s.kind == lightair_engine::soundboard::KIND).map(|s| s.id)
}

/// Plays `pad` on the workspace's soundboard (`None`: stops it). Used by the panel, the remote
/// API and automations.
pub fn play_pad(studio: &Studio, pad: Option<&Pad>) -> bool {
    let Some(src) = board_source(studio.workspace()) else { return false };
    let what = pad.map(|p| (p.file.clone(), p.looped, p.volume / 100.0));
    studio.engine().post(move |s| match &what {
        Some((file, looped, volume)) => s.reconciler.soundboard_play(src, Some((file.as_str(), *looped, *volume))),
        None => s.reconciler.soundboard_play(src, None),
    });
    true
}

/// Colours offered for pads (0xRRGGBB).
const PAD_COLOURS: &[u32] = &[0x5B8DEF, 0xF5A524, 0x17C964, 0xF31260, 0x9353D3, 0x06B7DB, 0xFF7A45, 0x8A8F98];

pub struct SoundboardPanel {
    studio: Entity<Studio>,
    focus: FocusHandle,
    playing: Option<Id>,
}

impl SoundboardPanel {
    pub fn new(studio: Entity<Studio>, cx: &mut Context<Self>) -> Self {
        cx.observe(&studio, |_, _, cx| cx.notify()).detach();
        // the pad playing loses its highlight when its sound ends
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(Duration::from_millis(300)).await;
                let Ok(()) = this.update(cx, |p, cx| {
                    if p.playing.is_some() {
                        let studio = p.studio.read(cx);
                        let playing = board_source(studio.workspace())
                            .is_some_and(|src| studio.engine().call(move |s| s.reconciler.soundboard_playing(src)));
                        if !playing {
                            p.playing = None;
                            cx.notify();
                        }
                    }
                }) else {
                    break;
                };
            }
        })
        .detach();
        Self { studio, focus: cx.focus_handle(), playing: None }
    }

    fn add_sounds(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let paths = cx.prompt_for_paths(PathPromptOptions { files: true, directories: false, multiple: true, prompt: None });
        let studio = self.studio.clone();
        cx.spawn_in(window, async move |_, cx| {
            let Ok(Ok(Some(paths))) = paths.await else { return };
            let _ = cx.update(|_, cx| {
                let workspace = studio.read(cx).workspace().clone();
                let mut commands = Vec::new();
                // the first pad brings the soundboard: its source and its mixer track
                if board_source(&workspace).is_none() {
                    let source = Source {
                        id: Id::new(),
                        name: "Soundboard".into(),
                        kind: lightair_engine::soundboard::KIND.into(),
                        settings: json!({}),
                        effects: Vec::new(),
                    };
                    let track = Track::new("Soundboard", source.id);
                    commands.push(Command::AddSource { source });
                    commands.push(Command::AddTrack { track, index: None });
                }
                let mut show = workspace.show.clone();
                for path in paths {
                    let name = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| "Sound".into());
                    let mut pad = Pad::new(name, path.to_string_lossy());
                    pad.color = PAD_COLOURS[show.pads.len() % PAD_COLOURS.len()];
                    show.pads.push(pad);
                }
                commands.push(Command::SetShow { show, label: "Add to the soundboard".into() });
                exec(&studio, Command::Batch { label: "Add to the soundboard".into(), commands }, cx);
            });
        })
        .detach();
    }

    fn press(&mut self, pad: &Pad, cx: &mut Context<Self>) {
        let again = self.playing == Some(pad.id);
        let studio = self.studio.read(cx);
        if again && pad.looped {
            play_pad(studio, None);
            self.playing = None;
        } else if play_pad(studio, Some(pad)) {
            self.playing = Some(pad.id);
        }
        cx.notify();
    }
}

impl Render for SoundboardPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let _ = window;
        let pads = self.studio.read(cx).workspace().show.pads.clone();
        let studio = self.studio.clone();
        let bar = toolbar()
            .child(
                Button::new("sb-add")
                    .label("Add sounds…")
                    .icon(ui::icon("plus"))
                    .xsmall()
                    .outline()
                    .on_click(cx.listener(|this, _, window, cx| this.add_sounds(window, cx))),
            )
            .child(div().flex_1())
            .when(self.playing.is_some(), |d| {
                d.child(Button::new("sb-stop").label("Stop").icon(ui::icon("square")).xsmall().ghost().on_click(cx.listener(
                    |this, _, _, cx| {
                        play_pad(this.studio.read(cx), None);
                        this.playing = None;
                        cx.notify();
                    },
                )))
            });
        let body = if pads.is_empty() {
            empty(
                "audio-lines",
                "Sounds for your show",
                "Add sound files (a laugh track, a drum roll, an airhorn) and play them on stream with a click. They go through the mixer's Soundboard track.",
            )
            .into_any_element()
        } else {
            let mut grid = div().flex().flex_wrap().gap_2().p_3();
            for (i, pad) in pads.iter().enumerate() {
                let on = self.playing == Some(pad.id);
                let colour = rgb(pad.color);
                let p = pad.clone();
                let (s1, s2, s3) = (studio.clone(), studio.clone(), studio.clone());
                let (id, looped, count) = (pad.id, pad.looped, pads.len());
                grid = grid.child(
                    div()
                        .id(("pad", i))
                        .relative()
                        .w(px(132.))
                        .h(px(68.))
                        .p_2()
                        .flex()
                        .flex_col()
                        .justify_between()
                        .rounded(px(8.))
                        .border_2()
                        .cursor_pointer()
                        .bg(if on { colour.into() } else { tok::bg_raised() })
                        .border_color(colour)
                        .hover(|d| d.opacity(0.9))
                        .child(
                            div()
                                .text_size(px(12.))
                                .font_weight(FontWeight::SEMIBOLD)
                                .text_color(if on { gpui::white() } else { tok::text() })
                                .line_clamp(2)
                                .child(pad.name.clone()),
                        )
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .gap_1()
                                .text_size(px(10.))
                                .text_color(if on { gpui::white() } else { tok::text_tertiary() })
                                .when(pad.looped, |d| d.child(ui::icon("repeat").size(px(11.))))
                                .when(!pad.hotkey.is_empty(), |d| d.child(pad.hotkey.clone())),
                        )
                        .on_click(cx.listener(move |this, _, _, cx| this.press(&p, cx)))
                        .context_menu(move |m, _, _| {
                            let (s1, s2, s3) = (s1.clone(), s2.clone(), s3.clone());
                            m.item(PopupMenuItem::new(if looped { "Play once" } else { "Loop" }).icon(ui::icon("repeat")).on_click(
                                move |_, _, cx| {
                                    set_show(&s1, "Change a pad", |s| {
                                        if let Some(p) = s.pads.iter_mut().find(|p| p.id == id) {
                                            p.looped = !p.looped;
                                        }
                                    }, cx)
                                },
                            ))
                            .item(PopupMenuItem::new("Next colour").icon(ui::icon("palette")).on_click(move |_, _, cx| {
                                set_show(&s2, "Change a pad", |s| {
                                    if let Some(p) = s.pads.iter_mut().find(|p| p.id == id) {
                                        let k = PAD_COLOURS.iter().position(|c| *c == p.color).map_or(0, |k| k + 1);
                                        p.color = PAD_COLOURS[k % PAD_COLOURS.len()];
                                    }
                                }, cx)
                            }))
                            .separator()
                            .item(
                                PopupMenuItem::new("Move earlier")
                                    .icon(ui::icon("arrow-left"))
                                    .disabled(i == 0)
                                    .on_click({
                                        let s = s3.clone();
                                        move |_, _, cx| set_show(&s, "Reorder pads", |sh| sh.pads.swap(i, i - 1), cx)
                                    }),
                            )
                            .item(
                                PopupMenuItem::new("Move later")
                                    .icon(ui::icon("arrow-right"))
                                    .disabled(i + 1 >= count)
                                    .on_click({
                                        let s = s3.clone();
                                        move |_, _, cx| set_show(&s, "Reorder pads", |sh| sh.pads.swap(i, i + 1), cx)
                                    }),
                            )
                            .separator()
                            .item(PopupMenuItem::new("Remove").icon(ui::icon("trash")).on_click(move |_, _, cx| {
                                set_show(&s3, "Remove a pad", |s| s.pads.retain(|p| p.id != id), cx)
                            }))
                        }),
                );
            }
            div().id("pads").flex_1().min_h_0().overflow_y_scroll().child(grid).into_any_element()
        };
        div().flex().flex_col().size_full().bg(tok::bg_panel()).child(bar).child(body)
    }
}

panel_traits!(SoundboardPanel, "soundboard", "audio-lines", "Soundboard");

// ---------------------------------------------------------------------------------------------
// Notes and teleprompter

pub struct NotesPanel {
    studio: Entity<Studio>,
    focus: FocusHandle,
    /// The editor and the scene it edits.
    editor: Option<(Id, Entity<EditorState>, Subscription)>,
    /// Keystrokes counted, so only the last one's pause saves.
    typed: u64,
    /// The teleprompter: on, scrolling, its speed (px a second), where it is, its text size.
    prompter: bool,
    rolling: bool,
    speed: f32,
    offset: f32,
    size: f32,
    last_tick: Instant,
}

impl NotesPanel {
    pub fn new(studio: Entity<Studio>, cx: &mut Context<Self>) -> Self {
        cx.observe(&studio, |_, _, cx| cx.notify()).detach();
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(Duration::from_millis(33)).await;
                let Ok(()) = this.update(cx, |p, cx| {
                    let now = Instant::now();
                    let dt = now.duration_since(p.last_tick).as_secs_f32();
                    p.last_tick = now;
                    if p.prompter && p.rolling {
                        p.offset += p.speed * dt;
                        cx.notify();
                    }
                }) else {
                    break;
                };
            }
        })
        .detach();
        Self {
            studio,
            focus: cx.focus_handle(),
            editor: None,
            typed: 0,
            prompter: false,
            rolling: false,
            speed: 40.0,
            offset: 0.0,
            size: 34.0,
            last_tick: Instant::now(),
        }
    }

    /// The scene whose notes show: the one being edited (selected), else the one on air.
    fn scene(&self, cx: &App) -> Option<Id> {
        let s = self.studio.read(cx);
        s.selected_scene.or(s.workspace().program)
    }

    fn save(&mut self, cx: &mut Context<Self>) {
        let Some((scene, state, _)) = &self.editor else { return };
        let text = state.read(cx).value().to_string();
        let scene = *scene;
        let same = self.studio.read(cx).workspace().scene(scene).is_some_and(|s| s.notes == text);
        if !same {
            exec(&self.studio, Command::SetSceneNotes { scene, notes: text }, cx);
        }
    }
}

impl Render for NotesPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let scene_id = self.scene(cx);
        let workspace = self.studio.read(cx).workspace().clone();
        let scene = scene_id.and_then(|s| workspace.scene(s)).cloned();
        let notes = scene.as_ref().map(|s| s.notes.clone()).unwrap_or_default();
        // an editor for this scene's notes (a new one when the scene changes)
        if self.editor.as_ref().map(|(s, _, _)| Some(*s)) != Some(scene_id)
            && let Some(sid) = scene_id
        {
            if self.editor.is_some() {
                self.save(cx);
            }
            let state = cx.new(|cx| EditorState::new(window, cx).line_number(false).default_value(notes.clone()));
            let sub = cx.subscribe_in(&state, window, |this, _, ev: &InputEvent, _, cx| match ev {
                InputEvent::Blur => this.save(cx),
                InputEvent::Change => {
                    this.typed += 1;
                    let n = this.typed;
                    cx.spawn(async move |this, cx| {
                        cx.background_executor().timer(Duration::from_millis(900)).await;
                        let _ = this.update(cx, |this, cx| {
                            if this.typed == n {
                                this.save(cx)
                            }
                        });
                    })
                    .detach();
                }
                _ => {}
            });
            self.editor = Some((sid, state, sub));
        } else if let Some((_, state, _)) = &self.editor {
            // undo or another window changed them
            let focused = state.read(cx).focus_handle(cx).is_focused(window);
            if !focused && state.read(cx).value() != notes.as_str() {
                state.update(cx, |s, cx| s.set_value(notes.clone(), window, cx));
            }
        }
        let name = scene.as_ref().map(|s| s.name.clone()).unwrap_or_else(|| "No scene".into());
        let mut bar = toolbar()
            .child(div().flex_1().min_w_0().truncate().text_size(px(12.)).text_color(tok::text_secondary()).child(name))
            .child(
                Button::new("notes-prompter")
                    .label(if self.prompter { "Edit" } else { "Teleprompter" })
                    .icon(ui::icon(if self.prompter { "pencil" } else { "scroll-text" }))
                    .xsmall()
                    .outline()
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.prompter = !this.prompter;
                        this.rolling = false;
                        this.offset = 0.0;
                        cx.notify();
                    })),
            );
        if self.prompter {
            bar = bar
                .child(
                    Button::new("prompter-roll")
                        .icon(ui::icon(if self.rolling { "pause" } else { "play" }))
                        .xsmall()
                        .ghost()
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.rolling = !this.rolling;
                            cx.notify();
                        })),
                )
                .child(Button::new("prompter-top").icon(ui::icon("arrow-up-to-line")).xsmall().ghost().on_click(cx.listener(
                    |this, _, _, cx| {
                        this.offset = 0.0;
                        cx.notify();
                    },
                )))
                .child(Button::new("prompter-slower").label("−").xsmall().ghost().on_click(cx.listener(|this, _, _, cx| {
                    this.speed = (this.speed - 10.0).max(10.0);
                    cx.notify();
                })))
                .child(div().w(px(56.)).text_center().text_size(px(11.)).font_family(ui::MONO).child(format!("{:.0} px/s", self.speed)))
                .child(Button::new("prompter-faster").label("+").xsmall().ghost().on_click(cx.listener(|this, _, _, cx| {
                    this.speed = (this.speed + 10.0).min(400.0);
                    cx.notify();
                })))
                .child(Button::new("prompter-smaller").label("A−").xsmall().ghost().on_click(cx.listener(|this, _, _, cx| {
                    this.size = (this.size - 4.0).max(14.0);
                    cx.notify();
                })))
                .child(Button::new("prompter-bigger").label("A+").xsmall().ghost().on_click(cx.listener(|this, _, _, cx| {
                    this.size = (this.size + 4.0).min(96.0);
                    cx.notify();
                })));
        }
        let body = match (&self.editor, self.prompter) {
            (None, _) => empty("sticky-note", "No scene", "Notes belong to a scene: pick one in Scenes.").into_any_element(),
            (Some(_), true) => {
                let text = if notes.trim().is_empty() { "This scene has no notes yet. Press Edit and write them.".to_string() } else { notes };
                // the text rises; a line marks where to read
                let paragraph = div()
                    .absolute()
                    .left(px(24.))
                    .right(px(24.))
                    .top(px(80.0 - self.offset))
                    .text_size(px(self.size))
                    .line_height(px(self.size * 1.35))
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(gpui::white())
                    .child(text);
                div()
                    .relative()
                    .flex_1()
                    .overflow_hidden()
                    .bg(gpui::black())
                    .child(paragraph)
                    .child(div().absolute().left_0().right_0().top(px(76.)).h(px(2.)).bg(tok::accent_primary()).opacity(0.6))
                    .into_any_element()
            }
            (Some((_, state, _)), false) => {
                div().flex_1().min_h_0().p_2().child(Editor::new(state).h_full()).into_any_element()
            }
        };
        div().flex().flex_col().size_full().bg(tok::bg_panel()).child(bar).child(body)
    }
}

panel_traits!(NotesPanel, "notes", "sticky-note", "Notes");

// ---------------------------------------------------------------------------------------------
// Rundown

pub struct RundownPanel {
    studio: Entity<Studio>,
    focus: FocusHandle,
    /// The segment on, and when it started.
    current: Option<(Id, Instant)>,
    /// When the show started (the first segment).
    started: Option<Instant>,
    /// The segment open for editing, and its title field.
    editing: Option<(Id, Entity<InputState>, Subscription)>,
}

impl RundownPanel {
    pub fn new(studio: Entity<Studio>, cx: &mut Context<Self>) -> Self {
        cx.observe(&studio, |_, _, cx| cx.notify()).detach();
        // the clocks tick
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(Duration::from_millis(500)).await;
                let Ok(()) = this.update(cx, |p, cx| {
                    if p.current.is_some() {
                        cx.notify();
                    }
                }) else {
                    break;
                };
            }
        })
        .detach();
        Self { studio, focus: cx.focus_handle(), current: None, started: None, editing: None }
    }

    /// Puts segment `index` on: its clock starts and its scene goes on air.
    fn go(&mut self, index: usize, cx: &mut Context<Self>) {
        let rundown = self.studio.read(cx).workspace().show.rundown.clone();
        let Some(seg) = rundown.get(index) else {
            // past the last one: the show is over
            self.current = None;
            cx.notify();
            return;
        };
        self.current = Some((seg.id, Instant::now()));
        self.started.get_or_insert_with(Instant::now);
        if let Some(scene) = seg.scene {
            exec(&self.studio, Command::SetProgram { scene: Some(scene) }, cx);
        }
        cx.notify();
    }

    fn index_of_current(&self, rundown: &[Segment]) -> Option<usize> {
        self.current.and_then(|(id, _)| rundown.iter().position(|s| s.id == id))
    }

    fn edit(&mut self, seg: &Segment, window: &mut Window, cx: &mut Context<Self>) {
        let id = seg.id;
        let input = cx.new(|cx| InputState::new(window, cx).default_value(seg.title.clone()));
        let sub = cx.subscribe_in(&input, window, move |this, input, ev: &InputEvent, _, cx| {
            if matches!(ev, InputEvent::Blur | InputEvent::PressEnter { .. }) {
                let title = input.read(cx).value().trim().to_string();
                if !title.is_empty() {
                    set_show(&this.studio, "Rename a segment", |s| {
                        if let Some(seg) = s.rundown.iter_mut().find(|s| s.id == id) {
                            seg.title = title;
                        }
                    }, cx);
                }
            }
        });
        self.editing = Some((id, input, sub));
        cx.notify();
    }
}

fn clock(secs: u64) -> String {
    if secs >= 3600 { format!("{}:{:02}:{:02}", secs / 3600, (secs / 60) % 60, secs % 60) } else { format!("{}:{:02}", secs / 60, secs % 60) }
}

impl Render for RundownPanel {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let workspace = self.studio.read(cx).workspace().clone();
        let rundown = workspace.show.rundown.clone();
        let at = self.index_of_current(&rundown);
        let planned: u64 = rundown.iter().map(|s| s.seconds as u64).sum();
        let studio = self.studio.clone();
        let mut bar = toolbar()
            .child(
                Button::new("rd-start")
                    .label(match at {
                        None => "Start",
                        Some(i) if i + 1 >= rundown.len() => "End",
                        Some(_) => "Next",
                    })
                    .icon(ui::icon(if at.is_none() { "play" } else { "skip-forward" }))
                    .xsmall()
                    .outline()
                    .disabled(rundown.is_empty())
                    .on_click(cx.listener(move |this, _, _, cx| {
                        let next = match at {
                            None => 0,
                            Some(i) => i + 1,
                        };
                        this.go(next, cx)
                    })),
            )
            .when(at.is_some_and(|i| i > 0), |d| {
                d.child(Button::new("rd-back").icon(ui::icon("skip-back")).xsmall().ghost().on_click(cx.listener(move |this, _, _, cx| {
                    if let Some(i) = at {
                        this.go(i.saturating_sub(1), cx)
                    }
                })))
            })
            .when(at.is_some(), |d| {
                d.child(Button::new("rd-reset").label("Reset").xsmall().ghost().on_click(cx.listener(|this, _, _, cx| {
                    this.current = None;
                    this.started = None;
                    cx.notify();
                })))
            })
            .child(div().flex_1());
        // the show's clock against the plan
        let total = self.started.map(|t| t.elapsed().as_secs()).unwrap_or(0);
        bar = bar.child(
            div()
                .text_size(px(11.))
                .font_family(ui::MONO)
                .text_color(if total > planned && planned > 0 { tok::live() } else { tok::text_tertiary() })
                .child(format!("{} / {}", clock(total), clock(planned))),
        );
        bar = bar.child(
            Button::new("rd-add").icon(ui::icon("plus")).xsmall().ghost().tooltip("Add a segment").on_click(cx.listener(
                |this, _, _, cx| {
                    let scene = this.studio.read(cx).selected_scene;
                    let n = this.studio.read(cx).workspace().show.rundown.len() + 1;
                    set_show(&this.studio, "Add a segment", |s| {
                        let mut seg = Segment::new(format!("Segment {n}"));
                        seg.scene = scene;
                        s.rundown.push(seg);
                    }, cx);
                },
            )),
        );
        let body = if rundown.is_empty() {
            empty(
                "list-ordered",
                "Plan the show",
                "Add segments in running order, each with how long it should take and the scene it starts on. Start runs the clock; Next moves on and switches scenes.",
            )
            .into_any_element()
        } else {
            let mut list = div().flex().flex_col().gap_1().p_2();
            for (i, seg) in rundown.iter().enumerate() {
                let on = at == Some(i);
                let done = at.is_some_and(|a| i < a);
                let elapsed = if on { self.current.map(|(_, t)| t.elapsed().as_secs()).unwrap_or(0) } else { 0 };
                let over = on && seg.seconds > 0 && elapsed > seg.seconds as u64;
                let scene_name = seg.scene.and_then(|s| workspace.scene(s)).map(|s| s.name.clone());
                let id = seg.id;
                let editing = self.editing.as_ref().filter(|(e, _, _)| *e == id).map(|(_, input, _)| input.clone());
                let s = seg.clone();
                let mut row = div()
                    .id(("seg", i))
                    .flex()
                    .flex_col()
                    .gap_1()
                    .px_3()
                    .py_2()
                    .rounded(px(7.))
                    .border_1()
                    .border_color(if on { tok::live() } else { tok::border() })
                    .when(on, |d| d.bg(tok::bg_selected()))
                    .when(done, |d| d.opacity(0.55))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(div().w(px(18.)).text_size(px(11.)).font_family(ui::MONO).text_color(tok::text_tertiary()).child(format!("{}", i + 1)))
                            .child(match &editing {
                                Some(input) => div().flex_1().child(TextInput::new(input).small()).into_any_element(),
                                None => div()
                                    .id(("seg-title", i))
                                    .flex_1()
                                    .min_w_0()
                                    .truncate()
                                    .text_size(px(13.))
                                    .font_weight(FontWeight::MEDIUM)
                                    .child(seg.title.clone())
                                    .on_click(cx.listener(move |this, ev: &ClickEvent, window, cx| {
                                        if ev.click_count() == 2 {
                                            this.edit(&s, window, cx)
                                        }
                                    }))
                                    .into_any_element(),
                            })
                            .child(
                                div()
                                    .text_size(px(11.))
                                    .font_family(ui::MONO)
                                    .text_color(if over { tok::live() } else { tok::text_secondary() })
                                    .child(if on {
                                        format!("{} / {}", clock(elapsed), clock(seg.seconds as u64))
                                    } else {
                                        clock(seg.seconds as u64)
                                    }),
                            ),
                    );
                // its length and scene, and going straight to it
                let (s1, s2) = (studio.clone(), studio.clone());
                let shorter = move |_: &ClickEvent, _: &mut Window, cx: &mut App| {
                    set_show(&s1, "Change a segment", |s| {
                        if let Some(seg) = s.rundown.iter_mut().find(|s| s.id == id) {
                            seg.seconds = seg.seconds.saturating_sub(60).max(0);
                        }
                    }, cx)
                };
                let longer = move |_: &ClickEvent, _: &mut Window, cx: &mut App| {
                    set_show(&s2, "Change a segment", |s| {
                        if let Some(seg) = s.rundown.iter_mut().find(|s| s.id == id) {
                            seg.seconds = (seg.seconds + 60).min(24 * 3600);
                        }
                    }, cx)
                };
                let mut scenes = div().flex().flex_wrap().gap_1();
                for (n, sc) in workspace.scenes.iter().enumerate() {
                    let sid = sc.id;
                    let st = studio.clone();
                    let chosen = seg.scene == Some(sid);
                    scenes = scenes.child(chip(SharedString::from(format!("seg-{i}-scene-{n}")), sc.name.clone(), chosen).on_click(
                        move |_, _, cx| {
                            set_show(&st, "Change a segment", |s| {
                                if let Some(seg) = s.rundown.iter_mut().find(|s| s.id == id) {
                                    seg.scene = if chosen { None } else { Some(sid) };
                                }
                            }, cx)
                        },
                    ));
                }
                let st = studio.clone();
                row = row.child(
                    div()
                        .flex()
                        .items_center()
                        .gap_1()
                        .pl(px(26.))
                        .child(Button::new(("seg-shorter", i)).label("−1m").xsmall().ghost().on_click(shorter))
                        .child(Button::new(("seg-longer", i)).label("+1m").xsmall().ghost().on_click(longer))
                        .child(div().flex_1().min_w_0().truncate().text_size(px(11.)).text_color(tok::text_tertiary()).child(
                            scene_name.map(|n| format!("starts on {n}")).unwrap_or_else(|| "keeps the scene on air".into()),
                        ))
                        .child(Button::new(("seg-go", i)).label("Go").xsmall().ghost().on_click(cx.listener(move |this, _, _, cx| this.go(i, cx))))
                        .child(Button::new(("seg-remove", i)).icon(ui::icon("trash")).xsmall().ghost().on_click(move |_, _, cx| {
                            set_show(&st, "Remove a segment", |s| s.rundown.retain(|s| s.id != id), cx)
                        })),
                );
                if editing.is_some() || on {
                    row = row.child(div().pl(px(26.)).child(scenes));
                }
                if !seg.notes.is_empty() {
                    row = row.child(div().pl(px(26.)).text_size(px(11.)).text_color(tok::text_secondary()).child(seg.notes.clone()));
                }
                list = list.child(row);
            }
            div().id("rundown").flex_1().min_h_0().overflow_y_scroll().child(list).into_any_element()
        };
        div().flex().flex_col().size_full().bg(tok::bg_panel()).child(bar).child(body)
    }
}

panel_traits!(RundownPanel, "rundown", "list-ordered", "Rundown");
