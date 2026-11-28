//! Floating dock windows (docs/05-interface-shell.md, "Windows & multi-display"): every window
//! is a dock. A panel opened from the Window menu, torn off by dragging its tab out of a
//! window, or sent to "Open in New Window" gets a window of its own holding a dock area, which
//! takes other panels like the main window's: tabbed together, split, dragged between windows.
//! Closing a floating window hides its panels (the Window menu brings them back). The canvas is
//! the exception: it always stays in the main window's dock (its own window is
//! shell/panel_window's).
//!
//! A drag doesn't leave the window it started in (the system sends the pointer's moves there
//! until the button is released), so each window's root routes it: past its edge, the moves
//! and the release are handed to the dock window under the pointer, whose tab groups then show
//! where it would land and take the drop. A release over no window tears the panel off into a
//! new one there.
//!
//! Where every panel is, and each floating window's frame, is kept per display setup with the
//! modes (shell/modes.rs) and comes back at launch.

use std::cell::RefCell;
use std::sync::Arc;

use gpui_kit::component::dock::{
    BasePanelView, DockArea, DockAreaState, DockEvent, DockLayout, DockPlacement, DockSkin, DragPanel, InsertTarget,
    PanelId,
};
use gpui_kit::component::menu::{PopupMenu, PopupMenuItem};
use gpui_kit::*;
use serde::{Deserialize, Serialize};

use super::StudioWindow;
use crate::ui::tok;

/// Every panel the Window menu lists, in its order: name (`BasePanel::panel_name`), title,
/// and the main window's region it docks into. The canvas isn't here: it never leaves.
pub(super) const PANELS: &[&[(&str, &str, DockPlacement)]] = &[
    &[
        ("scenes", "Scenes", DockPlacement::Left),
        ("layers", "Hierarchy", DockPlacement::Left),
        ("inspector", "Inspector", DockPlacement::Right),
        ("assets", "Assets", DockPlacement::Bottom),
    ],
    &[("mixer", "Audio Mixer", DockPlacement::Bottom), ("soundboard", "Soundboard", DockPlacement::Bottom)],
    &[
        ("outputs", "Outputs", DockPlacement::Right),
        ("chat", "Chat", DockPlacement::Right),
        ("activity", "Activity", DockPlacement::Right),
        ("stream", "Stream", DockPlacement::Right),
    ],
    &[
        ("rundown", "Rundown", DockPlacement::Bottom),
        ("notes", "Notes", DockPlacement::Bottom),
        ("automations", "Automations", DockPlacement::Bottom),
        ("ptz", "PTZ", DockPlacement::Bottom),
        ("packages", "Packages", DockPlacement::Bottom),
        ("plugin-panels", "Plugin panels", DockPlacement::Right),
    ],
];

fn panel_entry(name: &str) -> Option<(&'static str, &'static str, DockPlacement)> {
    PANELS.iter().flat_map(|g| g.iter()).find(|(n, ..)| *n == name).copied()
}

/// What a panel is called on screen.
pub(super) fn title(name: &str) -> &'static str {
    match name {
        "canvas" => "Canvas",
        _ => panel_entry(name).map(|(_, t, _)| t).unwrap_or("Panel"),
    }
}

/// Where a panel docks in the main window when it's sent back there.
fn home(name: &str) -> DockPlacement {
    panel_entry(name).map(|(.., p)| p).unwrap_or(DockPlacement::Right)
}

/// A floating window as it's saved: its frame (on its display) and its dock.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct FloatingLayout {
    /// x, y, width, height, in points, on its display.
    pub bounds: [f32; 4],
    #[serde(default)]
    pub display: Option<u64>,
    pub layout: DockAreaState,
}

/// Every dock as it's arranged: the main window's and the floating windows'.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct Arrangement {
    pub main: DockAreaState,
    #[serde(default)]
    pub floating: Vec<FloatingLayout>,
}

/// What a new floating window holds.
pub(super) enum Content {
    Panels(Vec<Arc<dyn BasePanelView>>),
    Saved(Box<DockAreaState>),
}

/// The root of a floating window: its dock area, on the panel background.
pub(super) struct DockWindow {
    me: WeakEntity<StudioWindow>,
    area: Entity<DockArea>,
    focus: FocusHandle,
    focused: bool,
    title: String,
}

impl Render for DockWindow {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if !self.focused {
            self.focused = true;
            window.focus(&self.focus, cx);
        }
        // titled after what it holds ("Chat", "Chat, Activity")
        let names: Vec<&str> = self.area.read(cx).panel_views().iter().map(|p| title(p.panel_name(cx))).collect();
        let title = if names.is_empty() { "Lightair".to_string() } else { names.join(", ") };
        if title != self.title {
            window.set_window_title(&title);
            self.title = title;
        }
        route_drags(
            div()
                .id("dock-window")
                .size_full()
                .flex()
                .flex_col()
                .bg(tok::bg_panel())
                .text_color(tok::text())
                .key_context("DockWindow")
                .track_focus(&self.focus)
                .on_action(cx.listener(|this, _: &super::app_menu::CloseWindow, window, cx| {
                    let handle = window.window_handle();
                    if let Some(me) = this.me.upgrade() {
                        me.update(cx, |s, cx| s.forget_dock_window(handle, cx));
                    }
                    window.remove_window();
                }))
                .child(div().flex_1().min_h_0().child(self.area.clone())),
            self.me.clone(),
        )
    }
}

impl Focusable for DockWindow {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

// --------------------------------------------------------------------- drags between windows

/// The drag in flight: the panel, and the window it's being handed to.
#[derive(Default)]
struct Crossing {
    drag: Option<DragPanel>,
    over: Option<AnyWindowHandle>,
}

thread_local! {
    static CROSSING: RefCell<Crossing> = RefCell::default();
    /// A move or release is being handed to another window: that window's own routing stays
    /// out of it (the pointer is over it, so it would take the drag back, clearing its drop).
    static HANDING: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    /// The main window and its view, for the app's own actions (the Window menu works
    /// whichever window is in front).
    static SHELL: RefCell<Option<(AnyWindowHandle, WeakEntity<StudioWindow>)>> = const { RefCell::new(None) };
}

/// A window's content area, in points on its display.
fn content_rect(window: &Window) -> Bounds<Pixels> {
    let b = window.bounds();
    let v = window.viewport_size();
    Bounds::new(point(b.origin.x, b.origin.y + (b.size.height - v.height).max(px(0.))), v)
}

fn display_of(window: &Window, cx: &App) -> Option<u64> {
    window.display(cx).map(|d| u64::from(d.id()))
}

/// Dispatches into another window with its routing held off (`HANDING`).
fn handing<R>(f: impl FnOnce() -> R) -> R {
    let was = HANDING.with(|h| h.replace(true));
    let out = f();
    HANDING.with(|h| h.set(was));
    out
}

fn handed() -> bool {
    HANDING.with(|h| h.get())
}

/// A pointer move with the button held, as if it happened in `handle` at `at`.
fn send_move(handle: AnyWindowHandle, at: Point<Pixels>, cx: &mut App) {
    let _ = handing(|| {
        handle.update(cx, |_, window, cx| {
            window.dispatch_event(
                PlatformInput::MouseMove(MouseMoveEvent {
                    position: at,
                    pressed_button: Some(MouseButton::Left),
                    modifiers: Modifiers::default(),
                }),
                cx,
            );
        })
    });
}

/// The dock window among `order` (front to back) whose content is under `screen` (a point on
/// `display`): its handle and where its content starts. While the pointer is still over
/// `source` (the window the drag started in), only a window in front of it counts: floating
/// windows sit over the main one.
fn dock_window_at(
    order: Vec<AnyWindowHandle>,
    screen: Point<Pixels>,
    source: AnyWindowHandle,
    inside: bool,
    display: Option<u64>,
    cx: &mut App,
) -> Option<(AnyWindowHandle, Point<Pixels>)> {
    let source_depth = order.iter().position(|h| *h == source).unwrap_or(usize::MAX);
    order.into_iter().enumerate().filter(|(depth, h)| *h != source && (!inside || *depth < source_depth)).find_map(
        |(_, h)| {
            let (rect, on) = h.update(cx, |_, w, cx| (content_rect(w), display_of(w, cx))).ok()?;
            (on == display && rect.contains(&screen)).then_some((h, rect.origin))
        },
    )
}

/// Far outside any window: tab groups that showed a drop clear it.
fn away() -> Point<Pixels> {
    point(px(-100_000.), px(-100_000.))
}

/// Routes a panel drag that leaves `el`'s window to the dock window under the pointer.
pub(super) fn route_drags<E: ParentElement + InteractiveElement>(el: E, me: WeakEntity<StudioWindow>) -> E {
    let on_move = me.clone();
    el.on_drag_move::<DragPanel>(move |ev, window, cx| {
        if handed() {
            return;
        }
        let drag = ev.drag(cx).clone();
        let at = ev.event.position;
        let inside = Bounds::new(Point::default(), window.viewport_size()).contains(&at);
        let screen = content_rect(window).origin + at;
        let candidates = on_move.upgrade().map(|s| s.read(cx).drop_windows()).unwrap_or_default();
        let target = dock_window_at(candidates, screen, window.window_handle(), inside, display_of(window, cx), cx)
            .map(|(h, origin)| (h, screen - origin));
        let before = CROSSING.with(|c| {
            let mut c = c.borrow_mut();
            c.drag = Some(drag);
            std::mem::replace(&mut c.over, target.map(|(h, _)| h))
        });
        cx.defer(move |cx| {
            if let Some(old) = before.filter(|old| Some(*old) != target.map(|(h, _)| h)) {
                send_move(old, away(), cx);
            }
            if let Some((h, local)) = target {
                send_move(h, local, cx);
            }
        });
    })
    .child(
        canvas(
            |_, _, _| {},
            move |_, _, window, _| {
                let me = me.clone();
                window.on_mouse_event(move |ev: &MouseUpEvent, phase, window, cx| {
                    if phase != DispatchPhase::Capture || ev.button != MouseButton::Left || handed() {
                        return;
                    }
                    let (drag, over) = CROSSING.with(|c| {
                        let mut c = c.borrow_mut();
                        (c.drag.take(), c.over.take())
                    });
                    let Some(drag) = drag.filter(|_| cx.has_active_drag()) else { return };
                    let inside = Bounds::new(Point::default(), window.viewport_size()).contains(&ev.position);
                    let screen = content_rect(window).origin + ev.position;
                    let display = display_of(window, cx);
                    let candidates = me.upgrade().map(|s| s.read(cx).drop_windows()).unwrap_or_default();
                    let target = dock_window_at(candidates, screen, window.window_handle(), inside, display, cx);
                    if target.is_none() && inside {
                        // let go in its own window: a drop there, as usual
                        if let Some(over) = over {
                            cx.defer(move |cx| send_move(over, away(), cx));
                        }
                        return;
                    }
                    window.refresh();
                    // not a drop in this window too
                    cx.stop_propagation();
                    match target {
                        // dropped on another window's dock: released there (the drag is still
                        // on, so its tab group takes it as a drop)
                        Some((h, origin)) => {
                            let at = screen - origin;
                            let _ = handing(|| {
                                h.update(cx, |_, w, cx| {
                                    w.dispatch_event(
                                        PlatformInput::MouseMove(MouseMoveEvent {
                                            position: at,
                                            pressed_button: Some(MouseButton::Left),
                                            modifiers: Modifiers::default(),
                                        }),
                                        cx,
                                    );
                                    w.dispatch_event(
                                        PlatformInput::MouseUp(MouseUpEvent {
                                            button: MouseButton::Left,
                                            position: at,
                                            modifiers: Modifiers::default(),
                                            click_count: 1,
                                        }),
                                        cx,
                                    );
                                })
                            });
                        }
                        // over no window: a window of its own, there
                        None => {
                            let (id, offset) = (drag.panel(), drag.drag_offset());
                            let me = me.clone();
                            cx.defer(move |cx| {
                                let _ = me.update(cx, |s, cx| s.float_panel(id, Some((screen - offset, display)), cx));
                            });
                        }
                    }
                });
            },
        )
        .absolute()
        .size_full(),
    )
}

// --------------------------------------------------------------------------- the app's actions

actions!(
    lightair_window,
    [
        WindowScenes,
        WindowHierarchy,
        WindowInspector,
        WindowAssets,
        WindowMixer,
        WindowSoundboard,
        WindowOutputs,
        WindowChat,
        WindowActivity,
        WindowStream,
        WindowRundown,
        WindowNotes,
        WindowAutomations,
        WindowPtz,
        WindowPackages,
        WindowPluginPanels,
        ResetLayout
    ]
);

/// The Window menu's item for a panel.
fn panel_item(name: &str, title: &str, checked: bool) -> MenuItem {
    let item = match name {
        "scenes" => MenuItem::action(title.to_string(), WindowScenes),
        "layers" => MenuItem::action(title.to_string(), WindowHierarchy),
        "inspector" => MenuItem::action(title.to_string(), WindowInspector),
        "assets" => MenuItem::action(title.to_string(), WindowAssets),
        "mixer" => MenuItem::action(title.to_string(), WindowMixer),
        "soundboard" => MenuItem::action(title.to_string(), WindowSoundboard),
        "outputs" => MenuItem::action(title.to_string(), WindowOutputs),
        "chat" => MenuItem::action(title.to_string(), WindowChat),
        "activity" => MenuItem::action(title.to_string(), WindowActivity),
        "stream" => MenuItem::action(title.to_string(), WindowStream),
        "rundown" => MenuItem::action(title.to_string(), WindowRundown),
        "notes" => MenuItem::action(title.to_string(), WindowNotes),
        "automations" => MenuItem::action(title.to_string(), WindowAutomations),
        "ptz" => MenuItem::action(title.to_string(), WindowPtz),
        "plugin-panels" => MenuItem::action(title.to_string(), WindowPluginPanels),
        _ => MenuItem::action(title.to_string(), WindowPackages),
    };
    item.checked(checked)
}

/// The Window menu's panel items, grouped, each checked when it's showing somewhere.
pub(super) fn panel_items(shown: &dyn Fn(&str) -> bool) -> Vec<MenuItem> {
    let mut items = Vec::new();
    for group in PANELS {
        items.push(MenuItem::separator());
        items.extend(group.iter().map(|(name, title, _)| panel_item(name, title, shown(name))));
    }
    items
}

/// The panel actions and Reset Layout work from any window (handled app-wide).
pub(super) fn bind(cx: &mut App) {
    macro_rules! show {
        ($action:ty, $name:expr) => {
            cx.on_action(|_: &$action, cx| with_shell(cx, |s, _, cx| s.show_panel($name, cx)));
        };
    }
    show!(WindowScenes, "scenes");
    show!(WindowHierarchy, "layers");
    show!(WindowInspector, "inspector");
    show!(WindowAssets, "assets");
    show!(WindowMixer, "mixer");
    show!(WindowSoundboard, "soundboard");
    show!(WindowOutputs, "outputs");
    show!(WindowChat, "chat");
    show!(WindowActivity, "activity");
    show!(WindowStream, "stream");
    show!(WindowRundown, "rundown");
    show!(WindowNotes, "notes");
    show!(WindowAutomations, "automations");
    show!(WindowPtz, "ptz");
    show!(WindowPackages, "packages");
    show!(WindowPluginPanels, "plugin-panels");
    cx.on_action(|_: &ResetLayout, cx| {
        let Some((main, me)) = SHELL.with(|s| s.borrow().clone()) else { return };
        cx.defer(move |cx| {
            let _ = main.update(cx, |_, window, cx| {
                let _ = me.update(cx, |s, cx| s.reset_layout(window, cx));
            });
        });
    });
    cx.bind_keys([KeyBinding::new(crate::ui::key("cmd-w"), super::app_menu::CloseWindow, Some("DockWindow"))]);
}

fn with_shell(cx: &mut App, f: impl FnOnce(&mut StudioWindow, AnyWindowHandle, &mut Context<StudioWindow>) + 'static) {
    let Some((main, me)) = SHELL.with(|s| s.borrow().clone()) else { return };
    let _ = me.update(cx, |s, cx| f(s, main, cx));
}

// ------------------------------------------------------------------------------- the shell

impl StudioWindow {
    /// Remembers the main window for the app-wide actions.
    pub(super) fn register_shell(&self, cx: &mut Context<Self>) {
        if let Some(main) = self.main_window {
            let me = cx.entity().downgrade();
            SHELL.with(|s| *s.borrow_mut() = Some((main, me)));
        }
    }

    /// The look and the tab menu every dock area shares.
    pub(super) fn dress_area(skin: &DockSkin, me: WeakEntity<Self>, cx: &mut App) {
        // Every group shows a tab bar, even with one panel, so any panel can be dropped onto
        // any header to join it as a tab (and the canvas gets a header like the rest).
        skin.set_panel_style(gpui_kit::component::dock::PanelStyle::TabBar, cx);
        // No "…" menu in panel headers: each panel's actions are its own buttons.
        skin.set_menu_button_visible(false, cx);
        skin.set_tab_menu(Self::tab_menu(me), cx);
    }

    /// Follows a dock area: panels dropped on it from other windows, and its changes (saved).
    pub(super) fn watch_area(&mut self, area: &Entity<DockArea>, cx: &mut Context<Self>) {
        cx.subscribe(area, |this, area, ev: &DockEvent, cx| match ev {
            DockEvent::ForeignPanelDropped { panel, target } => this.receive_panel(area, *panel, *target, cx),
            DockEvent::LayoutChanged => this.layout_changed(cx),
            _ => {}
        })
        .detach();
    }

    /// Every dock: the main window's, then each floating window's.
    fn docks(&self) -> Vec<(AnyWindowHandle, Entity<DockArea>)> {
        self.main_window.zip(self.dock.clone()).into_iter().chain(self.dock_windows.iter().cloned()).collect()
    }

    /// The dock holding a panel, and its window.
    fn holder(&self, id: PanelId, cx: &App) -> Option<(AnyWindowHandle, Entity<DockArea>)> {
        self.docks().into_iter().find(|(_, a)| a.read(cx).panel(id).is_some())
    }

    /// Whether a panel is showing somewhere (in the main window's dock or a floating window).
    pub(super) fn panel_shown(&self, name: &str, cx: &App) -> bool {
        self.docks().iter().any(|(_, a)| a.read(cx).panel_views().iter().any(|p| p.panel_name(cx) == name))
    }

    /// The floating window holding a panel, if it's in one.
    fn floating_holder(&self, id: PanelId, cx: &App) -> Option<(AnyWindowHandle, Entity<DockArea>)> {
        self.dock_windows.iter().find(|(_, a)| a.read(cx).panel(id).is_some()).cloned()
    }

    /// The windows a dragged panel can be dropped on, front to back: the floating ones (the
    /// one used last first; on macOS they float over the main window), then the main window
    /// while it shows its dock. (The system's own order leaves floating panels out.)
    fn drop_windows(&self) -> Vec<AnyWindowHandle> {
        let mut out: Vec<AnyWindowHandle> = self.dock_windows.iter().rev().map(|(h, _)| *h).collect();
        out.extend(self.main_window.filter(|_| self.place == super::Place::Live));
        out
    }

    /// A floating window came to the front: first in the drop order.
    fn raise_dock_window(&mut self, handle: AnyWindowHandle) {
        if let Some(ix) = self.dock_windows.iter().position(|(h, _)| *h == handle) {
            let entry = self.dock_windows.remove(ix);
            self.dock_windows.push(entry);
        }
    }

    /// A panel from another window's dock dropped on `area`: it moves there.
    fn receive_panel(&mut self, area: Entity<DockArea>, id: PanelId, target: InsertTarget, cx: &mut Context<Self>) {
        let Some((from_window, from)) = self.holder(id, cx) else { return };
        let Some(to_window) = self.docks().into_iter().find(|(_, a)| *a == area).map(|(h, _)| h) else { return };
        if from == area {
            return;
        }
        let Some(view) = from.read(cx).panel(id).cloned() else { return };
        // the canvas never leaves the main window's dock
        if view.panel_name(cx) == "canvas" {
            return;
        }
        let region = from.read(cx).region_of_panel(id).unwrap_or(DockPlacement::Center);
        let me = cx.entity().downgrade();
        cx.defer(move |cx| {
            let _ = from_window.update(cx, |_, w, cx| from.update(cx, |a, cx| a.remove_panel_view(id, w, cx)));
            let placed = to_window
                .update(cx, |_, w, cx| {
                    let placed = area.update(cx, |a, cx| {
                        let placed = a.insert_panel_view_at(view.clone(), target, w, cx);
                        if placed {
                            a.select_panel(id, w, cx);
                        }
                        placed
                    });
                    w.activate_window();
                    placed
                })
                .unwrap_or(false);
            // the drop's place went meanwhile: back where it was
            if !placed {
                let _ = from_window
                    .update(cx, |_, w, cx| from.update(cx, |a, cx| a.add_panel_view(view, region, None, w, cx)));
            }
            let _ = me.update(cx, |s, cx| s.after_move(cx));
        });
    }

    /// A panel into a floating window of its own: at `at` (a point on a display, where a
    /// dragged tab was let go) or beside the main window. The canvas gets its own window
    /// (shell/panel_window) instead.
    pub(super) fn float_panel(
        &mut self,
        id: PanelId,
        at: Option<(Point<Pixels>, Option<u64>)>,
        cx: &mut Context<Self>,
    ) {
        let Some((window, area)) = self.holder(id, cx) else { return };
        let Some(view) = area.read(cx).panel(id).cloned() else { return };
        let name = view.panel_name(cx);
        if name == "canvas" {
            let (Some(main), me) = (self.main_window, cx.entity().downgrade()) else { return };
            cx.defer(move |cx| {
                let _ = main.update(cx, |_, w, cx| {
                    let _ = me.update(cx, |s, cx| s.pop_out(view, w, cx));
                });
            });
            return;
        }
        let display = at.and_then(|(_, d)| d);
        // (all of it on its display: a tab let go near an edge)
        let bounds = self.on_display(self.float_bounds(name, at.map(|(p, _)| p)), display, cx);
        let me = cx.entity().downgrade();
        cx.defer(move |cx| {
            let _ = window.update(cx, |_, w, cx| area.update(cx, |a, cx| a.remove_panel_view(id, w, cx)));
            let _ = me.update(cx, |s, cx| s.open_dock_window(Content::Panels(vec![view]), bounds, display, cx));
        });
    }

    /// `bounds` moved (and shrunk if need be) to lie wholly on `display` (or the main
    /// window's), under the menu bar.
    fn on_display(&self, mut bounds: Bounds<Pixels>, display: Option<u64>, cx: &App) -> Bounds<Pixels> {
        let screen = cx
            .displays()
            .into_iter()
            .find(|d| Some(u64::from(d.id())) == display.or(self.main_display))
            .map(|d| d.bounds().size);
        if let Some(screen) = screen {
            let top = px(32.);
            bounds.size = bounds.size.min(&size(screen.width, screen.height - top));
            bounds.origin.x = bounds.origin.x.clamp(px(0.), screen.width - bounds.size.width);
            bounds.origin.y = bounds.origin.y.clamp(top, (screen.height - bounds.size.height).max(top));
        }
        bounds
    }

    /// A new floating window's frame: where a tab was let go, or beside the main window, a
    /// little further along for each one already open.
    fn float_bounds(&self, name: &str, at: Option<Point<Pixels>>) -> Bounds<Pixels> {
        let main = self.main_bounds;
        let size = match name {
            "mixer" => size(px(760.), px(340.)),
            "rundown" | "automations" | "assets" | "packages" => size(px(520.), px(560.)),
            _ => size(px(380.), main.size.height.min(px(640.)).max(px(360.))),
        };
        let origin = match at {
            // the tab stays under the pointer
            Some(p) => p - point(px(0.), px(36.)),
            None => {
                let step = px(28.) * self.dock_windows.len() as f32;
                point(main.right() - size.width - px(48.) - step, main.top() + px(96.) + step)
            }
        };
        Bounds::new(origin, size)
    }

    /// Opens a floating window holding `content` at `bounds` (on `display`, or the main
    /// window's).
    pub(super) fn open_dock_window(
        &mut self,
        content: Content,
        bounds: Bounds<Pixels>,
        display: Option<u64>,
        cx: &mut Context<Self>,
    ) {
        let me = cx.entity().downgrade();
        let display_id = display.map(DisplayId::from).or_else(|| self.main_display.map(DisplayId::from));
        let options = WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            titlebar: Some(TitlebarOptions { title: Some("Lightair".into()), ..Default::default() }),
            window_min_size: Some(size(px(240.), px(160.))),
            display_id,
            focus: true,
            // macOS: a utility panel, kept above the main window and hidden while Lightair
            // isn't the app in front
            kind: if cfg!(target_os = "macos") { WindowKind::Floating } else { WindowKind::Normal },
            ..Default::default()
        };
        // opened once this update is over: a panel drawn through this window's view (the
        // Inspector) can't draw while it's being updated
        cx.defer(move |cx| {
            let mut built = None;
            let opened = gpui_kit::open_window(options, cx, |window, cx| {
                let (area, skin) = DockSkin::dock_area("lightair-floating", Some(1), window, cx);
                StudioWindow::dress_area(&skin, me.clone(), cx);
                area.update(cx, |a, cx| {
                    // its last panel can leave too (the window then closes)
                    a.set_emptiable(true, window, cx);
                    match content {
                        Content::Panels(views) => {
                            let mut tabs = DockLayout::tabs();
                            for v in views {
                                tabs = tabs.panel_view(v, cx);
                            }
                            a.set_center(tabs, window, cx);
                        }
                        Content::Saved(state) => {
                            if let Err(e) = a.load(*state, window, cx) {
                                tracing::warn!(%e, "a floating window's layout didn't load");
                            }
                        }
                    }
                });
                // closing it hides what it holds (the Window menu brings each back)
                let closing = me.clone();
                window.on_window_should_close(cx, move |window, cx| {
                    let handle = window.window_handle();
                    let _ = closing.update(cx, |s, cx| s.forget_dock_window(handle, cx));
                    true
                });
                built = Some(area.clone());
                let me = me.clone();
                cx.new(|cx| {
                    // its frame is saved as it moves and resizes
                    let saving = me.clone();
                    cx.observe_window_bounds(window, move |_, _, cx| {
                        let _ = saving.update(cx, |s, cx| s.layout_changed(cx));
                    })
                    .detach();
                    // the one in front takes a drop where floating windows overlap
                    let raising = me.clone();
                    cx.observe_window_activation(window, move |_, window, cx| {
                        if window.is_window_active() {
                            let handle = window.window_handle();
                            let _ = raising.update(cx, |s, _| s.raise_dock_window(handle));
                        }
                    })
                    .detach();
                    DockWindow { me, area, focus: cx.focus_handle(), focused: false, title: String::new() }
                })
            });
            let _ = me.update(cx, |s, cx| match (opened, built) {
                (Ok((handle, _)), Some(area)) => {
                    s.watch_area(&area, cx);
                    // a panel shows in one place only (a saved layout naming one twice keeps
                    // the first)
                    let held: Vec<PanelId> =
                        s.docks().iter().flat_map(|(_, a)| a.read(cx).panel_views()).map(|p| p.panel_id(cx)).collect();
                    let twice: Vec<PanelId> = area
                        .read(cx)
                        .panel_views()
                        .iter()
                        .map(|p| p.panel_id(cx))
                        .filter(|id| held.contains(id))
                        .collect();
                    if !twice.is_empty() {
                        let area = area.clone();
                        let _ = handle.update(cx, |_, w, cx| {
                            area.update(cx, |a, cx| twice.iter().for_each(|id| a.remove_panel_view(*id, w, cx)));
                        });
                    }
                    // Windows: owned by the main window, so it stays in front of it (as a macOS
                    // utility panel does) without sitting over other apps
                    #[cfg(windows)]
                    if let Some(main) = s.main_window {
                        own_by(cx, main, handle);
                    }
                    s.dock_windows.push((handle, area));
                    s.after_move(cx);
                }
                (opened, _) => {
                    if let Err(e) = opened {
                        tracing::warn!(%e, "floating window");
                    }
                    s.layout_changed(cx);
                }
            });
        });
    }

    /// A floating window closed: what it held is hidden now.
    pub(super) fn forget_dock_window(&mut self, handle: AnyWindowHandle, cx: &mut Context<Self>) {
        self.dock_windows.retain(|(h, _)| *h != handle);
        self.after_move(cx);
    }

    /// Closes every floating window (their panels hidden), for a layout that replaces them.
    pub(super) fn close_dock_windows(&mut self, cx: &mut Context<Self>) {
        for (handle, _) in std::mem::take(&mut self.dock_windows) {
            cx.defer(move |cx| {
                let _ = handle.update(cx, |_, w, _| w.remove_window());
            });
        }
    }

    /// After panels moved between windows: empty floating windows close, the mixer knows
    /// whether it's in a window of its own, and the layout is saved.
    fn after_move(&mut self, cx: &mut Context<Self>) {
        let empty: Vec<AnyWindowHandle> =
            self.dock_windows.iter().filter(|(_, a)| a.read(cx).panel_views().is_empty()).map(|(h, _)| *h).collect();
        if !empty.is_empty() {
            self.dock_windows.retain(|(h, _)| !empty.contains(h));
            for handle in empty {
                cx.defer(move |cx| {
                    let _ = handle.update(cx, |_, w, _| w.remove_window());
                });
            }
        }
        let mixer_out =
            self.dock_windows.iter().any(|(_, a)| a.read(cx).panel_views().iter().any(|p| p.panel_name(cx) == "mixer"));
        if mixer_out != self.mixer_floating {
            self.mixer_floating = mixer_out;
            self.mixer_dock.update(cx, |m, cx| m.set_own_window(mixer_out, cx));
        }
        self.layout_changed(cx);
    }

    /// A dock changed: the Window menu's checks follow, and the arrangement is saved a moment
    /// later (one write for a burst of changes).
    pub(super) fn layout_changed(&mut self, cx: &mut Context<Self>) {
        if self.dock.is_none() || std::env::var_os("LIGHTAIR_OPEN").is_some() {
            return;
        }
        self.layout_save = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(std::time::Duration::from_millis(500)).await;
            let _ = this.update(cx, |s, cx| {
                // a floating window emptied by a tab closed in it closes too
                let empty = s.dock_windows.iter().any(|(_, a)| a.read(cx).panel_views().is_empty());
                if empty {
                    s.after_move(cx);
                    return;
                }
                s.save_arrangement(cx);
                s.refresh_window_menu(cx);
            });
        }));
    }

    /// The Window menu with a check beside each panel that's showing.
    pub(super) fn refresh_window_menu(&self, cx: &mut App) {
        let shown: Vec<&str> =
            PANELS.iter().flat_map(|g| g.iter()).map(|(n, ..)| *n).filter(|n| self.panel_shown(n, cx)).collect();
        cx.set_menus(super::app_menu::menus(&|n| shown.contains(&n)));
    }

    /// The floating windows put away (the canvas full screen, which they'd float over),
    /// their layouts kept.
    pub(super) fn stash_floating(&mut self, cx: &mut Context<Self>) {
        if self.floating_stash.is_some() || self.dock_windows.is_empty() {
            return;
        }
        let floating = self.arrangement(cx).map(|a| a.floating).unwrap_or_default();
        self.floating_stash = Some(floating);
        self.close_dock_windows(cx);
    }

    /// The floating windows put away by `stash_floating`, back.
    pub(super) fn unstash_floating(&mut self, cx: &mut Context<Self>) {
        for f in self.floating_stash.take().unwrap_or_default() {
            let [x, y, w, h] = f.bounds;
            let bounds = self.on_display(Bounds::new(point(px(x), px(y)), size(px(w), px(h))), f.display, cx);
            self.open_dock_window(Content::Saved(Box::new(f.layout)), bounds, f.display, cx);
        }
    }

    /// Every dock as it's arranged now.
    pub(super) fn arrangement(&self, cx: &mut App) -> Option<Arrangement> {
        let main = self.dock.as_ref()?.read(cx).dump(cx);
        let floating: Vec<FloatingLayout> = self
            .dock_windows
            .iter()
            .filter_map(|(h, area)| {
                let (b, display) = h.update(cx, |_, w, cx| (w.window_bounds().get_bounds(), display_of(w, cx))).ok()?;
                let layout = area.read(cx).dump(cx);
                Some(FloatingLayout {
                    bounds: [b.origin.x.into(), b.origin.y.into(), b.size.width.into(), b.size.height.into()],
                    display,
                    layout,
                })
            })
            .collect();
        // (put away while the canvas is full screen: still theirs)
        let floating = floating.into_iter().chain(self.floating_stash.iter().flatten().cloned()).collect();
        Some(Arrangement { main, floating })
    }

    /// Puts every dock as `arrangement` has it: the main window's, and floating windows.
    pub(super) fn apply_arrangement(
        &mut self,
        arrangement: Arrangement,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(dock) = self.dock.clone() else { return false };
        self.floating_stash = None;
        self.close_dock_windows(cx);
        if let Err(e) = dock.update(cx, |a, cx| a.load(arrangement.main, window, cx)) {
            tracing::warn!(%e, "the saved layout didn't load");
            return false;
        }
        // (a layout without the canvas isn't one this window can use)
        if !self.panel_shown("canvas", cx) {
            return false;
        }
        let displays: Vec<u64> = cx.displays().iter().map(|d| u64::from(d.id())).collect();
        for f in arrangement.floating {
            let [x, y, w, h] = f.bounds;
            let display = f.display.filter(|d| displays.contains(d));
            let bounds =
                self.on_display(Bounds::new(point(px(x), px(y)), size(px(w.max(240.)), px(h.max(160.)))), display, cx);
            self.open_dock_window(Content::Saved(Box::new(f.layout)), bounds, display, cx);
        }
        self.after_move(cx);
        true
    }

    /// Window › a panel: brings it forward where it is, or opens it in a floating window.
    pub(super) fn show_panel(&mut self, name: &str, cx: &mut Context<Self>) {
        let Some(view) = self.dock_panels.as_ref().and_then(|p| p.by_name(name, cx)) else { return };
        let id = view.panel_id(cx);
        match self.holder(id, cx) {
            Some((handle, area)) => {
                if Some(handle) == self.main_window && self.place != super::Place::Live {
                    self.show_place(super::Place::Live, cx);
                }
                cx.defer(move |cx| {
                    let _ = handle.update(cx, |_, w, cx| {
                        area.update(cx, |a, cx| {
                            if let Some(region) = a.region_of_panel(id).filter(|r| *r != DockPlacement::Center)
                                && !a.is_dock_open(region)
                            {
                                a.toggle_dock(region, w, cx);
                            }
                            a.select_panel(id, w, cx);
                        });
                        w.activate_window();
                    });
                });
            }
            None => {
                let bounds = self.float_bounds(name, None);
                self.open_dock_window(Content::Panels(vec![view]), bounds, None, cx);
            }
        }
    }

    /// A panel back in the main window's dock, in the region it belongs to.
    fn dock_home(&mut self, id: PanelId, cx: &mut Context<Self>) {
        let (Some((from_window, from)), Some(main), Some(dock)) =
            (self.floating_holder(id, cx), self.main_window, self.dock.clone())
        else {
            return;
        };
        let Some(view) = from.read(cx).panel(id).cloned() else { return };
        let region = home(view.panel_name(cx));
        if self.place != super::Place::Live {
            self.show_place(super::Place::Live, cx);
        }
        let me = cx.entity().downgrade();
        cx.defer(move |cx| {
            let _ = from_window.update(cx, |_, w, cx| from.update(cx, |a, cx| a.remove_panel_view(id, w, cx)));
            let _ = main.update(cx, |_, w, cx| {
                dock.update(cx, |a, cx| {
                    a.add_panel_view(view, region, None, w, cx);
                    if !a.is_dock_open(region) {
                        a.toggle_dock(region, w, cx);
                    }
                    a.select_panel(id, w, cx);
                });
                w.activate_window();
            });
            let _ = me.update(cx, |s, cx| s.after_move(cx));
        });
    }

    /// A panel hidden (its tab's Close): the Window menu brings it back.
    fn close_panel(&mut self, id: PanelId, cx: &mut Context<Self>) {
        let Some((window, area)) = self.holder(id, cx) else { return };
        let me = cx.entity().downgrade();
        cx.defer(move |cx| {
            let _ = window.update(cx, |_, w, cx| area.update(cx, |a, cx| a.remove_panel_view(id, w, cx)));
            let _ = me.update(cx, |s, cx| s.after_move(cx));
        });
    }

    /// Window › Reset Layout: the default layout, no floating windows.
    pub(super) fn reset_layout(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.switch_mode(super::modes::Preset::Edit.name(), window, cx);
        self.studio.update(cx, |s, _| s.issue = Some("Layout reset".into()));
    }

    /// A tab's right-click menu: a window of its own, back to the main window, or closed.
    pub(super) fn tab_menu(me: WeakEntity<Self>) -> gpui_kit::component::dock::TabMenu {
        std::rc::Rc::new(move |menu: PopupMenu, panel: Arc<dyn BasePanelView>, _: &mut Window, cx: &mut App| {
            let Some(shell) = me.upgrade() else { return menu };
            let id = panel.panel_id(cx);
            let name = panel.panel_name(cx);
            let floating = shell.read(cx).floating_holder(id, cx);
            // alone in a floating window: it has a window of its own already
            let alone = floating.as_ref().is_some_and(|(_, a)| a.read(cx).panel_views().len() == 1);
            let mut menu = menu;
            if !alone {
                let me = me.clone();
                menu = menu.item(PopupMenuItem::new("Open in New Window").on_click(move |_, _, cx| {
                    let _ = me.update(cx, |s, cx| s.float_panel(id, None, cx));
                }));
            }
            if floating.is_some() {
                let me = me.clone();
                menu = menu.item(PopupMenuItem::new("Dock in Main Window").on_click(move |_, _, cx| {
                    let _ = me.update(cx, |s, cx| s.dock_home(id, cx));
                }));
            }
            if name != "canvas" {
                let me = me.clone();
                menu = menu.item(PopupMenuItem::new("Close Tab").on_click(move |_, _, cx| {
                    let _ = me.update(cx, |s, cx| s.close_panel(id, cx));
                }));
            }
            menu
        })
    }
}

/// Makes `owner` the owner of window `owned` (Windows): an owned window is always in front of its
/// owner, minimises with it, and isn't over other apps' windows.
#[cfg(windows)]
fn own_by(cx: &mut App, owner: AnyWindowHandle, owned: AnyWindowHandle) {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    use windows_sys::Win32::UI::WindowsAndMessaging::{GWLP_HWNDPARENT, SetWindowLongPtrW};
    let hwnd = |cx: &mut App, h: AnyWindowHandle| {
        h.update(cx, |_, w, _| match HasWindowHandle::window_handle(w).map(|h| h.as_raw()) {
            Ok(RawWindowHandle::Win32(h)) => Some(h.hwnd.get()),
            _ => None,
        })
        .ok()
        .flatten()
    };
    if let (Some(owner), Some(owned)) = (hwnd(cx, owner), hwnd(cx, owned)) {
        // SAFETY: both are this process's live top-level windows.
        unsafe { SetWindowLongPtrW(owned as _, GWLP_HWNDPARENT, owner) };
    }
}

// ------------------------------------------------------------------------------- check hook

impl StudioWindow {
    /// The "dock-windows" check hook: steps from LIGHTAIR_DOCK_STEPS, separated by ";", run in
    /// order, then every dock logged: "show:chat" (Window › Chat), "drag:W:x0,y0>x1,y1" (a tab
    /// dragged in window W, 0 the main one and 1… the floating ones in the order they opened,
    /// from and to in that window's points, the end maybe past its edge), "close:W" (a floating
    /// window closed), "reset", "save", "log".
    pub(super) fn start_dock_check(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let steps = std::env::var("LIGHTAIR_DOCK_STEPS").unwrap_or_default();
        cx.spawn_in(window, async move |this, cx| {
            let executor = cx.background_executor().clone();
            let wait = |ms| executor.timer(std::time::Duration::from_millis(ms));
            wait(5000).await;
            for step in steps.split(';').map(str::trim).filter(|s| !s.is_empty()) {
                tracing::info!(step, "CHECK dock step");
                let (verb, arg) = step.split_once(':').unwrap_or((step, ""));
                match verb {
                    "show" => {
                        let name = arg.to_string();
                        let _ = this.update(cx, |s, cx| s.show_panel(&name, cx));
                    }
                    "close" => {
                        let ix: usize = arg.parse().unwrap_or(1);
                        let handle = this
                            .update(cx, |s, _| s.dock_windows.get(ix.saturating_sub(1)).map(|(h, _)| *h))
                            .ok()
                            .flatten();
                        if let Some(handle) = handle {
                            // as the close button does
                            let _ = this.update(cx, |s, cx| s.forget_dock_window(handle, cx));
                            let _ = handle.update(cx, |_, w, _| w.remove_window());
                        }
                    }
                    "reset" => {
                        let _ = this.update_in(cx, |s, window, cx| s.reset_layout(window, cx));
                    }
                    "save" => {
                        let _ = this.update(cx, |s, cx| s.save_arrangement(cx));
                    }
                    "drag" => {
                        let Some((w, path)) = arg.split_once(':') else { continue };
                        let ix: usize = w.parse().unwrap_or(0);
                        let nums: Vec<f32> = path.split([',', '>']).filter_map(|n| n.parse().ok()).collect();
                        let [x0, y0, x1, y1] = nums[..] else { continue };
                        let handle = this
                            .update(cx, |s, _| {
                                if ix == 0 { s.main_window } else { s.dock_windows.get(ix - 1).map(|(h, _)| *h) }
                            })
                            .ok()
                            .flatten();
                        let Some(handle) = handle else { continue };
                        // in front, as when it's done by hand (floating panels hide while the
                        // app isn't, and a window that isn't drawn takes no clicks)
                        let _ = cx.update(|_, cx| cx.activate(true));
                        // (a click there makes it the key window)
                        let _ = handle.update(cx, |_, w, _| w.activate_window());
                        wait(800).await;
                        let at = |x: f32, y: f32| point(px(x), px(y));
                        let send = |cx: &mut AsyncWindowContext, e: PlatformInput| {
                            let _ = handle.update(cx, |_, w, cx| w.dispatch_event(e, cx));
                        };
                        let left = Some(MouseButton::Left);
                        send(
                            cx,
                            PlatformInput::MouseMove(MouseMoveEvent {
                                position: at(x0, y0),
                                pressed_button: None,
                                modifiers: Modifiers::default(),
                            }),
                        );
                        send(
                            cx,
                            PlatformInput::MouseDown(MouseDownEvent {
                                button: MouseButton::Left,
                                position: at(x0, y0),
                                modifiers: Modifiers::default(),
                                click_count: 1,
                                first_mouse: false,
                            }),
                        );
                        for i in 1..=24 {
                            let t = i as f32 / 24.;
                            send(
                                cx,
                                PlatformInput::MouseMove(MouseMoveEvent {
                                    position: at(x0 + (x1 - x0) * t, y0 + (y1 - y0) * t),
                                    pressed_button: left,
                                    modifiers: Modifiers::default(),
                                }),
                            );
                            wait(30).await;
                        }
                        wait(1500).await;
                        send(
                            cx,
                            PlatformInput::MouseUp(MouseUpEvent {
                                button: MouseButton::Left,
                                position: at(x1, y1),
                                modifiers: Modifiers::default(),
                                click_count: 1,
                            }),
                        );
                    }
                    _ => {}
                }
                wait(2500).await;
                let _ = this.update(cx, |s, cx| s.log_docks(cx));
            }
        })
        .detach();
    }

    /// Every dock's panels and its window's content area (a check's log).
    fn log_docks(&self, cx: &mut App) {
        for (ix, (handle, area)) in self.docks().into_iter().enumerate() {
            let rect = handle.update(cx, |_, w, _| content_rect(w)).ok();
            let a = area.read(cx);
            let regions: Vec<String> =
                [DockPlacement::Center, DockPlacement::Left, DockPlacement::Right, DockPlacement::Bottom]
                    .into_iter()
                    .filter_map(|p| {
                        let tree = a.layout(p)?;
                        let names: Vec<&str> =
                            tree.panels().filter_map(|id| a.panel(id)).map(|v| v.panel_name(cx)).collect();
                        (!names.is_empty()).then(|| {
                            format!(
                                "{p:?}={}{}",
                                names.join(","),
                                if p != DockPlacement::Center && !a.is_dock_open(p) { "(closed)" } else { "" }
                            )
                        })
                    })
                    .collect();
            tracing::info!(window = ix, rect = ?rect, docks = %regions.join(" "), "CHECK docks");
        }
    }
}

#[cfg(test)]
mod tests {
    use gpui_kit::MenuItem;

    use super::PANELS;

    /// The Window menu's panel items: (title, checked).
    fn window_items(shown: &dyn Fn(&str) -> bool) -> Vec<(String, bool)> {
        let menus = super::super::app_menu::menus(shown);
        let window = menus.into_iter().find(|m| m.name == "Window").expect("a Window menu");
        let titles: Vec<&str> = PANELS.iter().flat_map(|g| g.iter()).map(|(_, t, _)| *t).collect();
        window
            .items
            .into_iter()
            .filter_map(|item| match item {
                MenuItem::Action { name, checked, .. } if titles.contains(&name.as_ref()) => {
                    Some((name.to_string(), checked))
                }
                _ => None,
            })
            .collect()
    }

    /// The Window menu lists every panel once and checks exactly the ones showing somewhere,
    /// so a panel closed or hidden in a floating window can always be found and brought back.
    #[test]
    fn the_window_menu_checks_the_panels_showing_and_only_those() {
        let items = window_items(&|name| name == "mixer" || name == "chat");
        let every: usize = PANELS.iter().map(|g| g.len()).sum();
        assert_eq!(items.len(), every, "every panel is listed once: {items:?}");
        let checked: Vec<&str> = items.iter().filter(|(_, c)| *c).map(|(t, _)| t.as_str()).collect();
        assert_eq!(checked, ["Audio Mixer", "Chat"], "{items:?}");

        let none = window_items(&|_| false);
        assert!(none.iter().all(|(_, c)| !c), "nothing showing, nothing checked: {none:?}");
    }
}
