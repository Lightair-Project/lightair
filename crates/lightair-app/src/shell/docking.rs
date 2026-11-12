//! The dock layout (docs/05-interface-shell.md, "Window anatomy"): every region is a gpui-kit
//! dock panel, so it can be resized, collapsed, tabbed with others or moved. `StudioWindow`
//! keeps the state and handlers; each region's panel renders through it.

use gpui_kit::component::Sizable;
use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::dock::{
    BasePanel, DockArea, DockLayout, DockPlacement, DockSkin, Panel, PanelEvent, panel_handle,
};
use gpui_kit::*;

use super::StudioWindow;
use crate::ui;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Region {
    Layers,
    Scenes,
    Canvas,
    Inspector,
}

pub(super) struct ShellPanel {
    shell: WeakEntity<StudioWindow>,
    region: Region,
    focus: FocusHandle,
}

impl ShellPanel {
    fn new(shell: Entity<StudioWindow>, region: Region, cx: &mut Context<Self>) -> Self {
        cx.observe(&shell, |_, _, cx| cx.notify()).detach();
        Self { shell: shell.downgrade(), region, focus: cx.focus_handle() }
    }
}

impl BasePanel for ShellPanel {
    fn panel_name(&self) -> &'static str {
        match self.region {
            Region::Layers => "layers",
            Region::Scenes => "scenes",
            Region::Canvas => "canvas",
            Region::Inspector => "inspector",
        }
    }
}

impl Panel for ShellPanel {
    fn tab_name(&self, _: &App) -> Option<SharedString> {
        Some(
            match self.region {
                Region::Layers => "Layers",
                Region::Scenes => "Scenes",
                Region::Canvas => "Canvas",
                Region::Inspector => "Inspector",
            }
            .into(),
        )
    }

    fn title(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.tab_name(cx).unwrap_or_default()
    }

    fn toolbar_buttons(&mut self, _: &mut Window, _: &mut Context<Self>) -> Option<Vec<Button>> {
        let shell = self.shell.clone();
        let button = move |id: &'static str, icon: &'static str, tip: &'static str| {
            Button::new(id).icon(ui::icon(icon)).ghost().xsmall().tooltip(tip)
        };
        match self.region {
            Region::Layers => Some(vec![button("add-layer", "plus", "Add layer").on_click(move |_, window, cx| {
                if let Some(s) = shell.upgrade() {
                    s.update(cx, |w, cx| w.toggle_add_menu(window, cx));
                }
            })]),
            Region::Scenes => {
                let import = shell.clone();
                Some(vec![
                    button("import-obs", "import", "Import OBS scenes…").on_click(move |_, window, cx| {
                        if let Some(s) = import.upgrade() {
                            s.update(cx, |w, cx| w.import_obs(window, cx));
                        }
                    }),
                    button("add-scene", "plus", "Add scene").on_click(move |_, _, cx| {
                        if let Some(s) = shell.upgrade() {
                            s.update(cx, |w, cx| w.add_scene(cx));
                        }
                    }),
                ])
            }
            Region::Canvas | Region::Inspector => None,
        }
    }

    fn zoom_control(&self, _: &App) -> Option<gpui_kit::component::dock::PanelControl> {
        None
    }

    fn inner_padding(&self, _: &App) -> bool {
        false
    }

    /// The canvas carries its own format bar.
    fn title_bar(&self, _: &App) -> bool {
        self.region != Region::Canvas
    }
}

impl EventEmitter<PanelEvent> for ShellPanel {}

impl Focusable for ShellPanel {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for ShellPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(shell) = self.shell.upgrade() else { return div().into_any_element() };
        if self.region == Region::Canvas {
            window.request_animation_frame();
        }
        let region = self.region;
        shell.update(cx, |w, cx| {
            let project = w.studio.read(cx).project().clone();
            match region {
                Region::Layers => w.layers_panel(&project, cx).into_any_element(),
                Region::Scenes => w.scenes_panel(&project, cx).into_any_element(),
                Region::Canvas => w.canvas(&project, cx).into_any_element(),
                Region::Inspector => w.inspector(&project, cx).into_any_element(),
            }
        })
    }
}

impl StudioWindow {
    /// Builds the dock on first render (panels need this window's entity).
    pub(super) fn ensure_dock(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.dock.is_some() {
            return;
        }
        let me = cx.entity();
        let panel = |region: Region, cx: &mut Context<Self>| cx.new(|cx| ShellPanel::new(me.clone(), region, cx));
        let (layers, scenes) = (panel(Region::Layers, cx), panel(Region::Scenes, cx));
        let (canvas, inspector) = (panel(Region::Canvas, cx), panel(Region::Inspector, cx));
        let (mixer, outputs) = (self.mixer.clone(), self.outputs.clone());
        let dock = cx.new(|cx| DockArea::new("lightair-studio", Some(1), window, cx).with_renderer(DockSkin::new(cx)));
        dock.update(cx, |area, cx| {
            area.set_center(
                DockLayout::v_split()
                    .child(DockLayout::tabs().panel_view(panel_handle(canvas), cx), None)
                    .child(DockLayout::tabs().panel_view(panel_handle(mixer), cx), Some(px(230.))),
                window,
                cx,
            );
            area.set_dock(
                DockPlacement::Left,
                DockLayout::v_split()
                    .child(DockLayout::tabs().panel_view(panel_handle(layers), cx), None)
                    .child(DockLayout::tabs().panel_view(panel_handle(scenes), cx), Some(px(300.))),
                window,
                cx,
            );
            area.set_dock(
                DockPlacement::Right,
                DockLayout::v_split()
                    .child(DockLayout::tabs().panel_view(panel_handle(outputs), cx), Some(px(220.)))
                    .child(DockLayout::tabs().panel_view(panel_handle(inspector), cx), None),
                window,
                cx,
            );
            area.set_dock_size(DockPlacement::Left, px(260.), window, cx);
            area.set_dock_size(DockPlacement::Right, px(320.), window, cx);
        });
        self.dock = Some(dock);
        // Development: open a dialog on launch for screenshots (no clicking needed).
        match std::env::var("LIGHTAIR_OPEN").as_deref() {
            Ok("add-layer") => self.open_add_layer(window, cx),
            Ok("settings") => self.toggle_settings(cx),
            Ok("palette") => self.open_palette(window, cx),
            _ => {}
        }
    }
}
