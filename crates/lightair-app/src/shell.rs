//! The studio window (docs/05-interface-shell.md): title bar, Layers + Scenes, Canvas, Mixer,
//! Outputs + Inspector, status bar. Every edit goes through `Studio::execute`.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::rc::Rc;

use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::menu::ContextMenuExt;
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
mod assets;
mod docking;
mod golive;
mod hierarchy;
mod inspector;
mod layer_props;
mod menus;
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
        ToggleRotateTool,
        ToggleMics,
        GroupSelection,
        UngroupSelection,
        CreateEmpty,
        CreateEmptyChild,
        CopySelection,
        CutSelection,
        PasteLayers,
        DuplicateSelection,
        BringToFront,
        SendToBack,
        BringForward,
        SendBackward,
        ToggleLayerVisible,
        ToggleLayerLocked,
        MoveOutOfParent,
        FillCanvas,
        SaveAsPrefab,
        ZoomIn,
        ZoomOut,
        ZoomToFit,
        ZoomToActual,
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
        KeyBinding::new("e", ToggleRotateTool, Some("Studio")),
        KeyBinding::new("cmd-shift-m", ToggleMics, Some("Studio")),
        KeyBinding::new("cmd-g", GroupSelection, Some("Studio")),
        KeyBinding::new("cmd-=", ZoomIn, Some("Studio")),
        KeyBinding::new("cmd-+", ZoomIn, Some("Studio")),
        KeyBinding::new("cmd--", ZoomOut, Some("Studio")),
        KeyBinding::new("cmd-0", ZoomToFit, Some("Studio")),
        KeyBinding::new("cmd-alt-0", ZoomToActual, Some("Studio")),
        KeyBinding::new("cmd-c", CopySelection, Some("Studio")),
        KeyBinding::new("cmd-x", CutSelection, Some("Studio")),
        KeyBinding::new("cmd-v", PasteLayers, Some("Studio")),
        KeyBinding::new("cmd-d", DuplicateSelection, Some("Studio")),
        KeyBinding::new("cmd-shift-]", BringToFront, Some("Studio")),
        KeyBinding::new("cmd-shift-[", SendToBack, Some("Studio")),
        KeyBinding::new("cmd-]", BringForward, Some("Studio")),
        KeyBinding::new("cmd-[", SendBackward, Some("Studio")),
        KeyBinding::new("cmd-shift-h", ToggleLayerVisible, Some("Studio")),
        KeyBinding::new("cmd-shift-l", ToggleLayerLocked, Some("Studio")),
        KeyBinding::new("cmd-shift-g", UngroupSelection, Some("Studio")),
        KeyBinding::new("cmd-shift-n", CreateEmpty, Some("Studio")),
        KeyBinding::new("alt-shift-n", CreateEmptyChild, Some("Studio")),
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
    /// ⌥-drag inside: slides the picture under its crop.
    CropPan((f32, f32)),
    /// The bottom knob: tilt in 3D, both axes.
    Tilt,
    /// A rotate-gizmo ring: tilt around one axis.
    TiltAxis(geo::Axis),
}

/// A handle of the selected layer, for hover and drag feedback.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Handle {
    Gizmo(geo::Axis),
    Rotate,
    Tilt,
    Corner(Corner),
    Edge(geo::Edge),
    /// ⌥ inside a cropped picture: slide it under the crop.
    Pan,
}

impl DragMode {
    /// The handle this drag holds (lit up while dragging).
    fn handle(self, gizmo: bool) -> Option<Handle> {
        Some(match self {
            DragMode::Move => return None,
            DragMode::Resize(c) | DragMode::Crop(c, _) => Handle::Corner(c),
            DragMode::Rotate if gizmo => Handle::Gizmo(geo::Axis::Z),
            DragMode::Rotate => Handle::Rotate,
            DragMode::CropEdge(e, _) => Handle::Edge(e),
            DragMode::CropPan(_) => Handle::Pan,
            DragMode::Tilt => Handle::Tilt,
            DragMode::TiltAxis(a) => Handle::Gizmo(a),
        })
    }

    fn is_crop(self) -> bool {
        matches!(self, DragMode::Crop(..) | DragMode::CropEdge(..) | DragMode::CropPan(..))
    }
}

/// The resize cursor along a direction (degrees, screen space).
fn resize_cursor(degrees: f32) -> CursorStyle {
    match (degrees / 45.0).round().rem_euclid(4.0) as i32 {
        0 => CursorStyle::ResizeLeftRight,
        1 => CursorStyle::ResizeUpLeftDownRight,
        2 => CursorStyle::ResizeUpDown,
        _ => CursorStyle::ResizeUpRightDownLeft,
    }
}

/// How long crop view stays after a crop drag (another crop handle can be grabbed meanwhile),
/// and how long its ghost takes to fade out.
const CROP_LINGER: std::time::Duration = std::time::Duration::from_millis(700);
const CROP_FADE: std::time::Duration = std::time::Duration::from_millis(240);

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
    /// Whether the pointer moved (a still click on the 3D knob toggles the rotate gizmo).
    moved: bool,
}

/// Inputs' pixel sizes and when they were last read.
type SizeCache = (Option<Instant>, HashMap<Id, (u32, u32)>);

/// A source's list choices: (label, setting value).
type SourceOptions = Vec<(String, serde_json::Value)>;

/// A selection box being dragged: format, start, current point (canvas units), adding (⇧).
type Marquee = (Id, (f32, f32), (f32, f32), bool);

/// A crop bar on a side: its two ends on the stage, and whether it is lit.
type CropBar = ((f32, f32), (f32, f32), bool);

/// A rotate-gizmo ring to paint: colour, stroke width, points on the stage.
type Ring = (Hsla, f32, Vec<(f32, f32)>);

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
    /// The rotate gizmo (Unity-style rings) is showing, and which ring is under the pointer.
    rotate_tool: bool,
    gizmo_hover: Option<geo::Axis>,
    /// The handle under the pointer (nothing dragged).
    hover_handle: Option<Handle>,
    /// The pointer over the canvas: format and canvas point (drag readouts sit by it).
    pointer: Option<(Id, (f32, f32))>,
    /// Bumped by every crop drag and exit, so a pending exit can tell it's stale.
    crop_epoch: u64,
    /// Crop view is fading out (its handles are already gone).
    crop_leaving: bool,
    /// Bumped by every grab, restarting the grabbed handle's pop.
    grabs: usize,
    /// Hierarchy rows folded shut.
    collapsed_layers: HashSet<Id>,
    /// Where a row being dragged in the Hierarchy would land.
    tree_drop: Option<(Id, hierarchy::TreeDrop)>,
    /// The assets folder's files, and when they were read.
    asset_cache: RefCell<(Option<Instant>, Vec<assets::Asset>)>,
    selected_asset: Option<PathBuf>,
    /// Canvas zoom (1 = the format fits the stage) and pan (screen pixels from centred).
    zoom: f32,
    pan: (f32, f32),
    /// A pan drag: where the pointer started and the pan then.
    panning: Option<(Point<Pixels>, (f32, f32))>,
    /// Space is held: dragging the canvas pans it.
    space_down: bool,
    /// Inputs' pixel sizes, refreshed a few times a second: frames and handles hug the picture.
    sizes: RefCell<SizeCache>,
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
/// How close (screen px) the pointer must be to a gizmo ring.
const GIZMO_TOL: f32 = 6.0;

/// The rotate gizmo's radius in canvas units: most of the picture, but never tiny on screen.
fn gizmo_radius(l: &Layout, scale: f32) -> f32 {
    (l.width.min(l.height) * 0.42).max(56.0 / scale)
}
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
            rotate_tool: false,
            gizmo_hover: None,
            hover_handle: None,
            pointer: None,
            crop_epoch: 0,
            crop_leaving: false,
            grabs: 0,
            collapsed_layers: HashSet::new(),
            tree_drop: None,
            asset_cache: RefCell::new((None, Vec::new())),
            selected_asset: None,
            zoom: 1.0,
            pan: (0.0, 0.0),
            panning: None,
            space_down: false,
            sizes: RefCell::default(),
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
            self.leave_crop_view(std::time::Duration::ZERO, cx);
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
    /// `layouts` are frames in canvas terms (as `frame_of` gives them); each is stored in its
    /// layer's parent space. An empty moves its pivot by as much as its frame moved.
    fn set_layouts(&self, scene: Id, format: Id, layouts: Vec<(Id, Layout)>, label: &str, cx: &mut Context<Self>) {
        let project = self.studio.read(cx).project().clone();
        let Some(sc) = project.scene(scene) else { return };
        let mut commands: Vec<Command> = layouts
            .into_iter()
            .filter_map(|(layer, frame)| {
                let l = sc.layer(layer)?;
                let world = if l.is_empty() {
                    let now = self.frame_of(sc, l, format)?;
                    let pivot = sc.world(layer, format)?;
                    let (dx, dy) = (frame.x - now.x, frame.y - now.y);
                    Layout { x: pivot.x + dx, y: pivot.y + dy, ..pivot }
                } else {
                    frame
                };
                let layout = sc.local(layer, format, &world)?;
                Some(Command::SetLayout { scene, layer, format, layout })
            })
            .collect();
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
        let s = (bw / cw).min(bh / ch) * self.zoom;
        Some((
            f32::from(b.origin.x) + (bw - cw * s) / 2.0 + self.pan.0,
            f32::from(b.origin.y) + (bh - ch * s) / 2.0 + self.pan.1,
            s,
        ))
    }

    /// Zooms by `factor`, keeping the canvas point under `anchor` (a window point; `None`: the
    /// middle of the stage) where it is.
    fn zoom_by(&mut self, factor: f32, anchor: Option<Point<Pixels>>, cx: &mut Context<Self>) {
        let project = self.studio.read(cx).project().clone();
        let format = anchor.and_then(|a| self.stage_at(&project, a)).unwrap_or_else(|| self.format_id(&project));
        let Some(b) = self.stage_bounds.borrow().get(&format).copied() else { return };
        let Some(f) = project.format(format) else { return };
        let (cw, ch) = (f.width as f32, f.height as f32);
        let (bw, bh) = (f32::from(b.size.width), f32::from(b.size.height));
        let fit = (bw / cw).min(bh / ch);
        let anchor = anchor.unwrap_or(b.center());
        let Some((ox, oy, s)) = self.mapping(&project, format) else { return };
        let (ax, ay) = (f32::from(anchor.x), f32::from(anchor.y));
        let (px_, py_) = ((ax - ox) / s, (ay - oy) / s);
        let zoom = (self.zoom * factor).clamp(0.1, 16.0);
        let s2 = fit * zoom;
        // Where the canvas origin lands centred at the new zoom, then shift so the point stays.
        let (bx, by) = (f32::from(b.origin.x) + (bw - cw * s2) / 2.0, f32::from(b.origin.y) + (bh - ch * s2) / 2.0);
        self.zoom = zoom;
        self.pan = (ax - bx - px_ * s2, ay - by - py_ * s2);
        cx.notify();
    }

    fn zoom_to_fit(&mut self, cx: &mut Context<Self>) {
        self.zoom = 1.0;
        self.pan = (0.0, 0.0);
        cx.notify();
    }

    /// One canvas pixel to one screen point.
    fn zoom_to_actual(&mut self, cx: &mut Context<Self>) {
        let project = self.studio.read(cx).project().clone();
        let format = self.format_id(&project);
        let Some((_, _, s)) = self.mapping(&project, format) else { return };
        self.zoom_by(1.0 / s, None, cx);
    }

    /// The current scale as a percentage (100: one canvas pixel per screen point).
    fn zoom_percent(&self, project: &Project) -> f32 {
        self.mapping(project, self.format_id(project)).map_or(100.0, |(_, _, s)| s * 100.0)
    }

    fn canvas_scroll(&mut self, ev: &ScrollEvent, _: &mut Window, cx: &mut Context<Self>) {
        let delta = match ev.delta {
            ScrollDelta::Pixels(p) => (f32::from(p.x), f32::from(p.y)),
            ScrollDelta::Lines(p) => (p.x * 20.0, p.y * 20.0),
        };
        if ev.modifiers.platform || ev.modifiers.control {
            // ⌘/Ctrl + scroll zooms at the pointer.
            self.zoom_by((delta.1 * 0.01).exp(), Some(ev.position), cx);
        } else {
            self.pan = (self.pan.0 + delta.0, self.pan.1 + delta.1);
            cx.notify();
        }
    }

    fn canvas_pinch(&mut self, ev: &PinchEvent, _: &mut Window, cx: &mut Context<Self>) {
        self.zoom_by(1.0 + ev.delta, Some(ev.position), cx);
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
        if self.space_down {
            self.panning = Some((ev.position, self.pan));
            return;
        }
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

        // A handle of the selected layer? (See `handle_at`.)
        if let Some(sel) = studio.selected_layer.and_then(|id| scene.layer(id))
            && let Some(frame) = self.frame_of(scene, sel, format).filter(|_| !scene.locked(sel.id) && !sel.is_empty())
        {
            // The frame hugs the picture: handles sit on it, and a drag snaps the box to it.
            let l = &frame;
            let size =
                sel.input.and_then(|i| self.sizes.borrow().1.get(&i).copied()).map(|(w, h)| (w as f32, h as f32));
            let handle = self.handle_at(l, (x, y), s, ev.modifiers.alt, size.is_some());
            if handle == Some(Handle::Tilt) && ev.click_count == 2 {
                // Double-click the 3D knob: flat again.
                let stored = sel.layouts[&format];
                let layout = Layout { tilt_x: 0.0, tilt_y: 0.0, ..stored };
                self.exec(Command::SetLayout { scene: scene_id, layer: sel.id, format, layout }, cx);
                return;
            }
            let mode = handle.and_then(|h| {
                Some(match h {
                    Handle::Gizmo(geo::Axis::Z) | Handle::Rotate => DragMode::Rotate,
                    Handle::Gizmo(a) => DragMode::TiltAxis(a),
                    Handle::Tilt => DragMode::Tilt,
                    Handle::Corner(c) => match (ev.modifiers.alt, size) {
                        (true, Some(sz)) => DragMode::Crop(c, sz),
                        _ => DragMode::Resize(c),
                    },
                    Handle::Edge(e) => DragMode::CropEdge(e, size?),
                    Handle::Pan => DragMode::CropPan(size?),
                })
            });
            if let Some(mode) = mode {
                // Cropping shows the whole picture, the cut-away part fading in as a ghost.
                if mode.is_crop() {
                    self.crop_epoch += 1;
                    if self.crop_leaving {
                        // Grabbed again while fading out: fade back in.
                        self.crop_leaving = false;
                        self.crop_preview_sent = None;
                    }
                    self.crop_mode = Some(sel.id);
                }
                self.grabs += 1;
                self.hover_handle = None;
                self.drag = Some(Drag {
                    scene: scene_id,
                    layer: sel.id,
                    format,
                    mode,
                    start: (x, y),
                    layout: *l,
                    others: vec![],
                    moved: false,
                });
                self.studio.update(cx, |s, _| s.begin_gesture());
                return;
            }
        }
        // Topmost visible picture under the pointer (the deepest layer: a child, not the empty
        // holding it). A selected empty is grabbed anywhere in the box round its layers.
        let selected_empty = studio
            .selected_layer
            .and_then(|id| scene.layer(id))
            .filter(|l| l.is_empty() && !scene.locked(l.id))
            .filter(|l| self.frame_of(scene, l, format).is_some_and(|lo| geo::contains(&lo, (x, y))));
        let hit =
            selected_empty.or_else(|| {
                scene.layers.iter().rev().filter(|l| !l.is_empty()).find(|l| {
                    self.frame_of(scene, l, format).is_some_and(|lo| lo.visible && geo::contains(&lo, (x, y)))
                })
            });
        let selection = studio.selection();
        match hit {
            Some(layer) if ev.modifiers.shift => self.toggle_selected(layer.id, cx),
            Some(layer) => {
                let id = layer.id;
                // Dragging one of several selected layers moves them all; otherwise select it.
                let in_selection = selection.contains(&id);
                if !scene.locked(layer.id) {
                    let Some(layout) = self.frame_of(scene, layer, format) else { return };
                    let others: Vec<(Id, Layout)> = if in_selection {
                        selection
                            .iter()
                            .filter(|o| **o != id)
                            .filter_map(|o| scene.layer(*o).filter(|l| !scene.locked(l.id)))
                            // Not inside another moving layer: it moves with that one.
                            .filter(|l| !selection.iter().any(|a| scene.is_under(l.id, *a)))
                            .filter_map(|l| self.frame_of(scene, l, format).map(|lo| (l.id, lo)))
                            .collect()
                    } else {
                        vec![]
                    };
                    let mode = DragMode::Move;
                    self.drag = Some(Drag {
                        scene: scene_id,
                        layer: id,
                        format,
                        mode,
                        start: (x, y),
                        layout,
                        others,
                        moved: false,
                    });
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
        if let Some((from, pan)) = self.panning {
            let d = ev.position - from;
            self.pan = (pan.0 + f32::from(d.x), pan.1 + f32::from(d.y));
            cx.notify();
            return;
        }
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
        {
            let project = self.studio.read(cx).project().clone();
            let format = self.drag.as_ref().map(|d| d.format).unwrap_or_else(|| self.format_id(&project));
            self.pointer = self.to_canvas(&project, format, ev.position).map(|p| (format, p));
        }
        // The handle under the pointer (lit up, with a matching cursor) when nothing is dragged.
        if self.drag.is_none() {
            let project = self.studio.read(cx).project().clone();
            let format = self.format_id(&project);
            let studio = self.studio.read(cx);
            let hover = studio.selected_scene.and_then(|sid| project.scene(sid)).and_then(|scene| {
                let layer = studio.selected_layer.and_then(|id| scene.layer(id)).filter(|l| !scene.locked(l.id))?;
                if layer.is_empty() {
                    return None;
                }
                let frame = self.frame_of(scene, layer, format)?;
                let (_, _, s) = self.mapping(&project, format)?;
                let p = self.pointer.map(|(_, p)| p)?;
                let has_size = layer.input.is_some_and(|i| self.sizes.borrow().1.contains_key(&i));
                self.handle_at(&frame, p, s, ev.modifiers.alt, has_size)
            });
            let gizmo = match hover {
                Some(Handle::Gizmo(a)) => Some(a),
                _ => None,
            };
            if hover != self.hover_handle || gizmo != self.gizmo_hover {
                self.hover_handle = hover;
                self.gizmo_hover = gizmo;
                cx.notify();
            }
            return;
        }
        if let Some(d) = &mut self.drag {
            d.moved = true;
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
            DragMode::TiltAxis(axis) => {
                let k = 0.35 * s;
                let snap = |a: f32| if ev.modifiers.shift { (a / 15.0).round() * 15.0 } else { a };
                match axis {
                    geo::Axis::X => l.tilt_x = snap((drag.layout.tilt_x + dy * k).clamp(-75.0, 75.0)),
                    _ => l.tilt_y = snap((drag.layout.tilt_y - dx * k).clamp(-75.0, 75.0)),
                }
            }
            DragMode::Tilt => {
                // Drag right: the right side turns away; drag down: the bottom does. Screen
                // degrees per pixel; ⇧ snaps to 15°.
                let k = 0.35 * s;
                let snap = |a: f32| if ev.modifiers.shift { (a / 15.0).round() * 15.0 } else { a };
                l.tilt_y = snap((drag.layout.tilt_y - dx * k).clamp(-75.0, 75.0));
                l.tilt_x = snap((drag.layout.tilt_x + dy * k).clamp(-75.0, 75.0));
            }
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
            // In Fit the picture keeps its shape, so the frame does too (Stretch to distort).
            DragMode::Resize(corner) => {
                l = geo::resize(&l, corner, (dx, dy), ev.modifiers.shift || l.fit == lightair_core::Fit::Fit)
            }
        }
        self.guides = guides;
        // The group follows the grabbed layer (after snapping).
        let (mx, my) = (l.x - drag.layout.x, l.y - drag.layout.y);
        let mut layouts = vec![(drag.layer, l)];
        layouts.extend(drag.others.iter().map(|(id, o)| (*id, Layout { x: o.x + mx, y: o.y + my, ..*o })));
        let (scene, format) = (drag.scene, drag.format);
        self.set_layouts(scene, format, layouts, "Move objects", cx);
    }

    fn canvas_up(&mut self, _: &MouseUpEvent, _window: &mut Window, cx: &mut Context<Self>) {
        if self.panning.take().is_some() {
            cx.notify();
            return;
        }
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
                        .filter(|l| !l.is_empty() && !scene.locked(l.id))
                        .filter(|l| {
                            scene
                                .world(l.id, format)
                                .is_some_and(|lo| scene.shown(l.id, format) && geo::touches(&lo, from, to))
                        })
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
        if let Some(drag) = self.drag.take() {
            self.guides.clear();
            if matches!(drag.mode, DragMode::Tilt) && !drag.moved {
                self.rotate_tool = !self.rotate_tool;
                self.gizmo_hover = None;
            }
            // A crop drag showed the whole picture: stay a moment (for another crop handle),
            // then fade back to the result.
            if drag.mode.is_crop() {
                self.leave_crop_view(CROP_LINGER, cx);
            }
            self.studio.update(cx, |s, cx| {
                s.end_gesture();
                cx.notify();
            });
        }
    }

    /// The handle of `l` (the selected layer's frame) at canvas point `p`. One set does
    /// everything: corners resize (⌥ crops), edges crop, the top knob rotates, the bottom knob
    /// tilts in 3D (and opens the rotate gizmo), ⌥-drag inside pans a cropped picture.
    fn handle_at(&self, l: &Layout, (x, y): (f32, f32), s: f32, alt: bool, has_size: bool) -> Option<Handle> {
        let tol = HANDLE / s;
        let near = |(px_, py_): (f32, f32)| (x - px_).abs() <= tol && (y - py_).abs() <= tol;
        if self.rotate_tool {
            if let Some(axis) = geo::gizmo_hit(l, gizmo_radius(l, s), (x, y), GIZMO_TOL / s) {
                return Some(Handle::Gizmo(axis));
            }
            // With the gizmo on, the other handles are hidden; inside still moves.
            return near(geo::tilt_handle(l, ROTATE_OFFSET / s)).then_some(Handle::Tilt);
        }
        if near(geo::rotate_handle(l, ROTATE_OFFSET / s)) {
            return Some(Handle::Rotate);
        }
        if near(geo::tilt_handle(l, ROTATE_OFFSET / s)) {
            return Some(Handle::Tilt);
        }
        if let Some(c) = CORNERS.iter().find(|c| near(geo::corner(l, **c))) {
            return Some(Handle::Corner(*c));
        }
        if has_size {
            let (_, edges) = geo::crop_handles(l);
            if let Some((e, _)) = edges.iter().find(|(_, p)| near(*p)) {
                return Some(Handle::Edge(*e));
            }
            if alt && l.crop != lightair_core::Crop::default() && geo::contains(l, (x, y)) {
                return Some(Handle::Pan);
            }
        }
        None
    }

    /// Leaves crop view after `delay`: the ghost fades out, then the crop applies again. A crop
    /// drag in the meantime cancels it.
    fn leave_crop_view(&mut self, delay: std::time::Duration, cx: &mut Context<Self>) {
        if self.crop_mode.is_none() {
            return;
        }
        self.crop_epoch += 1;
        let epoch = self.crop_epoch;
        cx.spawn(async move |this, cx| {
            if !delay.is_zero() {
                cx.background_executor().timer(delay).await;
            }
            let fading = this.update(cx, |this, cx| {
                if this.crop_epoch != epoch || this.drag.as_ref().is_some_and(|d| d.mode.is_crop()) {
                    return false;
                }
                this.crop_leaving = true;
                let studio = this.studio.read(cx);
                let project = studio.project().clone();
                studio.engine().post(move |s| s.reconciler.fade_crop_preview(&project));
                cx.notify();
                true
            });
            if !matches!(fading, Ok(true)) {
                return;
            }
            cx.background_executor().timer(CROP_FADE).await;
            let _ = this.update(cx, |this, cx| {
                if this.crop_epoch == epoch && this.crop_leaving {
                    this.crop_mode = None;
                    this.crop_leaving = false;
                    this.sync_crop_preview(cx);
                    cx.notify();
                }
            });
        })
        .detach();
    }

    /// The canvas's cursor: what a press would do here (or is doing).
    fn canvas_cursor(&self, project: &Project, cx: &App) -> CursorStyle {
        if self.panning.is_some() {
            return CursorStyle::ClosedHand;
        }
        if self.space_down {
            return CursorStyle::OpenHand;
        }
        let handle = match &self.drag {
            Some(d) if d.mode == DragMode::Move => return CursorStyle::ClosedHand,
            Some(d) => d.mode.handle(self.rotate_tool),
            None => self.hover_handle,
        };
        let rotation = {
            let studio = self.studio.read(cx);
            let format = self.format_id(project);
            studio
                .selected_scene
                .and_then(|id| project.scene(id))
                .and_then(|sc| studio.selected_layer.and_then(|l| sc.world(l, format)))
                .map_or(0.0, |l| l.rotation)
        };
        let grabbing = self.drag.is_some();
        match handle {
            Some(Handle::Corner(c)) => resize_cursor(
                match c {
                    Corner::TopLeft | Corner::BottomRight => 45.0,
                    Corner::TopRight | Corner::BottomLeft => 135.0,
                } + rotation,
            ),
            Some(Handle::Edge(e)) => resize_cursor(
                match e {
                    geo::Edge::Left | geo::Edge::Right => 0.0,
                    geo::Edge::Top | geo::Edge::Bottom => 90.0,
                } + rotation,
            ),
            Some(_) if grabbing => CursorStyle::ClosedHand,
            Some(_) => CursorStyle::OpenHand,
            None => CursorStyle::Arrow,
        }
    }

    fn nudge(&mut self, dx: f32, dy: f32, cx: &mut Context<Self>) {
        let studio = self.studio.read(cx);
        let project = studio.project().clone();
        let format = self.format_id(&project);
        let Some(scene_id) = studio.selected_scene else { return };
        let Some(scene) = project.scene(scene_id) else { return };
        let selection = studio.selection();
        let moved: Vec<(Id, Layout)> = selection
            .iter()
            .filter_map(|id| scene.layer(*id).filter(|l| !scene.locked(l.id)))
            .filter(|l| !selection.iter().any(|a| scene.is_under(l.id, *a)))
            .filter_map(|l| {
                self.frame_of(scene, l, format).map(|lo| (l.id, Layout { x: lo.x + dx, y: lo.y + dy, ..lo }))
            })
            .collect();
        self.set_layouts(scene_id, format, moved, "Nudge objects", cx);
    }

    fn delete_selection(&mut self, cx: &mut Context<Self>) {
        let studio = self.studio.read(cx);
        let Some(scene) = studio.selected_scene else { return };
        let Some(sc) = studio.project().scene(scene) else { return };
        // A layer inside another selected one goes with it.
        let selection = studio.selection();
        let mut commands: Vec<Command> = selection
            .iter()
            .filter(|l| !selection.iter().any(|a| sc.is_under(**l, *a)))
            .map(|layer| Command::RemoveLayer { scene, layer: *layer })
            .collect();
        match commands.len() {
            0 => {}
            1 => self.exec(commands.remove(0), cx),
            n => self.exec(Command::Batch { label: format!("Delete {n} objects"), commands }, cx),
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
        self.refresh_sizes(cx);
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
                // Esc leaves crop mode or the rotate gizmo first.
                if this.crop_mode.is_some() && !this.crop_leaving {
                    this.leave_crop_view(std::time::Duration::ZERO, cx);
                } else if this.rotate_tool {
                    this.rotate_tool = false;
                    cx.notify();
                } else {
                    this.select_layer(None, cx)
                }
            }))
            .on_action(cx.listener(|this, _: &RenameSelection, window, cx| {
                if this.crop_mode.is_some() && !this.crop_leaving {
                    this.leave_crop_view(std::time::Duration::ZERO, cx);
                } else {
                    this.rename_selection(window, cx)
                }
            }))
            .on_action(cx.listener(|this, _: &OpenPalette, window, cx| this.open_palette(window, cx)))
            .on_action(cx.listener(|this, _: &OpenSettings, _, cx| this.toggle_settings(cx)))
            .on_action(cx.listener(|this, _: &ToggleRotateTool, _, cx| {
                this.rotate_tool = !this.rotate_tool;
                this.gizmo_hover = None;
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &ToggleMics, _, cx| this.studio.update(cx, |s, cx| s.toggle_mics(cx))))
            .on_action(cx.listener(|this, _: &GroupSelection, window, cx| this.group_selection(window, cx)))
            .on_action(cx.listener(|this, _: &CopySelection, _, cx| {
                this.copy_selection(cx);
            }))
            .on_action(cx.listener(|this, _: &CutSelection, _, cx| this.cut_selection(cx)))
            .on_action(cx.listener(|this, _: &PasteLayers, _, cx| this.paste_layers(cx)))
            .on_action(cx.listener(|this, _: &DuplicateSelection, _, cx| this.duplicate_selection(cx)))
            .on_action(cx.listener(|this, _: &BringToFront, _, cx| this.arrange("Bring to front", |_, n| n - 1, cx)))
            .on_action(cx.listener(|this, _: &SendToBack, _, cx| this.arrange("Send to back", |_, _| 0, cx)))
            .on_action(cx.listener(|this, _: &BringForward, _, cx| this.arrange("Bring forward", |i, _| i + 1, cx)))
            .on_action(cx.listener(|this, _: &SendBackward, _, cx| {
                this.arrange("Send backward", |i, _| i.saturating_sub(1), cx)
            }))
            .on_action(cx.listener(|this, _: &ToggleLayerVisible, _, cx| this.toggle_selection_visible(cx)))
            .on_action(cx.listener(|this, _: &ToggleLayerLocked, _, cx| this.toggle_selection_locked(cx)))
            .on_action(cx.listener(|this, _: &MoveOutOfParent, _, cx| this.move_out_of_parent(cx)))
            .on_action(cx.listener(|this, _: &FillCanvas, _, cx| this.fill_canvas(cx)))
            .on_action(cx.listener(|this, _: &ZoomIn, _, cx| this.zoom_by(1.25, None, cx)))
            .on_action(cx.listener(|this, _: &ZoomOut, _, cx| this.zoom_by(0.8, None, cx)))
            .on_action(cx.listener(|this, _: &ZoomToFit, _, cx| this.zoom_to_fit(cx)))
            .on_action(cx.listener(|this, _: &ZoomToActual, _, cx| this.zoom_to_actual(cx)))
            // Space held: dragging the canvas pans it.
            .capture_key_down(cx.listener(|this, ev: &KeyDownEvent, _, cx| {
                if ev.keystroke.key == "space" && !ev.keystroke.modifiers.modified() && !this.space_down {
                    this.space_down = true;
                    cx.notify();
                }
            }))
            .capture_key_up(cx.listener(|this, ev: &KeyUpEvent, _, cx| {
                if ev.keystroke.key == "space" && this.space_down {
                    this.space_down = false;
                    cx.notify();
                }
            }))
            .on_action(cx.listener(|this, _: &SaveAsPrefab, _, cx| {
                if let Some(layer) = this.studio.read(cx).selected_layer {
                    this.save_prefab(layer, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &UngroupSelection, _, cx| this.ungroup_selection(cx)))
            .on_action(cx.listener(|this, _: &CreateEmpty, window, cx| this.create_empty(None, window, cx)))
            .on_action(cx.listener(|this, _: &CreateEmptyChild, window, cx| {
                let parent = this.studio.read(cx).selected_layer;
                this.create_empty(parent, window, cx)
            }))
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

    fn refresh_sizes(&self, cx: &mut Context<Self>) {
        let stale = self.sizes.borrow().0.is_none_or(|t| t.elapsed().as_millis() > 250);
        if stale {
            let sizes = self.studio.read(cx).engine().call(|s| s.reconciler.input_sizes());
            *self.sizes.borrow_mut() = (Some(Instant::now()), sizes);
        }
    }

    /// Where a layer's picture actually is in a format: its box, narrowed to the picture in Fit
    /// mode. The frame, handles and dragging use this, so they stick to the source.
    /// In canvas terms (a child's layout is in its parent's space). An empty's frame is the box
    /// round what it holds (a small square at its pivot when it holds nothing).
    fn frame_of(&self, scene: &Scene, layer: &Layer, format: Id) -> Option<Layout> {
        let world = scene.world(layer.id, format)?;
        if layer.is_empty() {
            let frames: Vec<Layout> = scene
                .layers
                .iter()
                .filter(|l| !l.is_empty() && scene.is_under(l.id, layer.id))
                .filter_map(|l| self.frame_of(scene, l, format))
                .collect();
            let bounds =
                frames.iter().map(geo::bounds).reduce(|a, b| (a.0.min(b.0), a.1.min(b.1), a.2.max(b.2), a.3.max(b.3)));
            let bounds = bounds.map(|(x0, y0, x1, y1)| Layout::boxed(x0, y0, x1 - x0, y1 - y0));
            let (px_, py_) = (world.x, world.y);
            return Some(Layout {
                visible: world.visible && scene.shown(layer.id, format),
                ..bounds.unwrap_or(Layout::boxed(px_ - 20.0, py_ - 20.0, 40.0, 40.0))
            });
        }
        let size = layer.input.and_then(|i| self.sizes.borrow().1.get(&i).copied()).map(|(w, h)| (w as f32, h as f32));
        let mut frame = geo::content_rect(&world, size);
        frame.visible = scene.shown(layer.id, format);
        Some(frame)
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
        let studio = self.studio.read(cx);
        let selected = studio.selected_scene;
        let program = project.program;
        let shell = cx.entity().downgrade();
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
                .on_mouse_down(
                    MouseButton::Right,
                    cx.listener(move |this, _, _, cx| {
                        this.studio.update(cx, |s, cx| {
                            s.selected_scene = Some(id);
                            cx.notify();
                        })
                    }),
                )
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
                .context_menu({
                    let shell = shell.clone();
                    move |m, _, cx| menus::scene_menu(id, m, &shell, cx)
                })
        });
        div().flex().flex_col().size_full().pt_1().child(
            div()
                .id("scenes-scroll")
                .flex()
                .flex_col()
                .flex_1()
                .min_h_0()
                .gap_0p5()
                .overflow_y_scroll()
                .children(rows)
                // Empty space below: its own menu.
                .child(
                    div()
                        .id("scenes-end")
                        .flex_1()
                        .min_h(px(24.))
                        .on_click(cx.listener(|this, _, _, cx| this.select_layer(None, cx)))
                        .context_menu({
                            let shell = cx.entity().downgrade();
                            move |m, _, _| menus::scenes_blank_menu(m, &shell)
                        }),
                ),
        )
    }

    fn format_tabs(&self, project: &Project, cx: &mut Context<Self>) -> impl IntoElement {
        let active = self.format_id(project);
        let mut bar = div()
            .flex()
            .items_center()
            .gap_1()
            .h(px(36.))
            .px_3()
            .bg(tok::bg_header())
            .border_b_1()
            .border_color(tok::border_header());
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
        // + right after the tabs: another shape for every scene.
        let open = self.format_menu.is_some();
        bar = bar.child(
            ui::icon_button("add-format", "plus", if open { tok::text() } else { tok::text_secondary() })
                .when(open, |d| d.bg(tok::bg_selected()))
                .tooltip(|w, cx| Tooltip::new("Add format (vertical, square, custom…)").build(w, cx))
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
        // Zoom: − 100% +, the number fits the canvas to the stage again.
        let percent = self.zoom_percent(project);
        let fitted = (self.zoom - 1.0).abs() < 0.001 && self.pan == (0.0, 0.0);
        let zoom = div()
            .flex()
            .items_center()
            .gap_0p5()
            .mr_2()
            .child(
                ui::icon_button("zoom-out", "minus", tok::text_secondary())
                    .tooltip(|w, cx| Tooltip::new("Zoom out (⌘−)").build(w, cx))
                    .on_click(cx.listener(|this, _, _, cx| this.zoom_by(0.8, None, cx))),
            )
            .child(
                div()
                    .id("zoom-fit")
                    .min_w(px(46.))
                    .h(px(22.))
                    .px_1()
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(px(4.))
                    .cursor_pointer()
                    .text_size(px(11.))
                    .font_family("Menlo")
                    .text_color(if fitted { tok::text_secondary() } else { tok::text() })
                    .hover(|d| d.bg(tok::bg_raised()))
                    .child(format!("{percent:.0}%"))
                    .tooltip(|w, cx| Tooltip::new("Fit (⌘0) · double-click: 100% (⌥⌘0)").build(w, cx))
                    .on_click(cx.listener(|this, ev: &ClickEvent, _, cx| {
                        if ev.click_count() == 2 { this.zoom_to_actual(cx) } else { this.zoom_to_fit(cx) }
                    })),
            )
            .child(
                ui::icon_button("zoom-in", "plus", tok::text_secondary())
                    .tooltip(|w, cx| Tooltip::new("Zoom in (⌘=)").build(w, cx))
                    .on_click(cx.listener(|this, _, _, cx| this.zoom_by(1.25, None, cx))),
            );
        let safe = if vertical { self.safe_vertical } else { self.safe_landscape };
        bar = bar.child(div().flex_1()).child(zoom).child(
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
                Some(f) => match (self.mapping(project, fid), self.stage_bounds.borrow().get(&fid).copied()) {
                    // Placed where the canvas is (zoomed and panned).
                    (Some((ox, oy, s)), Some(b)) => surface(f)
                        .absolute()
                        .left(px(ox - f32::from(b.origin.x)))
                        .top(px(oy - f32::from(b.origin.y)))
                        .w(px(format.width as f32 * s))
                        .h(px(format.height as f32 * s))
                        .object_fit(ObjectFit::Fill)
                        .into_any_element(),
                    _ => surface(f).absolute().size_full().object_fit(ObjectFit::Contain).into_any_element(),
                },
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
                .and_then(|l| {
                    let scene = studio.selected_scene.and_then(|id| project.scene(id))?;
                    self.frame_of(scene, l, fid).map(|lo| (lo, scene.locked(l.id) || l.is_empty(), l.id))
                });
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
                        .filter_map(|l| self.frame_of(scene, l, fid))
                        .map(|lo| geo::outline(&lo).map(|(px_, py_)| (lx + px_ * s, ly + py_ * s)))
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
                let cropping = editing && !locked && self.crop_mode == Some(lid) && !self.crop_leaving;
                // The handle being dragged, or else the one under the pointer: lit up.
                let hot: Option<Handle> = match self.drag.as_ref().filter(|d| d.layer == lid) {
                    Some(d) => d.mode.handle(self.rotate_tool),
                    None => self.hover_handle,
                };
                let grabbed = self.drag.as_ref().is_some_and(|d| d.layer == lid);
                let grabs = self.grabs;
                // A handle's dot: lit when hot, and a quick pop when grabbed.
                let dot = move |(x, y): (f32, f32), base: f32, round: bool, this: Option<Handle>| -> AnyElement {
                    let is_hot = this.is_some() && this == hot;
                    let size = if is_hot { base * 1.3 } else { base };
                    let el = div()
                        .absolute()
                        .left(px(x - size / 2.))
                        .top(px(y - size / 2.))
                        .size(px(size))
                        .when(round, |d| d.rounded_full())
                        .bg(if is_hot { tok::accent_primary() } else { tok::canvas_selection() })
                        .border_1()
                        .border_color(if is_hot { tok::canvas_selection() } else { tok::canvas_selection_edge() });
                    if is_hot && grabbed {
                        el.with_animation(
                            ElementId::from(("grab", grabs)),
                            Animation::new(std::time::Duration::from_millis(260)),
                            move |el, t| {
                                let sz = size + base * 0.55 * bounce(ease_out_quint())(t);
                                el.left(px(x - sz / 2.)).top(px(y - sz / 2.)).size(px(sz))
                            },
                        )
                        .into_any_element()
                    } else {
                        el.into_any_element()
                    }
                };
                // Crop mode: the whole picture (where it would sit uncropped) with the cut-away
                // part as a ghost (drawn by the engine); the shape's outline; handles on it.
                let crop_view = cropping.then(|| {
                    // The full frame needs the source's size; the shape and handles don't.
                    let input = studio
                        .selected_scene
                        .and_then(|id| project.scene(id))
                        .and_then(|sc| sc.layer(lid))
                        .and_then(|layer| layer.input);
                    let size = input.and_then(|i| studio.engine().call(move |s| s.reconciler.input_size(i)));
                    let full = size.map(|(w, h)| geo::outline(&l.uncropped((w as f32, h as f32))).map(to_stage));
                    let shape: Vec<(f32, f32)> = geo::shape_outline(&l).into_iter().map(to_stage).collect();
                    let (corners, edges) = geo::crop_handles(&l);
                    let handles: Vec<((f32, f32), Handle)> = corners
                        .iter()
                        .map(|(c, p)| (to_stage(*p), Handle::Corner(*c)))
                        .chain(edges.iter().map(|(e, p)| (to_stage(*p), Handle::Edge(*e))))
                        .collect();
                    (full, shape, handles)
                });
                let crop_paths = crop_view.as_ref().map(|(full, shape, _)| (*full, shape.clone()));
                let round_handles =
                    l.shape.is_some_and(|sh| sh.kind != lightair_core::ShapeKind::Rounded || sh.radius > 0.3);
                // The rotate gizmo (rings per axis) replaces the other handles while it's on.
                let gizmo_on = self.rotate_tool && !locked && editing && !cropping;
                let knob = (!locked && editing && !cropping && !gizmo_on).then(|| {
                    (to_stage(geo::rotate_handle(&l, 0.0)), to_stage(geo::rotate_handle(&l, ROTATE_OFFSET / s)))
                });
                // The 3D knob below, mirroring the rotate knob above (a click toggles the gizmo).
                let tilt_knob = (!locked && editing && !cropping)
                    .then(|| (to_stage(geo::tilt_handle(&l, 0.0)), to_stage(geo::tilt_handle(&l, ROTATE_OFFSET / s))));
                let active_axis = self
                    .drag
                    .as_ref()
                    .filter(|d| d.layer == lid)
                    .and_then(|d| match d.mode {
                        DragMode::TiltAxis(a) => Some(a),
                        DragMode::Rotate if gizmo_on => Some(geo::Axis::Z),
                        _ => None,
                    })
                    .or(self.gizmo_hover);
                let rings: Vec<Ring> = if gizmo_on {
                    geo::gizmo_rings(&l, gizmo_radius(&l, s))
                        .into_iter()
                        .map(|(axis, pts)| {
                            let hot = active_axis == Some(axis);
                            let colour = if hot {
                                tok::gizmo_active()
                            } else {
                                match axis {
                                    geo::Axis::X => tok::gizmo_x(),
                                    geo::Axis::Y => tok::gizmo_y(),
                                    geo::Axis::Z => tok::gizmo_z(),
                                }
                            };
                            (colour, if hot { 3.5 } else { 2.5 }, pts.into_iter().map(to_stage).collect())
                        })
                        .collect()
                } else {
                    Vec::new()
                };
                // Crop bars at the middle of each side (on the shape), along the side.
                let crop_bars: Vec<CropBar> = if knob.is_some() {
                    let (_, edges) = geo::crop_handles(&l);
                    let (sin, cos) = l.rotation.to_radians().sin_cos();
                    edges
                        .iter()
                        .map(|(e, p)| {
                            let (cx_, cy_) = to_stage(*p);
                            // Along the side: horizontal for top/bottom, vertical for left/right.
                            let (ux, uy) = match e {
                                geo::Edge::Top | geo::Edge::Bottom => (cos, sin),
                                geo::Edge::Left | geo::Edge::Right => (-sin, cos),
                            };
                            let lit = hot == Some(Handle::Edge(*e));
                            let half = if lit { 12.0 } else { 9.0 };
                            ((cx_ - ux * half, cy_ - uy * half), (cx_ + ux * half, cy_ + uy * half), lit)
                        })
                        .collect()
                } else {
                    Vec::new()
                };
                // How the picture looks tilted in 3D, when it is.
                let tilted =
                    (l.tilt_x.abs() > 0.01 || l.tilt_y.abs() > 0.01).then(|| geo::projected_outline(&l).map(to_stage));
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
                            for (colour, width, pts) in &rings {
                                let mut path = PathBuilder::stroke(px(*width));
                                path.move_to(at(pts[0]));
                                for p in &pts[1..] {
                                    path.line_to(at(*p));
                                }
                                path.close();
                                if let Ok(path) = path.build() {
                                    window.paint_path(path, *colour);
                                }
                            }
                            for (from, to) in knob.into_iter().chain(tilt_knob) {
                                let mut path = PathBuilder::stroke(px(1.));
                                path.move_to(at(from));
                                path.line_to(at(to));
                                if let Ok(path) = path.build() {
                                    window.paint_path(path, colour);
                                }
                            }
                            if let Some(t) = &tilted {
                                let mut path = PathBuilder::stroke(px(1.5));
                                path.move_to(at(t[0]));
                                for p in &t[1..] {
                                    path.line_to(at(*p));
                                }
                                path.close();
                                if let Ok(path) = path.build() {
                                    window.paint_path(path, tok::accent_primary());
                                }
                            }
                            for (width, c) in [(6.0, edge), (4.0, colour)] {
                                for (a, b, lit) in &crop_bars {
                                    let (width, c) = match (*lit, width > 5.0) {
                                        (true, true) => (8.0, edge),
                                        (true, false) => (6.0, tok::accent_primary()),
                                        _ => (width, c),
                                    };
                                    let mut path = PathBuilder::stroke(px(width));
                                    path.move_to(at(*a));
                                    path.line_to(at(*b));
                                    if let Ok(path) = path.build() {
                                        window.paint_path(path, c);
                                    }
                                }
                            }
                            if let Some((full, shape)) = &crop_paths {
                                // A hairline round the whole picture (its cut-away part is a ghost).
                                if let Some(full) = full {
                                    let mut line = PathBuilder::stroke(px(1.));
                                    line.move_to(at(full[0]));
                                    for p in &full[1..] {
                                        line.line_to(at(*p));
                                    }
                                    line.close();
                                    if let Ok(path) = line.build() {
                                        window.paint_path(path, tok::canvas_safe_area());
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
                    for (p, h) in handles {
                        stage = stage.child(dot(*p, 10., round_handles, Some(*h)));
                    }
                }
                if gizmo_on {
                    // The angles under the gizmo, in the rings' colours.
                    // Above the frame, clear of the 3D knob below it.
                    let (gx, _) = to_stage(geo::centre(&l));
                    let top = outline.iter().map(|p| p.1).fold(f32::MAX, f32::min);
                    stage = stage.child(
                        div()
                            .absolute()
                            .left(px(gx - 75.))
                            .top(px(top - 30.))
                            .w(px(150.))
                            .flex()
                            .justify_center()
                            .gap_2()
                            .px_2()
                            .py_0p5()
                            .rounded(px(5.))
                            .bg(tok::scrim())
                            .text_size(px(11.))
                            .font_family("Menlo")
                            .child(div().text_color(tok::gizmo_x()).child(format!("X {:.0}°", l.tilt_x)))
                            .child(div().text_color(tok::gizmo_y()).child(format!("Y {:.0}°", l.tilt_y)))
                            .child(div().text_color(tok::gizmo_z()).child(format!("Z {:.0}°", l.rotation))),
                    );
                }
                if let Some((_, (tx, ty))) = tilt_knob {
                    // A ring with a dot: "turn in space".
                    let k = if hot == Some(Handle::Tilt) { 18. } else { 14. };
                    stage = stage.child(
                        div()
                            .absolute()
                            .left(px(tx - k / 2.))
                            .top(px(ty - k / 2.))
                            .size(px(k))
                            .rounded_full()
                            .bg(if gizmo_on { tok::accent_primary() } else { tok::canvas_selection_edge() })
                            .border_2()
                            .border_color(tok::accent_primary())
                            .flex()
                            .items_center()
                            .justify_center()
                            .child(div().size(px(4.)).rounded_full().bg(tok::accent_primary())),
                    );
                }
                if let Some((_, k)) = knob {
                    stage = stage.child(dot(k, 10., true, Some(Handle::Rotate)));
                    // geo::outline goes round: top left, top right, bottom right, bottom left.
                    let corners = [Corner::TopLeft, Corner::TopRight, Corner::BottomRight, Corner::BottomLeft];
                    for (p, c) in outline.into_iter().zip(corners) {
                        stage = stage.child(dot(p, 8., false, Some(Handle::Corner(c))));
                    }
                }
                // What the drag is doing, by the pointer.
                if let (Some(d), Some((pf, p))) =
                    (self.drag.as_ref().filter(|d| d.layer == lid && d.moved), self.pointer)
                    && pf == fid
                {
                    let text = match d.mode {
                        DragMode::Move => format!("{:.0}, {:.0}", l.x, l.y),
                        DragMode::Resize(_) => format!("{:.0} × {:.0}", l.width, l.height),
                        DragMode::Rotate => format!("{:.0}°", l.rotation),
                        DragMode::Crop(_, (w, h)) | DragMode::CropEdge(_, (w, h)) | DragMode::CropPan((w, h)) => {
                            let c = l.crop;
                            format!(
                                "{:.0} × {:.0} px",
                                (w - (c.left + c.right) as f32).max(0.0),
                                (h - (c.top + c.bottom) as f32).max(0.0)
                            )
                        }
                        DragMode::Tilt => format!("X {:.0}°  Y {:.0}°", l.tilt_x, l.tilt_y),
                        DragMode::TiltAxis(geo::Axis::X) => format!("X {:.0}°", l.tilt_x),
                        DragMode::TiltAxis(_) => format!("Y {:.0}°", l.tilt_y),
                    };
                    let (px_, py_) = to_stage(p);
                    stage = stage.child(
                        div()
                            .absolute()
                            .left(px(px_ + 14.))
                            .top(px(py_ + 18.))
                            .px_1p5()
                            .py_0p5()
                            .rounded(px(5.))
                            .bg(tok::accent_primary())
                            .text_color(tok::canvas_selection())
                            .text_size(px(11.))
                            .font_family("Menlo")
                            .whitespace_nowrap()
                            .child(text),
                    );
                }
            }
            // A selected empty: its pivot, as a small crosshair.
            if let Some(pivot) = studio.selected_scene.and_then(|id| project.scene(id)).and_then(|scene| {
                let l = studio.selected_layer.and_then(|id| scene.layer(id)).filter(|l| l.is_empty())?;
                scene.world(l.id, fid)
            }) {
                let (px_, py_) = (lx + pivot.x * s, ly + pivot.y * s);
                for (w, h) in [(18.0, 2.0), (2.0, 18.0)] {
                    stage = stage.child(
                        div()
                            .absolute()
                            .left(px(px_ - w / 2.))
                            .top(px(py_ - h / 2.))
                            .w(px(w))
                            .h(px(h))
                            .bg(tok::accent_primary()),
                    );
                }
                stage = stage.child(
                    div()
                        .absolute()
                        .left(px(px_ - 5.))
                        .top(px(py_ - 5.))
                        .size(px(10.))
                        .rounded_full()
                        .border_2()
                        .border_color(tok::accent_primary()),
                );
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
        let well =
            div().flex().flex_1().min_h_0().child(
                div()
                    .id("canvas")
                    .relative()
                    .flex()
                    .flex_1()
                    .min_h_0()
                    .bg(tok::bg_canvas_well())
                    .when(live, |d| d.border_2().border_color(tok::live()))
                    .cursor(self.canvas_cursor(project, cx))
                    .on_mouse_down(MouseButton::Right, cx.listener(Self::canvas_right_down))
                    .on_mouse_down(
                        MouseButton::Middle,
                        cx.listener(|this, ev: &MouseDownEvent, _, _| this.panning = Some((ev.position, this.pan))),
                    )
                    .on_mouse_up(MouseButton::Middle, cx.listener(Self::canvas_up))
                    .on_scroll(cx.listener(Self::canvas_scroll))
                    .on_pinch(cx.listener(Self::canvas_pinch))
                    .overflow_hidden()
                    .on_mouse_down(MouseButton::Left, cx.listener(Self::canvas_down))
                    .on_mouse_move(cx.listener(Self::canvas_move))
                    .on_mouse_up(MouseButton::Left, cx.listener(Self::canvas_up))
                    .on_mouse_up_out(MouseButton::Left, cx.listener(Self::canvas_up))
                    .children(stages)
                    .drag_over::<assets::DraggedAsset>(|s, _, _, _| s.border_2().border_color(tok::accent_primary()))
                    .drag_over::<ExternalPaths>(|s, _, _, _| s.border_2().border_color(tok::accent_primary()))
                    .on_drop(cx.listener(|this, a: &assets::DraggedAsset, window, cx| {
                        this.drop_asset_on_canvas(&a.0, window, cx)
                    }))
                    .on_drop(cx.listener(|this, paths: &ExternalPaths, window, cx| {
                        this.drop_files_on_canvas(paths.paths(), window, cx)
                    }))
                    .context_menu({
                        let shell = cx.entity().downgrade();
                        move |m, window, cx| menus::layer_menu(m, &shell, window, cx)
                    }),
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
            .filter_map(|id| scene.layer(*id).filter(|l| !scene.locked(l.id)))
            .filter_map(|l| self.frame_of(scene, l, fid).map(|lo| (l.id, lo)))
            .collect();
        let align_button = |i: usize, icon: &'static str, tip: &'static str, how: geo::Align| {
            let layers = layers.clone();
            ui::icon_button(("align", i), icon, tok::text_secondary())
                .tooltip(move |w, cx| Tooltip::new(tip).build(w, cx))
                .on_click(cx.listener(move |this, _, _, cx| {
                    let layouts: Vec<Layout> = layers.iter().map(|(_, l)| *l).collect();
                    let aligned = geo::align(&layouts, how);
                    let moved = layers.iter().map(|(id, _)| *id).zip(aligned).collect();
                    this.set_layouts(sid, fid, moved, "Align objects", cx);
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
                    this.set_layouts(sid, fid, moved, "Distribute objects", cx);
                }))
        };
        let row = row.child(div().w(px(8.))).when(layers.len() >= 3, |r| {
            r.child(distribute_button(0, "align-horizontal-distribute-center", "Space evenly across", true))
                .child(distribute_button(1, "align-vertical-distribute-center", "Space evenly down", false))
        });
        body.child(div().font_weight(FontWeight::SEMIBOLD).child(format!("{} objects selected", selection.len())))
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
        let mut body = div().flex().flex_col().gap_3().px_3();
        if let (Some(sc), Some(l)) = (&scene, &layer)
            && selection.len() == 1
            && let Some(bar) = self.prefab_bar(sc.id, l, cx)
        {
            body = body.child(div().pt_2().child(bar));
        }
        let content = match (scene, layer) {
            (_, None) if track.is_some() => self.track_inspector(project, track.as_deref().unwrap(), body, cx),
            (Some(scene), Some(_)) if selection.len() > 1 => {
                self.multi_inspector(&scene, &format, &selection, body, cx)
            }
            (Some(scene), Some(layer)) if layer.is_empty() => self.empty_inspector(&scene, &layer, &format, body, cx),
            (Some(scene), Some(layer)) => self.layer_inspector(project, &scene, &layer, &format, body, cx),
            (Some(scene), None) => {
                let (sid, on_air) = (scene.id, project.program == Some(scene.id));
                let header = div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .py_2()
                    .child(ui::icon("layout-dashboard").size(px(16.)).text_color(tok::text_secondary()))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .overflow_hidden()
                            .text_ellipsis()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(scene.name.clone()),
                    )
                    .when(on_air, |d| {
                        d.child(
                            div()
                                .text_size(px(10.))
                                .font_weight(FontWeight::SEMIBOLD)
                                .text_color(tok::live())
                                .child("ON AIR"),
                        )
                    })
                    .child(
                        ui::icon_button("delete-scene", "trash", tok::text_secondary())
                            .tooltip(|w, cx| Tooltip::new("Delete scene").build(w, cx))
                            .on_click(
                                cx.listener(move |this, _, _, cx| this.exec(Command::RemoveScene { scene: sid }, cx)),
                            ),
                    );
                let info = div().text_size(px(12.)).text_color(tok::text_secondary()).child(format!(
                    "{} object{} · {}×{} @ {} fps",
                    scene.layers.len(),
                    if scene.layers.len() == 1 { "" } else { "s" },
                    format.width,
                    format.height,
                    format.fps_num / format.fps_den.max(1)
                ));
                // How scenes change on air (the same for every scene).
                let transition = self
                    .section("Transition", None, cx)
                    .child(
                        div()
                            .text_size(px(11.))
                            .text_color(tok::text_tertiary())
                            .child("How every scene change looks on air."),
                    )
                    .child(self.transition_picker(project, cx));
                body.child(header).child(info).child(transition).into_any_element()
            }
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
