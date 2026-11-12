//! The studio window (docs/05-interface-shell.md): title bar, Layers + Scenes, Canvas, Mixer,
//! Outputs + Inspector, status bar. Every edit goes through `Studio::execute`.

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::Rc;

use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::component::{Disableable, Sizable};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;
use lightair_core::{
    Command, Direction, Fit, Format, Id, Input, Layer, Layout, Project, Scene, TransitionKind, TransitionSettings,
};
use lightair_preview::Preview;
use serde_json::json;

mod layer_props;
mod palette;

use crate::geometry::{self as geo, CORNERS, Corner};
use crate::mixer::MixerPanel;
use crate::outputs_panel::{OutputsPanel, elapsed};
use crate::rows::{DraggedRow, Renaming, RowTarget};
use crate::safe_areas::{self, Profile};
use crate::studio::Studio;
use crate::ui::{self, tok};
use lightair_engine::outputs::{OutputKey, OutputState, runtime};
use std::time::Instant;

actions!(
    lightair,
    [
        Undo,
        Redo,
        DeleteSelection,
        NudgeLeft,
        NudgeRight,
        NudgeUp,
        NudgeDown,
        Deselect,
        RenameSelection,
        OpenPalette,
        Take,
        AddMarker
    ]
);

pub fn bind_keys(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("cmd-z", Undo, Some("Studio")),
        KeyBinding::new("cmd-shift-z", Redo, Some("Studio")),
        KeyBinding::new("backspace", DeleteSelection, Some("Studio")),
        KeyBinding::new("delete", DeleteSelection, Some("Studio")),
        KeyBinding::new("left", NudgeLeft, Some("Studio")),
        KeyBinding::new("right", NudgeRight, Some("Studio")),
        KeyBinding::new("up", NudgeUp, Some("Studio")),
        KeyBinding::new("down", NudgeDown, Some("Studio")),
        KeyBinding::new("escape", Deselect, Some("Studio")),
        KeyBinding::new("enter", RenameSelection, Some("Studio")),
        KeyBinding::new("cmd-k", OpenPalette, Some("Studio")),
        KeyBinding::new("cmd-enter", Take, Some("Studio")),
        KeyBinding::new("cmd-m", AddMarker, Some("Studio")),
    ]);
}

#[derive(Clone, Copy, PartialEq)]
enum DragMode {
    Move,
    Resize(Corner),
    /// Turn around the box centre.
    Rotate,
    /// ⌥ + corner: crop the source; `size` is the source's pixel size.
    Crop(Corner, (f32, f32)),
}

/// Distance of the rotation handle above the selection, in screen pixels.
const ROTATE_OFFSET: f32 = 22.0;

struct Drag {
    scene: Id,
    layer: Id,
    format: Id,
    mode: DragMode,
    start: (f32, f32),
    layout: Layout,
}

/// Snap guides shown while dragging: (vertical?, canvas coordinate).
type Guides = Vec<(bool, f32)>;

pub struct StudioWindow {
    studio: Entity<Studio>,
    mixer: Entity<MixerPanel>,
    outputs: Entity<OutputsPanel>,
    /// First click on "End stream" arms it; a second click within 3 s ends the stream.
    confirm_end: Option<Instant>,
    /// One preview per format (the primary one renders the main canvas).
    previews: HashMap<Id, Preview>,
    /// Screen bounds of each visible format's stage, from the last frame.
    stage_bounds: Rc<RefCell<HashMap<Id, Bounds<Pixels>>>>,
    /// The format being edited (tabs), or all formats side by side.
    active_format: Option<Id>,
    side_by_side: bool,
    focus: FocusHandle,
    drag: Option<Drag>,
    guides: Guides,
    add_menu: bool,
    /// Camera devices (label, id), refreshed when the Add Layer menu opens.
    camera_devices: Vec<(String, String)>,
    /// The open inline rename field in the Layers or Scenes list.
    renaming: Option<Renaming>,
    /// Studio mode (docs/07-scenes-layers-canvas.md): the stage shows the selected scene, a
    /// monitor shows Program, and Take puts the selected scene on air.
    studio_mode: bool,
    program_monitor: Option<Preview>,
    /// Inspector content sections (text field, media status).
    props: layer_props::LayerProps,
    /// Safe-area overlay shown on landscape and on vertical stages.
    safe_landscape: Option<Profile>,
    safe_vertical: Option<Profile>,
    /// The open command palette (⌘K).
    palette: Option<palette::Palette>,
}

const HANDLE: f32 = 10.0;
const SNAP_PX: f32 = 8.0;

impl StudioWindow {
    pub fn new(studio: Entity<Studio>, preview: Preview, window: &mut Window, cx: &mut Context<Self>) -> Self {
        cx.observe(&studio, |this, _, cx| {
            this.sync_studio_preview(cx);
            cx.notify()
        })
        .detach();
        let focus = cx.focus_handle();
        window.focus(&focus, cx);
        let mixer = cx.new(|cx| MixerPanel::new(studio.clone(), cx));
        let outputs = cx.new(|cx| OutputsPanel::new(studio.clone(), cx));
        let primary = studio.read(cx).project().primary_format().id;
        Self {
            previews: HashMap::from([(primary, preview)]),
            stage_bounds: Rc::default(),
            active_format: Some(primary),
            side_by_side: false,
            mixer,
            outputs,
            confirm_end: None,
            studio,
            focus,
            drag: None,
            guides: vec![],
            add_menu: false,
            camera_devices: vec![],
            renaming: None,
            studio_mode: false,
            program_monitor: None,
            props: Default::default(),
            safe_landscape: None,
            safe_vertical: None,
            palette: None,
        }
    }

    pub(crate) fn exec(&self, cmd: Command, cx: &mut Context<Self>) {
        self.studio.update(cx, |s, cx| {
            let _ = s.execute(cmd, cx);
        });
    }

    fn start_rename(&mut self, target: RowTarget, name: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.renaming = Some(Renaming::open(target, name, window, cx, |this, commit, window, cx| {
            this.finish_rename(commit, window, cx)
        }));
        cx.notify();
    }

    fn finish_rename(&mut self, commit: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(renaming) = self.renaming.take() else { return };
        if commit && let Some(cmd) = renaming.command(cx) {
            self.exec(cmd, cx);
        }
        window.focus(&self.focus, cx);
        cx.notify();
    }

    /// Enter renames the selected layer (or the selected scene when no layer is selected).
    fn rename_selection(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let studio = self.studio.read(cx);
        let project = studio.project().clone();
        let Some(scene) = studio.selected_scene.and_then(|id| project.scene(id)) else { return };
        match studio.selected_layer.and_then(|id| scene.layer(id)) {
            Some(layer) => {
                let target = RowTarget::Layer { scene: scene.id, layer: layer.id };
                let name = layer.name.clone();
                self.start_rename(target, &name, window, cx)
            }
            None => {
                let name = scene.name.clone();
                self.start_rename(RowTarget::Scene(scene.id), &name, window, cx)
            }
        }
    }

    /// Drops `row` onto the row at model index `to` of the same list.
    fn drop_row(&mut self, row: &DraggedRow, onto: RowTarget, to: usize, cx: &mut Context<Self>) {
        let cmd = match (row.target, onto) {
            (RowTarget::Scene(scene), RowTarget::Scene(other)) if scene != other => Command::MoveScene { scene, to },
            (RowTarget::Layer { scene, layer }, RowTarget::Layer { scene: s2, layer: other })
                if scene == s2 && layer != other =>
            {
                Command::MoveLayer { scene, layer, to }
            }
            _ => return,
        };
        self.exec(cmd, cx);
    }

    /// A row's name, or the rename field when it's being renamed.
    fn row_label(&self, target: RowTarget, name: &str, color: Hsla) -> AnyElement {
        match &self.renaming {
            Some(r) if r.target == target => div()
                .flex_1()
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .child(gpui_kit::component::input::Input::new(&r.input).small())
                .into_any_element(),
            _ => div()
                .flex_1()
                .overflow_hidden()
                .text_ellipsis()
                .text_color(color)
                .child(name.to_string())
                .into_any_element(),
        }
    }

    fn select_layer(&self, layer: Option<Id>, cx: &mut Context<Self>) {
        self.studio.update(cx, |s, cx| {
            s.selected_layer = layer;
            cx.notify();
        });
    }

    fn format_id(&self, project: &Project) -> Id {
        self.active_format.filter(|f| project.format(*f).is_some()).unwrap_or(project.primary_format().id)
    }

    fn format<'a>(&self, project: &'a Project) -> &'a Format {
        project.format(self.format_id(project)).unwrap_or(project.primary_format())
    }

    /// Formats shown on screen: the active one, or all of them side by side.
    fn visible_formats(&self, project: &Project) -> Vec<Format> {
        if self.side_by_side && project.formats.len() > 1 {
            project.formats.clone()
        } else {
            vec![self.format(project).clone()]
        }
    }

    /// Canvas-space mapping for a format's stage: (origin x, origin y, scale) in window pixels.
    fn mapping(&self, project: &Project, format: Id) -> Option<(f32, f32, f32)> {
        let b = *self.stage_bounds.borrow().get(&format)?;
        let f = project.format(format)?;
        let (cw, ch) = (f.width as f32, f.height as f32);
        let (bw, bh) = (f32::from(b.size.width), f32::from(b.size.height));
        let s = (bw / cw).min(bh / ch);
        Some((f32::from(b.origin.x) + (bw - cw * s) / 2.0, f32::from(b.origin.y) + (bh - ch * s) / 2.0, s))
    }

    fn to_canvas(&self, project: &Project, format: Id, p: Point<Pixels>) -> Option<(f32, f32)> {
        let (ox, oy, s) = self.mapping(project, format)?;
        Some(((f32::from(p.x) - ox) / s, (f32::from(p.y) - oy) / s))
    }

    /// The visible format whose stage contains the pointer.
    fn stage_at(&self, project: &Project, p: Point<Pixels>) -> Option<Id> {
        let bounds = self.stage_bounds.borrow();
        self.visible_formats(project)
            .into_iter()
            .map(|f| f.id)
            .find(|id| bounds.get(id).is_some_and(|b| b.contains(&p)))
    }

    fn set_studio_mode(&mut self, on: bool, cx: &mut Context<Self>) {
        self.studio_mode = on;
        self.program_monitor = on.then(Preview::attach);
        if let Some(m) = &self.program_monitor {
            let f = self.studio.read(cx).project().primary_format().clone();
            m.set_size(Some((512, 512 * f.height / f.width.max(1))));
        }
        self.sync_studio_preview(cx);
        cx.notify();
    }

    /// In Studio mode the primary stage shows the selected scene rather than Program.
    fn sync_studio_preview(&mut self, cx: &mut Context<Self>) {
        let studio = self.studio.read(cx);
        let format = studio.project().primary_format().id;
        let Some(preview) = self.previews.get(&format) else { return };
        let want = studio.selected_scene.filter(|_| self.studio_mode);
        let current = preview.source().map(|p| p as usize);
        let engine = studio.engine().clone();
        let next = match want {
            None => None,
            Some(scene) => {
                // Only take a new reference when the libobs scene changed (rebuilds replace it).
                let ptr = engine.call(move |s| s.reconciler.scene_source(scene, format).map(|p| p as usize));
                if ptr == current {
                    return;
                }
                engine.call(move |s| s.reconciler.scene_source_ref(scene, format).map(|p| p as usize))
            }
        };
        if next.is_some() || current.is_some() {
            preview.set_source(next.map(|p| p as *mut _));
        }
    }

    /// Studio mode's Take: the selected scene goes on air with the current transition.
    fn take(&mut self, cx: &mut Context<Self>) {
        let studio = self.studio.read(cx);
        let Some(scene) = studio.selected_scene else { return };
        if studio.project().program == Some(scene) {
            return;
        }
        self.exec(Command::SetProgram { scene: Some(scene) }, cx);
    }

    /// Creates previews for new formats and drops those of removed ones.
    fn sync_previews(&mut self, cx: &mut Context<Self>) {
        let project = self.studio.read(cx).project().clone();
        self.previews.retain(|id, _| project.format(*id).is_some());
        for f in &project.formats {
            if self.previews.contains_key(&f.id) {
                continue;
            }
            let id = f.id;
            let canvas = self.studio.read(cx).engine().call(move |s| s.reconciler.canvas_ref(id).map(|p| p as usize));
            if let Some(ptr) = canvas {
                self.previews.insert(id, Preview::attach_canvas(ptr as *mut _));
            }
        }
    }

    /// Adds a vertical format with every layer auto-reframed (one undo step).
    fn add_vertical(&mut self, cx: &mut Context<Self>) {
        let project = self.studio.read(cx).project().clone();
        let from = project.primary_format().clone();
        let to = Format::vertical();
        let to_id = to.id;
        let mut commands = vec![Command::AddFormat { format: to.clone() }];
        for scene in &project.scenes {
            for layer in &scene.layers {
                if let Some(l) = layer.layouts.get(&from.id) {
                    let layout = crate::reframe::reframe(l, &from, &to);
                    commands.push(Command::SetLayout { scene: scene.id, layer: layer.id, format: to_id, layout });
                }
            }
        }
        self.exec(Command::Batch { label: "Add vertical format".into(), commands }, cx);
        self.active_format = Some(to_id);
        self.side_by_side = true;
    }

    // ------------------------------------------------------------ canvas interaction
    fn canvas_down(&mut self, ev: &MouseDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        window.focus(&self.focus, cx);
        self.add_menu = false;
        let project = self.studio.read(cx).project().clone();
        let Some(format) = self.stage_at(&project, ev.position) else { return };
        self.active_format = Some(format);
        let studio = self.studio.read(cx);
        let (Some(scene_id), Some((x, y)), Some((_, _, s))) =
            (studio.selected_scene, self.to_canvas(&project, format, ev.position), self.mapping(&project, format))
        else {
            return;
        };
        let Some(scene) = project.scene(scene_id) else { return };
        let tol = HANDLE / s;

        // A handle of the selected layer?
        if let Some(sel) = studio.selected_layer.and_then(|id| scene.layer(id))
            && let Some(l) = sel.layouts.get(&format).filter(|_| !sel.locked)
        {
            let (hx, hy) = geo::rotate_handle(l, ROTATE_OFFSET / s);
            if (x - hx).abs() <= tol && (y - hy).abs() <= tol {
                self.drag = Some(Drag {
                    scene: scene_id,
                    layer: sel.id,
                    format,
                    mode: DragMode::Rotate,
                    start: (x, y),
                    layout: *l,
                });
                self.studio.update(cx, |s, _| s.begin_gesture());
                return;
            }
            if let Some(corner) = CORNERS.iter().find(|c| {
                let (px_, py_) = geo::corner(l, **c);
                (x - px_).abs() <= tol && (y - py_).abs() <= tol
            }) {
                let input = sel.input;
                let size =
                    ev.modifiers.alt.then(|| studio.engine().call(move |s| s.reconciler.input_size(input))).flatten();
                let mode = match size {
                    Some((w, h)) => DragMode::Crop(*corner, (w as f32, h as f32)),
                    None => DragMode::Resize(*corner),
                };
                self.drag = Some(Drag { scene: scene_id, layer: sel.id, format, mode, start: (x, y), layout: *l });
                self.studio.update(cx, |s, _| s.begin_gesture());
                return;
            }
        }
        // Topmost visible layer under the pointer.
        let hit = scene
            .layers
            .iter()
            .rev()
            .find(|l| l.layouts.get(&format).is_some_and(|lo| lo.visible && geo::contains(lo, (x, y))));
        match hit {
            Some(layer) => {
                let id = layer.id;
                if !layer.locked {
                    let layout = layer.layouts[&format];
                    let mode = DragMode::Move;
                    self.drag = Some(Drag { scene: scene_id, layer: id, format, mode, start: (x, y), layout });
                    self.studio.update(cx, |s, _| s.begin_gesture());
                }
                self.select_layer(Some(id), cx);
            }
            None => self.select_layer(None, cx),
        }
    }

    fn canvas_move(&mut self, ev: &MouseMoveEvent, _window: &mut Window, cx: &mut Context<Self>) {
        let Some(drag) = &self.drag else { return };
        if ev.pressed_button != Some(MouseButton::Left) {
            return;
        }
        let project = self.studio.read(cx).project().clone();
        let (Some((x, y)), Some((_, _, s))) =
            (self.to_canvas(&project, drag.format, ev.position), self.mapping(&project, drag.format))
        else {
            return;
        };
        let (dx, dy) = (x - drag.start.0, y - drag.start.1);
        let Some(f) = project.format(drag.format) else { return };
        let (cw, ch) = (f.width as f32, f.height as f32);
        let mut l = drag.layout;
        let mut guides = Guides::new();
        match drag.mode {
            DragMode::Rotate => {
                let (cx_, cy_) = (l.x + l.width / 2.0, l.y + l.height / 2.0);
                let mut angle = (y - cy_).atan2(x - cx_).to_degrees() + 90.0;
                if ev.modifiers.shift {
                    angle = (angle / 15.0).round() * 15.0;
                }
                l.rotation = geo::normalise_degrees(angle);
            }
            DragMode::Crop(corner, size) => l = geo::crop(&l, corner, size, (dx, dy)),
            DragMode::Move => {
                l.x += dx;
                l.y += dy;
                if !ev.modifiers.platform {
                    // Snap edges and centre to the canvas edges and centre.
                    let thr = SNAP_PX / s;
                    let snap = |pos: f32, size: f32, extent: f32, guides: &mut Guides, vertical: bool| -> f32 {
                        for (offset, target) in [(0.0, 0.0), (size / 2.0, extent / 2.0), (size, extent)] {
                            for t in [0.0, extent / 2.0, extent] {
                                if (pos + offset - t).abs() < thr {
                                    guides.push((vertical, t));
                                    return t - offset;
                                }
                            }
                            let _ = target;
                        }
                        pos
                    };
                    l.x = snap(l.x, l.width, cw, &mut guides, true);
                    l.y = snap(l.y, l.height, ch, &mut guides, false);
                }
            }
            DragMode::Resize(corner) => l = geo::resize(&l, corner, (dx, dy), ev.modifiers.shift),
        }
        self.guides = guides;
        let cmd = Command::SetLayout { scene: drag.scene, layer: drag.layer, format: drag.format, layout: l };
        self.exec(cmd, cx);
    }

    fn canvas_up(&mut self, _: &MouseUpEvent, _window: &mut Window, cx: &mut Context<Self>) {
        if self.drag.take().is_some() {
            self.guides.clear();
            self.studio.update(cx, |s, cx| {
                s.end_gesture();
                cx.notify();
            });
        }
    }

    fn nudge(&mut self, dx: f32, dy: f32, cx: &mut Context<Self>) {
        let studio = self.studio.read(cx);
        let project = studio.project().clone();
        let format = self.format_id(&project);
        let (Some(scene), Some(layer)) = (studio.selected_scene, studio.selected_layer) else { return };
        let Some(l) = project.scene(scene).and_then(|s| s.layer(layer)).and_then(|l| l.layouts.get(&format)).copied()
        else {
            return;
        };
        let layout = Layout { x: l.x + dx, y: l.y + dy, ..l };
        self.exec(Command::SetLayout { scene, layer, format, layout }, cx);
    }

    fn delete_selection(&mut self, cx: &mut Context<Self>) {
        let studio = self.studio.read(cx);
        if let (Some(scene), Some(layer)) = (studio.selected_scene, studio.selected_layer) {
            self.exec(Command::RemoveLayer { scene, layer }, cx);
        }
    }

    // ------------------------------------------------------------ adding things
    fn add_input_layer(
        &mut self,
        name: &str,
        kind: &str,
        settings: serde_json::Value,
        size: (f32, f32),
        cx: &mut Context<Self>,
    ) {
        self.add_menu = false;
        let studio = self.studio.read(cx);
        let project = studio.project().clone();
        let Some(scene) = studio.selected_scene else { return };
        let (w, h) = size;
        let layouts: Vec<(Id, Layout)> = project
            .formats
            .iter()
            .map(|f| {
                let s = (f.width as f32 / w).min(f.height as f32 / h).min(1.0);
                let (lw, lh) = (w * s, h * s);
                (f.id, Layout::boxed((f.width as f32 - lw) / 2.0, (f.height as f32 - lh) / 2.0, lw, lh))
            })
            .collect();
        let input = Input::new(name, kind, settings);
        let layer = Layer::new(name, input.id, layouts);
        let layer_id = layer.id;
        let cmd = Command::Batch {
            label: format!("Add {name}"),
            commands: vec![Command::AddInput { input }, Command::AddLayer { scene, layer, index: None }],
        };
        self.exec(cmd, cx);
        self.select_layer(Some(layer_id), cx);
    }

    /// A camera layer; the input gets Camera Studio (off until a look is chosen).
    fn add_camera(&mut self, label: &str, uid: &str, cx: &mut Context<Self>) {
        self.add_input_layer(label, "macos-avcapture", json!({"device": uid}), (1280.0, 720.0), cx);
        let project = self.studio.read(cx).project().clone();
        if let Some(input) = project.inputs.values().find(|i| i.settings["device"] == uid && i.effects.is_empty())
            && let Some(cmd) = crate::camera_studio::ensure_effect(input)
        {
            self.exec(cmd, cx);
        }
    }

    fn add_image(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.add_menu = false;
        let paths =
            cx.prompt_for_paths(PathPromptOptions { files: true, directories: false, multiple: false, prompt: None });
        cx.spawn_in(window, async move |this, cx| {
            let Ok(Ok(Some(paths))) = paths.await else { return };
            let Some(path): Option<PathBuf> = paths.into_iter().next() else { return };
            let name = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| "Image".into());
            let _ = this.update(cx, |this, cx| {
                this.add_input_layer(&name, "image_source", json!({"file": path.to_string_lossy()}), (640.0, 360.0), cx)
            });
        })
        .detach();
    }

    fn add_text(&mut self, cx: &mut Context<Self>) {
        let settings = json!({
            "text": "Lightair",
            "font": {"face": "Helvetica Neue", "size": 128, "flags": 1},
            "color1": 0xFFFFFFFFu32,
            "color2": 0xFFFFFFFFu32,
        });
        self.add_input_layer("Text", "text_ft2_source_v2", settings, (720.0, 180.0), cx)
    }

    /// A 5-minute countdown that starts when its scene goes on air.
    fn add_timer(&mut self, cx: &mut Context<Self>) {
        let settings = json!({
            "mode": "countdown",
            "seconds": 300,
            "autostart": true,
            "font": {"face": "Helvetica Neue", "size": 160, "flags": 1},
            "color1": 0xFFFFFFFFu32,
            "color2": 0xFFFFFFFFu32,
        });
        self.add_input_layer("Countdown", lightair_engine::timers::TIMER_KIND, settings, (560.0, 200.0), cx)
    }

    fn add_colour(&mut self, cx: &mut Context<Self>) {
        let settings = json!({"color": 0xFF4A3BFFu32, "width": 400, "height": 400});
        self.add_input_layer("Colour", "color_source_v3", settings, (400.0, 400.0), cx)
    }

    /// A video file layer: plays when its scene goes on air, loops by default.
    fn add_video(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.add_menu = false;
        let paths =
            cx.prompt_for_paths(PathPromptOptions { files: true, directories: false, multiple: false, prompt: None });
        cx.spawn_in(window, async move |this, cx| {
            let Ok(Ok(Some(paths))) = paths.await else { return };
            let Some(path): Option<PathBuf> = paths.into_iter().next() else { return };
            let name = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| "Video".into());
            let settings = json!({
                "local_file": path.to_string_lossy(),
                "is_local_file": true,
                "looping": true,
                "restart_on_activate": true,
            });
            let _ = this
                .update(cx, |this, cx| this.add_input_layer(&name, "ffmpeg_source", settings, (1920.0, 1080.0), cx));
        })
        .detach();
    }

    /// Imports an OBS scene collection (OBS keeps them in
    /// ~/Library/Application Support/obs-studio/basic/scenes) as one undo step.
    fn import_obs(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let paths = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some("Import".into()),
        });
        cx.spawn_in(window, async move |this, cx| {
            let Ok(Ok(Some(paths))) = paths.await else { return };
            let Some(path): Option<PathBuf> = paths.into_iter().next() else { return };
            let _ = this.update(cx, |this, cx| {
                this.studio.update(cx, |s, cx| s.import_obs(&path, cx));
            });
        })
        .detach();
    }

    fn add_scene(&mut self, cx: &mut Context<Self>) {
        let n = self.studio.read(cx).project().scenes.len() + 1;
        let scene = Scene::new(format!("Scene {n}"));
        let id = scene.id;
        self.exec(Command::AddScene { scene, index: None }, cx);
        self.studio.update(cx, |s, cx| {
            s.selected_scene = Some(id);
            s.selected_layer = None;
            cx.notify();
        });
    }
}

// ================================================================= render
impl Render for StudioWindow {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        window.request_animation_frame();
        self.sync_previews(cx);
        self.sync_layer_props(window, cx);
        let studio = self.studio.read(cx);
        let project = studio.project().clone();

        div()
            .relative()
            .key_context("Studio")
            .track_focus(&self.focus)
            .on_action(cx.listener(|this, _: &Undo, _, cx| this.studio.update(cx, |s, cx| s.undo(cx))))
            .on_action(cx.listener(|this, _: &Redo, _, cx| this.studio.update(cx, |s, cx| s.redo(cx))))
            .on_action(cx.listener(|this, _: &DeleteSelection, _, cx| this.delete_selection(cx)))
            .on_action(cx.listener(|this, _: &NudgeLeft, _, cx| this.nudge(-1.0, 0.0, cx)))
            .on_action(cx.listener(|this, _: &NudgeRight, _, cx| this.nudge(1.0, 0.0, cx)))
            .on_action(cx.listener(|this, _: &NudgeUp, _, cx| this.nudge(0.0, -1.0, cx)))
            .on_action(cx.listener(|this, _: &NudgeDown, _, cx| this.nudge(0.0, 1.0, cx)))
            .on_action(cx.listener(|this, _: &Deselect, _, cx| this.select_layer(None, cx)))
            .on_action(cx.listener(|this, _: &RenameSelection, window, cx| this.rename_selection(window, cx)))
            .on_action(cx.listener(|this, _: &OpenPalette, window, cx| this.open_palette(window, cx)))
            .on_action(cx.listener(|this, _: &AddMarker, _, cx| {
                if this.studio.read(cx).is_recording() {
                    this.studio.update(cx, |s, cx| s.add_marker(cx))
                }
            }))
            .on_action(cx.listener(|this, _: &Take, _, cx| {
                if this.studio_mode {
                    this.take(cx)
                }
            }))
            .size_full()
            .flex()
            .flex_col()
            .bg(tok::bg_app())
            .text_color(tok::text())
            .text_size(px(13.))
            .child(self.title_bar(&project, cx))
            .child(
                div()
                    .flex()
                    .flex_1()
                    .min_h_0()
                    .child(self.left_column(&project, cx))
                    .child(self.centre(&project, cx))
                    .child(self.right_column(&project, cx)),
            )
            .child(self.status_bar(cx))
            .children(self.palette_overlay(cx))
    }
}

impl StudioWindow {
    fn title_bar(&self, project: &Project, cx: &mut Context<Self>) -> impl IntoElement {
        let studio = self.studio.read(cx);
        let stats = studio.stats;
        let recent_lagged = studio.recent_lagged;
        let fps_color = if stats.active_fps > 0.0 && recent_lagged == 0 { tok::ok() } else { tok::warn() };
        let has_destination = project.outputs.destinations.iter().any(|d| d.enabled);

        let rec_status = runtime().get(OutputKey::Recording);
        let rec = rec_status.state.clone();
        let recording = matches!(rec, OutputState::Active { .. });
        let record = match rec {
            OutputState::Active { since } | OutputState::Reconnecting { since } => {
                let t = rec_status.recorded(since).as_secs();
                let time = format!("{:02}:{:02}:{:02}", t / 3600, (t / 60) % 60, t % 60);
                let label = if rec_status.paused { format!("❚❚ {time}") } else { format!("● {time}") };
                Button::new("record").label(label).small().danger().tooltip("Stop recording")
            }
            OutputState::Starting | OutputState::Stopping => {
                Button::new("record").label("Record").small().outline().loading(true)
            }
            _ => Button::new("record").label("Record").small().outline().tooltip("Start recording"),
        }
        .on_click(cx.listener(|this, _, _, cx| this.studio.update(cx, |s, cx| s.toggle_recording(cx))));
        let paused = rec_status.paused;
        let recording_tools = recording.then(|| {
            div()
                .flex()
                .items_center()
                .child(
                    ui::icon_button("rec-pause", if paused { "play" } else { "pause" }, tok::text_secondary())
                        .tooltip(move |w, cx| {
                            Tooltip::new(if paused { "Resume recording" } else { "Pause recording" }).build(w, cx)
                        })
                        .on_click(cx.listener(|this, _, _, cx| this.studio.update(cx, |s, cx| s.pause_recording(cx)))),
                )
                .child(
                    ui::icon_button("rec-marker", "bookmark", tok::text_secondary())
                        .tooltip(|w, cx| Tooltip::new("Add marker (⌘M)").build(w, cx))
                        .on_click(cx.listener(|this, _, _, cx| this.studio.update(cx, |s, cx| s.add_marker(cx)))),
                )
                .child(
                    ui::icon_button("rec-split", "split", tok::text_secondary())
                        .tooltip(|w, cx| Tooltip::new("Continue in a new file").build(w, cx))
                        .on_click(cx.listener(|this, _, _, cx| this.studio.update(cx, |s, cx| s.split_recording(cx)))),
                )
        });

        let live_since = runtime().stream_since();
        let streaming = runtime().any_stream_running();
        let armed = self.confirm_end.is_some_and(|t| t.elapsed().as_secs_f32() < 3.0);
        let go_live = if streaming {
            let label = if armed {
                "Click again to end".to_string()
            } else {
                live_since.map(|t| format!("Live {}", elapsed(t))).unwrap_or_else(|| "Going live…".into())
            };
            Button::new("go-live").label(label).small().danger().on_click(cx.listener(|this, _, _, cx| {
                if this.confirm_end.is_some_and(|t| t.elapsed().as_secs_f32() < 3.0) {
                    this.confirm_end = None;
                    this.studio.update(cx, |s, cx| s.end_stream(cx));
                } else {
                    this.confirm_end = Some(Instant::now());
                    cx.notify();
                }
            }))
        } else {
            Button::new("go-live")
                .label("Go live")
                .small()
                .primary()
                .disabled(!has_destination)
                .tooltip(if has_destination {
                    "Start every enabled destination"
                } else {
                    "Add a destination in Outputs first"
                })
                .on_click(cx.listener(|this, _, _, cx| this.studio.update(cx, |s, cx| s.go_live(cx))))
        };

        div()
            .flex()
            .items_center()
            .h(px(44.))
            .pl(px(84.))
            .pr_3()
            .gap_3()
            .border_b_1()
            .border_color(tok::border())
            .bg(tok::bg_app())
            .child(div().font_weight(FontWeight::SEMIBOLD).child(project.name.clone()))
            .when(streaming, |d| {
                d.child(
                    div()
                        .px_2()
                        .py_0p5()
                        .rounded(px(4.))
                        .bg(tok::live())
                        .text_size(px(10.))
                        .font_weight(FontWeight::BOLD)
                        .text_color(rgb(0xFFFFFF))
                        .child("LIVE"),
                )
            })
            .child(div().flex_1())
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .text_size(px(11.))
                    .text_color(tok::text_secondary())
                    .child(div().size(px(8.)).rounded_full().bg(fps_color))
                    .child(format!("{:.1} FPS", stats.active_fps))
                    .child(format!("{:.1} ms", stats.average_frame_time_ms)),
            )
            .child(go_live)
            .children(recording_tools)
            .child(record)
            .child({
                let ready = self.studio.read(cx).clips_ready();
                Button::new("clip")
                    .label("Clip")
                    .small()
                    .outline()
                    .disabled(!ready)
                    .tooltip(if ready { "Save the last seconds as a clip" } else { "Turn on Clips in Outputs" })
                    .on_click(cx.listener(|this, _, _, cx| this.studio.update(cx, |s, cx| s.save_clip(cx))))
            })
    }

    fn left_column(&self, project: &Project, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .flex()
            .flex_col()
            .w(px(264.))
            .h_full()
            .border_r_1()
            .border_color(tok::border())
            .bg(tok::bg_panel())
            .child(self.layers_panel(project, cx))
            .child(div().h(px(1.)).bg(tok::border()))
            .child(self.scenes_panel(project, cx))
    }

    fn layers_panel(&self, project: &Project, cx: &mut Context<Self>) -> impl IntoElement {
        let studio = self.studio.read(cx);
        let format = self.format_id(project);
        let scene = studio.selected_scene.and_then(|id| project.scene(id)).cloned();
        let selected = studio.selected_layer;
        let add = ui::icon_button("add-layer", "plus", tok::text_secondary())
            .tooltip(|w, cx| Tooltip::new("Add layer").build(w, cx))
            .on_click(cx.listener(|this, _, _, cx| {
                this.add_menu = !this.add_menu;
                // OBS 33's camera source can't list devices without an instance; the legacy source
                // lists the same AVFoundation device ids.
                if this.add_menu {
                    this.camera_devices = this
                        .studio
                        .read(cx)
                        .engine()
                        .call(|_| lightair_engine::obs::property_items("av_capture_input_v2", "device"));
                }
                cx.notify();
            }));
        let mut col =
            div().flex().flex_col().flex_1().min_h_0().child(ui::panel_header("Layers", Some(add.into_any_element())));
        if self.add_menu {
            col = col.child(self.add_menu(cx));
        }
        let Some(scene) = scene else {
            return col.child(div().px_3().text_color(tok::text_tertiary()).child("No scene selected"));
        };
        if scene.layers.is_empty() {
            col = col.child(
                div().px_3().py_2().text_color(tok::text_tertiary()).child("This scene is empty. Add a layer with +."),
            );
        }
        let scene_id = scene.id;
        let count = scene.layers.len();
        let rows = scene.layers.iter().rev().enumerate().map(|(i, layer)| {
            let id = layer.id;
            let kind = project.inputs.get(&layer.input).map(|i| i.kind.clone()).unwrap_or_default();
            let visible = layer.layouts.get(&format).is_some_and(|l| l.visible);
            let locked = layer.locked;
            let is_sel = selected == Some(id);
            let target = RowTarget::Layer { scene: scene_id, layer: id };
            let dragged = DraggedRow { target, name: layer.name.clone().into(), icon: ui::input_icon(&kind) };
            let name = layer.name.clone();
            // The list shows the top layer first; the model stores the bottom first.
            let model_index = count - 1 - i;
            div()
                .id(("layer", i))
                .flex()
                .items_center()
                .gap_2()
                .h(px(36.))
                .mx_2()
                .px_2()
                .rounded(px(6.))
                .cursor_pointer()
                .when(is_sel, |d| d.bg(tok::bg_selected()))
                .hover(|d| d.bg(tok::bg_raised()))
                .on_click(cx.listener(move |this, ev: &ClickEvent, window, cx| {
                    this.select_layer(Some(id), cx);
                    if ev.click_count() == 2 {
                        this.start_rename(target, &name, window, cx);
                    }
                }))
                .on_drag(dragged, |row, _, _, cx| cx.new(|_| row.clone()))
                .drag_over::<DraggedRow>(|style, _, _, _| style.bg(tok::bg_selected()))
                .on_drop(cx.listener(move |this, row: &DraggedRow, _, cx| this.drop_row(row, target, model_index, cx)))
                .child(ui::icon(ui::input_icon(&kind)).size(px(15.)).text_color(if visible {
                    tok::text_secondary()
                } else {
                    tok::text_tertiary()
                }))
                .child(self.row_label(target, &layer.name, if visible { tok::text() } else { tok::text_tertiary() }))
                .child(
                    ui::icon_button(
                        ("lock", i),
                        if locked { "lock" } else { "lock-open" },
                        if locked { tok::text() } else { tok::text_tertiary() },
                    )
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.exec(Command::SetLayerLocked { scene: scene_id, layer: id, locked: !locked }, cx)
                    })),
                )
                .child(
                    ui::icon_button(
                        ("eye", i),
                        if visible { "eye" } else { "eye-off" },
                        if visible { tok::text_secondary() } else { tok::text_tertiary() },
                    )
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.exec(
                            Command::SetLayerVisible { scene: scene_id, layer: id, format, visible: !visible },
                            cx,
                        )
                    })),
                )
        });
        col.child(div().id("layers-scroll").flex().flex_col().gap_0p5().overflow_y_scroll().children(rows))
    }

    fn add_menu(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let item = |id: &'static str, icon: &'static str, label: &'static str| {
            div()
                .id(id)
                .flex()
                .items_center()
                .gap_2()
                .h(px(30.))
                .px_2()
                .rounded(px(6.))
                .cursor_pointer()
                .hover(|d| d.bg(tok::bg_selected()))
                .child(ui::icon(icon).size(px(14.)).text_color(tok::text_secondary()))
                .child(label)
        };
        let mut cameras = Vec::new();
        for (i, (label, uid)) in self.camera_devices.iter().enumerate() {
            let (label, uid) = (label.clone(), uid.clone());
            cameras.push(
                div()
                    .id(("add-camera", i))
                    .flex()
                    .items_center()
                    .gap_2()
                    .h(px(30.))
                    .px_2()
                    .rounded(px(6.))
                    .cursor_pointer()
                    .hover(|d| d.bg(tok::bg_selected()))
                    .child(ui::icon("camera").size(px(14.)).text_color(tok::text_secondary()))
                    .child(label.clone())
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.add_camera(&label, &uid, cx);
                    })),
            );
        }
        let display_settings = {
            let mut v = json!({"type": 0, "show_cursor": true, "hide_obs": true});
            if let Some(uuid) = lightair_engine::displays::main_display_uuid() {
                v["display_uuid"] = uuid.into();
            }
            v
        };
        div()
            .mx_2()
            .mb_2()
            .p_1()
            .rounded(px(10.))
            .bg(tok::bg_raised())
            .border_1()
            .border_color(tok::border())
            .child(item("add-display", "monitor", "Display").on_click(cx.listener(move |this, _, _, cx| {
                this.add_input_layer("Display", "screen_capture", display_settings.clone(), (1280.0, 720.0), cx)
            })))
            .children(cameras)
            .child(
                item("add-image", "image", "Image…")
                    .on_click(cx.listener(|this, _, window, cx| this.add_image(window, cx))),
            )
            .child(
                item("add-video", "clapperboard", "Video…")
                    .on_click(cx.listener(|this, _, window, cx| this.add_video(window, cx))),
            )
            .child(item("add-text", "type", "Text").on_click(cx.listener(|this, _, _, cx| this.add_text(cx))))
            .child(item("add-colour", "square", "Colour").on_click(cx.listener(|this, _, _, cx| this.add_colour(cx))))
            .child(item("add-timer", "timer", "Timer").on_click(cx.listener(|this, _, _, cx| this.add_timer(cx))))
    }

    fn transition_picker(&self, project: &Project, cx: &mut Context<Self>) -> impl IntoElement {
        let current = project.transition;
        let seg = |i: usize, kind: TransitionKind| {
            let active = current.kind == kind;
            div()
                .id(("tr-kind", i))
                .flex()
                .items_center()
                .justify_center()
                .h(px(22.))
                .px_2()
                .rounded(px(4.))
                .text_size(px(11.))
                .cursor_pointer()
                .when(active, |d| d.bg(tok::bg_selected()).text_color(tok::text()))
                .when(!active, |d| d.text_color(tok::text_secondary()))
                .hover(|d| d.bg(tok::bg_raised()))
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.exec(Command::SetTransition { settings: TransitionSettings { kind, ..current } }, cx)
                }))
                .child(kind.label())
        };
        let next_ms = match current.duration_ms {
            0..=300 => 500,
            301..=500 => 800,
            501..=800 => 1200,
            _ => 300,
        };
        let direction = current.kind.directional().then(|| {
            let icon = match current.direction {
                Direction::Left => "arrow-left",
                Direction::Right => "arrow-right",
                Direction::Up => "arrow-up",
                Direction::Down => "arrow-down",
            };
            ui::icon_button("tr-direction", icon, tok::text_secondary())
                .tooltip(|w, cx| Tooltip::new("Direction").build(w, cx))
                .on_click(cx.listener(move |this, _, _, cx| {
                    let settings = TransitionSettings { direction: current.direction.next(), ..current };
                    this.exec(Command::SetTransition { settings }, cx)
                }))
        });
        let kinds: Vec<_> = TransitionKind::ALL.iter().enumerate().map(|(i, k)| seg(i, *k)).collect();
        div()
            .flex()
            .flex_wrap()
            .items_center()
            .gap_1()
            .px_3()
            .pb_2()
            .child(div().text_size(px(11.)).text_color(tok::text_tertiary()).mr_1().child("Transition"))
            .children(kinds)
            .child(div().flex_1())
            .children(direction)
            .child(
                div()
                    .id("tr-duration")
                    .px_1p5()
                    .h(px(22.))
                    .flex()
                    .items_center()
                    .rounded(px(4.))
                    .border_1()
                    .border_color(tok::border_strong())
                    .text_size(px(11.))
                    .font_family("Menlo")
                    .text_color(tok::text_secondary())
                    .cursor_pointer()
                    .hover(|d| d.bg(tok::bg_raised()))
                    .when(current.kind == TransitionKind::Cut, |d| d.opacity(0.4))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.exec(
                            Command::SetTransition { settings: TransitionSettings { duration_ms: next_ms, ..current } },
                            cx,
                        )
                    }))
                    .child(format!("{} ms", current.duration_ms)),
            )
    }

    fn scenes_panel(&self, project: &Project, cx: &mut Context<Self>) -> impl IntoElement {
        let picker = self.transition_picker(project, cx).into_any_element();
        let studio = self.studio.read(cx);
        let selected = studio.selected_scene;
        let program = project.program;
        let add = ui::icon_button("add-scene", "plus", tok::text_secondary())
            .tooltip(|w, cx| Tooltip::new("Add scene").build(w, cx))
            .on_click(cx.listener(|this, _, _, cx| this.add_scene(cx)));
        let import = ui::icon_button("import-obs", "import", tok::text_secondary())
            .tooltip(|w, cx| Tooltip::new("Import OBS scenes…").build(w, cx))
            .on_click(cx.listener(|this, _, window, cx| this.import_obs(window, cx)));
        let actions = div().flex().gap_1().child(import).child(add);
        let rows = project.scenes.iter().enumerate().map(|(i, scene)| {
            let id = scene.id;
            let on_air = program == Some(id);
            let target = RowTarget::Scene(id);
            let dragged = DraggedRow { target, name: scene.name.clone().into(), icon: "layout-dashboard" };
            let name = scene.name.clone();
            div()
                .id(("scene", i))
                .relative()
                .flex()
                .items_center()
                .h(px(36.))
                .mx_2()
                .px_3()
                .rounded(px(6.))
                .cursor_pointer()
                .when(selected == Some(id), |d| d.bg(tok::bg_selected()))
                .hover(|d| d.bg(tok::bg_raised()))
                .on_click(cx.listener(move |this, ev: &ClickEvent, window, cx| {
                    if ev.click_count() == 2 {
                        this.start_rename(target, &name, window, cx);
                        return;
                    }
                    let studio_mode = this.studio_mode;
                    this.studio.update(cx, |s, cx| {
                        s.selected_scene = Some(id);
                        s.selected_layer = None;
                        if studio_mode {
                            cx.notify();
                        } else {
                            let _ = s.execute(Command::SetProgram { scene: Some(id) }, cx);
                        }
                    })
                }))
                .on_drag(dragged, |row, _, _, cx| cx.new(|_| row.clone()))
                .drag_over::<DraggedRow>(|style, _, _, _| style.bg(tok::bg_selected()))
                .on_drop(cx.listener(move |this, row: &DraggedRow, _, cx| this.drop_row(row, target, i, cx)))
                .when(on_air, |d| {
                    d.child(
                        div()
                            .absolute()
                            .left(px(0.))
                            .top(px(8.))
                            .bottom(px(8.))
                            .w(px(3.))
                            .rounded_full()
                            .bg(tok::live()),
                    )
                })
                .child(self.row_label(target, &scene.name, tok::text()))
                .when(on_air, |d| {
                    d.child(
                        div()
                            .text_size(px(10.))
                            .text_color(tok::live())
                            .font_weight(FontWeight::SEMIBOLD)
                            .child("ON AIR"),
                    )
                })
        });
        div()
            .flex()
            .flex_col()
            .h(px(280.))
            .child(ui::panel_header("Scenes", Some(actions.into_any_element())))
            .child(picker)
            .child(div().id("scenes-scroll").flex().flex_col().gap_0p5().overflow_y_scroll().children(rows))
    }

    fn centre(&self, project: &Project, cx: &mut Context<Self>) -> impl IntoElement {
        div().flex().flex_col().flex_1().min_w_0().child(self.canvas(project, cx)).child(self.mixer.clone())
    }

    fn format_tabs(&self, project: &Project, cx: &mut Context<Self>) -> impl IntoElement {
        let active = self.format_id(project);
        let mut bar = div().flex().items_center().gap_1().h(px(36.)).px_3().border_b_1().border_color(tok::border());
        for (i, f) in project.formats.iter().enumerate() {
            let id = f.id;
            let is_active = !self.side_by_side && id == active;
            let (gw, gh) = if f.width >= f.height { (14.0, 9.0) } else { (8.0, 13.0) };
            bar = bar.child(
                div()
                    .id(("format-tab", i))
                    .flex()
                    .items_center()
                    .gap_1p5()
                    .h(px(26.))
                    .px_2()
                    .rounded(px(6.))
                    .cursor_pointer()
                    .text_size(px(12.))
                    .when(is_active, |d| d.bg(tok::bg_selected()))
                    .hover(|d| d.bg(tok::bg_raised()))
                    .child(div().w(px(gw)).h(px(gh)).rounded(px(2.)).border_1().border_color(tok::text_secondary()))
                    .child(format!("{} {}×{}", f.name, f.width, f.height))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.active_format = Some(id);
                        this.side_by_side = false;
                        cx.notify();
                    })),
            );
        }
        if project.formats.len() > 1 {
            bar = bar.child(div().w(px(1.)).h(px(16.)).mx_1().bg(tok::border())).child(
                div()
                    .id("side-by-side")
                    .flex()
                    .items_center()
                    .h(px(26.))
                    .px_2()
                    .rounded(px(6.))
                    .cursor_pointer()
                    .text_size(px(12.))
                    .when(self.side_by_side, |d| d.bg(tok::bg_selected()))
                    .hover(|d| d.bg(tok::bg_raised()))
                    .child("Side by side")
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.side_by_side = !this.side_by_side;
                        cx.notify();
                    })),
            );
        }
        let shape = self.format(project).clone();
        let vertical = shape.height > shape.width;
        let safe = if vertical { self.safe_vertical } else { self.safe_landscape };
        bar = bar.child(div().flex_1()).child(
            div()
                .id("safe-areas")
                .flex()
                .items_center()
                .gap_1p5()
                .h(px(26.))
                .px_2()
                .rounded(px(6.))
                .cursor_pointer()
                .text_size(px(12.))
                .when(safe.is_some(), |d| d.bg(tok::bg_selected()))
                .hover(|d| d.bg(tok::bg_raised()))
                .child(ui::icon("scan").size(px(14.)).text_color(tok::text_secondary()))
                .child(safe.map_or("Safe areas", Profile::label))
                .tooltip(|w, cx| {
                    Tooltip::new("Where platforms draw their own buttons and captions (approximate)").build(w, cx)
                })
                .on_click(cx.listener(move |this, _, _, cx| {
                    let next = safe_areas::next(safe, shape.width, shape.height);
                    if vertical {
                        this.safe_vertical = next;
                    } else {
                        this.safe_landscape = next;
                    }
                    cx.notify();
                })),
        );
        bar = bar.child(
            div()
                .id("studio-mode")
                .flex()
                .items_center()
                .gap_1p5()
                .h(px(26.))
                .px_2()
                .rounded(px(6.))
                .cursor_pointer()
                .text_size(px(12.))
                .when(self.studio_mode, |d| d.bg(tok::bg_selected()))
                .hover(|d| d.bg(tok::bg_raised()))
                .child(ui::icon("columns-2").size(px(14.)).text_color(tok::text_secondary()))
                .child("Studio mode")
                .tooltip(|w, cx| Tooltip::new("Edit a scene off air, then Take it live").build(w, cx))
                .on_click(cx.listener(|this, _, _, cx| this.set_studio_mode(!this.studio_mode, cx))),
        );
        if !project.formats.iter().any(|f| f.height > f.width) {
            bar = bar.child(
                Button::new("add-vertical")
                    .label("Add vertical")
                    .xsmall()
                    .ghost()
                    .tooltip("Add a 9:16 format: every scene gets a vertical layout")
                    .on_click(cx.listener(|this, _, _, cx| this.add_vertical(cx))),
            );
        }
        bar
    }

    /// One format's stage: live preview plus selection overlay and guides.
    fn stage(&self, project: &Project, format: &Format, cx: &mut Context<Self>) -> impl IntoElement {
        let studio = self.studio.read(cx);
        let bounds_map = self.stage_bounds.clone();
        let fid = format.id;
        let frame = self.previews.get(&fid).and_then(|p| p.latest_frame());
        let editing = fid == self.format_id(project);

        let mut stage = div()
            .absolute()
            .top(px(16.))
            .left(px(16.))
            .right(px(16.))
            .bottom(px(16.))
            .child(
                canvas(
                    move |b, _, _| {
                        bounds_map.borrow_mut().insert(fid, b);
                    },
                    |_, _, _, _| {},
                )
                .absolute()
                .size_full(),
            )
            .child(match frame {
                Some(f) => surface(f).absolute().size_full().object_fit(ObjectFit::Contain).into_any_element(),
                None => div()
                    .absolute()
                    .size_full()
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_color(tok::text_tertiary())
                    .child("Starting preview…")
                    .into_any_element(),
            });

        let bounds = self.stage_bounds.borrow().get(&fid).copied();
        if let (Some((ox, oy, s)), Some(b)) = (self.mapping(project, fid), bounds) {
            let (lx, ly) = (ox - f32::from(b.origin.x), oy - f32::from(b.origin.y));
            let profile = if format.height > format.width { self.safe_vertical } else { self.safe_landscape };
            let (cw, ch) = (format.width as f32 * s, format.height as f32 * s);
            for z in profile.map_or(&[][..], Profile::zones) {
                let (zx, zy, zw, zh) = z.rect;
                let zone = div()
                    .absolute()
                    .left(px(lx + zx * cw))
                    .top(px(ly + zy * ch))
                    .w(px(zw * cw))
                    .h(px(zh * ch))
                    .border_1()
                    .border_color(tok::canvas_safe_area())
                    .p_1()
                    .text_size(px(10.))
                    .text_color(tok::canvas_safe_area())
                    .child(z.label);
                stage =
                    stage.child(if z.outline { zone.border_dashed() } else { zone.bg(tok::canvas_safe_area_fill()) });
            }
            let selected = studio
                .selected_scene
                .and_then(|id| project.scene(id))
                .and_then(|scene| studio.selected_layer.and_then(|l| scene.layer(l)))
                .and_then(|l| l.layouts.get(&fid).map(|lo| (*lo, l.locked)));
            if let Some((l, locked)) = selected {
                let colour = if editing { tok::canvas_selection() } else { tok::text_secondary() };
                let to_stage = move |(px_, py_): (f32, f32)| (lx + px_ * s, ly + py_ * s);
                let outline = geo::outline(&l).map(to_stage);
                let knob = (!locked && editing).then(|| {
                    (to_stage(geo::rotate_handle(&l, 0.0)), to_stage(geo::rotate_handle(&l, ROTATE_OFFSET / s)))
                });
                let edge = tok::canvas_selection_edge();
                // The outline turns with the layer, so it's painted as a path.
                stage = stage.child(
                    canvas(
                        |_, _, _| {},
                        move |bounds, _, window, _| {
                            let o = bounds.origin;
                            let at = |(x, y): (f32, f32)| point(o.x + px(x), o.y + px(y));
                            for (width, c) in [(3.0, edge), (1.0, colour)] {
                                let mut path = PathBuilder::stroke(px(width));
                                path.move_to(at(outline[0]));
                                for p in &outline[1..] {
                                    path.line_to(at(*p));
                                }
                                path.close();
                                if let Ok(path) = path.build() {
                                    window.paint_path(path, c);
                                }
                            }
                            if let Some((from, to)) = knob {
                                let mut path = PathBuilder::stroke(px(1.));
                                path.move_to(at(from));
                                path.line_to(at(to));
                                if let Ok(path) = path.build() {
                                    window.paint_path(path, colour);
                                }
                            }
                        },
                    )
                    .absolute()
                    .size_full(),
                );
                if let Some((_, (kx, ky))) = knob {
                    stage = stage.child(
                        div()
                            .absolute()
                            .left(px(kx - 5.))
                            .top(px(ky - 5.))
                            .size(px(10.))
                            .rounded_full()
                            .bg(tok::canvas_selection())
                            .border_1()
                            .border_color(tok::canvas_selection_edge()),
                    );
                    for (cx_, cy_) in outline {
                        stage = stage.child(
                            div()
                                .absolute()
                                .left(px(cx_ - 4.))
                                .top(px(cy_ - 4.))
                                .size(px(8.))
                                .bg(tok::canvas_selection())
                                .border_1()
                                .border_color(tok::canvas_selection_edge()),
                        );
                    }
                }
            }
            if editing {
                for (vertical, at) in &self.guides {
                    let g = div().absolute().bg(tok::canvas_guide());
                    stage = stage.child(if *vertical {
                        g.left(px(lx + at * s)).top(px(ly)).w(px(1.)).h(px(format.height as f32 * s))
                    } else {
                        g.top(px(ly + at * s)).left(px(lx)).h(px(1.)).w(px(format.width as f32 * s))
                    });
                }
            }
        }
        div().relative().flex_1().h_full().min_w_0().child(stage)
    }

    fn canvas(&self, project: &Project, cx: &mut Context<Self>) -> impl IntoElement {
        let live = runtime().any_stream_running();
        let stages: Vec<AnyElement> =
            self.visible_formats(project).iter().map(|f| self.stage(project, f, cx).into_any_element()).collect();
        let well = div().flex().flex_1().min_h_0().child(
            div()
                .id("canvas")
                .relative()
                .flex()
                .flex_1()
                .min_h_0()
                .bg(tok::bg_canvas_well())
                .when(live, |d| d.border_2().border_color(tok::live()))
                .on_mouse_down(MouseButton::Left, cx.listener(Self::canvas_down))
                .on_mouse_move(cx.listener(Self::canvas_move))
                .on_mouse_up(MouseButton::Left, cx.listener(Self::canvas_up))
                .on_mouse_up_out(MouseButton::Left, cx.listener(Self::canvas_up))
                .children(stages),
        );
        let well = match self.studio_mode.then(|| self.program_column(project, cx)) {
            Some(column) => well.child(column),
            None => well,
        };
        div().flex().flex_col().flex_1().min_h_0().child(self.format_tabs(project, cx)).child(well)
    }

    /// Per-format visibility, and copying this layer's layout from another format.
    fn formats_section(&self, project: &Project, layer: &Layer, scene: Id, cx: &mut Context<Self>) -> AnyElement {
        let active = self.format_id(project);
        let mut col = div().flex().flex_col().gap_1p5().pt_3().mt_1().border_t_1().border_color(tok::border()).child(
            div()
                .text_size(px(11.))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(tok::text_secondary())
                .child("FORMATS"),
        );
        let Some(here) = project.format(active).cloned() else { return col.into_any_element() };
        for (i, f) in project.formats.iter().enumerate() {
            let (fid, lid) = (f.id, layer.id);
            let visible = layer.layouts.get(&fid).is_some_and(|l| l.visible);
            let mut row = div()
                .flex()
                .items_center()
                .gap_2()
                .child(
                    ui::icon_button(
                        ("format-eye", i),
                        if visible { "eye" } else { "eye-off" },
                        if visible { tok::text_secondary() } else { tok::text_tertiary() },
                    )
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.exec(Command::SetLayerVisible { scene, layer: lid, format: fid, visible: !visible }, cx)
                    })),
                )
                .child(
                    div()
                        .flex_1()
                        .text_color(if visible { tok::text() } else { tok::text_tertiary() })
                        .child(format!("{} {}×{}", f.name, f.width, f.height)),
                );
            // Copy this format's layout into the format being edited (reframed to its shape).
            if fid != active
                && let (Some(from), Some(current)) = (layer.layouts.get(&fid), layer.layouts.get(&active))
            {
                let layout = Layout { visible: current.visible, ..crate::reframe::reframe(from, f, &here) };
                let cmd = Command::SetLayout { scene, layer: lid, format: active, layout };
                row = row.child(
                    Button::new(("copy-layout", i))
                        .label("Use this layout")
                        .xsmall()
                        .ghost()
                        .tooltip(format!("Copy the {} layout into {}", f.name, here.name))
                        .on_click(cx.listener(move |this, _, _, cx| this.exec(cmd.clone(), cx))),
                );
            }
            col = col.child(row);
        }
        col.into_any_element()
    }

    /// Studio mode's Program monitor and Take button.
    fn program_column(&self, project: &Project, cx: &mut Context<Self>) -> impl IntoElement {
        let studio = self.studio.read(cx);
        let frame = self.program_monitor.as_ref().and_then(|p| p.latest_frame());
        let program_name = project.program.and_then(|id| project.scene(id)).map(|s| s.name.clone());
        let preview_name = studio.selected_scene.and_then(|id| project.scene(id)).map(|s| s.name.clone());
        let can_take = studio.selected_scene.is_some() && studio.selected_scene != project.program;
        let f = project.primary_format();
        let label = |text: &str, name: Option<String>, color: Hsla| {
            div()
                .flex()
                .items_center()
                .gap_2()
                .text_size(px(11.))
                .child(div().font_weight(FontWeight::SEMIBOLD).text_color(color).child(text.to_string()))
                .child(div().text_color(tok::text_secondary()).child(name.unwrap_or_else(|| "—".into())))
        };
        div()
            .flex()
            .flex_col()
            .gap_3()
            .w(px(280.))
            .p_3()
            .border_l_1()
            .border_color(tok::border())
            .bg(tok::bg_panel())
            .child(label("PREVIEW", preview_name, tok::text()))
            .child(label("PROGRAM", program_name, tok::live()))
            .child(
                div()
                    .relative()
                    .w_full()
                    .h(px(256. * f.height as f32 / f.width.max(1) as f32))
                    .rounded(px(4.))
                    .overflow_hidden()
                    .border_1()
                    .border_color(tok::live())
                    .bg(tok::bg_canvas_well())
                    .when_some(frame, |d, frame| {
                        d.child(surface(frame).absolute().size_full().object_fit(ObjectFit::Contain))
                    }),
            )
            .child(
                Button::new("take")
                    .label("Take")
                    .primary()
                    .w_full()
                    .disabled(!can_take)
                    .tooltip("Put the scene you're editing on air")
                    .on_click(cx.listener(|this, _, _, cx| this.take(cx))),
            )
            .child(
                div()
                    .text_size(px(11.))
                    .text_color(tok::text_tertiary())
                    .child("Clicking a scene selects it for editing; Take puts it on air with the current transition."),
            )
    }

    fn right_column(&self, project: &Project, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .flex()
            .flex_col()
            .w(px(300.))
            .h_full()
            .border_l_1()
            .border_color(tok::border())
            .bg(tok::bg_panel())
            .child(self.outputs.clone())
            .child(div().h(px(1.)).bg(tok::border()))
            .child(self.inspector(project, cx))
    }

    fn inspector(&self, project: &Project, cx: &mut Context<Self>) -> impl IntoElement {
        let studio = self.studio.read(cx);
        let format = self.format(project).clone();
        let scene = studio.selected_scene.and_then(|id| project.scene(id)).cloned();
        let layer = scene.as_ref().and_then(|s| studio.selected_layer.and_then(|l| s.layer(l))).cloned();
        let body = div().flex().flex_col().gap_3().px_3();
        let content = match (scene, layer) {
            (Some(scene), Some(layer)) => {
                let lo = layer.layouts.get(&format.id).copied().unwrap_or(Layout::boxed(0., 0., 100., 100.));
                let (sid, lid, fid) = (scene.id, layer.id, format.id);
                let kind = project.inputs.get(&layer.input).map(|i| i.kind.clone()).unwrap_or_default();
                let camera_studio: Option<AnyElement> = project.inputs.get(&layer.input).and_then(|input| {
                    self.camera_studio_section(input, cx).or_else(|| {
                        let cmd = crate::camera_studio::is_camera(&input.kind)
                            .then(|| crate::camera_studio::ensure_effect(input))
                            .flatten()?;
                        Some(
                            Button::new("add-camera-studio")
                                .label("Add Camera Studio")
                                .small()
                                .outline()
                                .on_click(cx.listener(move |this, _, _, cx| this.exec(cmd.clone(), cx)))
                                .into_any_element(),
                        )
                    })
                });
                let set = move |f: fn(&mut Layout)| {
                    let mut l = lo;
                    f(&mut l);
                    Command::SetLayout { scene: sid, layer: lid, format: fid, layout: l }
                };
                let stepper = |id: &'static str, label: &'static str, value: f32, minus: Command, plus: Command| {
                    div()
                        .flex()
                        .items_center()
                        .gap_1()
                        .child(div().w(px(76.)).text_color(tok::text_secondary()).child(label))
                        .child(
                            Button::new((id, 0usize))
                                .label("−")
                                .xsmall()
                                .ghost()
                                .on_click(cx.listener(move |this, _, _, cx| this.exec(minus.clone(), cx))),
                        )
                        .child(div().w(px(64.)).text_center().font_family("Menlo").child(format!("{value:.0}")))
                        .child(
                            Button::new((id, 1usize))
                                .label("+")
                                .xsmall()
                                .ghost()
                                .on_click(cx.listener(move |this, _, _, cx| this.exec(plus.clone(), cx))),
                        )
                };
                let fit_button = |id: &'static str, label: &'static str, fit: Fit| {
                    let cmd = Command::SetLayout { scene: sid, layer: lid, format: fid, layout: Layout { fit, ..lo } };
                    let b = Button::new(id)
                        .label(label)
                        .xsmall()
                        .on_click(cx.listener(move |this, _, _, cx| this.exec(cmd.clone(), cx)));
                    if lo.fit == fit { b.primary() } else { b.ghost() }
                };
                body.child(
                    div()
                        .flex()
                        .items_center()
                        .gap_2()
                        .child(ui::icon(ui::input_icon(&kind)).size(px(16.)).text_color(tok::text_secondary()))
                        .child(div().font_weight(FontWeight::SEMIBOLD).child(layer.name.clone())),
                )
                .child(stepper("x", "X", lo.x, set(|l| l.x -= 10.0), set(|l| l.x += 10.0)))
                .child(stepper("y", "Y", lo.y, set(|l| l.y -= 10.0), set(|l| l.y += 10.0)))
                .child(stepper(
                    "w",
                    "Width",
                    lo.width,
                    set(|l| l.width = (l.width - 10.0).max(8.0)),
                    set(|l| l.width += 10.0),
                ))
                .child(stepper(
                    "h",
                    "Height",
                    lo.height,
                    set(|l| l.height = (l.height - 10.0).max(8.0)),
                    set(|l| l.height += 10.0),
                ))
                .child(stepper("r", "Rotation", lo.rotation, set(|l| l.rotation -= 15.0), set(|l| l.rotation += 15.0)))
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap_1()
                        .child(div().w(px(76.)).text_color(tok::text_secondary()).child("Fit"))
                        .child(fit_button("fit-fit", "Fit", Fit::Fit))
                        .child(fit_button("fit-fill", "Fill", Fit::Fill))
                        .child(fit_button("fit-stretch", "Stretch", Fit::Stretch))
                        .child(fit_button("fit-original", "Original", Fit::Original)),
                )
                .child(
                    div()
                        .flex()
                        .gap_2()
                        .pt_2()
                        .child({
                            let full = Command::SetLayout {
                                scene: sid,
                                layer: lid,
                                format: fid,
                                layout: Layout {
                                    x: 0.0,
                                    y: 0.0,
                                    width: format.width as f32,
                                    height: format.height as f32,
                                    rotation: 0.0,
                                    ..lo
                                },
                            };
                            Button::new("fill-canvas")
                                .label("Fill canvas")
                                .small()
                                .outline()
                                .on_click(cx.listener(move |this, _, _, cx| this.exec(full.clone(), cx)))
                        })
                        .when(lo.crop != lightair_core::Crop::default(), |d| {
                            let uncropped = Command::SetLayout {
                                scene: sid,
                                layer: lid,
                                format: fid,
                                layout: Layout { crop: lightair_core::Crop::default(), ..lo },
                            };
                            d.child(
                                Button::new("reset-crop")
                                    .label("Reset crop")
                                    .small()
                                    .outline()
                                    .tooltip("⌥-drag a corner on the canvas to crop")
                                    .on_click(cx.listener(move |this, _, _, cx| this.exec(uncropped.clone(), cx))),
                            )
                        })
                        .child(Button::new("delete-layer").label("Delete").small().danger().on_click(cx.listener(
                            move |this, _, _, cx| this.exec(Command::RemoveLayer { scene: sid, layer: lid }, cx),
                        ))),
                )
                .children((project.formats.len() > 1).then(|| self.formats_section(project, &layer, sid, cx)))
                .children(project.inputs.get(&layer.input).and_then(|input| self.content_section(input, cx)))
                .children(project.inputs.get(&layer.input).map(|input| self.effects_section(input, cx)))
                .children(camera_studio)
                .into_any_element()
            }
            (Some(scene), None) => body
                .child(div().font_weight(FontWeight::SEMIBOLD).child(scene.name.clone()))
                .child(div().text_color(tok::text_secondary()).child(format!(
                    "{} layer{} · {}×{} @ {} fps",
                    scene.layers.len(),
                    if scene.layers.len() == 1 { "" } else { "s" },
                    format.width,
                    format.height,
                    format.fps_num / format.fps_den.max(1)
                )))
                .child(
                    div()
                        .text_color(tok::text_tertiary())
                        .child("Select a layer on the canvas or in Layers to edit it."),
                )
                .into_any_element(),
            _ => body.child(div().text_color(tok::text_tertiary()).child("Nothing selected")).into_any_element(),
        };
        div()
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .child(ui::panel_header("Inspector", None))
            .child(div().id("inspector-scroll").flex_1().min_h_0().pb_3().overflow_y_scroll().child(content))
    }

    fn status_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let studio = self.studio.read(cx);
        let issue = studio.issue.clone();
        let stats = studio.stats;
        div()
            .flex()
            .items_center()
            .gap_3()
            .h(px(26.))
            .px_3()
            .border_t_1()
            .border_color(tok::border())
            .text_size(px(11.))
            .text_color(tok::text_secondary())
            .children(issue.map(|i| div().child(i)))
            .child(div().flex_1())
            .child(format!(
                "{} frames · {} lagged · preview {}",
                stats.total_frames,
                stats.lagged_frames,
                self.previews.values().map(|p| p.frames()).max().unwrap_or(0)
            ))
            .child(div().text_color(tok::text_tertiary()).child(concat!("Lightair ", env!("CARGO_PKG_VERSION"))))
    }
}
