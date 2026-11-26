# gpui-base 0.9.1, patched for Lightair

A copy of the published crate with these changes, each marked "Lightair patch":

- **Docking beside a whole column or row.** A thin band along each edge of a tab group
  (`dock::drag::outer_edge_at`: 12% of the side, at most 28 px) drops the panel beside the
  outermost split that edge belongs to, not beside the group alone: drag a tab to the outer
  edge of a stacked sidebar and it becomes a second sidebar, full height. The container tells
  each group which split each of its edges belongs to (`dock_area.rs`, `plan_node`;
  `TabGroup::set_outer`), the group resolves the drop and its placeholder against that
  split's laid-out bounds (`ResizableState::bounds`), and `DropIndicator::outer` carries the
  target (`drag.rs`, `tab_group.rs`, `resizable/mod.rs`). A column beside a whole side dock
  widens the dock by one rather than halving what's there (`DockArea::move_panel`).
- **A panel taken out by id, and where a panel is.** `DockArea::remove_panel_view` and
  `DockArea::region_of_panel`, for Lightair's panels in windows of their own (the host holds
  the panel while it's out, and docks it again when its window closes).
- **Panels moved between areas.** A drop of a panel the area doesn't hold (dragged from another
  window's area) is reported as `DockEvent::ForeignPanelDropped` instead of ignored, and
  `DockArea::insert_panel_view_at` puts a panel at that drop's target; `DockArea::panel_views`
  lists what an area holds. `DockArea::set_emptiable` lets the center's last panel be dragged
  out or closed (an area whose window closes once it's empty). Lightair's floating dock windows
  use them.

The workspace uses it through `[patch.crates-io]` in the root `Cargo.toml`. Drop it once
upstream docks beside a whole split and exposes these.
