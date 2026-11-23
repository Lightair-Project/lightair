//! The PNGtuber editor (docs/plans/pngtuber.md, "Making one"): a window of its own to build a
//! PNGtuber from pictures and save it as a `.lairava` (lightair-core's `lightair-pngtuber`).
//!
//! - **Layers:** drop PNGs (or a folder of them) on the window, or Add pictures…; each gets a role
//!   from its name (a mouth frame, eyes shut…), changeable; ordered back to front.
//! - **The preview:** the avatar moving with the chosen mic, as it will on air. Click it to set the
//!   selected layer's pivot, or to place an anchor.
//! - **Expressions and costumes:** each shows and hides layers; choosing one previews it.
//! - **Voice, motion, blinking:** the mic's open, close and gate levels against its live meter,
//!   attack and release; bounce, squash, breathing, hop; how often it blinks.
//!
//! Saving puts it in the avatar library (and on the avatar it was opened from).

use std::cell::Cell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use gpui_kit::component::Disableable;
use gpui_kit::component::Sizable;
use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::input::{Input as TextInput, InputState};
use gpui_kit::component::menu::{DropdownMenu, PopupMenuItem};
use gpui_kit::component::slider::{Slider, SliderEvent, SliderState};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;
use lightair_model::Id;
use lightair_pngtuber::{Anchor, Avatar, Expression, Inputs, Layer, Puppet, Role};

use crate::ui::{self, tok};

/// What a click on the preview does.
#[derive(Clone, Copy, PartialEq)]
enum Click {
    Nothing,
    Pivot,
    Anchor,
}

/// Called with where it was saved.
type OnSaved = Rc<dyn Fn(PathBuf, &mut App)>;

pub struct PngEditor {
    avatar: Avatar,
    /// Each layer's picture, PNG bytes, by its `image` path; and as GPUI shows it.
    pngs: HashMap<String, Vec<u8>>,
    images: HashMap<String, Arc<Image>>,
    puppet: Puppet,
    dirty: bool,
    selected: Option<usize>,
    click: Click,
    /// The expression being set up (its layers shown and hidden), and so previewed.
    expression: Option<usize>,
    path: Option<PathBuf>,
    tracks: Vec<(Id, String)>,
    mic: Option<Id>,
    level_db: f32,
    name: Entity<InputState>,
    sliders: HashMap<String, (Entity<SliderState>, Subscription)>,
    preview: Rc<Cell<Bounds<Pixels>>>,
    note: Option<String>,
    on_saved: OnSaved,
    last: Instant,
}

const PREVIEW: f32 = 440.0;

thread_local! {
    /// The editor opened last (a check script steps it: `check`).
    static LAST: std::cell::RefCell<Option<WeakEntity<PngEditor>>> = const { std::cell::RefCell::new(None) };
}

/// Development: a check script's step on the editor opened last: "save", "expression <name>
/// <layer id>" (an expression showing that layer, previewed), "select <layer index>".
pub fn check(command: &str, cx: &mut App) {
    let Some(e) = LAST.with(|l| l.borrow().as_ref().and_then(WeakEntity::upgrade)) else { return };
    let (cmd, rest) = command.split_once(' ').unwrap_or((command, ""));
    e.update(cx, |this, cx| match cmd {
        "save" => this.save(cx),
        "expression" => {
            let (name, layer) = rest.rsplit_once(' ').unwrap_or((rest, ""));
            this.avatar.expressions.push(Expression {
                name: name.into(),
                show: vec![layer.into()],
                ..Default::default()
            });
            this.expression = Some(this.avatar.expressions.len() - 1);
            this.changed(cx);
        }
        "select" => {
            this.selected = rest.parse().ok();
            cx.notify();
        }
        _ => {}
    });
}

/// Opens the editor: a new PNGtuber, or the one at `path`. `tracks`: the mixer's tracks to listen
/// to (the first chosen); `on_saved`: told where it was saved.
/// `add`: pictures added as layers at once.
pub fn open(
    path: Option<PathBuf>,
    add: Vec<PathBuf>,
    tracks: Vec<(Id, String)>,
    mic: Option<Id>,
    on_saved: OnSaved,
    cx: &mut App,
) {
    let options = WindowOptions {
        window_bounds: Some(WindowBounds::centered(size(px(1180.), px(780.)), cx)),
        titlebar: Some(TitlebarOptions { title: Some("PNGtuber · Lightair".into()), ..Default::default() }),
        window_min_size: Some(size(px(900.), px(600.))),
        ..Default::default()
    };
    let opened = gpui_kit::open_window(options, cx, move |window, cx| {
        cx.new(|cx| {
            let (avatar, pngs, note) = match path.as_deref().map(read) {
                Some(Ok((a, p))) => (a, p, None),
                Some(Err(e)) => (Avatar::new("My PNGtuber", 1024, 1024), HashMap::new(), Some(e)),
                None => (Avatar::new("My PNGtuber", 1024, 1024), HashMap::new(), None),
            };
            let name = cx.new(|cx| InputState::new(window, cx).default_value(avatar.name.clone()));
            let images = pngs
                .iter()
                .map(|(k, v)| (k.clone(), Arc::new(Image::from_bytes(ImageFormat::Png, v.clone()))))
                .collect();
            let mic = mic.or(tracks.first().map(|(id, _)| *id));
            let mut e = PngEditor {
                puppet: Puppet::new(avatar.clone()),
                avatar,
                pngs,
                images,
                dirty: false,
                selected: None,
                click: Click::Nothing,
                expression: None,
                path,
                tracks,
                mic,
                level_db: f32::NEG_INFINITY,
                name,
                sliders: HashMap::new(),
                preview: Rc::new(Cell::new(Bounds::default())),
                note,
                on_saved,
                last: Instant::now(),
            };
            e.start(window, cx);
            LAST.with(|l| *l.borrow_mut() = Some(cx.entity().downgrade()));
            if !add.is_empty() {
                e.add_pictures(&add, cx);
            }
            e
        })
    });
    if let Err(e) = opened {
        tracing::warn!(%e, "PNGtuber editor window");
    }
}

/// Reads a `.lairava` for editing: its description and its pictures as PNG bytes.
fn read(path: &Path) -> Result<(Avatar, HashMap<String, Vec<u8>>), String> {
    let loaded = lightair_pngtuber::file::read_path(path).map_err(|e| e.to_string())?;
    let mut pngs = HashMap::new();
    for (l, p) in loaded.avatar.layers.iter().zip(&loaded.pictures) {
        pngs.insert(l.image.clone(), lightair_pngtuber::file::encode_png(p)?);
    }
    Ok((loaded.avatar, pngs))
}

/// The PNGs among dropped paths (a folder's, by name), in order.
fn pngs_in(paths: &[PathBuf]) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for p in paths {
        if p.is_dir() {
            let mut inside: Vec<PathBuf> =
                std::fs::read_dir(p).map(|r| r.flatten().map(|e| e.path()).collect()).unwrap_or_default();
            inside.sort();
            out.extend(inside.into_iter().filter(|f| f.extension().is_some_and(|x| x.eq_ignore_ascii_case("png"))));
        } else if p.extension().is_some_and(|x| x.eq_ignore_ascii_case("png")) {
            out.push(p.clone());
        }
    }
    out
}

impl PngEditor {
    /// The preview's clock: the mic's level in, a frame of motion out.
    fn start(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        cx.spawn_in(window, async move |this, cx| {
            loop {
                cx.background_executor().timer(Duration::from_millis(33)).await;
                if this.update(cx, |this, cx| this.tick(cx)).is_err() {
                    break;
                }
            }
        })
        .detach();
    }

    fn tick(&mut self, cx: &mut Context<Self>) {
        let now = Instant::now();
        let dt = now.duration_since(self.last).as_secs_f32();
        self.last = now;
        self.level_db = self
            .mic
            .and_then(|id| lightair_engine::meters::meters().get(id))
            .map_or(f32::NEG_INFINITY, |m| m.levels().magnitude.iter().copied().fold(f32::NEG_INFINITY, f32::max));
        if self.dirty {
            self.puppet.set_avatar(self.avatar.clone());
            self.dirty = false;
        }
        let expression = self.expression.and_then(|i| self.avatar.expressions.get(i)).map(|e| e.name.clone());
        self.puppet.step(&Inputs {
            level_db: self.level_db,
            push_to_talk: None,
            expression: expression.as_deref(),
            dt,
        });
        cx.notify();
    }

    fn changed(&mut self, cx: &mut Context<Self>) {
        self.dirty = true;
        self.note = None;
        cx.notify();
    }

    /// Adds pictures as layers, in front. The first sets the canvas's size; each is centred on it.
    fn add_pictures(&mut self, paths: &[PathBuf], cx: &mut Context<Self>) {
        let files = pngs_in(paths);
        let mut refused = Vec::new();
        for path in files {
            let Ok(bytes) = std::fs::read(&path) else { continue };
            let Ok(pic) = lightair_pngtuber::file::decode_png(&bytes) else {
                refused.push(path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default());
                continue;
            };
            if self.avatar.layers.is_empty() {
                self.avatar.width = pic.width;
                self.avatar.height = pic.height;
            }
            let stem = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| "Layer".into());
            let id = self.avatar.free_id(&stem);
            let (role, frame) = Role::suggested(&stem);
            let (w, h) = (pic.width as f32, pic.height as f32);
            let (x, y) = ((self.avatar.width as f32 - w) / 2.0, (self.avatar.height as f32 - h) / 2.0);
            let image = format!("layers/{id}.png");
            self.images.insert(image.clone(), Arc::new(Image::from_bytes(ImageFormat::Png, bytes.clone())));
            self.pngs.insert(image.clone(), bytes);
            self.avatar.layers.push(Layer {
                id,
                name: stem,
                image,
                x,
                y,
                width: w,
                height: h,
                pivot: [x + w / 2.0, y + h / 2.0],
                role,
                frame,
                motion: 1.0,
                opacity: 1.0,
            });
            self.selected = Some(self.avatar.layers.len() - 1);
        }
        self.changed(cx);
        if !refused.is_empty() {
            self.note = Some(format!("These aren't PNG pictures Lightair can read: {}", refused.join(", ")));
        }
    }

    fn pick_pictures(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let paths = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: true,
            multiple: true,
            prompt: Some("Add".into()),
        });
        cx.spawn_in(window, async move |this, cx| {
            let Ok(Ok(Some(paths))) = paths.await else { return };
            let _ = this.update(cx, |this, cx| this.add_pictures(&paths, cx));
        })
        .detach();
    }

    fn save(&mut self, cx: &mut Context<Self>) {
        let name = self.name.read(cx).value().trim().to_string();
        self.avatar.name = if name.is_empty() { "My PNGtuber".into() } else { name };
        if self.avatar.layers.is_empty() {
            self.note = Some("Add a picture first.".into());
            cx.notify();
            return;
        }
        let path =
            self.path.clone().unwrap_or_else(|| {
                let dir = crate::avatar_library::dir().join("pngtubers");
                let _ = std::fs::create_dir_all(&dir);
                let stem: String = self
                    .avatar
                    .name
                    .chars()
                    .map(|c| if c.is_alphanumeric() || c == ' ' || c == '-' { c } else { '_' })
                    .collect();
                (1..)
                    .map(|n| {
                        if n == 1 {
                            dir.join(format!("{stem}.lairava"))
                        } else {
                            dir.join(format!("{stem} {n}.lairava"))
                        }
                    })
                    .find(|p| !p.exists())
                    .unwrap_or(dir.join("PNGtuber.lairava"))
            });
        match lightair_pngtuber::file::write_path(&path, &self.avatar, &self.pngs) {
            Ok(()) => {
                self.path = Some(path.clone());
                self.note = Some("Saved".into());
                // (told once this update is over: it updates the main window)
                let told = self.on_saved.clone();
                cx.defer(move |cx| told(path, cx));
            }
            Err(e) => self.note = Some(format!("It couldn't be saved: {e}")),
        }
        cx.notify();
    }

    /// A slider for one setting at `value`; `set` writes it.
    fn slider(
        &mut self,
        key: String,
        range: (f32, f32, f32),
        value: f32,
        set: impl Fn(&mut PngEditor, f32) + 'static,
        cx: &mut Context<Self>,
    ) -> Slider {
        let stale =
            self.sliders.get(&key).is_none_or(|(s, _)| (s.read(cx).value().end() - value).abs() > range.2 * 0.5);
        if stale {
            let state = cx.new(|_| SliderState::new().min(range.0).max(range.1).step(range.2).default_value(value));
            let sub = cx.subscribe(&state, move |this, _, ev: &SliderEvent, cx| {
                let v = match ev {
                    SliderEvent::Change(v) | SliderEvent::Release(v) => v.end(),
                };
                set(this, v);
                this.changed(cx);
            });
            self.sliders.insert(key.clone(), (state, sub));
        }
        Slider::new(&self.sliders[&key].0)
    }

    fn click_preview(&mut self, at: Point<Pixels>, cx: &mut Context<Self>) {
        let b = self.preview.get();
        let s = self.scale();
        if s <= 0.0 {
            return;
        }
        let (x, y) = (f32::from(at.x - b.origin.x) / s, f32::from(at.y - b.origin.y) / s);
        match self.click {
            Click::Pivot => {
                if let Some(l) = self.selected.and_then(|i| self.avatar.layers.get_mut(i)) {
                    l.pivot = [x, y];
                }
            }
            Click::Anchor => {
                let n = self.avatar.anchors.len() + 1;
                let layer = self.selected.and_then(|i| self.avatar.layers.get(i)).map(|l| l.id.clone());
                self.avatar.anchors.push(Anchor {
                    name: if n == 1 { "Head".into() } else { format!("Point {n}") },
                    x,
                    y,
                    layer,
                });
            }
            Click::Nothing => {
                // the top layer shown there
                let hit = self.avatar.layers.iter().enumerate().rev().find(|(i, l)| {
                    self.puppet.pose().layers.get(*i).is_some_and(|p| p.visible)
                        && x >= l.x
                        && x <= l.x + l.width
                        && y >= l.y
                        && y <= l.y + l.height
                });
                self.selected = hit.map(|(i, _)| i);
            }
        }
        self.click = Click::Nothing;
        self.changed(cx);
    }

    fn scale(&self) -> f32 {
        (PREVIEW / self.avatar.width as f32).min(PREVIEW / self.avatar.height as f32)
    }
}

/// A section's heading.
fn heading(text: &'static str) -> Div {
    div()
        .pt_3()
        .pb_1()
        .text_size(px(11.))
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(tok::text_secondary())
        .child(text.to_uppercase())
}

/// A labelled row: the label (with its ⓘ), then the control.
fn row(label: &'static str, info: Option<(&'static str, &'static str)>, control: impl IntoElement) -> Div {
    div()
        .flex()
        .items_center()
        .gap_2()
        .py_0p5()
        .child(
            div()
                .flex()
                .items_center()
                .gap_1()
                .w(px(86.))
                .flex_none()
                .child(div().text_size(px(12.)).text_color(tok::text_secondary()).child(label))
                .children(info.map(|(k, t)| ui::info(k, t))),
        )
        .child(div().flex_1().min_w_0().child(control))
}

/// A slider with its value at the right.
fn with_value(slider: Slider, value: String) -> Div {
    div().flex().items_center().gap_2().child(div().flex_1().child(slider)).child(
        div()
            .w(px(52.))
            .text_right()
            .text_size(px(11.))
            .font_family(ui::MONO)
            .text_color(tok::text_tertiary())
            .child(value),
    )
}

impl Render for PngEditor {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let me = cx.entity().downgrade();
        let s = self.scale();
        let (cw, ch) = (self.avatar.width as f32 * s, self.avatar.height as f32 * s);
        let pose = self.puppet.pose().clone();
        let editing = self.expression.and_then(|i| self.avatar.expressions.get(i)).cloned();

        // ---------------------------------------------------------------- the layers
        let mut layers = div().flex().flex_col().gap_0p5();
        let n = self.avatar.layers.len();
        for (i, l) in self.avatar.layers.iter().enumerate().rev() {
            let on = self.selected == Some(i);
            let role = l.role;
            let frame = l.frame;
            let thumb = self.images.get(&l.image).cloned();
            let menu_me = me.clone();
            let role_menu = Button::new(("layer-role", i)).label(role.label()).xsmall().outline().dropdown_menu(
                move |mut m, _, _| {
                    for r in Role::ALL {
                        let me = menu_me.clone();
                        m = m.item(PopupMenuItem::new(r.label()).checked(r == role).on_click(move |_, _, cx| {
                            if let Some(me) = me.upgrade() {
                                me.update(cx, |this, cx| {
                                    if let Some(l) = this.avatar.layers.get_mut(i) {
                                        l.role = r;
                                    }
                                    this.changed(cx);
                                });
                            }
                        }));
                    }
                    m
                },
            );
            // in an expression: shown, hidden, or as its role says
            let in_expression = editing.as_ref().map(|e| {
                if e.show.contains(&l.id) {
                    "Show"
                } else if e.hide.contains(&l.id) {
                    "Hide"
                } else {
                    "—"
                }
            });
            let id = l.id.clone();
            layers =
                layers.child(
                    div()
                        .id(("layer", i))
                        .flex()
                        .items_center()
                        .gap_2()
                        .p_1()
                        .rounded(px(6.))
                        .cursor_pointer()
                        .when(on, |d| d.bg(tok::bg_selected()))
                        .hover(|d| d.bg(tok::bg_raised()))
                        .child(
                            div()
                                .flex_none()
                                .size(px(36.))
                                .rounded(px(4.))
                                .overflow_hidden()
                                .bg(tok::bg_app())
                                .children(thumb.map(|t| img(t).size_full().object_fit(ObjectFit::Contain))),
                        )
                        .child(
                            div()
                                .flex()
                                .flex_col()
                                .flex_1()
                                .min_w_0()
                                .child(
                                    div()
                                        .overflow_hidden()
                                        .text_ellipsis()
                                        .whitespace_nowrap()
                                        .text_size(px(12.))
                                        .child(l.name.clone()),
                                )
                                .child(div().flex().items_center().gap_1().child(role_menu).when(
                                    role == Role::Mouth,
                                    |d| {
                                        d.child(Button::new(("frame-down", i)).label("−").xsmall().ghost().on_click(
                                            cx.listener(move |this, _, _, cx| {
                                                if let Some(l) = this.avatar.layers.get_mut(i) {
                                                    l.frame = l.frame.saturating_sub(1);
                                                }
                                                this.changed(cx);
                                            }),
                                        ))
                                        .child(div().text_size(px(11.)).text_color(tok::text_secondary()).child(
                                            if frame == 0 { "Shut".to_string() } else { format!("Frame {frame}") },
                                        ))
                                        .child(
                                            Button::new(("frame-up", i)).label("+").xsmall().ghost().on_click(
                                                cx.listener(move |this, _, _, cx| {
                                                    if let Some(l) = this.avatar.layers.get_mut(i) {
                                                        l.frame = (l.frame + 1).min(4);
                                                    }
                                                    this.changed(cx);
                                                }),
                                            ),
                                        )
                                    },
                                )),
                        )
                        .children(in_expression.map(|state| {
                            let id = id.clone();
                            Button::new(("layer-expression", i)).label(state).xsmall().outline().on_click(cx.listener(
                                move |this, _, _, cx| {
                                    if let Some(e) = this.expression.and_then(|x| this.avatar.expressions.get_mut(x)) {
                                        // — → Show → Hide → —
                                        if e.show.contains(&id) {
                                            e.show.retain(|x| *x != id);
                                            e.hide.push(id.clone());
                                        } else if e.hide.contains(&id) {
                                            e.hide.retain(|x| *x != id);
                                        } else {
                                            e.show.push(id.clone());
                                        }
                                    }
                                    this.changed(cx);
                                },
                            ))
                        }))
                        .child(
                            div()
                                .flex()
                                .flex_col()
                                .child(
                                    Button::new(("layer-up", i))
                                        .icon(ui::icon("chevron-up"))
                                        .xsmall()
                                        .ghost()
                                        .disabled(i + 1 == n)
                                        .on_click(cx.listener(move |this, _, _, cx| {
                                            if i + 1 < this.avatar.layers.len() {
                                                this.avatar.layers.swap(i, i + 1);
                                                this.selected = Some(i + 1);
                                            }
                                            this.changed(cx);
                                        })),
                                )
                                .child(
                                    Button::new(("layer-down", i))
                                        .icon(ui::icon("chevron-down"))
                                        .xsmall()
                                        .ghost()
                                        .disabled(i == 0)
                                        .on_click(cx.listener(move |this, _, _, cx| {
                                            if i > 0 {
                                                this.avatar.layers.swap(i, i - 1);
                                                this.selected = Some(i - 1);
                                            }
                                            this.changed(cx);
                                        })),
                                ),
                        )
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.selected = Some(i);
                            cx.notify();
                        })),
                );
        }
        if n == 0 {
            layers = layers.child(
                div()
                    .py_6()
                    .text_center()
                    .text_size(px(12.))
                    .text_color(tok::text_tertiary())
                    .child("Drop PNG pictures here"),
            );
        }

        // ---------------------------------------------------------------- the preview
        let mut stage = div()
            .relative()
            .w(px(cw))
            .h(px(ch))
            .flex_none()
            .bg(tok::bg_canvas_well())
            .rounded(px(6.))
            .overflow_hidden();
        for (i, l) in self.avatar.layers.iter().enumerate() {
            let (Some(p), Some(t)) = (pose.layers.get(i), self.images.get(&l.image)) else { continue };
            if !p.visible {
                continue;
            }
            let [x, y] = p.place([l.x, l.y]);
            stage = stage.child(
                img(t.clone())
                    .absolute()
                    .left(px(x * s))
                    .top(px(y * s))
                    .w(px(l.width * p.sx * s))
                    .h(px(l.height * p.sy * s))
                    .opacity(p.opacity),
            );
        }
        // the selected layer's box and pivot, and the anchors
        if let Some(l) = self.selected.and_then(|i| self.avatar.layers.get(i)) {
            stage = stage
                .child(
                    div()
                        .absolute()
                        .left(px(l.x * s))
                        .top(px(l.y * s))
                        .w(px(l.width * s))
                        .h(px(l.height * s))
                        .border_1()
                        .border_color(tok::accent_primary()),
                )
                .child(
                    div()
                        .absolute()
                        .left(px(l.pivot[0] * s - 4.0))
                        .top(px(l.pivot[1] * s - 4.0))
                        .size(px(8.))
                        .rounded_full()
                        .bg(tok::accent_primary()),
                );
        }
        for a in &self.avatar.anchors {
            stage = stage.child(
                div()
                    .absolute()
                    .left(px(a.x * s - 5.0))
                    .top(px(a.y * s - 5.0))
                    .size(px(10.))
                    .rounded_full()
                    .border_2()
                    .border_color(tok::warn()),
            );
        }
        let bounds = self.preview.clone();
        let stage = stage
            .child(canvas(move |b, _, _| bounds.set(b), |_, _, _, _| {}).absolute().size_full())
            .id("png-preview")
            .cursor(if self.click == Click::Nothing { CursorStyle::Arrow } else { CursorStyle::Crosshair })
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, ev: &MouseDownEvent, _, cx| this.click_preview(ev.position, cx)),
            );

        // the mic's level against its lines
        let mic = self.avatar.mic;
        let to_x = |db: f32| ((db + 80.0) / 80.0).clamp(0.0, 1.0);
        let meter_w = 360.0;
        let level = if self.level_db.is_finite() { to_x(self.level_db) } else { 0.0 };
        let meter = div()
            .relative()
            .w(px(meter_w))
            .h(px(10.))
            .rounded(px(3.))
            .bg(tok::bg_raised())
            .child(
                div().absolute().left_0().top_0().h_full().w(px(meter_w * level)).rounded(px(3.)).bg(if pose.talking {
                    tok::ok()
                } else {
                    tok::text_tertiary()
                }),
            )
            .child(div().absolute().top_0().h_full().w(px(2.)).left(px(meter_w * to_x(mic.open_db))).bg(tok::text()))
            .child(
                div()
                    .absolute()
                    .top_0()
                    .h_full()
                    .w(px(1.))
                    .left(px(meter_w * to_x(mic.close_db)))
                    .bg(tok::text_secondary()),
            )
            .child(
                div()
                    .absolute()
                    .top_0()
                    .h_full()
                    .w(px(1.))
                    .left(px(meter_w * to_x(mic.gate_db)))
                    .bg(tok::text_tertiary()),
            );
        let mic_name = self
            .mic
            .and_then(|id| self.tracks.iter().find(|(t, _)| *t == id))
            .map_or("No mic".to_string(), |(_, n)| n.clone());
        let tracks = self.tracks.clone();
        let current = self.mic;
        let menu_me = me.clone();
        let mic_menu = Button::new("png-mic").label(mic_name).small().outline().dropdown_menu(move |mut m, _, _| {
            for (id, name) in tracks.clone() {
                let me = menu_me.clone();
                m = m.item(PopupMenuItem::new(name).checked(Some(id) == current).on_click(move |_, _, cx| {
                    if let Some(me) = me.upgrade() {
                        me.update(cx, |this, cx| {
                            this.mic = Some(id);
                            cx.notify();
                        });
                    }
                }));
            }
            m
        });

        // ---------------------------------------------------------------- the settings
        let sel = self.selected.and_then(|i| self.avatar.layers.get(i)).cloned();
        let layer_panel = sel.map(|l| {
            let i = self.selected.unwrap_or(0);
            let (mv, ov) = (l.motion, l.opacity);
            let motion = self.slider(
                format!("layer-motion-{}", l.id),
                (0.0, 1.0, 0.05),
                mv,
                move |this, v| {
                    if let Some(l) = this.avatar.layers.get_mut(i) {
                        l.motion = v;
                    }
                },
                cx,
            );
            let opacity = self.slider(
                format!("layer-opacity-{}", l.id),
                (0.0, 1.0, 0.05),
                ov,
                move |this, v| {
                    if let Some(l) = this.avatar.layers.get_mut(i) {
                        l.opacity = v;
                    }
                },
                cx,
            );
            div()
                .child(heading("Layer"))
                .child(row("Name", None, div().text_size(px(12.)).child(l.name.clone())))
                .child(row(
                    "Moves",
                    Some((
                        "png-layer-motion",
                        "How much it moves with the bounce and breath: 0 keeps a background still.",
                    )),
                    with_value(motion, format!("{:.0}%", mv * 100.0)),
                ))
                .child(row("Opacity", None, with_value(opacity, format!("{:.0}%", ov * 100.0))))
                .child(
                    div()
                        .flex()
                        .gap_1()
                        .pt_1()
                        .child(Button::new("set-pivot").label("Set pivot").xsmall().outline().on_click(cx.listener(
                            |this, _, _, cx| {
                                this.click = Click::Pivot;
                                this.note = Some("Click the preview where it turns".into());
                                cx.notify();
                            },
                        )))
                        .child(
                            Button::new("delete-layer")
                                .icon(ui::icon("trash"))
                                .label("Remove")
                                .xsmall()
                                .ghost()
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    if i < this.avatar.layers.len() {
                                        let gone = this.avatar.layers.remove(i);
                                        for e in &mut this.avatar.expressions {
                                            e.show.retain(|x| *x != gone.id);
                                            e.hide.retain(|x| *x != gone.id);
                                        }
                                        this.selected = None;
                                    }
                                    this.changed(cx);
                                })),
                        ),
                )
                .into_any_element()
        });

        let mut expressions = div().flex().flex_col().gap_0p5();
        for (i, e) in self.avatar.expressions.iter().enumerate() {
            let on = self.expression == Some(i);
            expressions = expressions.child(
                div()
                    .id(("png-expression", i))
                    .flex()
                    .items_center()
                    .gap_2()
                    .px_2()
                    .py_1()
                    .rounded(px(6.))
                    .cursor_pointer()
                    .when(on, |d| d.bg(tok::bg_selected()))
                    .hover(|d| d.bg(tok::bg_raised()))
                    .child(div().flex_1().text_size(px(12.)).child(e.name.clone()))
                    .child(div().text_size(px(11.)).text_color(tok::text_tertiary()).child(if e.costume {
                        "Costume"
                    } else {
                        "Expression"
                    }))
                    .child(Button::new(("png-expression-remove", i)).icon(ui::icon("x")).xsmall().ghost().on_click(
                        cx.listener(move |this, _, _, cx| {
                            if i < this.avatar.expressions.len() {
                                this.avatar.expressions.remove(i);
                                this.expression = None;
                            }
                            this.changed(cx);
                        }),
                    ))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.expression = if this.expression == Some(i) { None } else { Some(i) };
                        cx.notify();
                    })),
            );
        }

        let open =
            self.slider("mic-open".into(), (-80.0, 0.0, 1.0), mic.open_db, |this, v| this.avatar.mic.open_db = v, cx);
        let close = self.slider(
            "mic-close".into(),
            (-80.0, 0.0, 1.0),
            mic.close_db,
            |this, v| this.avatar.mic.close_db = v,
            cx,
        );
        let gate =
            self.slider("mic-gate".into(), (-90.0, 0.0, 1.0), mic.gate_db, |this, v| this.avatar.mic.gate_db = v, cx);
        let attack = self.slider(
            "mic-attack".into(),
            (0.0, 300.0, 5.0),
            mic.attack_ms,
            |this, v| this.avatar.mic.attack_ms = v,
            cx,
        );
        let release = self.slider(
            "mic-release".into(),
            (0.0, 600.0, 10.0),
            mic.release_ms,
            |this, v| this.avatar.mic.release_ms = v,
            cx,
        );
        let m = self.avatar.motion;
        let bounce = self.slider(
            "motion-bounce".into(),
            (0.0, 2.0, 0.05),
            m.bounce,
            |this, v| this.avatar.motion.bounce = v,
            cx,
        );
        let squash = self.slider(
            "motion-squash".into(),
            (0.0, 2.0, 0.05),
            m.squash,
            |this, v| this.avatar.motion.squash = v,
            cx,
        );
        let breathing = self.slider(
            "motion-breathing".into(),
            (0.0, 2.0, 0.05),
            m.breathing,
            |this, v| this.avatar.motion.breathing = v,
            cx,
        );
        let hop = self.slider("motion-hop".into(), (0.0, 2.0, 0.05), m.hop, |this, v| this.avatar.motion.hop = v, cx);
        let b = self.avatar.blink;
        let blink =
            self.slider("blink-every".into(), (0.0, 10.0, 0.5), b.every_s, |this, v| this.avatar.blink.every_s = v, cx);

        let left = div()
            .id("png-left")
            .flex()
            .flex_col()
            .w(px(320.))
            .flex_none()
            .h_full()
            .p_3()
            .gap_1()
            .border_r_1()
            .border_color(tok::border())
            .overflow_y_scroll()
            .child(row("Name", None, TextInput::new(&self.name).small()))
            .child(
                div().flex().items_center().justify_between().child(heading("Layers")).child(
                    Button::new("add-pictures")
                        .icon(ui::icon("plus"))
                        .label("Add pictures…")
                        .xsmall()
                        .ghost()
                        .on_click(cx.listener(|this, _, window, cx| this.pick_pictures(window, cx))),
                ),
            )
            .child(layers);

        let middle = div()
            .flex()
            .flex_col()
            .flex_1()
            .min_w_0()
            .items_center()
            .justify_center()
            .gap_3()
            .p_4()
            .child(stage)
            .child(
                div().flex().items_center().gap_3().child(mic_menu).child(meter).child(
                    div()
                        .w(px(64.))
                        .text_size(px(11.))
                        .text_color(if pose.talking { tok::ok() } else { tok::text_tertiary() })
                        .child(if pose.talking { "Talking" } else { "Quiet" }),
                ),
            )
            .children(self.note.clone().map(|n| div().text_size(px(12.)).text_color(tok::text_secondary()).child(n)));

        let right = div()
            .id("png-right")
            .flex()
            .flex_col()
            .w(px(320.))
            .flex_none()
            .h_full()
            .p_3()
            .border_l_1()
            .border_color(tok::border())
            .overflow_y_scroll()
            .children(layer_panel)
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(heading("Anchors"))
                    .child(Button::new("add-anchor").icon(ui::icon("plus")).label("Add").xsmall().ghost().on_click(cx.listener(|this, _, _, cx| {
                        this.click = Click::Anchor;
                        this.note = Some("Click the preview where it goes (it moves with the selected layer)".into());
                        cx.notify();
                    }))),
            )
            .children(self.avatar.anchors.iter().enumerate().map(|(i, a)| {
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(div().flex_1().text_size(px(12.)).child(a.name.clone()))
                    .child(div().text_size(px(11.)).text_color(tok::text_tertiary()).child(a.layer.clone().unwrap_or_else(|| "Whole avatar".into())))
                    .child(Button::new(("anchor-remove", i)).icon(ui::icon("x")).xsmall().ghost().on_click(cx.listener(move |this, _, _, cx| {
                        if i < this.avatar.anchors.len() {
                            this.avatar.anchors.remove(i);
                        }
                        this.changed(cx);
                    })))
            }))
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(div().flex().items_center().gap_1().child(heading("Expressions")).child(ui::info("png-expressions-info", "Each shows and hides layers. Choose one, then set each layer in the list to Show or Hide. Its shortcut is set on the avatar in Lightair.")))
                    .child(
                        div()
                            .flex()
                            .gap_1()
                            .child(Button::new("add-expression").icon(ui::icon("plus")).label("Expression").xsmall().ghost().on_click(cx.listener(|this, _, _, cx| {
                                let n = this.avatar.expressions.len() + 1;
                                this.avatar.expressions.push(Expression { name: format!("Expression {n}"), ..Default::default() });
                                this.expression = Some(this.avatar.expressions.len() - 1);
                                this.changed(cx);
                            })))
                            .child(Button::new("add-costume").icon(ui::icon("plus")).label("Costume").xsmall().ghost().on_click(cx.listener(|this, _, _, cx| {
                                let n = this.avatar.expressions.iter().filter(|e| e.costume).count() + 1;
                                this.avatar.expressions.push(Expression { name: format!("Costume {n}"), costume: true, ..Default::default() });
                                this.expression = Some(this.avatar.expressions.len() - 1);
                                this.changed(cx);
                            }))),
                    ),
            )
            .child(expressions)
            .child(heading("Voice"))
            .child(row("Opens at", Some(("png-open", "The mouth opens when your voice is louder than this (the bright line on the meter).")), with_value(open, format!("{:.0} dB", mic.open_db))))
            .child(row("Closes at", Some(("png-close", "It closes once your voice stays below this, lower than Opens at, so it doesn't flicker.")), with_value(close, format!("{:.0} dB", mic.close_db))))
            .child(row("Silence", Some(("png-gate", "Anything quieter is the room, not you.")), with_value(gate, format!("{:.0} dB", mic.gate_db))))
            .child(row("Attack", None, with_value(attack, format!("{:.0} ms", mic.attack_ms))))
            .child(row("Release", None, with_value(release, format!("{:.0} ms", mic.release_ms))))
            .child(heading("Motion"))
            .child(row("Bounce", None, with_value(bounce, format!("{:.0}%", m.bounce * 100.0))))
            .child(row("Squash", None, with_value(squash, format!("{:.0}%", m.squash * 100.0))))
            .child(row("Breathing", None, with_value(breathing, format!("{:.0}%", m.breathing * 100.0))))
            .child(row("Hop", None, with_value(hop, format!("{:.0}%", m.hop * 100.0))))
            .child(heading("Blinking"))
            .child(row("Every", Some(("png-blink", "About this often, varying a little; sometimes twice. It needs an Eyes shut layer.")), with_value(blink, if b.every_s <= 0.0 { "Never".into() } else { format!("{:.1} s", b.every_s) })));

        let footer = div()
            .flex()
            .items_center()
            .justify_end()
            .gap_2()
            .px_3()
            .py_2()
            .border_t_1()
            .border_color(tok::border())
            .child(Button::new("png-save").label("Save").small().on_click(cx.listener(|this, _, _, cx| this.save(cx))));

        div()
            .id("png-editor")
            .size_full()
            .flex()
            .flex_col()
            .bg(tok::bg_panel())
            .text_color(tok::text())
            .text_size(px(13.))
            .drag_over::<ExternalPaths>(|s, _, _, _| s.bg(tok::bg_raised()))
            .on_drop(cx.listener(|this, paths: &ExternalPaths, _, cx| this.add_pictures(paths.paths(), cx)))
            .child(div().flex().flex_1().min_h_0().child(left).child(middle).child(right))
            .child(footer)
    }
}
