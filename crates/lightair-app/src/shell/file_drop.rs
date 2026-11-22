//! Files dragged in from Finder or Explorer (docs/plans/drag-drop-files.md): onto the canvas, a
//! Hierarchy row (above, into or below it), the Hierarchy's empty space, Assets or a folder in it.
//!
//! What can come in is what Assets knows ([`AssetKind`]): pictures, video, sound, prefabs, scenes,
//! avatars (a Live2D model with its whole folder) and folders of those; an OBS scene collection
//! offers the OBS import. Anything else is turned away with one line in the status bar naming it.
//! Files are copied into the assets folder (a free name when one is taken; nothing copied for a
//! file already in it), off the UI thread, with the progress in the status bar for big ones; then
//! everything dropped is placed in one undo step and the last one selected.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use gpui_kit::*;
use lightair_model::Id;

use super::StudioWindow;
use super::assets::{Asset, AssetKind, Placement, free_name, live2d_model_in};
use super::hierarchy::TreeDrop;

/// Where files were dropped.
#[derive(Clone, Debug)]
pub(super) enum DropTo {
    /// Into this folder of Assets (copied only).
    Assets(PathBuf),
    /// A Hierarchy row (`None`: below the rows).
    Tree(Option<(Id, TreeDrop)>),
    /// The canvas.
    Canvas(Placement),
}

/// What a drop will do: the copies (from, to, whole folder), the assets to place in order, the
/// OBS collections to offer, and what's turned away.
#[derive(Default)]
struct Plan {
    copies: Vec<(PathBuf, PathBuf, Copy)>,
    placed: Vec<PathBuf>,
    obs: Vec<PathBuf>,
    refused: Vec<String>,
    bytes: u64,
}

#[derive(Clone, Copy, PartialEq)]
enum Copy {
    File,
    /// A folder with everything in it (a Live2D model's).
    Whole,
    /// A folder with only what Lightair can use in it.
    Usable,
}

/// An OBS scene collection: a JSON file with its scenes' order and sources.
pub(super) fn is_obs_collection(path: &Path) -> bool {
    let name = path.file_name().map(|n| n.to_string_lossy().to_lowercase()).unwrap_or_default();
    if !name.ends_with(".json")
        || name.ends_with(".model3.json")
        || std::fs::metadata(path).map_or(true, |m| m.len() > 64 << 20)
    {
        return false;
    }
    std::fs::read_to_string(path)
        .ok()
        .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
        .is_some_and(|v| v.get("scene_order").is_some() && v.get("sources").is_some())
}

/// Whether Lightair can use a file or folder dropped in.
fn usable(path: &Path) -> bool {
    path.is_dir() || AssetKind::of(path) != AssetKind::Other
}

fn size_of(path: &Path, copy: Copy) -> u64 {
    if path.is_file() {
        return std::fs::metadata(path).map_or(0, |m| m.len());
    }
    std::fs::read_dir(path)
        .map(|r| {
            r.flatten()
                .map(|e| e.path())
                .filter(|p| !hidden(p) && (copy == Copy::Whole || usable(p)))
                .map(|p| size_of(&p, if live2d_model_in(&p).is_some() { Copy::Whole } else { copy }))
                .sum()
        })
        .unwrap_or(0)
}

fn hidden(p: &Path) -> bool {
    p.file_name().is_some_and(|n| n.to_string_lossy().starts_with('.'))
}

fn file_name(p: &Path) -> String {
    p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
}

/// What's in a folder Lightair can place, by name: files and avatar folders (not other folders).
pub(super) fn placeable_in(dir: &Path) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = std::fs::read_dir(dir)
        .map(|r| {
            r.flatten()
                .map(|e| e.path())
                .filter(|p| {
                    !hidden(p)
                        && (if p.is_dir() {
                            live2d_model_in(p).is_some()
                        } else {
                            AssetKind::of(p) != AssetKind::Other
                        })
                })
                .collect()
        })
        .unwrap_or_default();
    out.sort_by_key(|p| file_name(p).to_lowercase());
    out
}

fn plan(paths: &[PathBuf], assets: &Path, into: &Path) -> Plan {
    let mut p = Plan::default();
    // (names taken by this drop already, before anything is copied)
    let mut taken: Vec<PathBuf> = Vec::new();
    let free = |stem: &str, ext: &str, taken: &mut Vec<PathBuf>| {
        let mut to = free_name(into, stem, ext);
        let mut n = 2;
        while taken.contains(&to) {
            let dot = if ext.is_empty() { String::new() } else { format!(".{ext}") };
            to = into.join(format!("{stem} {n}{dot}"));
            n += 1;
        }
        taken.push(to.clone());
        to
    };
    for path in paths {
        let inside = path.starts_with(assets);
        let name = file_name(path);
        let lower = name.to_lowercase();
        if path.is_dir() {
            let copy = if live2d_model_in(path).is_some() { Copy::Whole } else { Copy::Usable };
            let to = if inside { path.clone() } else { free(&name, "", &mut taken) };
            if !inside {
                p.bytes += size_of(path, copy);
                p.copies.push((path.clone(), to.clone(), copy));
            }
            p.placed.push(to);
        } else if lower.ends_with(".model3.json") {
            // a Live2D model: its whole folder (textures, physics, expressions, motions)
            let Some(folder) = path.parent() else { continue };
            if inside {
                p.placed.push(path.clone());
            } else {
                let to = free(&file_name(folder), "", &mut taken);
                p.bytes += size_of(folder, Copy::Whole);
                p.copies.push((folder.to_path_buf(), to.clone(), Copy::Whole));
                p.placed.push(to.join(&name));
            }
        } else if AssetKind::of(path) != AssetKind::Other {
            if inside {
                p.placed.push(path.clone());
            } else {
                let stem = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| "Asset".into());
                let ext = path.extension().map(|e| e.to_string_lossy().into_owned()).unwrap_or_default();
                let to = free(&stem, &ext, &mut taken);
                p.bytes += size_of(path, Copy::File);
                p.copies.push((path.clone(), to.clone(), Copy::File));
                p.placed.push(to);
            }
        } else if is_obs_collection(path) {
            p.obs.push(path.clone());
        } else {
            p.refused.push(name);
        }
    }
    p
}

/// Copies a file or folder, adding what's copied to `done`.
fn copy(from: &Path, to: &Path, how: Copy, done: &AtomicU64) -> std::io::Result<()> {
    if how == Copy::File {
        let n = std::fs::copy(from, to)?;
        done.fetch_add(n, Ordering::Relaxed);
        return Ok(());
    }
    std::fs::create_dir_all(to)?;
    for e in std::fs::read_dir(from)?.flatten() {
        let p = e.path();
        if hidden(&p) {
            continue;
        }
        let dest = to.join(e.file_name());
        if p.is_dir() {
            let inner = if how == Copy::Whole || live2d_model_in(&p).is_some() { Copy::Whole } else { Copy::Usable };
            copy(&p, &dest, inner, done)?;
        } else if how == Copy::Whole || usable(&p) {
            copy(&p, &dest, Copy::File, done)?;
        }
    }
    Ok(())
}

/// "Lightair can't use .psd files: Cover.psd" for what a drop turned away.
fn refusal(refused: &[String]) -> Option<String> {
    let first = refused.first()?;
    let ext = |n: &str| Path::new(n).extension().map(|e| format!(".{}", e.to_string_lossy().to_lowercase()));
    let exts: Vec<Option<String>> = refused.iter().map(|n| ext(n)).collect();
    let same = exts.iter().all(|e| *e == exts[0]);
    Some(match (refused.len(), same, &exts[0]) {
        (1, _, Some(e)) => format!("Lightair can't use {e} files: {first}"),
        (1, _, None) => format!("Lightair can't use {first}"),
        (n, true, Some(e)) => format!("Lightair can't use {e} files: {n} left out"),
        (n, _, _) => format!("Lightair can't use {n} of these files: {}", refused.join(", ")),
    })
}

impl StudioWindow {
    /// Takes files dropped from Finder or Explorer.
    pub(super) fn drop_files(&mut self, paths: &[PathBuf], to: DropTo, window: &mut Window, cx: &mut Context<Self>) {
        let Some(assets) = self.studio.read(cx).assets_dir() else { return };
        let into = match &to {
            DropTo::Assets(folder) if folder.starts_with(&assets) => folder.clone(),
            _ => self.asset_folder.clone().filter(|f| f.starts_with(&assets) && f.is_dir()).unwrap_or(assets.clone()),
        };
        let _ = std::fs::create_dir_all(&into);
        let plan = plan(paths, &assets, &into);
        let note = refusal(&plan.refused);
        let total = plan.bytes.max(1);
        let copies = plan.copies;
        let (placed, obs) = (plan.placed, plan.obs);
        let done = Arc::new(AtomicU64::new(0));
        let finished = Arc::new(AtomicBool::new(false));
        let (d, f) = (done.clone(), finished.clone());
        let job = cx.background_executor().spawn(async move {
            let mut failed = Vec::new();
            for (from, to, how) in &copies {
                if let Err(e) = copy(from, to, *how, &d) {
                    tracing::warn!(%e, from = %from.display(), "could not copy into assets");
                    failed.push(to.clone());
                }
            }
            f.store(true, Ordering::Release);
            failed
        });
        let first = placed.first().map(|p| file_name(p)).unwrap_or_default();
        cx.spawn_in(window, async move |this, cx| {
            // big copies: their progress in the status bar
            while !finished.load(Ordering::Acquire) {
                cx.background_executor().timer(Duration::from_millis(150)).await;
                if total > 16 << 20 {
                    let pct = done.load(Ordering::Relaxed) * 100 / total;
                    let line = if placed.len() > 1 {
                        format!("Copying {} files… {pct}%", placed.len())
                    } else {
                        format!("Copying {first}… {pct}%")
                    };
                    let _ = this.update(cx, |this, cx| {
                        this.studio.update(cx, |s, cx| {
                            s.issue = Some(line.into());
                            cx.notify();
                        })
                    });
                }
            }
            let failed = job.await;
            let _ = this.update_in(cx, |this, window, cx| {
                this.refresh_assets();
                let failed_note = (!failed.is_empty()).then(|| format!("{} couldn't be copied", file_name(&failed[0])));
                this.studio.update(cx, |s, cx| {
                    s.issue = note.clone().or(failed_note).map(Into::into);
                    cx.notify();
                });
                let assets: Vec<Asset> = placed
                    .iter()
                    .filter(|p| !failed.contains(p) && p.exists())
                    .map(|p| Asset { kind: AssetKind::of(p), path: p.clone() })
                    .collect();
                match to {
                    DropTo::Assets(_) => {
                        this.selected_asset = assets.last().map(|a| a.path.clone());
                    }
                    DropTo::Tree(target) => {
                        if let Some(at) = this.tree_placement(target, cx) {
                            this.place_assets(&assets, at, cx);
                        }
                    }
                    DropTo::Canvas(at) => this.place_assets(&assets, at, cx),
                }
                for path in obs {
                    let name = file_name(&path);
                    let answer = window.prompt(
                        PromptLevel::Info,
                        &format!("Import “{name}” as an OBS scene collection?"),
                        Some("Its scenes and sources are added to this workspace."),
                        &["Import", "Cancel"],
                        cx,
                    );
                    cx.spawn(async move |this, cx| {
                        if answer.await == Ok(0) {
                            let _ = this.update(cx, |this, cx| this.studio.update(cx, |s, cx| s.import_obs(&path, cx)));
                        }
                    })
                    .detach();
                }
                cx.notify();
            });
        })
        .detach();
    }
}
