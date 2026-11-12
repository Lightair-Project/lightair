# gpui-component 0.9.1, patched for Lightair

A copy of the published crate (without its tests) with one change: `DockSkin::set_menu_button_visible`
hides the "…" panel menu that tab bars otherwise always draw (`src/dock/mod.rs`,
`src/dock/tab_panel.rs`, marked "Lightair patch"). The workspace uses it through
`[patch.crates-io]` in the root `Cargo.toml`. Drop it once upstream offers the switch.
