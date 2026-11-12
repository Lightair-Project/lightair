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
    Command, Direction, Format, Id, Input, Layer, Layout, Project, Scene, TransitionKind, TransitionSettings,
};
use lightair_preview::Preview;
use serde_json::json;

mod add_layer;
mod docking;
mod golive;
mod inspector;
mod layer_props;
mod palette;
mod settings;
mod source;

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
        AddMarker,
        SelectAll,
        OpenSettings,
        ToggleMics,
        Scene1,
        Scene2,
        Scene3,
        Scene4,
        Scene5,
        Scene6,
        Scene7,
        Scene8,
        Scene9
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
        KeyBinding::new("cmd-a", SelectAll, Some("Studio")),
        KeyBinding::new("cmd-,", OpenSettings, Some("Studio")),
        KeyBinding::new("cmd-shift-m", ToggleMics, Some("Studio")),
        KeyBinding::new("cmd-1", Scene1, Some("Studio")),
        KeyBinding::new("cmd-2", Scene2, Some("Studio")),
        KeyBinding::new("cmd-3", Scene3, Some("Studio")),
        KeyBinding::new("cmd-4", Scene4, Some("Studio")),
        KeyBinding::new("cmd-5", Scene5, Some("Studio")),
        KeyBinding::new("cmd-6", Scene6, Some("Studio")),
        KeyBinding::new("cmd-7", Scene7, Some("Studio")),
        KeyBinding::new("cmd-8", Scene8, Some("Studio")),
        KeyBinding::new("cmd-9", Scene9, Some("Studio")),
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
    /// Crop mode: an edge handle crops one side.
    CropEdge(geo::Edge, (f32, f32)),
    /// Crop mode: dragging inside slides the picture under the crop.
    CropPan((f32, f32)),
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
    /// Other selected layers moving with it (Move only), with their starting layouts.
    others: Vec<(Id, Layout)>,
}

/// A source's list choices: (label, setting value).
type SourceOptions = Vec<(String, serde_json::Value)>;

/// A selection box being dragged: format, start, current point (canvas units), adding (⇧).
type Marquee = (Id, (f32, f32), (f32, f32), bool);

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
    /// Inspector sections the user collapsed, slider states, and the input/effect whose looks
    /// dialog is open.
    collapsed: RefCell<std::collections::HashSet<&'static str>>,
    sliders: RefCell<HashMap<String, inspector::BoundSlider>>,
    looks_for: Option<(Id, Id)>,
    transition_menu: bool,
    /// The layer showing crop handles instead of resize handles.
    crop_mode: Option<Id>,
    crop_preview_sent: Option<(Id, Id, Id)>,
    /// The source picker, and fetched source lists per (input, setting).
    source_picker: Option<source::SourcePicker>,
    source_cache: RefCell<HashMap<(Id, &'static str), SourceOptions>>,
    /// The Add Layer picker, while open.
    add_layer: Option<add_layer::AddLayer>,
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
    /// The Go Live review sheet, while open.
    go_live_sheet: Option<golive::GoLiveSheet>,
    /// A drag-to-select box on the canvas: format, start and current point (canvas units), and
    /// whether it adds to the selection (⇧).
    marquee: Option<Marquee>,
    /// The Add format row, with its custom width and height fields.
    settings_open: bool,
    prefs: crate::prefs::Preferences,
    /// Resizable, collapsible, rearrangeable panels (built on first render).
    dock: Option<Entity<gpui_kit::component::dock::DockArea>>,
    format_menu:
        Option<(Entity<gpui_kit::component::input::InputState>, Entity<gpui_kit::component::input::InputState>)>,
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
            add_layer: None,
            collapsed: RefCell::new(["Formats"].into_iter().collect()),
            sliders: RefCell::default(),
            looks_for: None,
            transition_menu: false,
            crop_mode: None,
            crop_preview_sent: None,
            source_picker: None,
            source_cache: RefCell::default(),
            camera_devices: vec![],
            renaming: None,
            studio_mode: false,
            program_monitor: None,
            props: Default::default(),
            safe_landscape: None,
            safe_vertical: None,
            palette: None,
            go_live_sheet: None,
            marquee: None,
            format_menu: None,
            settings_open: false,
            dock: None,
            prefs: crate::prefs::load_from(&crate::prefs::path()),
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

    /// Selects just this layer (or nothing).
    fn select_layer(&mut self, layer: Option<Id>, cx: &mut Context<Self>) {
        if self.crop_mode.is_some() && self.crop_mode != layer {
            self.crop_mode = None;
        }
        self.studio.update(cx, |s, cx| {
            s.selected_layer = layer;
            s.also_selected.clear();
            if layer.is_some() {
                s.selected_track = None;
            }
            cx.notify();
        });
    }

    fn toggle_selected(&self, layer: Id, cx: &mut Context<Self>) {
        self.studio.update(cx, |s, cx| {
            s.toggle_selected(layer);
            cx.notify();
        });
    }

    /// One command for several layers' layouts (a plain SetLayout for one).
    fn set_layouts(&self, scene: Id, format: Id, layouts: Vec<(Id, Layout)>, label: &str, cx: &mut Context<Self>) {
        let mut commands: Vec<Command> =
            layouts.into_iter().map(|(layer, layout)| Command::SetLayout { scene, layer, format, layout }).collect();
        match commands.len() {
            0 => {}
            1 => self.exec(commands.remove(0), cx),
            _ => self.exec(Command::Batch { label: label.into(), commands }, cx),
        }
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
        self.add_format(Format::vertical(), cx)
    }

    /// Adds a format with every layer reframed into it (one undo step), and shows both.
    fn add_format(&mut self, to: Format, cx: &mut Context<Self>) {
        self.format_menu = None;
        let project = self.studio.read(cx).project().clone();
        let from = project.primary_format().clone();
        let to_id = to.id;
        let label = format!("Add {} format", to.name.to_lowercase());
        let mut commands = vec![Command::AddFormat { format: to.clone() }];
        for scene in &project.scenes {
            for layer in &scene.layers {
                if let Some(l) = layer.layouts.get(&from.id) {
                    let layout = crate::reframe::reframe(l, &from, &to);
                    commands.push(Command::SetLayout { scene: scene.id, layer: layer.id, format: to_id, layout });
                }
            }
        }
        self.exec(Command::Batch { label, commands }, cx);
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
            // Crop mode: corners and edges crop.
            if self.crop_mode == Some(sel.id) {
                let input = sel.input;
                let size = studio.engine().call(move |s| s.reconciler.input_size(input));
                let near = |(px_, py_): (f32, f32)| (x - px_).abs() <= tol && (y - py_).abs() <= tol;
                let mode = match size {
                    Some((w, h)) => {
                        let size = (w as f32, h as f32);
                        // Handles sit on the shape (a circle's on the circle); inside pans.
                        let (corners, edges) = geo::crop_handles(l);
                        corners
                            .iter()
                            .find(|(_, p)| near(*p))
                            .map(|(c, _)| DragMode::Crop(*c, size))
                            .or_else(|| edges.iter().find(|(_, p)| near(*p)).map(|(e, _)| DragMode::CropEdge(*e, size)))
                            .or_else(|| geo::contains(l, (x, y)).then_some(DragMode::CropPan(size)))
                    }
                    None => None,
                };
                if let Some(mode) = mode {
                    self.drag = Some(Drag {
                        scene: scene_id,
                        layer: sel.id,
                        format,
                        mode,
                        start: (x, y),
                        layout: *l,
                        others: vec![],
                    });
                    self.studio.update(cx, |s, _| s.begin_gesture());
                    return;
                }
            }
            let (hx, hy) = geo::rotate_handle(l, ROTATE_OFFSET / s);
            if self.crop_mode != Some(sel.id) && (x - hx).abs() <= tol && (y - hy).abs() <= tol {
                self.drag = Some(Drag {
                    scene: scene_id,
                    layer: sel.id,
                    format,
                    mode: DragMode::Rotate,
                    start: (x, y),
                    layout: *l,
                    others: vec![],
                });
                self.studio.update(cx, |s, _| s.begin_gesture());
                return;
            }
            if let Some(corner) = CORNERS.iter().find(|c| {
                let (px_, py_) = geo::corner(l, **c);
                self.crop_mode != Some(sel.id) && (x - px_).abs() <= tol && (y - py_).abs() <= tol
            }) {
                let input = sel.input;
                let size =
                    ev.modifiers.alt.then(|| studio.engine().call(move |s| s.reconciler.input_size(input))).flatten();
                let mode = match size {
                    Some((w, h)) => DragMode::Crop(*corner, (w as f32, h as f32)),
                    None => DragMode::Resize(*corner),
                };
                self.drag = Some(Drag {
                    scene: scene_id,
                    layer: sel.id,
                    format,
                    mode,
                    start: (x, y),
                    layout: *l,
                    others: vec![],
                });
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
        let selection = studio.selection();
        match hit {
            Some(layer) if ev.modifiers.shift => self.toggle_selected(layer.id, cx),
            // Double-click: crop handles on, or off again.
            Some(layer) if ev.click_count == 2 && !layer.locked => {
                let id = layer.id;
                let crop = if self.crop_mode == Some(id) { None } else { Some(id) };
                self.select_layer(Some(id), cx);
                self.crop_mode = crop;
                cx.notify();
            }
            Some(layer) => {
                let id = layer.id;
                // Dragging one of several selected layers moves them all; otherwise select it.
                let in_selection = selection.contains(&id);
                if !layer.locked {
                    let layout = layer.layouts[&format];
                    let others: Vec<(Id, Layout)> = if in_selection {
                        selection
                            .iter()
                            .filter(|o| **o != id)
                            .filter_map(|o| scene.layer(*o).filter(|l| !l.locked))
                            .filter_map(|l| l.layouts.get(&format).map(|lo| (l.id, *lo)))
                            .collect()
                    } else {
                        vec![]
                    };
                    let mode = DragMode::Move;
                    self.drag = Some(Drag { scene: scene_id, layer: id, format, mode, start: (x, y), layout, others });
                    self.studio.update(cx, |s, _| s.begin_gesture());
                }
                if in_selection {
                    // Keep the group; the grabbed layer becomes the Inspector's.
                    self.studio.update(cx, |s, cx| {
                        if s.selected_layer != Some(id) {
                            s.also_selected.retain(|l| *l != id);
                            if let Some(prev) = s.selected_layer.replace(id) {
                                s.also_selected.insert(0, prev);
                            }
                        }
                        cx.notify();
                    });
                } else {
                    self.select_layer(Some(id), cx);
                }
            }
            // Empty space: start a selection box (⇧ adds to the selection).
            None => {
                if !ev.modifiers.shift {
                    self.select_layer(None, cx);
                }
                self.marquee = Some((format, (x, y), (x, y), ev.modifiers.shift));
            }
        }
    }

    fn canvas_move(&mut self, ev: &MouseMoveEvent, _window: &mut Window, cx: &mut Context<Self>) {
        if let Some((format, ..)) = self.marquee {
            let project = self.studio.read(cx).project().clone();
            if let Some(p) = self.to_canvas(&project, format, ev.position)
                && let Some((_, _, to, _)) = &mut self.marquee
            {
                *to = p;
                cx.notify();
            }
            return;
        }
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
            DragMode::CropEdge(edge, size) => l = geo::crop_edge(&l, edge, size, (dx, dy)),
            DragMode::CropPan(size) => l = geo::crop_pan(&l, size, (dx, dy)),
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
        // The group follows the grabbed layer (after snapping).
        let (mx, my) = (l.x - drag.layout.x, l.y - drag.layout.y);
        let mut layouts = vec![(drag.layer, l)];
        layouts.extend(drag.others.iter().map(|(id, o)| (*id, Layout { x: o.x + mx, y: o.y + my, ..*o })));
        let (scene, format) = (drag.scene, drag.format);
        self.set_layouts(scene, format, layouts, "Move layers", cx);
    }

    fn canvas_up(&mut self, _: &MouseUpEvent, _window: &mut Window, cx: &mut Context<Self>) {
        if let Some((format, from, to, additive)) = self.marquee.take() {
            let moved = (from.0 - to.0).abs() > 2.0 || (from.1 - to.1).abs() > 2.0;
            self.studio.update(cx, |s, cx| {
                let p = s.project().clone();
                if let Some(scene) = s.selected_scene.and_then(|id| p.scene(id))
                    && moved
                {
                    let hit: Vec<Id> = scene
                        .layers
                        .iter()
                        .rev()
                        .filter(|l| !l.locked)
                        .filter(|l| l.layouts.get(&format).is_some_and(|lo| lo.visible && geo::touches(lo, from, to)))
                        .map(|l| l.id)
                        .collect();
                    let mut all = if additive { s.selection() } else { vec![] };
                    for id in hit {
                        if !all.contains(&id) {
                            all.push(id);
                        }
                    }
                    s.selected_layer = all.first().copied();
                    s.also_selected = all.into_iter().skip(1).collect();
                }
                cx.notify();
            });
            return;
        }
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
        let Some(scene_id) = studio.selected_scene else { return };
        let Some(scene) = project.scene(scene_id) else { return };
        let moved: Vec<(Id, Layout)> = studio
            .selection()
            .into_iter()
            .filter_map(|id| scene.layer(id).filter(|l| !l.locked))
            .filter_map(|l| l.layouts.get(&format).map(|lo| (l.id, Layout { x: lo.x + dx, y: lo.y + dy, ..*lo })))
            .collect();
        self.set_layouts(scene_id, format, moved, "Nudge layers", cx);
    }

    fn delete_selection(&mut self, cx: &mut Context<Self>) {
        let studio = self.studio.read(cx);
        let Some(scene) = studio.selected_scene else { return };
        let mut commands: Vec<Command> =
            studio.selection().into_iter().map(|layer| Command::RemoveLayer { scene, layer }).collect();
        match commands.len() {
            0 => {}
            1 => self.exec(commands.remove(0), cx),
            n => self.exec(Command::Batch { label: format!("Delete {n} layers"), commands }, cx),
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

    /// Picks the stinger video (cut point defaults to halfway through a typical 1 s stinger).
    fn choose_stinger(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let paths =
            cx.prompt_for_paths(PathPromptOptions { files: true, directories: false, multiple: false, prompt: None });
        cx.spawn_in(window, async move |this, cx| {
            let Ok(Ok(Some(paths))) = paths.await else { return };
            let Some(path): Option<PathBuf> = paths.into_iter().next() else { return };
            let _ = this.update(cx, |this, cx| {
                let cut_ms = this.studio.read(cx).project().stinger.as_ref().map_or(500, |s| s.cut_ms);
                let stinger = lightair_core::Stinger { path: path.to_string_lossy().into_owned(), cut_ms };
                this.exec(Command::SetStinger { stinger: Some(stinger) }, cx)
            });
        })
        .detach();
    }

    /// ⌘1–⌘9: the nth scene goes on air (or is selected, in Studio mode).
    fn scene_shortcut(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let scene = self.studio.read(cx).project().scenes.get(index).map(|s| s.id);
        if let Some(id) = scene {
            self.run_palette_action(palette::PaletteAction::Scene(id), window, cx);
        }
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
        self.ensure_dock(window, cx);
        self.sync_crop_preview(cx);
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
            .on_action(cx.listener(|this, _: &Deselect, _, cx| {
                // Esc leaves crop mode first.
                if this.crop_mode.take().is_some() {
                    cx.notify();
                } else {
                    this.select_layer(None, cx)
                }
            }))
            .on_action(cx.listener(|this, _: &RenameSelection, window, cx| {
                if this.crop_mode.take().is_some() {
                    cx.notify();
                } else {
                    this.rename_selection(window, cx)
                }
            }))
            .on_action(cx.listener(|this, _: &OpenPalette, window, cx| this.open_palette(window, cx)))
            .on_action(cx.listener(|this, _: &OpenSettings, _, cx| this.toggle_settings(cx)))
            .on_action(cx.listener(|this, _: &ToggleMics, _, cx| this.studio.update(cx, |s, cx| s.toggle_mics(cx))))
            .on_action(cx.listener(|this, _: &Scene1, w, cx| this.scene_shortcut(0, w, cx)))
            .on_action(cx.listener(|this, _: &Scene2, w, cx| this.scene_shortcut(1, w, cx)))
            .on_action(cx.listener(|this, _: &Scene3, w, cx| this.scene_shortcut(2, w, cx)))
            .on_action(cx.listener(|this, _: &Scene4, w, cx| this.scene_shortcut(3, w, cx)))
            .on_action(cx.listener(|this, _: &Scene5, w, cx| this.scene_shortcut(4, w, cx)))
            .on_action(cx.listener(|this, _: &Scene6, w, cx| this.scene_shortcut(5, w, cx)))
            .on_action(cx.listener(|this, _: &Scene7, w, cx| this.scene_shortcut(6, w, cx)))
            .on_action(cx.listener(|this, _: &Scene8, w, cx| this.scene_shortcut(7, w, cx)))
            .on_action(cx.listener(|this, _: &Scene9, w, cx| this.scene_shortcut(8, w, cx)))
            .on_action(cx.listener(|this, _: &SelectAll, _, cx| {
                this.studio.update(cx, |s, cx| {
                    let p = s.project().clone();
                    let layers: Vec<Id> = s
                        .selected_scene
                        .and_then(|id| p.scene(id))
                        .map(|scene| scene.layers.iter().rev().map(|l| l.id).collect())
                        .unwrap_or_default();
                    s.selected_layer = layers.first().copied();
                    s.also_selected = layers.into_iter().skip(1).collect();
                    cx.notify();
                })
            }))
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
            .child(div().flex().flex_1().min_h_0().children(self.dock.clone()))
            .child(self.status_bar(cx))
            .children(self.add_layer_overlay(cx))
            .children(self.looks_overlay(cx))
            .children(self.source_picker_overlay(cx))
            .children(self.go_live_overlay(cx))
            .children(self.settings_overlay(cx))
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
                .on_click(cx.listener(|this, _, _, cx| this.open_go_live(cx)))
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
            .when(self.studio.read(cx).last_clip.is_some(), |d| {
                d.child(
                    Button::new("replay")
                        .label("Replay")
                        .small()
                        .ghost()
                        .tooltip("Play the last clip on the live scene")
                        .on_click(cx.listener(|this, _, _, cx| this.studio.update(cx, |s, cx| s.replay_last_clip(cx)))),
                )
            })
            .child(
                ui::icon_button("open-settings", "settings", tok::text_secondary())
                    .tooltip(|w, cx| Tooltip::new("Settings (⌘,)").build(w, cx))
                    .on_click(cx.listener(|this, _, _, cx| this.toggle_settings(cx))),
            )
    }

    /// Keeps libobs showing the layer being cropped whole (crop mode), or not.
    fn sync_crop_preview(&mut self, cx: &mut Context<Self>) {
        let studio = self.studio.read(cx);
        let project = studio.project().clone();
        let want = self
            .crop_mode
            .and_then(|layer| studio.selected_scene.map(|scene| (scene, self.format_id(&project), layer)));
        if want != self.crop_preview_sent {
            self.crop_preview_sent = want;
            studio.engine().post(move |s| s.reconciler.set_crop_preview(want, &project));
        }
    }

    fn toggle_add_menu(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.open_add_layer(window, cx);
    }

    fn layers_panel(&self, project: &Project, cx: &mut Context<Self>) -> impl IntoElement {
        let studio = self.studio.read(cx);
        let format = self.format_id(project);
        let scene = studio.selected_scene.and_then(|id| project.scene(id)).cloned();
        let selected = studio.selected_layer;
        let also = studio.also_selected.clone();
        let mut col = div().flex().flex_col().size_full().pt_1();
        let Some(scene) = scene else {
            return col.child(div().px_3().text_color(tok::text_tertiary()).child("No scene selected"));
        };
        if scene.layers.is_empty() {
            col = col.child(div().px_3().py_2().text_color(tok::text_tertiary()).child("Empty scene"));
        }
        let scene_id = scene.id;
        let count = scene.layers.len();
        let rows = scene.layers.iter().rev().enumerate().map(|(i, layer)| {
            let id = layer.id;
            let kind = project.inputs.get(&layer.input).map(|i| i.kind.clone()).unwrap_or_default();
            let visible = layer.layouts.get(&format).is_some_and(|l| l.visible);
            let locked = layer.locked;
            let is_sel = selected == Some(id) || also.contains(&id);
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
                    let m = ev.modifiers();
                    if m.shift || m.platform {
                        this.toggle_selected(id, cx);
                        return;
                    }
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

    fn transition_picker(&self, project: &Project, cx: &mut Context<Self>) -> impl IntoElement {
        let current = project.transition;
        // The list of kinds, shown under the row while open.
        let item = |i: usize, kind: TransitionKind| {
            let active = current.kind == kind;
            div()
                .id(("tr-kind", i))
                .flex()
                .items_center()
                .gap_2()
                .h(px(28.))
                .px_2()
                .rounded(px(5.))
                .text_size(px(12.))
                .cursor_pointer()
                .when(active, |d| d.bg(tok::bg_selected()).text_color(tok::text()))
                .when(!active, |d| d.text_color(tok::text_secondary()))
                .hover(|d| d.bg(tok::bg_raised()))
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.transition_menu = false;
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
        let kinds: Vec<_> = if self.transition_menu {
            TransitionKind::ALL.iter().enumerate().map(|(i, k)| item(i, *k)).collect()
        } else {
            Vec::new()
        };
        // Stinger: its video and when the scene changes underneath it.
        let stinger = (current.kind == TransitionKind::Stinger).then(|| {
            let st = project.stinger.clone();
            let name = st
                .as_ref()
                .and_then(|s| std::path::Path::new(&s.path).file_name().map(|n| n.to_string_lossy().into_owned()))
                .unwrap_or_else(|| "Choose video…".into());
            let mut row = div().flex().items_center().gap_1().w_full().child(
                Button::new("stinger-file")
                    .label(name)
                    .xsmall()
                    .outline()
                    .on_click(cx.listener(|this, _, window, cx| this.choose_stinger(window, cx))),
            );
            if let Some(st) = st {
                let step = |delta: i64| {
                    let mut s = st.clone();
                    s.cut_ms = (s.cut_ms as i64 + delta).clamp(0, 10_000) as u32;
                    Command::SetStinger { stinger: Some(s) }
                };
                let (earlier, later) = (step(-100), step(100));
                row = row
                    .child(div().text_size(px(11.)).text_color(tok::text_tertiary()).ml_1().child("cut at"))
                    .child(
                        Button::new("stinger-earlier")
                            .label("−")
                            .xsmall()
                            .ghost()
                            .on_click(cx.listener(move |this, _, _, cx| this.exec(earlier.clone(), cx))),
                    )
                    .child(div().text_size(px(11.)).font_family("Menlo").child(format!("{} ms", st.cut_ms)))
                    .child(
                        Button::new("stinger-later")
                            .label("+")
                            .xsmall()
                            .ghost()
                            .on_click(cx.listener(move |this, _, _, cx| this.exec(later.clone(), cx))),
                    );
            }
            row
        });
        let row = div()
            .flex()
            .items_center()
            .gap_1()
            .child(
                div()
                    .id("tr-current")
                    .flex()
                    .flex_1()
                    .items_center()
                    .gap_1p5()
                    .h(px(26.))
                    .px_2()
                    .rounded(px(5.))
                    .cursor_pointer()
                    .hover(|d| d.bg(tok::bg_raised()))
                    .child(ui::icon("arrow-right-left").size(px(13.)).text_color(tok::text_tertiary()))
                    .child(div().flex_1().text_size(px(12.)).child(current.kind.label()))
                    .child(
                        ui::icon(if self.transition_menu { "chevron-up" } else { "chevron-down" })
                            .size(px(12.))
                            .text_color(tok::text_tertiary()),
                    )
                    .tooltip(|w, cx| Tooltip::new("Transition between scenes").build(w, cx))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.transition_menu = !this.transition_menu;
                        cx.notify();
                    })),
            )
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
            );
        div().flex().flex_col().gap_1().px_2().pb_2().child(row).children(kinds).children(stinger)
    }

    fn scenes_panel(&self, project: &Project, cx: &mut Context<Self>) -> impl IntoElement {
        let picker = self.transition_picker(project, cx).into_any_element();
        let studio = self.studio.read(cx);
        let selected = studio.selected_scene;
        let program = project.program;
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
                .gap_2()
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
                .when(i < 9, |d| {
                    d.child(
                        div()
                            .text_size(px(10.))
                            .font_family("Menlo")
                            .text_color(tok::text_tertiary())
                            .child(format!("⌘{}", i + 1)),
                    )
                })
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
            .size_full()
            .pt_1()
            .child(
                div()
                    .id("scenes-scroll")
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_h_0()
                    .gap_0p5()
                    .overflow_y_scroll()
                    .children(rows),
            )
            .child(div().border_t_1().border_color(tok::border()).pt_2().child(picker))
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
                    }))
                    .when(i > 0, |d| {
                        d.child(
                            ui::icon_button(("format-remove", i), "x", tok::text_tertiary())
                                .size(px(16.))
                                .tooltip(|w, cx| Tooltip::new("Remove this format (undo brings it back)").build(w, cx))
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    cx.stop_propagation();
                                    if this.active_format == Some(id) {
                                        this.active_format = None;
                                    }
                                    this.exec(Command::RemoveFormat { format: id }, cx);
                                })),
                        )
                    }),
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
        bar = bar.child(
            Button::new("add-format")
                .label("Add format")
                .xsmall()
                .ghost()
                .tooltip("Another shape for every scene (vertical, square, …)")
                .on_click(cx.listener(|this, _, window, cx| {
                    this.format_menu = match this.format_menu.take() {
                        Some(_) => None,
                        None => {
                            let w = cx
                                .new(|cx| gpui_kit::component::input::InputState::new(window, cx).placeholder("Width"));
                            let h = cx.new(|cx| {
                                gpui_kit::component::input::InputState::new(window, cx).placeholder("Height")
                            });
                            Some((w, h))
                        }
                    };
                    cx.notify();
                })),
        );
        let Some((w_in, h_in)) = &self.format_menu else { return div().w_full().child(bar) };
        let has = |w: u32, h: u32| project.formats.iter().any(|f| f.width == w && f.height == h);
        let mut row =
            div().flex().flex_wrap().items_center().gap_1().px_3().py_2().border_b_1().border_color(tok::border());
        for (i, f) in [Format::vertical(), Format::square(), Format::portrait()].into_iter().enumerate() {
            if has(f.width, f.height) {
                continue;
            }
            let label = format!("{} {}×{}", f.name, f.width, f.height);
            row = row.child(
                Button::new(("format-preset", i))
                    .label(label)
                    .xsmall()
                    .outline()
                    .on_click(cx.listener(move |this, _, _, cx| this.add_format(f.clone(), cx))),
            );
        }
        let (w_in, h_in) = (w_in.clone(), h_in.clone());
        row = row
            .child(div().w(px(8.)))
            .child(div().w(px(72.)).child(gpui_kit::component::input::Input::new(&w_in).xsmall()))
            .child("×")
            .child(div().w(px(72.)).child(gpui_kit::component::input::Input::new(&h_in).xsmall()))
            .child(Button::new("format-custom").label("Add custom").xsmall().ghost().on_click(cx.listener(
                move |this, _, _, cx| {
                    let read = |e: &Entity<gpui_kit::component::input::InputState>, cx: &App| {
                        e.read(cx).value().trim().parse::<u32>().ok()
                    };
                    match (read(&w_in, cx), read(&h_in, cx)) {
                        (Some(w), Some(h)) if w >= 16 && h >= 16 => {
                            let f = Format::custom("Custom", w, h);
                            this.add_format(f, cx)
                        }
                        _ => this.studio.update(cx, |s, cx| {
                            s.issue = Some("Enter a width and height in pixels (16 or more).".into());
                            cx.notify();
                        }),
                    }
                },
            )));
        div().w_full().child(bar).child(row)
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
                .and_then(|l| l.layouts.get(&fid).map(|lo| (*lo, l.locked, l.id)));
            if let Some((mf, a, b, _)) = self.marquee
                && mf == fid
            {
                let (x0, y0) = (lx + a.0.min(b.0) * s, ly + a.1.min(b.1) * s);
                let (w, h) = ((a.0 - b.0).abs() * s, (a.1 - b.1).abs() * s);
                stage = stage.child(
                    div()
                        .absolute()
                        .left(px(x0))
                        .top(px(y0))
                        .w(px(w))
                        .h(px(h))
                        .border_1()
                        .border_color(tok::canvas_selection())
                        .bg(tok::canvas_safe_area_fill()),
                );
            }
            // Other selected layers: outlines only.
            let extras: Vec<[(f32, f32); 4]> = studio
                .selected_scene
                .and_then(|id| project.scene(id))
                .map(|scene| {
                    studio
                        .also_selected
                        .iter()
                        .filter_map(|id| scene.layer(*id))
                        .filter_map(|l| l.layouts.get(&fid))
                        .map(|lo| geo::outline(lo).map(|(px_, py_)| (lx + px_ * s, ly + py_ * s)))
                        .collect()
                })
                .unwrap_or_default();
            if !extras.is_empty() {
                let colour = tok::canvas_selection();
                stage = stage.child(
                    canvas(
                        |_, _, _| {},
                        move |bounds, _, window, _| {
                            let o = bounds.origin;
                            for outline in &extras {
                                let mut path = PathBuilder::stroke(px(1.));
                                path.move_to(point(o.x + px(outline[0].0), o.y + px(outline[0].1)));
                                for p in &outline[1..] {
                                    path.line_to(point(o.x + px(p.0), o.y + px(p.1)));
                                }
                                path.close();
                                if let Ok(path) = path.build() {
                                    window.paint_path(path, colour);
                                }
                            }
                        },
                    )
                    .absolute()
                    .size_full(),
                );
            }
            if let Some((l, locked, lid)) = selected {
                let colour = if editing { tok::canvas_selection() } else { tok::text_secondary() };
                let to_stage = move |(px_, py_): (f32, f32)| (lx + px_ * s, ly + py_ * s);
                let outline = geo::outline(&l).map(to_stage);
                let cropping = editing && !locked && self.crop_mode == Some(lid);
                // Crop mode: the whole picture (where it would sit uncropped), dimmed outside the
                // crop's shape; the shape's outline; handles on the shape.
                let crop_view = cropping.then(|| {
                    // The full frame needs the source's size; the shape and handles don't.
                    let input = studio
                        .selected_scene
                        .and_then(|id| project.scene(id))
                        .and_then(|sc| sc.layer(lid))
                        .map(|layer| layer.input);
                    let size = input.and_then(|i| studio.engine().call(move |s| s.reconciler.input_size(i)));
                    let full = size.map(|(w, h)| geo::outline(&l.uncropped((w as f32, h as f32))).map(to_stage));
                    let shape: Vec<(f32, f32)> = geo::shape_outline(&l).into_iter().map(to_stage).collect();
                    let (corners, edges) = geo::crop_handles(&l);
                    let handles: Vec<(f32, f32)> = corners
                        .iter()
                        .map(|(_, p)| to_stage(*p))
                        .chain(edges.iter().map(|(_, p)| to_stage(*p)))
                        .collect();
                    (full, shape, handles)
                });
                let crop_paths = crop_view.as_ref().map(|(full, shape, _)| (*full, shape.clone()));
                let round_handles =
                    l.shape.is_some_and(|sh| sh.kind != lightair_core::ShapeKind::Rounded || sh.radius > 0.3);
                let knob = (!locked && editing && !cropping).then(|| {
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
                            // While cropping, the shape's outline replaces the box.
                            for (width, c) in [(3.0, edge), (1.0, colour)].into_iter().filter(|_| crop_paths.is_none())
                            {
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
                            if let Some((full, shape)) = &crop_paths {
                                // Dim the cut-away part: the full frame with the shape as a hole.
                                if let Some(full) = full {
                                    let mut dim = PathBuilder::fill();
                                    dim.move_to(at(full[0]));
                                    for p in &full[1..] {
                                        dim.line_to(at(*p));
                                    }
                                    dim.close();
                                    dim.move_to(at(shape[0]));
                                    for p in shape[1..].iter().rev() {
                                        dim.line_to(at(*p));
                                    }
                                    dim.close();
                                    if let Ok(path) = dim.build() {
                                        window.paint_path(path, tok::crop_dim());
                                    }
                                }
                                for (width, c) in [(3.0, edge), (1.5, colour)] {
                                    let mut path = PathBuilder::stroke(px(width));
                                    path.move_to(at(shape[0]));
                                    for p in &shape[1..] {
                                        path.line_to(at(*p));
                                    }
                                    path.close();
                                    if let Ok(path) = path.build() {
                                        window.paint_path(path, c);
                                    }
                                }
                            }
                        },
                    )
                    .absolute()
                    .size_full(),
                );
                if let Some((_, _, handles)) = &crop_view {
                    for (hx, hy) in handles {
                        stage = stage.child(
                            div()
                                .absolute()
                                .left(px(hx - 5.))
                                .top(px(hy - 5.))
                                .size(px(10.))
                                .when(round_handles, |d| d.rounded_full())
                                .bg(tok::canvas_selection())
                                .border_1()
                                .border_color(tok::canvas_selection_edge()),
                        );
                    }
                }
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
        div().flex().flex_col().size_full().child(self.format_tabs(project, cx)).child(well)
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
    }

    /// Several layers selected: align them, or delete them.
    fn multi_inspector(
        &self,
        scene: &Scene,
        format: &Format,
        selection: &[Id],
        body: Div,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let (sid, fid) = (scene.id, format.id);
        let layers: Vec<(Id, Layout)> = selection
            .iter()
            .filter_map(|id| scene.layer(*id).filter(|l| !l.locked))
            .filter_map(|l| l.layouts.get(&fid).map(|lo| (l.id, *lo)))
            .collect();
        let align_button = |i: usize, icon: &'static str, tip: &'static str, how: geo::Align| {
            let layers = layers.clone();
            ui::icon_button(("align", i), icon, tok::text_secondary())
                .tooltip(move |w, cx| Tooltip::new(tip).build(w, cx))
                .on_click(cx.listener(move |this, _, _, cx| {
                    let layouts: Vec<Layout> = layers.iter().map(|(_, l)| *l).collect();
                    let aligned = geo::align(&layouts, how);
                    let moved = layers.iter().map(|(id, _)| *id).zip(aligned).collect();
                    this.set_layouts(sid, fid, moved, "Align layers", cx);
                }))
        };
        let row = div()
            .flex()
            .gap_1()
            .child(align_button(0, "align-start-vertical", "Align left edges", geo::Align::Left))
            .child(align_button(1, "align-center-vertical", "Align centres", geo::Align::Centre))
            .child(align_button(2, "align-end-vertical", "Align right edges", geo::Align::Right))
            .child(div().w(px(8.)))
            .child(align_button(3, "align-start-horizontal", "Align top edges", geo::Align::Top))
            .child(align_button(4, "align-center-horizontal", "Align middles", geo::Align::Middle))
            .child(align_button(5, "align-end-horizontal", "Align bottom edges", geo::Align::Bottom));
        let distribute_button = |i: usize, icon: &'static str, tip: &'static str, horizontal: bool| {
            let layers = layers.clone();
            ui::icon_button(("distribute", i), icon, tok::text_secondary())
                .tooltip(move |w, cx| Tooltip::new(tip).build(w, cx))
                .on_click(cx.listener(move |this, _, _, cx| {
                    let layouts: Vec<Layout> = layers.iter().map(|(_, l)| *l).collect();
                    let spaced = geo::distribute(&layouts, horizontal);
                    let moved = layers.iter().map(|(id, _)| *id).zip(spaced).collect();
                    this.set_layouts(sid, fid, moved, "Distribute layers", cx);
                }))
        };
        let row = row.child(div().w(px(8.))).when(layers.len() >= 3, |r| {
            r.child(distribute_button(0, "align-horizontal-distribute-center", "Space evenly across", true))
                .child(distribute_button(1, "align-vertical-distribute-center", "Space evenly down", false))
        });
        body.child(div().font_weight(FontWeight::SEMIBOLD).child(format!("{} layers selected", selection.len())))
            .child(div().text_color(tok::text_tertiary()).child("Drag any of them to move them together."))
            .child(row)
            .child(
                Button::new("delete-layers")
                    .label("Delete")
                    .small()
                    .danger()
                    .on_click(cx.listener(|this, _, _, cx| this.delete_selection(cx))),
            )
            .into_any_element()
    }

    fn inspector(&self, project: &Project, cx: &mut Context<Self>) -> impl IntoElement {
        let studio = self.studio.read(cx);
        let format = self.format(project).clone();
        let scene = studio.selected_scene.and_then(|id| project.scene(id)).cloned();
        let layer = scene.as_ref().and_then(|s| studio.selected_layer.and_then(|l| s.layer(l))).cloned();
        let selection = studio.selection();
        let track = studio.selected_track.and_then(|t| project.track(t)).cloned();
        let body = div().flex().flex_col().gap_3().px_3();
        let content = match (scene, layer) {
            (_, None) if track.is_some() => self.track_inspector(project, track.as_deref().unwrap(), body, cx),
            (Some(scene), Some(_)) if selection.len() > 1 => {
                self.multi_inspector(&scene, &format, &selection, body, cx)
            }
            (Some(scene), Some(layer)) => self.layer_inspector(project, &scene, &layer, &format, body, cx),
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
                .into_any_element(),
            _ => body.child(div().text_color(tok::text_tertiary()).child("Nothing selected")).into_any_element(),
        };
        div()
            .flex()
            .flex_col()
            .size_full()
            .pt_2()
            .child(div().id("inspector-scroll").flex_1().min_h_0().w_full().pb_3().overflow_y_scroll().child(content))
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
