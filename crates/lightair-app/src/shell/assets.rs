//! Assets and prefabs (docs/07-scenes-objects-canvas.md, "Assets"): the workspace's `assets` folder
//! as a panel. Files dropped from Finder are copied in; images, video and sound drag onto the
//! canvas or into the Hierarchy as objects. A object dragged here (or "Save as prefab") becomes a
//! prefab: it and everything under it, reusable. Placed prefabs stay linked: an instance can
//! send its changes to the prefab (and every other instance), or go back to it.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use gpui_kit::component::menu::{ContextMenuExt, PopupMenuItem};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;
use lightair_model::{Clip, Command, Id, Source, Object, Layout, Workspace};
use serde_json::json;

use super::StudioWindow;
use super::hierarchy::TreeDrop;
use crate::rows::{DraggedRow, RowTarget};
use crate::ui::{self, tok};

#[derive(Clone, Copy, PartialEq, Debug)]
pub(crate) enum AssetKind {
    Prefab,
    /// A whole scene (`.scene`), added back as a new scene.
    Scene,
    Image,
    Video,
    Audio,
    /// An avatar model: VRM (or a GLB with VRM data), Live2D (its `.model3.json`, or the folder
    /// holding one), Inochi2D, Umamo.
    Avatar,
    /// A folder of assets.
    Folder,
    Other,
}

/// The Live2D model a folder holds (its `.model3.json`), if it holds one.
pub(crate) fn live2d_model_in(dir: &Path) -> Option<PathBuf> {
    if !dir.is_dir() {
        return None;
    }
    let mut found: Vec<PathBuf> = std::fs::read_dir(dir)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.file_name().is_some_and(|n| n.to_string_lossy().to_lowercase().ends_with(".model3.json")))
        .collect();
    found.sort();
    found.into_iter().next()
}

impl AssetKind {
    pub(crate) fn of(path: &Path) -> AssetKind {
        if path.is_dir() {
            return if live2d_model_in(path).is_some() { AssetKind::Avatar } else { AssetKind::Folder };
        }
        let name = path.file_name().map(|n| n.to_string_lossy().to_lowercase()).unwrap_or_default();
        if name.ends_with(".model3.json") {
            return AssetKind::Avatar;
        }
        let ext = path.extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default();
        match ext.as_str() {
            "prefab" => AssetKind::Prefab,
            "scene" => AssetKind::Scene,
            "vrm" | "glb" | "inp" | "inx" | "uma" | "lairava" => AssetKind::Avatar,
            "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp" | "tga" | "heic" => AssetKind::Image,
            "mp4" | "mov" | "m4v" | "mkv" | "webm" | "avi" => AssetKind::Video,
            "mp3" | "wav" | "m4a" | "aac" | "flac" | "ogg" | "aiff" => AssetKind::Audio,
            _ => AssetKind::Other,
        }
    }

    fn icon(self) -> &'static str {
        match self {
            AssetKind::Prefab => "package",
            AssetKind::Scene => "layout-dashboard",
            AssetKind::Image => "image",
            AssetKind::Video => "film",
            AssetKind::Audio => "music",
            AssetKind::Avatar => "person-standing",
            AssetKind::Folder => "folder",
            AssetKind::Other => "file",
        }
    }
}

/// A file in the assets folder.
#[derive(Clone, Debug)]
pub(crate) struct Asset {
    pub path: PathBuf,
    pub kind: AssetKind,
}

impl Asset {
    /// The model file an avatar asset loads (a Live2D folder's `.model3.json`).
    pub(crate) fn model_file(&self) -> PathBuf {
        live2d_model_in(&self.path).unwrap_or_else(|| self.path.clone())
    }

    fn name(&self) -> String {
        let stem = self.path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
        match self.kind {
            AssetKind::Prefab | AssetKind::Scene => stem,
            _ if self.path.is_dir() => self.path.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or(stem),
            _ => self.path.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or(stem),
        }
    }
}

/// An asset being dragged. Rendered as the drag preview.
#[derive(Clone)]
pub(crate) struct DraggedAsset(pub Asset);

impl Render for DraggedAsset {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
            .flex()
            .items_center()
            .gap_2()
            .h(px(30.))
            .px_3()
            .rounded(px(6.))
            .bg(tok::bg_raised())
            .border_1()
            .border_color(tok::accent_primary())
            .text_color(tok::text())
            .shadow_md()
            .child(ui::icon(self.0.kind.icon()).size(px(14.)).text_color(tok::text_secondary()))
            .child(self.0.name())
    }
}

/// Where a placed asset goes.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Placement {
    /// The middle of the canvas, at the top level, in front.
    Centre,
    /// Centred on a canvas point in a canvas, at the top level, in front.
    At(Id, (f32, f32)),
    /// In the Hierarchy: inside `parent`, at `index` among its children.
    Into(Option<Id>, Option<usize>),
}

/// A name in `dir` like `name` that isn't taken ("Logo.png", "Logo 2.png", …).
pub(super) fn free_name(dir: &Path, stem: &str, ext: &str) -> PathBuf {
    let dot = if ext.is_empty() { String::new() } else { format!(".{ext}") };
    let first = dir.join(format!("{stem}{dot}"));
    if !first.exists() {
        return first;
    }
    (2..).map(|n| dir.join(format!("{stem} {n}{dot}"))).find(|p| !p.exists()).unwrap_or(first)
}

impl StudioWindow {
    /// The assets folder's files (read at most once a second).
    pub(super) fn assets(&self, cx: &App) -> Vec<Asset> {
        let mut cache = self.asset_cache.borrow_mut();
        if cache.0.is_some_and(|t| t.elapsed() < Duration::from_secs(1)) {
            return cache.1.clone();
        }
        let mut list: Vec<Asset> = self
            .asset_dir(cx)
            .and_then(|dir| std::fs::read_dir(dir).ok())
            .map(|entries| {
                entries
                    .flatten()
                    .map(|e| e.path())
                    .filter(|p| !p.file_name().is_some_and(|n| n.to_string_lossy().starts_with('.')))
                    .map(|path| Asset { kind: AssetKind::of(&path), path })
                    .collect()
            })
            .unwrap_or_default();
        // Folders, then prefabs, then by name.
        list.sort_by_key(|a| (a.kind != AssetKind::Folder, a.kind != AssetKind::Prefab, a.name().to_lowercase()));
        *cache = (Some(Instant::now()), list.clone());
        list
    }

    pub(super) fn refresh_assets(&self) {
        self.asset_cache.borrow_mut().0 = None;
    }

    /// The folder Assets shows: the assets folder, or a folder in it.
    pub(super) fn asset_dir(&self, cx: &App) -> Option<PathBuf> {
        let root = self.studio.read(cx).assets_dir()?;
        Some(self.asset_folder.clone().filter(|f| f.starts_with(&root) && *f != root && f.is_dir()).unwrap_or(root))
    }

    fn open_asset_folder(&mut self, folder: Option<PathBuf>, cx: &mut Context<Self>) {
        self.asset_folder = folder;
        self.selected_asset = None;
        self.refresh_assets();
        cx.notify();
    }

    pub(super) fn assets_panel(&self, _workspace: &Workspace, cx: &mut Context<Self>) -> impl IntoElement {
        let assets = self.assets(cx);
        let selected = self.selected_asset.clone();
        let shell = cx.entity().downgrade();
        let mut grid = div().flex().flex_wrap().gap_2().p_2().content_start();
        for (i, asset) in assets.iter().enumerate() {
            let is_sel = selected.as_ref() == Some(&asset.path);
            let name = asset.name();
            let thumb = div()
                .size(px(64.))
                .rounded(px(6.))
                .overflow_hidden()
                .bg(tok::bg_raised())
                .flex()
                .items_center()
                .justify_center()
                .child(match asset.kind {
                    AssetKind::Image => {
                        img(asset.path.clone()).size_full().object_fit(ObjectFit::Cover).into_any_element()
                    }
                    kind => ui::icon(kind.icon())
                        .size(px(26.))
                        .text_color(if kind == AssetKind::Prefab { tok::prefab() } else { tok::text_secondary() })
                        .into_any_element(),
                });
            let (a1, a2, a3) = (asset.clone(), asset.clone(), asset.clone());
            let menu_shell = shell.clone();
            grid = grid.child(
                div()
                    .id(("asset", i))
                    .w(px(80.))
                    .p_1()
                    .flex()
                    .flex_col()
                    .items_center()
                    .gap_1()
                    .rounded(px(6.))
                    .cursor_pointer()
                    .when(is_sel, |d| d.bg(tok::bg_selected()))
                    .when(!is_sel, |d| d.hover(|d| d.bg(tok::bg_raised())))
                    .on_click(cx.listener(move |this, ev: &ClickEvent, _, cx| {
                        this.selected_asset = Some(a1.path.clone());
                        if ev.click_count() == 2 {
                            if a1.kind == AssetKind::Folder {
                                return this.open_asset_folder(Some(a1.path.clone()), cx);
                            }
                            this.place_asset(&a1, Placement::Centre, cx);
                        }
                        cx.notify();
                    }))
                    // files from Finder onto a folder: into it
                    .when(asset.kind == AssetKind::Folder, |d| {
                        let folder = asset.path.clone();
                        d.drag_over::<ExternalPaths>(|s, _, _, _| s.bg(tok::bg_selected()))
                            .on_drop(cx.listener(move |this, paths: &ExternalPaths, window, cx| {
                                cx.stop_propagation();
                                this.drop_files(paths.paths(), super::file_drop::DropTo::Assets(folder.clone()), window, cx);
                            }))
                    })
                    .on_drag(DraggedAsset(a2), |a, _, _, cx| cx.new(|_| a.clone()))
                    .child(thumb)
                    .child(
                        div()
                            .w_full()
                            .text_center()
                            .text_size(px(11.))
                            .overflow_hidden()
                            .text_ellipsis()
                            .whitespace_nowrap()
                            .text_color(tok::text_secondary())
                            .child(name),
                    )
                    .context_menu(move |menu, _, _| {
                        let (place, reveal) = (menu_shell.clone(), a3.path.clone());
                        let asset = a3.clone();
                        let label = if asset.kind == AssetKind::Scene { "Add as a scene" } else { "Add to scene" };
                        menu.item(PopupMenuItem::new(label).icon(ui::icon("plus")).on_click(
                            move |_, _, cx| {
                                if let Some(s) = place.upgrade() {
                                    s.update(cx, |w, cx| w.place_asset(&asset, Placement::Centre, cx));
                                }
                            },
                        ))
                        .separator()
                        .item(
                            PopupMenuItem::new("Reveal in Finder")
                                .icon(ui::icon("folder-open"))
                                .on_click(move |_, _, cx| cx.reveal_path(&reveal)),
                        )
                    }),
            );
        }
        let empty = assets.is_empty();
        // in a folder: back up to the one holding it
        let root = self.studio.read(cx).assets_dir();
        let here = self.asset_dir(cx);
        let crumb = here.filter(|h| Some(h) != root.as_ref()).map(|h| {
            let up = h.parent().map(Path::to_path_buf).filter(|p| Some(p) != root.as_ref());
            div()
                .id("assets-up")
                .flex()
                .items_center()
                .gap_1()
                .px_2()
                .pt_2()
                .text_size(px(12.))
                .text_color(tok::text_secondary())
                .cursor_pointer()
                .hover(|d| d.text_color(tok::text()))
                .child(ui::icon("chevron-left").size(px(13.)))
                .child(h.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default())
                .on_click(cx.listener(move |this, _, _, cx| this.open_asset_folder(up.clone(), cx)))
        });
        div()
            .id("assets-panel")
            .size_full()
            .flex()
            .flex_col()
            .overflow_y_scroll()
            // From Finder: copied in, into the folder shown.
            .drag_over::<ExternalPaths>(|s, _, _, _| s.bg(tok::bg_raised()))
            .on_drop(cx.listener(|this, paths: &ExternalPaths, window, cx| {
                let Some(folder) = this.asset_dir(cx) else { return };
                this.drop_files(paths.paths(), super::file_drop::DropTo::Assets(folder), window, cx);
            }))
            // A object from the Hierarchy: saved as a prefab.
            .drag_over::<DraggedRow>(|s, _, _, _| s.bg(tok::bg_raised()))
            .on_drop(cx.listener(|this, row: &DraggedRow, _, cx| match row.target {
                RowTarget::Object { object, .. } => this.save_prefab(object, cx),
                RowTarget::Scene(scene) => this.save_scene_asset(scene, cx),
                RowTarget::Strip(_) | RowTarget::MoveToFolder(_) | RowTarget::RenameFolder(_) => {}
            }))
            .on_click(cx.listener(|this, _, _, cx| {
                this.selected_asset = None;
                cx.notify();
            }))
            .children(crumb)
            .child(grid)
            .when(empty, |d| {
                d.child(
                    div()
                        .px_4()
                        .py_2()
                        .text_size(px(12.))
                        .text_color(tok::text_tertiary())
                        .child(
                            "Drop images, video, sound or avatars here. Drag an object from the Hierarchy to make a prefab, or a scene to keep it.",
                        ),
                )
            })
    }

    /// Copies files into the assets folder (new names when taken) and returns where they went.
    pub(super) fn import_files(&mut self, paths: &[PathBuf], cx: &mut Context<Self>) -> Vec<PathBuf> {
        let Some(dir) = self.studio.read(cx).assets_dir() else { return vec![] };
        let mut out = Vec::new();
        for path in paths.iter().filter(|p| p.is_file()) {
            // Already in the folder: nothing to copy.
            if path.parent() == Some(dir.as_path()) {
                out.push(path.clone());
                continue;
            }
            let stem = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| "Asset".into());
            let ext = path.extension().map(|e| e.to_string_lossy().into_owned()).unwrap_or_default();
            let to = free_name(&dir, &stem, &ext);
            match std::fs::copy(path, &to) {
                Ok(_) => out.push(to),
                Err(e) => tracing::warn!(%e, from = %path.display(), "could not copy into assets"),
            }
        }
        self.refresh_assets();
        cx.notify();
        out
    }

    /// Asks for files and copies them in.
    pub(super) fn import_assets(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let paths =
            cx.prompt_for_paths(PathPromptOptions { files: true, directories: false, multiple: true, prompt: None });
        cx.spawn_in(window, async move |this, cx| {
            let Ok(Ok(Some(paths))) = paths.await else { return };
            let _ = this.update(cx, |this, cx| this.import_files(&paths, cx));
        })
        .detach();
    }

    pub(super) fn reveal_assets(&self, cx: &mut Context<Self>) {
        if let Some(dir) = self.studio.read(cx).assets_dir() {
            cx.reveal_path(&dir);
        }
    }

    /// Saves `object` and everything under it as `<name>.prefab`, and links it to the prefab.
    pub(super) fn save_prefab(&mut self, object: Id, cx: &mut Context<Self>) {
        let studio = self.studio.read(cx);
        let Some(dir) = studio.assets_dir() else { return };
        let Some(scene) = studio.selected_scene else { return };
        let Some(mut clip) = Clip::copy(studio.workspace(), scene, &[object]) else { return };
        let name = clip.objects[0].name.clone();
        clip.objects[0].prefab = None;
        clip.objects[0].prefab_id = None;
        // its own id, kept in the file: instances stay linked when it's renamed
        let prefab_id = lightair_model::Id::new();
        clip.prefab_id = Some(prefab_id);
        let path = free_name(&dir, &name, "prefab");
        let Ok(text) = serde_json::to_string_pretty(&clip) else { return };
        if let Err(e) = std::fs::write(&path, text) {
            tracing::warn!(%e, "could not save the prefab");
            return;
        }
        let file = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        self.exec(Command::SetObjectPrefab { scene, object, prefab: Some(file), prefab_id: Some(prefab_id) }, cx);
        self.selected_asset = Some(path);
        self.refresh_assets();
    }

    /// Saves a whole scene as `<name>.scene`: its objects and sources, to add again later or in
    /// another workspace.
    pub(super) fn save_scene_asset(&mut self, scene: Id, cx: &mut Context<Self>) {
        let studio = self.studio.read(cx);
        let Some(dir) = studio.assets_dir() else { return };
        let Some(asset) = lightair_model::SceneAsset::of(studio.workspace(), scene) else { return };
        let path = free_name(&dir, &asset.name, "scene");
        let Ok(text) = serde_json::to_string_pretty(&asset) else { return };
        if let Err(e) = std::fs::write(&path, text) {
            tracing::warn!(%e, "could not save the scene");
            return;
        }
        self.selected_asset = Some(path);
        self.refresh_assets();
        cx.notify();
    }

    /// Adds a scene asset as a new scene, after the selected one, and selects it.
    pub(super) fn add_scene_asset(&mut self, asset: &Asset, cx: &mut Context<Self>) {
        let Some(saved) = std::fs::read_to_string(&asset.path)
            .ok()
            .and_then(|t| serde_json::from_str::<lightair_model::SceneAsset>(&t).ok())
        else {
            self.studio.update(cx, |s, _| s.issue = Some("That scene file couldn't be read.".into()));
            return;
        };
        let studio = self.studio.read(cx);
        let workspace = studio.workspace().clone();
        let index = studio.selected_scene.and_then(|s| workspace.scene_index(s)).map(|i| i + 1);
        let (commands, id) = saved.add(&workspace, index);
        self.exec(Command::Batch { label: format!("Add scene {}", saved.name), commands }, cx);
        self.studio.update(cx, |s, cx| {
            s.select_scene(id);
            cx.notify();
        });
    }

    /// A prefab file's contents. One saved before prefabs had ids is given one now (and saved
    /// with it), so what's placed from it from here on links by it.
    fn read_prefab(&self, file: &str, cx: &App) -> Option<Clip> {
        let path = self.studio.read(cx).assets_dir()?.join(file);
        let mut clip: Clip = serde_json::from_str(&std::fs::read_to_string(&path).ok()?).ok()?;
        if clip.prefab_id.is_none() {
            clip.prefab_id = Some(lightair_model::Id::new());
            if let Ok(text) = serde_json::to_string_pretty(&clip) {
                let _ = std::fs::write(&path, text);
            }
        }
        Some(clip)
    }

    /// An instance's prefab: its file where it was last, else (renamed) the file in the assets
    /// folder with its id; its path and contents. An instance from before prefabs had ids goes
    /// by its file name alone.
    fn find_prefab(&self, object: &Object, cx: &App) -> Option<(std::path::PathBuf, Clip)> {
        let dir = self.studio.read(cx).assets_dir()?;
        let read = |p: &Path| -> Option<Clip> { serde_json::from_str(&std::fs::read_to_string(p).ok()?).ok() };
        if let Some(file) = &object.prefab
            && let Some(clip) = read(&dir.join(file))
            && (object.prefab_id.is_none() || clip.prefab_id == object.prefab_id)
        {
            return Some((dir.join(file), clip));
        }
        let id = object.prefab_id?;
        std::fs::read_dir(&dir).ok()?.flatten().map(|e| e.path()).filter(|p| p.extension().is_some_and(|x| x == "prefab")).find_map(|p| {
            let clip = read(&p)?;
            (clip.prefab_id == Some(id)).then_some((p, clip))
        })
    }

    /// Adds an asset to the scene: a prefab instance, or a object for an image, video, sound or
    /// avatar; a folder's assets in order; a scene asset as a new scene.
    pub(super) fn place_asset(&mut self, asset: &Asset, at: Placement, cx: &mut Context<Self>) {
        self.place_assets(std::slice::from_ref(asset), at, cx);
    }

    /// Adds assets in order, as one undo step, and selects the last; a scene asset becomes a new
    /// scene of its own.
    pub(super) fn place_assets(&mut self, assets: &[Asset], at: Placement, cx: &mut Context<Self>) {
        // a folder: what's in it that can be placed
        let assets: Vec<Asset> = assets
            .iter()
            .flat_map(|a| match a.kind {
                AssetKind::Folder => super::file_drop::placeable_in(&a.path).into_iter().map(|path| Asset { kind: AssetKind::of(&path), path }).collect(),
                _ => vec![a.clone()],
            })
            .collect();
        let mut commands = Vec::new();
        let mut last = None;
        let mut labels = Vec::new();
        for (n, asset) in assets.iter().enumerate() {
            if asset.kind == AssetKind::Scene {
                self.add_scene_asset(asset, cx);
                continue;
            }
            // several into one place: one after another, each in front of the last
            let at = match at {
                Placement::Into(p, Some(i)) => Placement::Into(p, Some(i + n)),
                other => other,
            };
            if let Some((c, root, label)) = self.asset_commands(asset, at, cx) {
                commands.extend(c);
                last = root.or(last);
                labels.push(label);
            }
        }
        if commands.is_empty() {
            return;
        }
        if let Placement::Into(Some(p), _) = at {
            self.collapsed_objects.remove(&p);
        }
        let label = if labels.len() == 1 { labels.remove(0) } else { format!("Add {} files", labels.len()) };
        self.exec(Command::Batch { label, commands }, cx);
        self.select_object(last, cx);
    }

    /// The commands that add one asset (not a scene or folder), the object they add, and a label.
    fn asset_commands(&mut self, asset: &Asset, at: Placement, cx: &mut Context<Self>) -> Option<(Vec<Command>, Option<Id>, String)> {
        let studio = self.studio.read(cx);
        let workspace = studio.workspace().clone();
        let scene = studio.selected_scene.and_then(|id| workspace.scene(id)).cloned()?;
        let (parent, index) = match at {
            Placement::Into(p, i) => (p, i),
            _ => (None, None),
        };
        // Where the middle goes (canvas terms), per canvas.
        let centre = |canvas: &lightair_model::Canvas| match at {
            Placement::At(f, p) if f == canvas.id => p,
            _ => (canvas.width as f32 / 2.0, canvas.height as f32 / 2.0),
        };
        let (commands, root, label) = match asset.kind {
            AssetKind::Prefab => {
                let file = asset.path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                let mut clip = self.read_prefab(&file, cx)?;
                // Move the prefab's root so its frame is centred where it's going.
                if let Some(root) = clip.objects.first_mut() {
                    for f in &workspace.canvases {
                        if let Some(lo) = root.layouts.get_mut(&f.id)
                            && matches!(at, Placement::At(..))
                        {
                            let (cx_, cy_) = centre(f);
                            lo.x = cx_ - lo.width / 2.0;
                            lo.y = cy_ - lo.height / 2.0;
                        }
                    }
                }
                let (commands, root) = clip.instantiate(&workspace, scene.id, &file, parent, index);
                (commands, root, format!("Place {}", asset.name()))
            }
            AssetKind::Image | AssetKind::Video | AssetKind::Audio | AssetKind::Avatar => {
                let path = asset.path.to_string_lossy().into_owned();
                let mut name = asset.path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
                let (kind, settings, size) = match asset.kind {
                    AssetKind::Image => ("image_source", json!({"file": path}), (640.0, 360.0)),
                    // an avatar: kept in the avatar library too, with its look
                    AssetKind::Avatar => {
                        let model = asset.model_file();
                        let entry = self.avatar_library.for_file(&model);
                        self.avatar_library.save();
                        let e = self.avatar_library.get(entry).cloned()?;
                        name = e.name.clone();
                        let mut base = json!({"file": e.file, "device": self.face_camera(cx), "width": 1080, "height": 1080, "framing": "bust", "mirror": true, "library": e.id});
                        // a PNGtuber: listening to a mic, its whole picture shown
                        if crate::avatar_library::kind_of(&e.file) == "PNGtuber" {
                            base["mic"] = json!(self.default_mic(cx));
                            base["framing"] = json!("full");
                            base["device"] = json!("");
                        }
                        let settings = crate::avatar_library::with_look(&base, &e.look);
                        let size = (settings["width"].as_f64().unwrap_or(1080.0) as f32, settings["height"].as_f64().unwrap_or(1080.0) as f32);
                        (super::avatar_props::KIND, settings, size)
                    }
                    _ => (
                        "ffmpeg_source",
                        json!({"local_file": path, "is_local_file": true, "looping": true, "restart_on_activate": true}),
                        (1280.0, 720.0),
                    ),
                };
                let source = Source::new(&name, kind, settings);
                let layouts: Vec<(Id, Layout)> = workspace
                    .canvases
                    .iter()
                    .filter_map(|f| {
                        let (w, h) = size;
                        let s = (f.width as f32 / w).min(f.height as f32 / h).min(1.0);
                        let (lw, lh) = (w * s, h * s);
                        let (cx_, cy_) = centre(f);
                        let world = Layout::boxed(cx_ - lw / 2.0, cy_ - lh / 2.0, lw, lh);
                        Some((f.id, world.to_local(&scene.frame_of(parent, f.id)?)))
                    })
                    .collect();
                let mut object = Object::new(&name, source.id, layouts);
                object.parent = parent;
                let id = object.id;
                (
                    vec![Command::AddSource { source }, Command::AddObject { scene: scene.id, object, index }],
                    Some(id),
                    format!("Add {name}"),
                )
            }
            AssetKind::Scene | AssetKind::Folder | AssetKind::Other => return None,
        };
        Some((commands, root, label))
    }

    /// Drops onto the canvas: assets and files from Finder land where they're dropped.
    pub(super) fn drop_asset_on_canvas(&mut self, asset: &Asset, window: &Window, cx: &mut Context<Self>) {
        let workspace = self.studio.read(cx).workspace().clone();
        let at = window.mouse_position();
        let placement = self
            .stage_at(&workspace, at)
            .and_then(|f| self.to_canvas(&workspace, f, at).map(|p| Placement::At(f, p)))
            .unwrap_or(Placement::Centre);
        self.place_asset(asset, placement, cx);
    }

    pub(super) fn drop_files_on_canvas(&mut self, paths: &[PathBuf], window: &mut Window, cx: &mut Context<Self>) {
        let workspace = self.studio.read(cx).workspace().clone();
        let at = window.mouse_position();
        let placement = self
            .stage_at(&workspace, at)
            .and_then(|f| self.to_canvas(&workspace, f, at).map(|p| Placement::At(f, p)))
            .unwrap_or(Placement::Centre);
        self.drop_files(paths, super::file_drop::DropTo::Canvas(placement), window, cx);
    }

    /// Where something dropped on a Hierarchy row goes (`None`: below the rows).
    pub(super) fn tree_placement(&self, target: Option<(Id, TreeDrop)>, cx: &App) -> Option<Placement> {
        let workspace = self.studio.read(cx).workspace().clone();
        let scene = self.studio.read(cx).selected_scene.and_then(|id| workspace.scene(id))?;
        Some(match target {
            None => Placement::Into(None, Some(0)),
            Some((t, TreeDrop::Into)) => Placement::Into(Some(t), None),
            Some((t, zone)) => {
                let l = scene.object(t)?;
                let siblings: Vec<Id> = scene.children(l.parent).map(|s| s.id).collect();
                let at = siblings.iter().position(|s| *s == t).unwrap_or(0);
                Placement::Into(l.parent, Some(if zone == TreeDrop::Above { at + 1 } else { at }))
            }
        })
    }

    /// Drops onto a Hierarchy row (`None`: below the rows).
    pub(super) fn drop_asset_on_tree(&mut self, asset: &Asset, target: Option<(Id, TreeDrop)>, cx: &mut Context<Self>) {
        if let Some(placement) = self.tree_placement(target, cx) {
            self.place_asset(asset, placement, cx);
        }
    }

    // ------------------------------------------------------------ prefab instances

    /// The bar on an instance's Inspector: which prefab, and what to do with the link.
    pub(super) fn prefab_bar(&self, scene: Id, object: &Object, cx: &mut Context<Self>) -> Option<AnyElement> {
        let file = object.prefab.clone()?;
        let found = self.find_prefab(object, cx);
        let shown = found.as_ref().map_or(Path::new(&file).to_path_buf(), |(p, _)| p.clone());
        let name = shown.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or(file.clone());
        let missing = found.is_none();
        let id = object.id;
        let button = |key: &'static str, label: &'static str, tip: &'static str| {
            gpui_kit::component::button::Button::new(key).label(label).tooltip(tip)
        };
        use gpui_kit::component::button::ButtonVariants;
        use gpui_kit::component::{Disableable, Sizable};
        Some(
            div()
                .flex()
                .flex_col()
                .gap_1p5()
                .p_2()
                .rounded(px(6.))
                .bg(tok::bg_raised())
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap_1p5()
                        .child(ui::icon("package").size(px(14.)).text_color(tok::prefab()))
                        .child(div().flex_1().text_size(px(12.)).child(format!("Prefab · {name}")))
                        .when(missing, |d| {
                            d.child(div().text_size(px(11.)).text_color(tok::text_tertiary()).child("file missing"))
                        }),
                )
                .child(
                    div()
                        .flex()
                        .gap_1()
                        .child(
                            button("prefab-apply", "Apply", "Save this into the prefab and update every instance")
                                .xsmall()
                                .outline()
                                .disabled(missing)
                                .on_click(cx.listener(move |this, _, _, cx| this.apply_prefab(scene, id, cx))),
                        )
                        .child(
                            button("prefab-revert", "Revert", "Back to the prefab (keeps its place and name)")
                                .xsmall()
                                .ghost()
                                .disabled(missing)
                                .on_click(cx.listener(move |this, _, _, cx| this.revert_prefab(scene, id, cx))),
                        )
                        .child(
                            button("prefab-unpack", "Unpack", "Ordinary objects, no longer linked")
                                .xsmall()
                                .ghost()
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.exec(Command::SetObjectPrefab { scene, object: id, prefab: None, prefab_id: None }, cx)
                                })),
                        ),
                )
                .into_any_element(),
        )
    }

    /// Back to the prefab: the instance is rebuilt from the file.
    pub(super) fn revert_prefab(&mut self, scene: Id, root: Id, cx: &mut Context<Self>) {
        let workspace = self.studio.read(cx).workspace().clone();
        let Some(object) = workspace.scene(scene).and_then(|s| s.object(root)).cloned() else { return };
        let Some((_, clip)) = self.find_prefab(&object, cx) else { return };
        let (commands, new_root) = clip.replace_instance(&workspace, scene, root);
        if commands.is_empty() {
            return;
        }
        self.exec(Command::Batch { label: "Revert to prefab".into(), commands }, cx);
        self.select_object(new_root, cx);
    }

    /// Saves this instance into its prefab, then rebuilds every other instance from it.
    pub(super) fn apply_prefab(&mut self, scene: Id, root: Id, cx: &mut Context<Self>) {
        let studio = self.studio.read(cx);
        let workspace = studio.workspace().clone();
        let Some(object) = workspace.scene(scene).and_then(|s| s.object(root)).cloned() else { return };
        let Some(file) = object.prefab.clone() else { return };
        // into its file, wherever it is now (renamed: found by its id)
        let path = match self.find_prefab(&object, cx) {
            Some((p, _)) => p,
            None => match self.studio.read(cx).assets_dir() {
                Some(dir) => dir.join(&file),
                None => return,
            },
        };
        let Some(mut clip) = Clip::copy(&workspace, scene, &[root]) else { return };
        clip.objects[0].prefab = None;
        clip.objects[0].prefab_id = None;
        clip.prefab_id = object.prefab_id;
        let Ok(text) = serde_json::to_string_pretty(&clip) else { return };
        if let Err(e) = std::fs::write(&path, text) {
            tracing::warn!(%e, "could not save the prefab");
            return;
        }
        // Every other instance, in every scene, each rebuilt on top of the last.
        let mut scratch = (*workspace).clone();
        let mut commands = Vec::new();
        for sc in &workspace.scenes {
            // its instances: by its id (or, from before ids, by the file's name)
            let same = |l: &&Object| match object.prefab_id {
                Some(id) => l.prefab_id == Some(id),
                None => l.prefab_id.is_none() && l.prefab.as_deref() == Some(file.as_str()),
            };
            for l in sc.objects.iter().filter(same).filter(|l| l.id != root) {
                let (cmds, _) = clip.replace_instance(&scratch, sc.id, l.id);
                for c in &cmds {
                    let _ = c.apply(&mut scratch);
                }
                commands.extend(cmds);
            }
        }
        self.refresh_assets();
        if !commands.is_empty() {
            self.exec(Command::Batch { label: "Apply to prefab".into(), commands }, cx);
        }
    }
}
