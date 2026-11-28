//! The Packages panel: the packages installed on this computer (Installed), with the catalog
//! and updates to come; installing packages from files (Install…, or dropped on the window),
//! each after a plain prompt saying what it adds, its size and where it's from; using what a
//! package holds (widgets, scenes, media, sounds, LUTs, automations); and the open show's
//! packages, with a notice when some aren't installed.

use std::collections::{BTreeSet, HashSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use gpui_kit::component::Sizable;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::dock::PanelEvent;
use gpui_kit::component::input::{InputEvent, InputState};
use gpui_kit::component::menu::{ContextMenuExt, PopupMenuItem};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;
use lightair_model::{Command, Effect};
use lightair_packages::{Conflict, Installed, Mismatch, Missing, MissingReason, PackageId};
use semver::Version;
use serde_json::{Value, json};

use super::StudioWindow;
use super::assets::{Asset, AssetKind, DraggedAsset, Placement};
use crate::packages::{self as pk, Item, Use};

pub(crate) mod catalog_ui;
mod helper_ui;
use crate::studio::Studio;
use crate::ui::{self, tok};

/// The window's packages state: the Export sheet while it's open, and the package files a drop
/// just installed (so the window's own drop handler doesn't install them again).
#[derive(Default)]
pub(super) struct PackagesUi {
    pub(super) export: Option<super::package_export::ExportSheet>,
    last_drop: Option<(Vec<PathBuf>, Instant)>,
    /// The full-access question while it's open (catalog_ui.rs).
    pub(super) full_access: Option<catalog_ui::FullAccessAsk>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tab {
    Installed,
    Catalog,
    Updates,
}

/// The open show's packages, as last checked.
#[derive(Default)]
struct ShowCheck {
    dir: Option<PathBuf>,
    generation: u64,
    /// What its `packages.json` lists.
    needed: BTreeSet<PackageId>,
    missing: Vec<Missing>,
    mismatched: Vec<Mismatch>,
    conflicts: Vec<Conflict>,
}

pub(crate) struct PackagesPanel {
    shell: WeakEntity<StudioWindow>,
    studio: Entity<Studio>,
    focus: FocusHandle,
    tab: Tab,
    /// Packages whose contents are shown.
    open: HashSet<PackageId>,
    show: ShowCheck,
    /// Packages something in the show comes from, and when that was last looked at (at most
    /// once a second, while the panel is drawn).
    used: (Option<Instant>, BTreeSet<PackageId>),
    /// The Catalog tab's search.
    search: Entity<InputState>,
    /// A community catalog's address, to add.
    catalog_url: Entity<InputState>,
    /// A helper's text or number setting being edited: the field, and which (package, key).
    helper_edit: Entity<InputState>,
    helper_editing: Option<(String, String)>,
}

/// A copy started by a check or screenshot hook: no prompts.
fn unattended() -> bool {
    std::env::var_os("LIGHTAIR_OPEN").is_some()
}

fn file_name(p: &Path) -> String {
    p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
}

/// A sentence with a capital first letter.
fn sentence(text: &str) -> String {
    let mut c = text.chars();
    c.next().map(|f| f.to_uppercase().collect::<String>() + c.as_str()).unwrap_or_default()
}

/// A missing package's line: its version, size and source when the show's lock knows them, and
/// why it can't be used when it's installed but unusable.
fn missing_detail(m: &Missing) -> String {
    let mut parts = Vec::new();
    match &m.locked {
        Some(l) => {
            parts.push(l.version.to_string());
            if let Some(size) = l.size {
                parts.push(pk::size(size));
            }
            parts.push(pk::source_label(&l.source));
        }
        None => parts.push(m.range.to_string()),
    }
    match &m.reason {
        MissingReason::NotInstalled => {}
        MissingReason::NoMatchingVersion { installed } => {
            let list: Vec<String> = installed.iter().map(|v| v.to_string()).collect();
            parts.push(format!("{} installed", list.join(", ")));
        }
        MissingReason::NotForThisPlatform => parts.push("not for this computer".into()),
        MissingReason::NotUsable(problems) => {
            if let Some(p) = problems.first() {
                parts.push(p.to_string());
            }
        }
    }
    parts.join(" · ")
}

/// Choices side by side in one well (a segmented control)...
fn segments() -> Div {
    div().flex().gap(px(2.)).p(px(2.)).rounded(px(7.)).bg(tok::bg_app())
}

/// ...and one of them.
fn segment(id: &'static str, text: &'static str, on: bool) -> Stateful<Div> {
    div()
        .id(id)
        .px_2p5()
        .h(px(22.))
        .flex()
        .items_center()
        .rounded(px(5.))
        .text_size(px(12.))
        .when(on, |d| d.bg(tok::bg_selected()).text_color(tok::text()).font_weight(FontWeight::MEDIUM))
        .when(!on, |d| d.cursor_pointer().text_color(tok::text_secondary()).hover(|d| d.text_color(tok::text())))
        .child(text)
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
        .child(
            div()
                .max_w(px(340.))
                .text_size(px(12.))
                .text_color(tok::text_tertiary())
                .text_center()
                .child(about.to_string()),
        )
}

fn chip(text: &'static str) -> Div {
    div()
        .flex_none()
        .px_1p5()
        .h(px(16.))
        .flex()
        .items_center()
        .rounded(px(4.))
        .bg(tok::bg_selected())
        .text_size(px(10.))
        .text_color(tok::text_secondary())
        .child(text)
}

impl PackagesPanel {
    pub(super) fn new(
        shell: Entity<StudioWindow>,
        studio: Entity<Studio>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        // another show opened, or packages installed or removed: the show checked again
        cx.observe_in(&studio, window, |this, _, window, cx| {
            this.check_show(window, cx);
            cx.notify();
        })
        .detach();
        cx.defer_in(window, |this, window, cx| this.check_show(window, cx));
        let search = cx.new(|cx| InputState::new(window, cx).placeholder("Search the catalogs"));
        cx.subscribe_in(&search, window, |_, _, ev: &InputEvent, _, cx| {
            if let InputEvent::Change = ev {
                cx.notify();
            }
        })
        .detach();
        let catalog_url = cx.new(|cx| InputState::new(window, cx).placeholder("A catalog's address (https://…)"));
        let helper_edit = cx.new(|cx| InputState::new(window, cx));
        // catalogs fetched, packages installed or withdrawn: drawn again
        cx.spawn(async move |this, cx| {
            let mut seen = (0, 0);
            loop {
                cx.background_executor().timer(Duration::from_millis(500)).await;
                let now = (crate::catalogs::generation(), pk::generation() + crate::helpers_host::generation());
                if now != seen {
                    seen = now;
                    if this.update(cx, |_, cx| cx.notify()).is_err() {
                        break;
                    }
                }
                if this.upgrade().is_none() {
                    break;
                }
            }
        })
        .detach();
        PackagesPanel {
            search,
            catalog_url,
            helper_edit,
            helper_editing: None,
            shell: shell.downgrade(),
            studio,
            focus: cx.focus_handle(),
            tab: Tab::Installed,
            open: HashSet::new(),
            show: ShowCheck::default(),
            used: (None, BTreeSet::new()),
        }
    }

    /// Shows a package's contents (after it's installed).
    pub(super) fn reveal(&mut self, id: PackageId, cx: &mut Context<Self>) {
        self.tab = Tab::Installed;
        self.open.insert(id);
        cx.notify();
    }

    /// Resolves the open show's packages when the show or the store changed; when another show
    /// was opened and some of its packages aren't installed, says so.
    fn check_show(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let dir = self.studio.read(cx).workspace_dir();
        let generation = pk::generation();
        if self.show.dir == dir && self.show.generation == generation {
            return;
        }
        let opened = self.show.dir != dir;
        let mut check = ShowCheck { dir: dir.clone(), generation, ..Default::default() };
        if let Some((project, res)) = dir.as_deref().and_then(pk::check_show) {
            check.needed = project.requirements.packages.keys().cloned().collect();
            check.missing = res.missing;
            check.mismatched = res.mismatched;
            check.conflicts = res.conflicts;
        }
        self.show = check;
        self.used.0 = None;
        if !opened || self.show.missing.is_empty() || unattended() {
            return;
        }
        let n = self.show.missing.len();
        let title = if n == 1 {
            "This show uses a package that isn't installed".to_string()
        } else {
            format!("This show uses {n} packages that aren't installed")
        };
        let mut lines: Vec<String> =
            self.show.missing.iter().take(8).map(|m| format!("{} {}", m.id, missing_detail(m))).collect();
        if n > 8 {
            lines.push(format!("and {} more", n - 8));
        }
        let detail = lines.join("\n");
        // a show exported with its packages carries them: offered first
        let wanted: Vec<(PackageId, Option<Version>)> =
            self.show.missing.iter().map(|m| (m.id.clone(), m.locked.as_ref().map(|l| l.version.clone()))).collect();
        let bundled = dir.as_deref().map(|d| pk::bundled_for(d, &wanted)).unwrap_or_default();
        let buttons: &[&str] = if bundled.is_empty() { &["Show packages", "Not now"] } else { &["Install from this show", "Show packages", "Not now"] };
        let answer = window.prompt(PromptLevel::Warning, &title, Some(&detail), buttons, cx);
        let shell = self.shell.clone();
        let offset = usize::from(!bundled.is_empty());
        cx.spawn_in(window, async move |_, cx| {
            let choice = answer.await;
            let _ = cx.update(|window, cx| {
                let Some(s) = shell.upgrade() else { return };
                if offset == 1 && choice == Ok(0) {
                    // each through the install screen and its checks, as a dropped file
                    s.update(cx, |w, cx| w.install_package_paths(bundled, window, cx));
                } else if choice == Ok(offset) {
                    s.update(cx, |w, cx| w.show_packages_panel(window, cx));
                }
            });
        })
        .detach();
    }

    /// What in the show comes from a package (looked at again at most once a second).
    fn used(&mut self, cx: &App) -> BTreeSet<PackageId> {
        if self.used.0.is_none_or(|t| t.elapsed() > Duration::from_secs(1)) {
            let workspace = self.studio.read(cx).workspace().clone();
            self.used = (Some(Instant::now()), pk::used_by(&workspace).into_keys().collect());
        }
        self.used.1.clone()
    }

    fn with_shell(&self, cx: &mut App, f: impl FnOnce(&mut StudioWindow, &mut Context<StudioWindow>)) {
        if let Some(s) = self.shell.upgrade() {
            s.update(cx, f);
        }
    }

    fn toolbar(&self, cx: &mut Context<Self>) -> Div {
        let tab = |id: &'static str, label: &'static str, t: Tab, cx: &mut Context<Self>| {
            segment(id, label, self.tab == t).on_click(cx.listener(move |this, _, _, cx| {
                this.tab = t;
                cx.notify();
            }))
        };
        div()
            .flex()
            .items_center()
            .gap_1p5()
            .px_3()
            .py_2()
            .border_b_1()
            .border_color(tok::border())
            .child(
                segments()
                    .child(tab("pkg-tab-installed", "Installed", Tab::Installed, cx))
                    .child(tab("pkg-tab-catalog", "Catalog", Tab::Catalog, cx))
                    .child(tab("pkg-tab-updates", "Updates", Tab::Updates, cx)),
            )
            .child(div().flex_1())
            .child(
                Button::new("pkg-reveal")
                    .icon(ui::icon("folder-open"))
                    .xsmall()
                    .ghost()
                    .tooltip(if cfg!(target_os = "macos") { "Show in Finder" } else { "Show in Explorer" })
                    .on_click(|_, _, cx| {
                        if let Some(store) = pk::store() {
                            cx.reveal_path(store.root());
                        }
                    }),
            )
            .child(
                Button::new("pkg-install")
                    .label("Install…")
                    .icon(ui::icon("package-plus"))
                    .xsmall()
                    .outline()
                    .tooltip("Install a .airpkg, a package folder or a .lwidget")
                    .on_click(
                        cx.listener(|this, _, window, cx| {
                            this.with_shell(cx, |w, cx| w.pick_package_files(window, cx))
                        }),
                    ),
            )
    }

    /// The show's packages that can't be used here: missing, changed or clashing.
    fn show_notice(&self, cx: &mut Context<Self>) -> Option<Div> {
        let s = &self.show;
        if s.missing.is_empty() && s.mismatched.is_empty() && s.conflicts.is_empty() {
            return None;
        }
        let title = match s.missing.len() {
            0 => "This show's packages need attention".to_string(),
            1 => "This show needs a package that isn't installed".to_string(),
            n => format!("This show needs {n} packages that aren't installed"),
        };
        let line = |name: String, detail: String| {
            div()
                .flex()
                .items_baseline()
                .gap_2()
                .min_w_0()
                .child(div().flex_none().text_size(px(12.)).text_color(tok::text()).child(name))
                .child(div().min_w_0().truncate().text_size(px(11.)).text_color(tok::text_tertiary()).child(detail))
        };
        let mut lines = div().flex().flex_col().gap_1();
        for m in &s.missing {
            lines = lines.child(line(m.id.to_string(), missing_detail(m)));
        }
        for m in &s.mismatched {
            lines = lines.child(line(
                format!("{} {}", m.id, m.version),
                "differs from the copy this show was saved with".into(),
            ));
        }
        for c in &s.conflicts {
            let by = c.required_by.as_ref().map(|b| format!(" (for {b})")).unwrap_or_default();
            lines =
                lines.child(line(c.id.to_string(), format!("{} is installed, {} is needed{by}", c.picked, c.range)));
        }
        let (count, bytes) = self.missing_size();
        Some(
            div()
                .flex()
                .flex_col()
                .gap_2()
                .m_3()
                .p_3()
                .rounded(px(8.))
                .bg(tok::bg_raised())
                .border_1()
                .border_color(tok::border_strong())
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap_1p5()
                        .child(ui::icon("triangle-alert").size(px(14.)).text_color(tok::warn()))
                        .child(div().flex_1().text_size(px(12.)).font_weight(FontWeight::SEMIBOLD).child(title))
                        .when(count > 0, |d| {
                            d.child(div().text_size(px(11.)).text_color(tok::text_tertiary()).child(pk::size(bytes)))
                        }),
                )
                .child(lines)
                .when(!s.missing.is_empty(), |d| {
                    let from_catalogs = self.missing_from_catalogs();
                    d.child(
                        div()
                            .flex()
                            .gap_1p5()
                            .when(!from_catalogs.is_empty(), |d| {
                                d.child(Button::new("pkg-install-missing-catalog").label("Install").xsmall().primary().on_click(
                                    cx.listener(move |this, _, window, cx| {
                                        for (catalog, id, version) in from_catalogs.clone() {
                                            this.with_shell(cx, |w, cx| {
                                                w.install_from_catalog(Some(catalog), id, Some(version), window, cx)
                                            });
                                        }
                                    }),
                                ))
                            })
                            .child(Button::new("pkg-install-missing").label("Install from file…").xsmall().outline().on_click(
                                cx.listener(|this, _, window, cx| {
                                    this.with_shell(cx, |w, cx| w.pick_package_files(window, cx))
                                }),
                            )),
                    )
                }),
        )
    }

    /// The total size of the missing packages the show's lock knows the size of, and how many.
    fn missing_size(&self) -> (usize, u64) {
        let sizes: Vec<u64> = self.show.missing.iter().filter_map(|m| m.locked.as_ref()?.size).collect();
        (sizes.len(), sizes.iter().sum())
    }

    fn row(&self, i: usize, versions: &[Installed], in_show: bool, cx: &mut Context<Self>) -> AnyElement {
        let p = &versions[0];
        let m = &p.manifest;
        let id = m.id.clone();
        let open = self.open.contains(&id);
        let mut problems = pk::problems(p);
        if let Some(reason) = crate::catalogs::revoked_reason(m.id.as_str(), &m.version) {
            problems.insert(0, format!("Withdrawn and switched off: {reason}"));
        }
        let mut facts = vec![m.summary(), pk::tier_label(m.highest_tier()).to_string()];
        if !m.licence.is_empty() {
            facts.push(m.licence.clone());
        }
        facts.push(pk::size(p.record.size));
        // the details on hover
        let mut about = vec![m.id.to_string()];
        if !m.description.is_empty() {
            about.push(m.description.clone());
        }
        if !m.author.is_empty() {
            about.push(format!("By {}", m.author));
        }
        about.push(format!("From {}", pk::source_label(&p.record.source)));
        if versions.len() > 1 {
            let others: Vec<String> = versions[1..].iter().map(|v| v.version().to_string()).collect();
            about.push(format!("Also installed: {}", others.join(", ")));
        }
        if !m.platforms.is_empty() {
            let list: Vec<&str> = m.platforms.iter().map(|p| p.as_str()).collect();
            about.push(format!("Works on {}", list.join(", ")));
        }
        let name = m.name.clone();
        let (remove_id, remove_name, size) = (id.clone(), name.clone(), p.record.size);
        let menu_versions: Vec<Version> = versions.iter().map(|v| v.version().clone()).collect();
        let (menu_id, menu_name, menu_path) = (id.clone(), name.clone(), p.path.clone());
        let me = cx.entity().downgrade();
        div()
            .id(("pkg-row", i))
            .group("pkg-row")
            .flex()
            .items_center()
            .gap_2()
            .px_3()
            .py(px(7.))
            .border_b_1()
            .border_color(tok::border())
            .cursor_pointer()
            .when(open, |d| d.bg(tok::bg_raised()))
            .hover(|d| d.bg(tok::bg_raised()))
            .on_click(cx.listener(move |this, _, _, cx| {
                if !this.open.remove(&id) {
                    this.open.insert(id.clone());
                }
                cx.notify();
            }))
            .child(
                ui::icon(if open { "chevron-down" } else { "chevron-right" })
                    .size(px(12.))
                    .text_color(tok::text_tertiary()),
            )
            .child(
                ui::icon(if problems.is_empty() { "package" } else { "package-x" })
                    .size(px(16.))
                    .text_color(if problems.is_empty() { tok::text_secondary() } else { tok::warn() }),
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
                            .gap_1p5()
                            .min_w_0()
                            .child(div().min_w_0().truncate().text_size(px(13.)).text_color(tok::text()).child(name))
                            .child(
                                div()
                                    .flex_none()
                                    .text_size(px(11.))
                                    .font_family(ui::MONO)
                                    .text_color(tok::text_tertiary())
                                    .child(m.version.to_string()),
                            )
                            .when(in_show, |d| d.child(chip("In this show"))),
                    )
                    .child(match problems.first() {
                        // switched off: why, as the line to act on
                        Some(problem) => div()
                            .flex()
                            .items_center()
                            .gap_1()
                            .min_w_0()
                            .text_size(px(11.))
                            .text_color(tok::warn())
                            .child(ui::icon("triangle-alert").size(px(11.)))
                            .child(div().min_w_0().truncate().child(sentence(problem))),
                        None => div()
                            .truncate()
                            .text_size(px(11.))
                            .text_color(tok::text_tertiary())
                            .child(facts.join(" · ")),
                    }),
            )
            .child(ui::info(("pkg-about", i), about.join("\n")))
            .child(
                ui::icon_button(("pkg-remove", i), "trash", tok::text_secondary())
                    .invisible()
                    .group_hover("pkg-row", |s| s.visible())
                    .tooltip(ui::tip("Remove"))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        cx.stop_propagation();
                        this.remove(remove_id.clone(), None, &remove_name, size, in_show, window, cx);
                    })),
            )
            .context_menu(move |menu, _, _| {
                let mut menu = menu;
                if menu_versions.len() > 1 {
                    for v in &menu_versions {
                        let (me, id, name, v) = (me.clone(), menu_id.clone(), menu_name.clone(), v.clone());
                        menu = menu.item(PopupMenuItem::new(format!("Remove {v}")).icon(ui::icon("trash")).on_click(
                            move |_, window, cx| {
                                if let Some(p) = me.upgrade() {
                                    p.update(cx, |p, cx| {
                                        p.remove(id.clone(), Some(v.clone()), &name, 0, in_show, window, cx)
                                    });
                                }
                            },
                        ));
                    }
                } else {
                    let (me, id, name) = (me.clone(), menu_id.clone(), menu_name.clone());
                    menu = menu.item(PopupMenuItem::new("Remove").icon(ui::icon("trash")).on_click(
                        move |_, window, cx| {
                            if let Some(p) = me.upgrade() {
                                p.update(cx, |p, cx| p.remove(id.clone(), None, &name, size, in_show, window, cx));
                            }
                        },
                    ));
                }
                let path = menu_path.clone();
                let me = me.clone();
                menu.separator()
                    .item(PopupMenuItem::new("Remove unused from this show").icon(ui::icon("package-minus")).on_click(
                        move |_, _, cx| {
                            if let Some(p) = me.upgrade() {
                                p.update(cx, |p, cx| p.remove_unused(cx));
                            }
                        },
                    ))
                    .item(
                        PopupMenuItem::new(if cfg!(target_os = "macos") {
                            "Show in Finder"
                        } else {
                            "Show in Explorer"
                        })
                        .icon(ui::icon("folder-open"))
                        .on_click(move |_, _, cx| cx.reveal_path(&path)),
                    )
            })
            .into_any_element()
    }

    /// A package's contents, each with what can be done with it.
    fn contents(&self, i: usize, p: &Installed, cx: &mut Context<Self>) -> Div {
        let usable = pk::problems(p).is_empty();
        let (id, version) = (p.id().clone(), p.version().clone());
        let mut list = div()
            .flex()
            .flex_col()
            .pl(px(42.))
            .pr_3()
            .py_1()
            .border_b_1()
            .border_color(tok::border())
            .bg(tok::bg_raised());
        for (j, item) in pk::items(p).into_iter().enumerate() {
            let action: AnyElement = match (&item.how, usable) {
                (Use::Later, _) => div()
                    .flex()
                    .items_center()
                    .gap_1()
                    .text_size(px(11.))
                    .text_color(tok::text_tertiary())
                    .child("Not used yet")
                    .child(ui::info(
                        ("pkg-later", i * 1000 + j),
                        "Installed and kept with your shows. This Lightair doesn't use this kind yet.",
                    ))
                    .into_any_element(),
                (Use::Empty, _) => {
                    div().text_size(px(11.)).text_color(tok::text_tertiary()).child("Nothing to use").into_any_element()
                }
                (_, false) => div().into_any_element(),
                (how, true) => {
                    let (label, tip) = match how {
                        Use::Widget(_) => ("Add", "Add it to the scene"),
                        Use::Scene(_) => ("Add scene", "Add it as a new scene"),
                        Use::Place(files) if files.len() > 1 => ("Add", "Add them to the scene"),
                        Use::Place(_) => ("Add", "Add it to the scene, or drag it onto the canvas"),
                        Use::Sounds(_) => ("Add to soundboard", "Each sound a pad"),
                        Use::Lut(_) => ("Add to selected", "A LUT effect on the selected object"),
                        Use::Recipe(_) => ("Add to automations", "Added switched off, to look over first"),
                        Use::Later | Use::Empty => ("", ""),
                    };
                    let (id, version, item) = (id.clone(), version.clone(), item.clone());
                    Button::new(("pkg-use", i * 1000 + j))
                        .label(label)
                        .xsmall()
                        .outline()
                        .tooltip(tip)
                        .on_click(cx.listener(move |this, _, window, cx| {
                            let (id, version, item) = (id.clone(), version.clone(), item.clone());
                            this.with_shell(cx, |w, cx| w.use_package_item(&id, &version, &item, window, cx));
                        }))
                        .into_any_element()
                }
            };
            let drag = match (&item.how, usable) {
                (Use::Place(files), true) if files.len() == 1 => {
                    Some(Asset { kind: AssetKind::of(&files[0]), path: files[0].clone() })
                }
                _ => None,
            };
            list = list.child(
                div()
                    .id(("pkg-item", i * 1000 + j))
                    .flex()
                    .items_center()
                    .gap_2()
                    .h(px(30.))
                    .child(ui::icon(pk::kind_icon(&item.kind)).size(px(14.)).text_color(tok::text_secondary()))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_size(px(12.))
                            .text_color(tok::text())
                            .child(item.name.clone()),
                    )
                    .child(
                        div()
                            .flex_none()
                            .text_size(px(11.))
                            .text_color(tok::text_tertiary())
                            .child(pk::kind_label(&item.kind)),
                    )
                    .child(div().flex_none().child(action))
                    .when_some(drag, |d, asset| {
                        d.cursor_grab().on_drag(DraggedAsset(asset), |a, _, _, cx| cx.new(|_| a.clone()))
                    }),
            );
        }
        list
    }

    /// Removes a package (every version) or one version of it, after asking.
    #[allow(clippy::too_many_arguments)]
    fn remove(
        &mut self,
        id: PackageId,
        version: Option<Version>,
        name: &str,
        size: u64,
        in_show: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let title = match &version {
            Some(v) => format!("Remove {name} {v}?"),
            None => format!("Remove {name}?"),
        };
        let detail = if in_show {
            "This show uses it: what comes from it will be missing until it's installed again.".to_string()
        } else if size > 0 {
            format!("{} is deleted from this computer.", pk::size(size))
        } else {
            "It's deleted from this computer.".to_string()
        };
        let answer = window.prompt(PromptLevel::Warning, &title, Some(&detail), &["Remove", "Cancel"], cx);
        let studio = self.studio.clone();
        let name = name.to_string();
        cx.spawn(async move |this, cx| {
            if answer.await != Ok(0) {
                return;
            }
            let (task_id, task_version) = (id.clone(), version.clone());
            let result =
                cx.background_executor().spawn(async move { pk::remove(&task_id, task_version.as_ref()) }).await;
            let _ = this.update(cx, |this, cx| {
                this.open.remove(&id);
                let note = match result {
                    Ok(()) => format!("Removed {name}"),
                    Err(e) => format!("Couldn't remove {name}: {e}"),
                };
                studio.update(cx, |s, cx| {
                    s.issue = Some(note.into());
                    cx.notify();
                });
            });
        })
        .detach();
    }

    /// Remove unused packages: the show stops listing packages nothing in it comes from.
    fn remove_unused(&mut self, cx: &mut Context<Self>) {
        let (dir, workspace) = {
            let studio = self.studio.read(cx);
            (studio.workspace_dir(), studio.workspace().clone())
        };
        let Some(dir) = dir else { return };
        let note = match pk::remove_unused(&dir, &workspace) {
            Ok(0) => "This show uses every package it lists".to_string(),
            Ok(1) => "Removed 1 unused package from this show".to_string(),
            Ok(n) => format!("Removed {n} unused packages from this show"),
            Err(e) => format!("Couldn't update this show's packages: {e}"),
        };
        // checked again
        self.show.generation = 0;
        self.studio.update(cx, |s, cx| {
            s.issue = Some(note.into());
            cx.notify();
        });
    }

    fn installed_tab(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let groups = pk::by_package();
        let used = self.used(cx);
        let notice = self.show_notice(cx);
        if groups.is_empty() && notice.is_none() {
            return empty(
                "package",
                "No packages yet",
                "Drop a .airpkg, a package folder or a .lwidget here, or choose Install…",
            )
            .into_any_element();
        }
        let mut list = div().id("pkg-list").flex().flex_col().flex_1().min_h_0().overflow_y_scroll().children(notice);
        for (i, versions) in groups.iter().enumerate() {
            let id = versions[0].id();
            let in_show = used.contains(id) || self.show.needed.contains(id);
            list = list.child(self.row(i, versions, in_show, cx));
            if self.open.contains(id) {
                list = list.child(self.contents(i, &versions[0], cx));
                list = list.children(self.permissions(i, &versions[0], cx));
                list = list.children(self.helper_section(i, &versions[0], cx));
            }
        }
        list.into_any_element()
    }
}

impl Render for PackagesPanel {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let body = match self.tab {
            Tab::Installed => self.installed_tab(cx),
            Tab::Catalog => self.catalog_tab(cx),
            Tab::Updates => self.updates_tab(cx),
        };
        div()
            .id("packages-panel")
            .flex()
            .flex_col()
            .size_full()
            .bg(tok::bg_panel())
            .drag_over::<ExternalPaths>(|s, _, _, _| s.bg(tok::bg_raised()))
            .on_drop(cx.listener(|this, paths: &ExternalPaths, window, cx| {
                cx.stop_propagation();
                let paths = paths.paths().to_vec();
                this.with_shell(cx, |w, cx| w.install_package_paths(paths, window, cx));
            }))
            .child(self.toolbar(cx))
            .child(body)
    }
}

impl gpui_kit::component::dock::BasePanel for PackagesPanel {
    fn panel_name(&self) -> &'static str {
        "packages"
    }
}

impl gpui_kit::component::dock::Panel for PackagesPanel {
    fn tab_name(&self, _: &App) -> Option<SharedString> {
        None
    }

    fn title(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        ui::panel_title("package", "Packages")
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

impl EventEmitter<PanelEvent> for PackagesPanel {}

impl Focusable for PackagesPanel {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

/// `value`'s strings that name a file beside `base` (a template's media, written relative to
/// its `scene.json`) made absolute, so the show points at the installed files.
fn absolutise(value: &mut Value, base: &Path) {
    match value {
        Value::String(s) => {
            let rel = Path::new(s.as_str());
            if !s.is_empty()
                && !s.contains("://")
                && rel.is_relative()
                && rel.components().all(|c| matches!(c, std::path::Component::Normal(_)))
                && base.join(rel).is_file()
            {
                *s = base.join(rel).to_string_lossy().into_owned();
            }
        }
        Value::Array(a) => a.iter_mut().for_each(|v| absolutise(v, base)),
        Value::Object(m) => m.values_mut().for_each(|v| absolutise(v, base)),
        _ => {}
    }
}

impl StudioWindow {
    fn package_note(&self, note: impl Into<SharedString>, cx: &mut Context<Self>) {
        let note = note.into();
        tracing::info!(target: "packages", "{note}");
        self.studio.update(cx, |s, cx| {
            s.issue = Some(note);
            cx.notify();
        });
    }

    fn packages_panel(&self) -> Option<Entity<PackagesPanel>> {
        self.dock_panels.as_ref().map(|p| p.packages.clone())
    }

    /// Brings the Packages panel forward where it is, or in a window of its own.
    pub(super) fn show_packages_panel(&mut self, _: &mut Window, cx: &mut Context<Self>) {
        self.show_panel("packages", cx);
    }

    /// Install…: asks for package files and folders, then installs them.
    pub(super) fn pick_package_files(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let paths = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: true,
            multiple: true,
            prompt: Some("Install".into()),
        });
        cx.spawn_in(window, async move |this, cx| {
            let Ok(Ok(Some(paths))) = paths.await else { return };
            let _ = this.update_in(cx, |this, window, cx| this.install_package_paths(paths, window, cx));
        })
        .detach();
    }

    /// From files dropped on the window: installs the packages among them and returns the rest.
    pub(super) fn take_package_drop(
        &mut self,
        paths: &[PathBuf],
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<PathBuf> {
        let (packages, rest): (Vec<PathBuf>, Vec<PathBuf>) =
            paths.iter().cloned().partition(|p| pk::is_package_path(p));
        if !packages.is_empty() {
            self.packages.last_drop = Some((packages.clone(), Instant::now()));
            self.install_package_paths(packages, window, cx);
        }
        rest
    }

    /// Files dropped anywhere on the window: the packages among them are installed (unless the
    /// panel or place they were dropped on just took them).
    pub(super) fn drop_on_window(&mut self, paths: &[PathBuf], window: &mut Window, cx: &mut Context<Self>) {
        let packages: Vec<PathBuf> = paths.iter().filter(|p| pk::is_package_path(p)).cloned().collect();
        if packages.is_empty() {
            return;
        }
        if let Some((taken, at)) = &self.packages.last_drop
            && at.elapsed() < Duration::from_secs(2)
            && packages.iter().all(|p| taken.contains(p))
        {
            return;
        }
        self.packages.last_drop = Some((packages.clone(), Instant::now()));
        self.install_package_paths(packages, window, cx);
    }

    /// Installs each package after a prompt saying what it adds, its size and where it's from.
    /// Nothing is downloaded: everything comes from the files given.
    pub(super) fn install_package_paths(&mut self, paths: Vec<PathBuf>, window: &mut Window, cx: &mut Context<Self>) {
        let paths: Vec<PathBuf> = paths.into_iter().filter(|p| pk::is_package_path(p)).collect();
        if paths.is_empty() {
            self.package_note("That isn't a Lightair package (.airpkg, a package folder or .lwidget)", cx);
            return;
        }
        cx.spawn_in(window, async move |this, cx| {
            let mut last: Option<PackageId> = None;
            for path in paths {
                let shown = file_name(&path);
                let task_path = path.clone();
                let read = cx.background_executor().spawn(async move { pk::prepare(&task_path) }).await;
                let prepared = match read {
                    Ok(p) => p,
                    Err(e) => {
                        let title = format!("“{shown}” can't be installed");
                        let detail = sentence(&e);
                        let answer = this.update_in(cx, |_, window, cx| {
                            window.prompt(PromptLevel::Critical, &title, Some(&detail), &["OK"], cx)
                        });
                        if let Ok(answer) = answer {
                            let _ = answer.await;
                        }
                        continue;
                    }
                };
                if let Some(id) = StudioWindow::confirm_and_install(&this, cx, prepared).await {
                    last = Some(id);
                }
            }
            // the panel, showing what came in
            if let Some(id) = last {
                let _ = this.update_in(cx, |this, window, cx| {
                    this.show_packages_panel(window, cx);
                    if let Some(panel) = this.packages_panel() {
                        panel.update(cx, |p, cx| p.reveal(id, cx));
                    }
                });
            }
        })
        .detach();
    }

    /// Uses one content entry of an installed package where its kind lives.
    pub(super) fn use_package_item(
        &mut self,
        id: &PackageId,
        version: &Version,
        item: &Item,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match &item.how {
            Use::Widget(dir) => self.add_widget_from_package(dir, &item.name, id, version, None, window, cx),
            Use::Scene(file) => self.add_scene_from_package(file, id, version, cx),
            Use::Place(files) => {
                let assets: Vec<Asset> =
                    files.iter().map(|p| Asset { kind: AssetKind::of(p), path: p.clone() }).collect();
                self.place_assets(&assets, Placement::Centre, cx);
            }
            Use::Sounds(files) => {
                crate::show_panel::add_pads(&self.studio, files, cx);
                self.package_note(format!("Added {} to the soundboard", item.name), cx);
            }
            Use::Lut(file) => self.add_lut_from_package(file, &item.name, cx),
            Use::Recipe(file) => self.add_recipe_from_package(file, cx),
            Use::Later | Use::Empty => {}
        }
    }

    /// Adds a package's widget to the scene, its code copied in and the source marked as from
    /// the package (so the show records it).
    #[allow(clippy::too_many_arguments)]
    fn add_widget_from_package(
        &mut self,
        dir: &Path,
        name: &str,
        id: &PackageId,
        version: &Version,
        overlay: Option<&str>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let custom = match lightair_widgets::import_folder(dir) {
            Ok(c) => c,
            Err(e) => return self.package_note(format!("Couldn't read the widget {name}: {e}"), cx),
        };
        let (w, h) = pk::widget_size(dir).unwrap_or((800, 600));
        let instance = lightair_widgets::Instance::new("custom", Some(custom));
        let mut settings = json!({"url": instance.address(), "width": w, "height": h, "fps": 30, "widget": instance});
        settings[pk::MARK] = pk::mark(id, version);
        // a script plugin's overlay: its page and the script exchange messages
        if let Some(overlay) = overlay {
            settings["lightair_overlay"] = json!({"package": id.as_str(), "overlay": overlay});
        }
        self.add_placed_object(name, lightair_host::web::KIND, settings, (w as f32, h as f32), (0.5, 0.5), true, cx);
        if self.add_object.is_some() {
            self.close_add_object(window, cx);
        }
    }

    /// Add Object › Widgets › From packages: the `index`th of the packages' widgets.
    pub(super) fn add_package_widget(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(w) = pk::widgets().get(index).cloned() else { return };
        self.add_widget_from_package(&w.dir, w.name, &w.id, &w.version, w.overlay.as_deref(), window, cx);
    }

    /// Adds a package's template as a new scene after the selected one: its media from the
    /// installed package, its sources marked as from the package.
    fn add_scene_from_package(&mut self, file: &Path, id: &PackageId, version: &Version, cx: &mut Context<Self>) {
        let read = std::fs::read(file).ok().and_then(|b| serde_json::from_slice::<Value>(&b).ok());
        let Some(mut value) = read else {
            return self.package_note("That template couldn't be read", cx);
        };
        if let Some(base) = file.parent() {
            absolutise(&mut value, base);
        }
        let Ok(mut saved) = serde_json::from_value::<lightair_model::SceneAsset>(value) else {
            return self.package_note("That template couldn't be read", cx);
        };
        for source in &mut saved.clip.sources {
            if source.settings.is_object() {
                source.settings[pk::MARK] = pk::mark(id, version);
            }
        }
        let studio = self.studio.read(cx);
        let workspace = studio.workspace().clone();
        let index = studio.selected_scene.and_then(|s| workspace.scene_index(s)).map(|i| i + 1);
        let (commands, scene) = saved.add(&workspace, index);
        self.exec(Command::Batch { label: format!("Add scene {}", saved.name), commands }, cx);
        self.studio.update(cx, |s, cx| {
            s.select_scene(scene);
            cx.notify();
        });
    }

    /// Puts a package's LUT on the selected object, as a LUT effect.
    fn add_lut_from_package(&mut self, file: &Path, name: &str, cx: &mut Context<Self>) {
        let source = {
            let studio = self.studio.read(cx);
            let workspace = studio.workspace();
            studio
                .selected_scene
                .and_then(|s| workspace.scene(s))
                .and_then(|scene| studio.selected_object.and_then(|o| scene.object(o)))
                .and_then(|o| o.source)
        };
        let Some(source) = source else {
            return self.package_note("Select an object for the LUT first", cx);
        };
        let mut effect = lightair_model::catalog::def("clut_filter")
            .map(|d| d.create())
            .unwrap_or_else(|| Effect::new("LUT", "clut_filter", json!({"clut_amount": 1.0})));
        effect.name = name.to_string();
        effect.settings["image_path"] = json!(file.to_string_lossy());
        self.exec(Command::AddSourceEffect { source, effect, index: None }, cx);
    }

    /// Adds a package's automation to Automations, switched off (its steps are looked over
    /// before it's switched on).
    fn add_recipe_from_package(&mut self, file: &Path, cx: &mut Context<Self>) {
        let read =
            std::fs::read(file).ok().and_then(|b| serde_json::from_slice::<lightair_automation::Automation>(&b).ok());
        let Some(mut automation) = read else {
            return self.package_note("That automation couldn't be read", cx);
        };
        automation.id = lightair_automation::new_id();
        automation.enabled = false;
        let name = automation.name.clone();
        self.automations.set(automation);
        self.package_note(format!("Added {name} to Automations, switched off"), cx);
    }
}
