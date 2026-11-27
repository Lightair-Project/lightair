//! The Packages panel's Catalog and Updates tabs, each installed package's permissions, adding
//! and removing community catalogs, and installing from a catalog (or a `lightair://` link)
//! through the one install screen (plugins.md §3.5–3.8, §4.8).

use gpui_kit::component::Sizable;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::Input as TextInput;
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;
use lightair_packages::{Installed, Permission};

use super::{PackagesPanel, Tab, empty, file_name, sentence};
use crate::catalogs::{self, Offer};
use crate::links::Link;
use crate::packages::{self as pk, Prepared};
use crate::shell::StudioWindow;
use crate::ui::{self, tok};

/// The install screen's title and text: what it adds, what it may do (one line each, new ones
/// marked), and where it's from.
pub(crate) fn install_screen(p: &Prepared) -> (String, String) {
    let title = format!("Install “{}” {}?", p.name, p.version);
    let mut detail = format!("Adds {}.", p.summary);
    let lines = p.permission_lines();
    if lines.is_empty() {
        detail.push_str("\n\nIt asks for no permissions.");
    } else {
        detail.push_str("\n\nIt can:");
        for l in lines {
            detail.push_str(&format!("\n• {l}"));
        }
    }
    let mut facts = vec![p.origin.line(), pk::size(p.size)];
    if matches!(p.origin, pk::Origin::File | pk::Origin::Folder) {
        facts.push(format!("from {}", file_name(&p.path)));
    }
    if !p.licence.is_empty() {
        facts.push(p.licence.clone());
    }
    if !p.author.is_empty() {
        facts.push(format!("by {}", p.author));
    }
    detail.push_str(&format!("\n\n{}", facts.join(" · ")));
    (title, detail)
}

/// The second question for a package that runs with full access: its own, worded as strongly
/// as the design asks (plugins.md §4.12).
pub(crate) fn full_access_screen(p: &Prepared) -> (String, String) {
    let mut detail = "This plugin runs like any app you install: it can read your files, use the network and \
                      your devices. Lightair can't limit or watch what it does."
        .to_string();
    if !p.origin.is_lightair() {
        detail.push_str("\n\nIt isn't checked by Lightair.");
    }
    (format!("Give “{}” full access?", p.name), detail)
}

impl StudioWindow {
    /// Installs a prepared package after the install screen (and the full-access question when it
    /// asks for it). Shared by files, folders and catalogs.
    pub(crate) async fn confirm_and_install(
        this: &WeakEntity<StudioWindow>,
        cx: &mut AsyncWindowContext,
        prepared: Prepared,
    ) -> Option<lightair_packages::PackageId> {
        if prepared.already {
            let note = format!("{} {} is already installed", prepared.name, prepared.version);
            let id = lightair_packages::PackageId::new(&prepared.id).ok();
            let _ = this.update(cx, |this, cx| this.package_note(note, cx));
            prepared.discard();
            return id;
        }
        let (title, detail) = install_screen(&prepared);
        // a check run (LIGHTAIR_OPEN) has nobody to answer: the screen's text goes to the log and
        // it installs, so a script can check both
        let unattended = std::env::var_os("LIGHTAIR_OPEN").is_some();
        if unattended {
            tracing::info!(target: "packages", "install screen: {title} {}", detail.replace('\n', " · "));
        } else {
            let answer = this
                .update_in(cx, |_, window, cx| window.prompt(PromptLevel::Info, &title, Some(&detail), &["Install", "Cancel"], cx))
                .ok()?;
            if answer.await != Ok(0) {
                prepared.discard();
                return None;
            }
        }
        if prepared.full_access && unattended {
            tracing::info!(target: "packages", "full access asked: a check run doesn't give it");
            prepared.discard();
            return None;
        }
        if prepared.full_access {
            let (title, detail) = full_access_screen(&prepared);
            let answer = this
                .update_in(cx, |_, window, cx| {
                    window.prompt(PromptLevel::Critical, &title, Some(&detail), &["Cancel", "Give full access"], cx)
                })
                .ok()?;
            if answer.await != Ok(1) {
                prepared.discard();
                return None;
            }
        }
        let name = prepared.name.clone();
        let result = cx.background_executor().spawn(async move { pk::install(prepared) }).await;
        let (note, id) = match &result {
            Ok(install) => {
                let m = &install.installed.manifest;
                let mut note = format!("Installed {} {}", m.name, m.version);
                if !install.missing_dependencies.is_empty() {
                    let needs: Vec<String> = install.missing_dependencies.iter().map(|(id, r)| format!("{id} {r}")).collect();
                    note.push_str(&format!(". It also needs {}, not installed", needs.join(", ")));
                }
                (note, Some(install.installed.id().clone()))
            }
            Err(e) => (format!("Couldn't install {name}: {e}"), None),
        };
        let _ = this.update(cx, |this, cx| this.package_note(note, cx));
        id
    }

    /// Installs `id` (a version, or the newest) from a catalog: downloaded, checked against the
    /// catalog's attestation, then the install screen. `catalog`: `None` looks in every catalog,
    /// Lightair's first.
    pub(crate) fn install_from_catalog(
        &mut self,
        catalog: Option<String>,
        id: String,
        version: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let catalog = catalog.map(|c| if c.ends_with('/') { c } else { format!("{c}/") }).or_else(|| {
            catalogs::offers().into_iter().find(|o| o.id == id).map(|o| o.catalog_url)
        });
        let Some(catalog) = catalog else {
            self.package_note(format!("{id} isn't in any catalog Lightair knows"), cx);
            return;
        };
        self.package_note(format!("Downloading {id}…"), cx);
        cx.spawn_in(window, async move |this, cx| {
            let (c, i, v) = (catalog.clone(), id.clone(), version.clone());
            let read = cx.background_executor().spawn(async move { catalogs::prepare(&c, &i, v.as_deref()) }).await;
            let prepared = match read {
                Ok(p) => p,
                Err(e) => {
                    let title = format!("{id} can't be installed");
                    let detail = sentence(&e);
                    if let Ok(answer) = this.update_in(cx, |_, window, cx| window.prompt(PromptLevel::Critical, &title, Some(&detail), &["OK"], cx)) {
                        let _ = answer.await;
                    }
                    return;
                }
            };
            if let Some(id) = StudioWindow::confirm_and_install(&this, cx, prepared).await {
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

    /// Asks whether to add a community catalog (its name, address and key fingerprint, and that
    /// it isn't checked by Lightair), then adds it.
    pub(crate) fn add_catalog_from_url(&mut self, url: String, window: &mut Window, cx: &mut Context<Self>) {
        cx.spawn_in(window, async move |this, cx| {
            let u = url.clone();
            let read = cx.background_executor().spawn(async move { catalogs::read_new(&u) }).await;
            let state = match read {
                Ok(s) => s,
                Err(e) => {
                    let detail = sentence(&e);
                    if let Ok(answer) = this.update_in(cx, |_, window, cx| window.prompt(PromptLevel::Warning, "That catalog can't be added", Some(&detail), &["OK"], cx)) {
                        let _ = answer.await;
                    }
                    return;
                }
            };
            let title = format!("Add the catalog “{}”?", state.name());
            let detail = format!(
                "Packages from this catalog aren't checked by Lightair.\n\n{}\nKey: {}\n\nIf the catalog's own page shows a different key, don't add it.",
                state.base_url,
                state.fingerprint()
            );
            let Ok(answer) = this.update_in(cx, |_, window, cx| window.prompt(PromptLevel::Info, &title, Some(&detail), &["Add", "Cancel"], cx)) else { return };
            if answer.await == Ok(0) {
                let name = state.name().to_string();
                catalogs::add(state);
                let _ = this.update_in(cx, |this, window, cx| {
                    this.package_note(format!("Added the catalog {name}"), cx);
                    this.show_packages_panel(window, cx);
                    if let Some(panel) = this.packages_panel() {
                        panel.update(cx, |p, cx| {
                            p.tab = Tab::Catalog;
                            cx.notify();
                        });
                    }
                });
            }
        })
        .detach();
    }

    /// Handles queued `lightair://` links: never installs without the install screen.
    pub(crate) fn handle_links(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        for link in crate::links::take() {
            match link {
                Link::Install { id, version, catalog } => {
                    if let Some(c) = &catalog {
                        let known = catalogs::catalogs().iter().any(|k| k.url.trim_end_matches('/') == c.trim_end_matches('/'));
                        if !known {
                            // a community catalog the user hasn't added: add it first
                            self.add_catalog_from_url(c.clone(), window, cx);
                            continue;
                        }
                    }
                    self.show_packages_panel(window, cx);
                    self.install_from_catalog(catalog, id, version, window, cx);
                }
                Link::Catalog { url } => self.add_catalog_from_url(url, window, cx),
                Link::Git { url, rev } => self.install_from_git(url, rev, window, cx),
            }
        }
    }

    /// Installs a package from a git repository: cloned off the UI thread, then the install screen.
    pub(crate) fn install_from_git(&mut self, url: String, rev: String, window: &mut Window, cx: &mut Context<Self>) {
        self.show_packages_panel(window, cx);
        self.package_note(format!("Getting {url} at {rev}…"), cx);
        cx.spawn_in(window, async move |this, cx| {
            let read = cx.background_executor().spawn(async move { pk::prepare_git(&url, &rev) }).await;
            match read {
                Ok(prepared) => {
                    let _ = StudioWindow::confirm_and_install(&this, cx, prepared).await;
                }
                Err(e) => {
                    let _ = this.update(cx, |this, cx| this.package_note(format!("Couldn't install from git: {e}"), cx));
                }
            }
        })
        .detach();
    }

    /// Updates every package with a newer version: one question for those asking for nothing
    /// new, and each one's own install screen when it asks for more.
    fn update_all(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let all = catalogs::updates();
        if all.is_empty() {
            return;
        }
        cx.spawn_in(window, async move |this, cx| {
            let plain: Vec<_> = all.iter().filter(|u| u.added_permissions.is_empty()).cloned().collect();
            let mut go_plain = plain.is_empty();
            if !plain.is_empty() {
                let names: Vec<String> = plain.iter().map(|u| format!("{} {}", u.name, u.newer)).collect();
                let title = format!("Update {} {}?", plain.len(), if plain.len() == 1 { "package" } else { "packages" });
                let detail = format!("{}\n\nShows keep the versions they were saved with until you update them.", names.join("\n"));
                if let Ok(answer) = this.update_in(cx, |_, window, cx| window.prompt(PromptLevel::Info, &title, Some(&detail), &["Update", "Cancel"], cx)) {
                    go_plain = answer.await == Ok(0);
                }
            }
            for u in all {
                let asks = !u.added_permissions.is_empty();
                if !asks && !go_plain {
                    continue;
                }
                let (c, i, v) = (u.catalog_url.clone(), u.id.clone(), u.newer.clone());
                let read = cx.background_executor().spawn(async move { catalogs::prepare(&c, &i, Some(&v)) }).await;
                match read {
                    Ok(prepared) if asks => {
                        StudioWindow::confirm_and_install(&this, cx, prepared).await;
                    }
                    Ok(prepared) => {
                        let name = prepared.name.clone();
                        let result = cx.background_executor().spawn(async move { pk::install(prepared) }).await;
                        let note = match result {
                            Ok(i) => format!("Updated {} to {}", name, i.installed.manifest.version),
                            Err(e) => format!("Couldn't update {name}: {e}"),
                        };
                        let _ = this.update(cx, |this, cx| this.package_note(note, cx));
                    }
                    Err(e) => {
                        let note = format!("Couldn't update {}: {e}", u.name);
                        let _ = this.update(cx, |this, cx| this.package_note(note, cx));
                    }
                }
            }
        })
        .detach();
    }
}

impl StudioWindow {
    /// Development only (the check driver): the catalog paths without the native prompts, which a
    /// script can't answer. `pkg-catalog-add <url>`, `pkg-install <id> [version]` (logs the install
    /// screen's text, then installs), `pkg-widget <id>`, `pkg-revoke <id> <permission>`,
    /// `pkg-grant <id> <permission>`, `pkg-refresh`, `pkg-panel installed|catalog|updates`,
    /// `pkg-request <id>` (`catalogs::request_install`), `pkg-link <url>` (parsed and logged), `pkg-log`.
    pub(crate) fn package_check(&mut self, cmd: &str, rest: &str, window: &mut Window, cx: &mut Context<Self>) {
        let mut args = rest.split_whitespace();
        match cmd {
            "pkg-catalog-add" => match catalogs::read_new(rest.trim()) {
                Ok(state) => {
                    tracing::info!(name = state.name(), key = %state.fingerprint(), "CHECK catalog read");
                    catalogs::add(state);
                }
                Err(e) => tracing::warn!("CHECK catalog can't be added: {e}"),
            },
            "pkg-install" => {
                let id = args.next().unwrap_or_default().to_string();
                let version = args.next().map(str::to_string);
                let Some(offer) = catalogs::offers().into_iter().find(|o| o.id == id) else {
                    tracing::warn!(id, "CHECK not in any catalog");
                    return;
                };
                match catalogs::prepare(&offer.catalog_url, &id, version.as_deref()) {
                    Ok(p) => {
                        let (title, detail) = install_screen(&p);
                        tracing::info!("CHECK install screen: {title} / {}", detail.replace('\n', " | "));
                        match pk::install(p) {
                            Ok(i) => tracing::info!(source = %pk::source_label(&i.installed.record.source), "CHECK installed {}", i.installed.id()),
                            Err(e) => tracing::warn!("CHECK install failed: {e}"),
                        }
                    }
                    Err(e) => tracing::warn!("CHECK prepare failed: {e}"),
                }
            }
            "pkg-widget" => {
                let id = rest.trim();
                match pk::widgets().iter().position(|w| w.id.as_str() == id) {
                    Some(i) => self.add_package_widget(i, window, cx),
                    None => tracing::warn!(id, "CHECK no widget from that package"),
                }
            }
            "pkg-revoke" | "pkg-grant" => {
                let (id, perm) = (args.next().unwrap_or_default(), args.next().unwrap_or_default());
                if cmd == "pkg-revoke" {
                    crate::grants::revoke(id, perm);
                } else {
                    crate::grants::grant(id, perm);
                }
            }
            "pkg-refresh" => catalogs::refresh_now(),
            "pkg-panel" => {
                self.show_packages_panel(window, cx);
                let tab = match rest.trim() {
                    "catalog" => Tab::Catalog,
                    "updates" => Tab::Updates,
                    _ => Tab::Installed,
                };
                if let Some(panel) = self.packages_panel() {
                    panel.update(cx, |p, cx| {
                        p.tab = tab;
                        cx.notify();
                    });
                }
            }
            // as the co-host's Install button does: queued, then the window's install flow
            "pkg-request" => catalogs::request_install(rest.trim(), None, None),
            "pkg-link" => {
                tracing::info!(link = ?crate::links::parse(rest.trim()), "CHECK link");
            }
            "pkg-log" => {
                for c in catalogs::catalogs() {
                    tracing::info!(name = c.name, url = c.url, error = ?c.error, packages = c.packages, current = c.current, "CHECK catalog");
                }
                for p in pk::installed().iter() {
                    tracing::info!(
                        id = %p.id(), version = %p.version(), source = %pk::source_label(&p.record.source),
                        withdrawn = ?catalogs::revoked_reason(p.id().as_str(), p.version()),
                        taken_back = ?crate::grants::revoked(p.id().as_str()),
                        granted = ?pk::granted_permissions(p),
                        hosts = ?pk::hosts_of(p.id(), p.version()),
                        "CHECK installed"
                    );
                }
                for u in catalogs::updates() {
                    tracing::info!(id = u.id, newer = u.newer, added = ?u.added_permissions, "CHECK update");
                }
                for (plugin, kinds) in crate::plugin_activity::tallies() {
                    for (k, t) in kinds {
                        tracing::info!(plugin, kind = k.label(), count = t.count, refused = t.refused, "CHECK activity");
                    }
                }
                for n in crate::plugin_activity::notices() {
                    tracing::info!(text = n.text, refused = n.refused, "CHECK notice");
                }
            }
            // helpers (crate::helpers_host)
            "pkg-helpers" => {
                for p in pk::installed().iter() {
                    let id = p.id().as_str();
                    if let Some(st) = crate::helpers_host::status(id) {
                        tracing::info!(id, running = st.running, ready = st.ready, full_access = st.full_access, error = ?st.error, enabled = crate::helpers_host::enabled(id), declared = ?crate::helpers_host::declared(id), settings = ?crate::helpers_host::setting_values(id), "CHECK helper");
                    } else if p.manifest.contents.iter().any(|c| c.kind == lightair_packages::Kind::Helper) {
                        tracing::info!(id, enabled = crate::helpers_host::enabled(id), "CHECK helper not running");
                    }
                }
                for (package, name, c) in crate::helpers_host::commands() {
                    tracing::info!(package, name, command = c.id, title = c.title, "CHECK palette command");
                }
                for (package, name, st) in crate::helpers_host::steps() {
                    tracing::info!(package, name, step = st.id, title = st.title, "CHECK automation step");
                }
            }
            "pkg-command" => {
                let (id, command) = (args.next().unwrap_or_default().to_string(), args.next().unwrap_or_default().to_string());
                std::thread::spawn(move || tracing::info!(result = ?crate::helpers_host::run_command(&id, &command), "CHECK command"));
            }
            "pkg-step" => {
                let (id, step) = (args.next().unwrap_or_default().to_string(), args.next().unwrap_or_default().to_string());
                let params: serde_json::Value = serde_json::from_str(&args.collect::<Vec<_>>().join(" ")).unwrap_or_else(|_| serde_json::json!({}));
                std::thread::spawn(move || {
                    let mut call = params.clone();
                    call["package"] = serde_json::json!(id);
                    call["step"] = serde_json::json!(step);
                    tracing::info!(result = ?crate::helpers_host::api("plugins.step", &call), "CHECK step");
                });
            }
            "pkg-export" => {
                let Some(dir) = self.studio.read(cx).workspace_dir() else { return };
                let workspace = self.studio.read(cx).workspace().clone();
                pk::record_show(&dir, &workspace);
                tracing::info!(result = ?pk::export_show(&dir, std::path::Path::new(rest.trim())), "CHECK export");
            }
            "pkg-bundled" => {
                let show = std::path::PathBuf::from(rest.trim());
                let missing: Vec<(lightair_packages::PackageId, Option<semver::Version>)> = pk::check_show(&show)
                    .map(|(_, res)| res.missing.iter().map(|m| (m.id.clone(), m.locked.as_ref().map(|l| l.version.clone()))).collect())
                    .unwrap_or_default();
                let found = pk::bundled_for(&show, &missing);
                tracing::info!(missing = ?missing, bundled = ?found, "CHECK bundled");
                for path in found {
                    match pk::prepare(&path) {
                        Ok(p) => {
                            let (title, detail) = install_screen(&p);
                            tracing::info!("CHECK install screen: {title} / {}", detail.replace('\n', " | "));
                            tracing::info!(result = ?pk::install(p).map(|i| i.installed.id().to_string()), "CHECK installed from the show");
                        }
                        Err(e) => tracing::warn!("CHECK prepare failed: {e}"),
                    }
                }
            }
            "pkg-remove" => tracing::info!(result = ?lightair_packages::PackageId::new(rest.trim()).map_err(|e| e.to_string()).and_then(|id| pk::remove(&id, None)), "CHECK removed"),
            "pkg-git" => {
                let (url, rev) = (args.next().unwrap_or_default().to_string(), args.next().unwrap_or("main").to_string());
                std::thread::spawn(move || match pk::prepare_git(&url, &rev) {
                    Ok(p) => {
                        let (title, detail) = install_screen(&p);
                        tracing::info!("CHECK install screen: {title} / {}", detail.replace('\n', " | "));
                        tracing::info!(result = ?pk::install(p).map(|i| format!("{} from {}", i.installed.id(), pk::source_label(&i.installed.record.source))), "CHECK installed from git");
                    }
                    Err(e) => tracing::warn!("CHECK git failed: {e}"),
                });
            }
            "pkg-helper-on" | "pkg-helper-off" => crate::helpers_host::set_enabled(rest.trim(), cmd == "pkg-helper-on"),
            "pkg-setting" => {
                let (id, key) = (args.next().unwrap_or_default().to_string(), args.next().unwrap_or_default().to_string());
                let v: serde_json::Value = serde_json::from_str(&args.collect::<Vec<_>>().join(" ")).unwrap_or(serde_json::Value::Null);
                crate::helpers_host::set_setting(&id, &key, v);
            }
            _ => tracing::warn!(cmd, "CHECK unknown command"),
        }
    }
}

/// "just now", "2 min ago", "3 h ago" from seconds since 1970.
fn ago(at: u64) -> String {
    let s = lightair_packages::catalog::now_unix().saturating_sub(at);
    match s {
        0..=59 => "just now".into(),
        60..=3599 => format!("{} min ago", s / 60),
        3600..=86399 => format!("{} h ago", s / 3600),
        _ => format!("{} days ago", s / 86400),
    }
}

impl PackagesPanel {
    pub(super) fn catalog_tab(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let query = self.search.read(cx).value().trim().to_lowercase();
        let offers: Vec<Offer> = catalogs::offers()
            .into_iter()
            .filter(|o| {
                query.is_empty()
                    || [&o.listing.name, &o.listing.summary, &o.listing.publisher, &o.id, &o.catalog_name]
                        .iter()
                        .any(|t| t.to_lowercase().contains(&query))
                    || o.listing.kinds.iter().any(|k| k.contains(&query))
            })
            .collect();
        let installed = pk::by_package();
        let mut list = div().id("pkg-catalog").flex().flex_col().flex_1().min_h_0().overflow_y_scroll();
        list = list.child(
            div().px_3().pt_2().pb_1().child(TextInput::new(&self.search).small().cleanable(true)),
        );
        if !catalogs::has_lightair() {
            list = list.child(
                div()
                    .mx_3()
                    .my_1()
                    .text_size(px(11.))
                    .text_color(tok::text_tertiary())
                    .child("Lightair's catalog isn't set up in this build. Community catalogs you add are listed here."),
            );
        }
        if offers.is_empty() {
            let (title, about) = if catalogs::catalogs().is_empty() {
                ("No catalogs yet", "Add a community catalog below, by its address.")
            } else if query.is_empty() {
                ("Nothing listed yet", if catalogs::fetching() { "Checking the catalogs…" } else { "The catalogs list nothing, or haven't been reached." })
            } else {
                ("Nothing matches", "")
            };
            list = list.child(empty("store", title, about));
        }
        for (i, o) in offers.iter().enumerate() {
            let have = installed.iter().find(|v| v[0].id().as_str() == o.id).map(|v| v[0].clone());
            list = list.child(self.offer_row(i, o, have.as_ref(), cx));
        }
        list = list.child(self.catalogs_section(cx));
        list.into_any_element()
    }

    fn offer_row(&self, i: usize, o: &Offer, have: Option<&Installed>, cx: &mut Context<Self>) -> AnyElement {
        let l = &o.listing;
        let newest = l.versions.iter().filter(|(_, v)| !v.yanked && !v.gone).map(|(k, _)| k.clone()).max_by(|a, b| {
            semver::Version::parse(a).ok().cmp(&semver::Version::parse(b).ok())
        });
        let mut line = Vec::new();
        if !l.publisher.is_empty() {
            line.push(format!("by {}", l.publisher));
        }
        line.push(if o.lightair { "Lightair's catalog".to_string() } else { o.catalog_name.clone() });
        if !l.summary.is_empty() {
            line.push(l.summary.clone());
        }
        let mut about = vec![o.id.clone()];
        if !l.description.is_empty() {
            about.push(l.description.clone());
        }
        if !l.platforms.is_empty() {
            about.push(format!("Works on {}", l.platforms.join(", ")));
        }
        if let Some(r) = &l.repository {
            about.push(format!("Source: {r}"));
        }
        if !o.lightair {
            about.push("Not checked by Lightair".into());
        }
        let action: AnyElement = match (have, &newest) {
            (_, None) => div().text_size(px(11.)).text_color(tok::text_tertiary()).child("Withdrawn").into_any_element(),
            (Some(h), Some(n)) if semver::Version::parse(n).ok().as_ref().is_some_and(|v| v <= h.version()) => {
                div().text_size(px(11.)).text_color(tok::text_tertiary()).child("Installed").into_any_element()
            }
            (have, Some(n)) => {
                let (catalog, id, version) = (o.catalog_url.clone(), o.id.clone(), n.clone());
                Button::new(("pkg-offer-install", i))
                    .label(if have.is_some() { "Update" } else { "Install" })
                    .xsmall()
                    .outline()
                    .on_click(cx.listener(move |this, _, window, cx| {
                        let (catalog, id, version) = (catalog.clone(), id.clone(), version.clone());
                        this.with_shell(cx, |w, cx| w.install_from_catalog(Some(catalog), id, Some(version), window, cx));
                    }))
                    .into_any_element()
            }
        };
        div()
            .id(("pkg-offer", i))
            .flex()
            .items_center()
            .gap_2()
            .px_3()
            .py(px(7.))
            .border_b_1()
            .border_color(tok::border())
            .child(ui::icon(if l.tier >= 3 { "puzzle" } else { "package" }).size(px(16.)).text_color(tok::text_secondary()))
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
                            .child(div().min_w_0().truncate().text_size(px(13.)).text_color(tok::text()).child(l.name.clone()))
                            .children(newest.clone().map(|n| {
                                div().flex_none().text_size(px(11.)).font_family(ui::MONO).text_color(tok::text_tertiary()).child(n)
                            })),
                    )
                    .child(div().truncate().text_size(px(11.)).text_color(tok::text_tertiary()).child(line.join(" · "))),
            )
            .child(ui::info(("pkg-offer-about", i), about.join("\n")))
            .child(div().flex_none().child(action))
            .into_any_element()
    }

    fn catalogs_section(&self, cx: &mut Context<Self>) -> Div {
        let mut section = div()
            .flex()
            .flex_col()
            .gap_1()
            .mx_3()
            .my_3()
            .p_3()
            .rounded(px(8.))
            .bg(tok::bg_raised())
            .child(
                div()
                    .flex()
                    .items_center()
                    .child(div().flex_1().text_size(px(12.)).font_weight(FontWeight::SEMIBOLD).child("Catalogs"))
                    .child(
                        Button::new("pkg-catalogs-refresh")
                            .icon(ui::icon("refresh-cw"))
                            .xsmall()
                            .ghost()
                            .tooltip("Check the catalogs now")
                            .on_click(|_, _, _| catalogs::refresh_now()),
                    ),
            );
        for (i, c) in catalogs::catalogs().into_iter().enumerate() {
            let status = match (&c.error, c.fetched) {
                (Some(e), _) => sentence(e),
                (None, Some(t)) if c.current => format!("Checked {} · {} packages", ago(t), c.packages),
                (None, Some(_)) => "Out of date: its list expired".into(),
                (None, None) => if catalogs::fetching() { "Checking…".into() } else { "Not checked yet".into() },
            };
            let url = c.url.clone();
            section = section.child(
                div()
                    .id(("pkg-catalog-row", i))
                    .group("pkg-catalog-row")
                    .flex()
                    .items_center()
                    .gap_2()
                    .py_1()
                    .child(ui::icon(if c.lightair { "badge-check" } else { "store" }).size(px(14.)).text_color(tok::text_secondary()))
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .flex_1()
                            .min_w_0()
                            .child(div().truncate().text_size(px(12.)).text_color(tok::text()).child(c.name.clone()))
                            .child(
                                div()
                                    .truncate()
                                    .text_size(px(11.))
                                    .text_color(if c.error.is_some() { tok::warn() } else { tok::text_tertiary() })
                                    .child(status),
                            ),
                    )
                    .child(ui::info(("pkg-catalog-about", i), format!("{}\nKey: {}{}", c.url, c.fingerprint, if c.lightair { "" } else { "\nNot checked by Lightair" })))
                    .when(!c.lightair, |d| {
                        d.child(
                            ui::icon_button(("pkg-catalog-remove", i), "trash", tok::text_secondary())
                                .invisible()
                                .group_hover("pkg-catalog-row", |s| s.visible())
                                .tooltip(ui::tip("Remove (what you installed from it stays)"))
                                .on_click(move |_, _, _| catalogs::remove(&url)),
                        )
                    }),
            );
        }
        section.child(
            div()
                .flex()
                .items_center()
                .gap_1p5()
                .pt_1()
                .child(div().flex_1().child(TextInput::new(&self.catalog_url).small()))
                .child(Button::new("pkg-catalog-add").label("Add catalog").xsmall().outline().on_click(cx.listener(
                    |this, _, window, cx| {
                        let url = this.catalog_url.read(cx).value().trim().to_string();
                        if url.is_empty() {
                            return;
                        }
                        this.catalog_url.update(cx, |s, cx| s.set_value("", window, cx));
                        this.with_shell(cx, |w, cx| w.add_catalog_from_url(url, window, cx));
                    },
                ))),
        )
    }

    pub(super) fn updates_tab(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let updates = catalogs::updates();
        if updates.is_empty() {
            let about = if catalogs::catalogs().is_empty() { "Packages from catalogs show their new versions here." } else { "Everything from the catalogs is up to date." };
            return empty("refresh-cw", "No updates", about).into_any_element();
        }
        let mut list = div().id("pkg-updates").flex().flex_col().flex_1().min_h_0().overflow_y_scroll().child(
            div().flex().items_center().px_3().py_2().child(div().flex_1().text_size(px(11.)).text_color(tok::text_tertiary()).child(
                "Shows keep the versions they were saved with until you update them.",
            )).child(
                Button::new("pkg-update-all").label("Update all").xsmall().outline().on_click(cx.listener(|this, _, window, cx| {
                    this.with_shell(cx, |w, cx| w.update_all(window, cx));
                })),
            ),
        );
        for (i, u) in updates.into_iter().enumerate() {
            let more = !u.added_permissions.is_empty();
            let asks: Vec<String> = u
                .added_permissions
                .iter()
                .map(|p| Permission::parse(p).map_or_else(|_| p.clone(), |q| q.describe()))
                .collect();
            let (catalog, id, version) = (u.catalog_url.clone(), u.id.clone(), u.newer.clone());
            list = list.child(
                div()
                    .id(("pkg-update", i))
                    .flex()
                    .items_center()
                    .gap_2()
                    .px_3()
                    .py(px(7.))
                    .border_b_1()
                    .border_color(tok::border())
                    .child(ui::icon("package").size(px(16.)).text_color(tok::text_secondary()))
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .flex_1()
                            .min_w_0()
                            .child(div().truncate().text_size(px(13.)).text_color(tok::text()).child(u.name.clone()))
                            .child(div().truncate().text_size(px(11.)).text_color(tok::text_tertiary()).child(format!(
                                "{} → {} · {}",
                                u.installed, u.newer, u.catalog_name
                            )))
                            .when(more, |d| {
                                d.child(div().text_size(px(11.)).text_color(tok::warn()).child(format!("Asks for more: {}", asks.join(", "))))
                            }),
                    )
                    .child(Button::new(("pkg-update-one", i)).label("Update").xsmall().outline().on_click(cx.listener(
                        move |this, _, window, cx| {
                            let (catalog, id, version) = (catalog.clone(), id.clone(), version.clone());
                            this.with_shell(cx, |w, cx| w.install_from_catalog(Some(catalog), id, Some(version), window, cx));
                        },
                    ))),
            );
        }
        list.into_any_element()
    }

    /// An installed package's permissions, each in plain words with Take back / Allow again.
    pub(super) fn permissions(&self, i: usize, p: &Installed, cx: &mut Context<Self>) -> Option<Div> {
        let declared = &p.manifest.permissions;
        if declared.is_empty() {
            return None;
        }
        let id = p.id().as_str().to_string();
        let taken = crate::grants::revoked(&id);
        let mut list = div()
            .flex()
            .flex_col()
            .gap_0p5()
            .pl(px(42.))
            .pr_3()
            .py_2()
            .border_b_1()
            .border_color(tok::border())
            .bg(tok::bg_raised())
            .child(div().text_size(px(11.)).font_weight(FontWeight::SEMIBOLD).text_color(tok::text_secondary()).child("Permissions"));
        for (j, perm) in declared.iter().enumerate() {
            let words = Permission::parse(perm).map_or_else(|_| perm.clone(), |q| q.describe());
            let off = taken.contains(perm);
            let (id, perm) = (id.clone(), perm.clone());
            list = list.child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .h(px(24.))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_size(px(12.))
                            .text_color(if off { tok::text_tertiary() } else { tok::text() })
                            .when(off, |d| d.line_through())
                            .child(words),
                    )
                    .child(
                        Button::new(("pkg-perm", i * 1000 + j))
                            .label(if off { "Allow again" } else { "Take back" })
                            .xsmall()
                            .ghost()
                            .on_click(cx.listener(move |_, _, _, cx| {
                                if off {
                                    crate::grants::grant(&id, &perm);
                                } else {
                                    crate::grants::revoke(&id, &perm);
                                }
                                cx.notify();
                            })),
                    ),
            );
        }
        Some(list)
    }

    /// The missing packages the show's lock says came from a catalog Lightair knows.
    pub(super) fn missing_from_catalogs(&self) -> Vec<(String, String, String)> {
        let known: Vec<String> = catalogs::catalogs().into_iter().map(|c| c.url).collect();
        self.show
            .missing
            .iter()
            .filter_map(|m| {
                let locked = m.locked.as_ref()?;
                match &locked.source {
                    lightair_packages::Source::Catalog { catalog, .. } if known.contains(catalog) => {
                        Some((catalog.clone(), m.id.to_string(), locked.version.to_string()))
                    }
                    _ => None,
                }
            })
            .collect()
    }
}
